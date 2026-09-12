//! Implementation of [`cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer`]

use std::{
    collections::{HashMap, HashSet},
    future::Future,
};

use async_broadcast::TrySendError;
use bitcoin::{Block, BlockHash, OutPoint, Transaction, Txid, hashes::Hash as _};
use cusf_enforcer_mempool::cusf_enforcer::{
    ConnectBlockAction, CusfEnforcer, DisconnectBlockAction, SyncToTipError, TxAcceptAction,
};
use error_fatality::{Nested as _, Split};
use fallible_iterator::FallibleIterator;
use miette::Diagnostic;
use ouroboros::self_referencing;
use sneed::{RoTxn, RwTxn, db, env, rwtxn};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    errors::ErrorChain,
    messages::{parse_m8_tx, parse_op_drivechain},
    proto::mainchain::HeaderSyncProgress,
    types::{Ctip, Event, SidechainNumber},
    validator::{
        Validator,
        dbs::SeenDeposits,
        task::{self, BlockHandler, error::ValidateTransaction as ValidateTransactionError},
    },
};

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct SyncError(#[from] task::error::Sync);

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct InvalidBlockReason(Box<<task::error::ConnectBlock as Split>::Jfyi>);

#[derive(Debug, Diagnostic, Error)]
enum ConnectBlockErrorInner {
    #[error(transparent)]
    CommitWriteTxn(#[from] rwtxn::error::Commit),
    #[error(transparent)]
    ConnectBlock(#[from] Box<<task::error::ConnectBlock as Split>::Fatal>),
    #[error(transparent)]
    Db(Box<db::Error>),
    #[error(transparent)]
    DbPut(#[from] db::error::Put),
    #[error(transparent)]
    DbTryGet(#[from] db::error::TryGet),
    #[error(transparent)]
    DbRange(Box<db::error::Range>),
    #[error(transparent)]
    NestedWriteTxn(#[from] env::error::NestedWriteTxn),
    #[error(transparent)]
    WriteTxn(#[from] env::error::WriteTxn),
}

impl From<db::Error> for ConnectBlockErrorInner {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

impl From<db::error::Range> for ConnectBlockErrorInner {
    fn from(err: db::error::Range) -> Self {
        Self::DbRange(Box::new(err))
    }
}

impl From<<task::error::ConnectBlock as Split>::Fatal> for ConnectBlockErrorInner {
    fn from(err: <task::error::ConnectBlock as Split>::Fatal) -> Self {
        Self::from(Box::new(err))
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct ConnectBlockError(ConnectBlockErrorInner);

impl<Err> From<Err> for ConnectBlockError
where
    ConnectBlockErrorInner: From<Err>,
{
    fn from(err: Err) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Diagnostic, Error)]
enum DisconnectBlockErrorInner {
    #[error(transparent)]
    CommitWriteTxn(#[from] rwtxn::error::Commit),
    #[error(transparent)]
    DisconnectBlock(#[from] task::error::DisconnectBlock),
    #[error(transparent)]
    WriteTxn(#[from] env::error::WriteTxn),
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct DisconnectBlockError(DisconnectBlockErrorInner);

impl<Err> From<Err> for DisconnectBlockError
where
    DisconnectBlockErrorInner: From<Err>,
{
    fn from(err: Err) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Diagnostic, Error)]
enum AcceptTxErrorInner {
    #[error(transparent)]
    Commit(#[from] rwtxn::error::Commit),
    #[error(transparent)]
    Db(#[from] db::Error),
    #[error(transparent)]
    ValidateTransaction(#[from] ValidateTransactionError),
    #[error(transparent)]
    WriteTxn(#[from] env::error::WriteTxn),
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct AcceptTxError(AcceptTxErrorInner);

impl<Err> From<Err> for AcceptTxError
where
    AcceptTxErrorInner: From<Err>,
{
    fn from(err: Err) -> Self {
        Self(err.into())
    }
}

/// Parent and child rwtxn
#[self_referencing]
struct ParentChildRwTxn<'a> {
    parent: RwTxn<'a>,
    // Annotated not_covariant because covariance is not needed.
    // May be covariant
    #[borrows(mut parent)]
    #[not_covariant]
    child: RwTxn<'this>,
}

impl<'a> ParentChildRwTxn<'a> {
    /// Abort child rwtxn and return parent
    fn abort_child(self) -> RwTxn<'a> {
        let ((), heads) = self.destruct_into_heads(|tails| tails.child.abort());
        heads.parent
    }

    /// Commit child rwtxn and return parent
    fn commit_child(self) -> Result<RwTxn<'a>, rwtxn::error::Commit> {
        let (commit_res, heads) = self.destruct_into_heads(|tails| tails.child.commit());
        let () = commit_res?;
        Ok(heads.parent)
    }
}

#[derive(Debug, Error)]
enum RejectReason {
    #[error(transparent)]
    ConnectBlock(#[from] <task::error::ConnectBlock as Split>::Jfyi),
    #[error("Missing parent (`{parent}`) height for block hash `{block_hash}`")]
    MissingParentHeight {
        block_hash: BlockHash,
        parent: BlockHash,
    },
}

/// Connect block action, with rwtxns that can be committed or aborted
enum ConnectBlockRwTxnAction<'a> {
    Accept {
        event: Event,
        remove_mempool_txs: HashSet<Txid>,
        rwtxns: ParentChildRwTxn<'a>,
    },
    Reject {
        /// rwtxn to write header
        header_rwtxn: RwTxn<'a>,
        reason: RejectReason,
    },
}

/// Connect a block without commiting the rwtxn.
/// The rwtxn is returned and can be committed or aborted.
/// If connecting the block results in a header write, the header write is
/// always committed. The block connect is not committed.
#[expect(clippy::result_large_err)]
fn connect_block_no_commit<'validator>(
    validator: &'validator Validator,
    block: &Block,
) -> Result<ConnectBlockRwTxnAction<'validator>, ConnectBlockError> {
    let block_hash = block.block_hash();
    let parent = block.header.prev_blockhash;
    // Always commit, to store header if necessary
    let mut parent_rwtxn = validator.dbs.write_txn()?;
    if !validator
        .dbs
        .block_hashes
        .contains_header(&parent_rwtxn, &block_hash)?
    {
        let height = if parent == BlockHash::all_zeros() {
            0
        } else if let Some(parent_height) = validator
            .dbs
            .block_hashes
            .height()
            .try_get(&parent_rwtxn, &parent)?
        {
            parent_height + 1
        } else {
            let reject_reason = RejectReason::MissingParentHeight { block_hash, parent };
            return Ok(ConnectBlockRwTxnAction::Reject {
                header_rwtxn: parent_rwtxn,
                reason: reject_reason,
            });
        };
        tracing::trace!("Storing header");
        validator
            .dbs
            .block_hashes
            .put_headers(&mut parent_rwtxn, &[(block.header, height)])?;
    }
    // Commit on block accept, abort on block reject
    let mut parent_child_rwtxn = ParentChildRwTxnTryBuilder {
        parent: parent_rwtxn,
        child_builder: |parent: &mut RwTxn| validator.dbs.nested_write_txn(parent),
    }
    .try_build()?;
    let handler = BlockHandler::new(&validator.dbs, validator.network, validator.network_params);
    match parent_child_rwtxn
        .with_child_mut(|child_rwtxn| handler.connect_block(child_rwtxn, block))
        .into_nested()?
    {
        Ok(event) => {
            let mut remove_mempool_txs: HashSet<Txid> = parent_child_rwtxn
                .with_child(|child_rotxn| {
                    validator
                        .dbs
                        .block_hashes
                        .get_seen_bmm_requests_for_parent_block(child_rotxn, parent)
                })?
                .into_values()
                .flat_map(|bmm_requests| bmm_requests.into_values().flatten())
                .collect();
            // The competing deposits that this block did not connect can never
            // be connected either, as they do not spend the treasury UTXO that
            // it created. Nothing else evicts them, and block production wedges
            // on them for as long as they are mirrored.
            let obsolete_seen_deposits = parent_child_rwtxn.with_child_mut(|child_rwtxn| {
                validator.dbs.take_obsolete_seen_deposits(child_rwtxn)
            })?;
            for (ctip, seen_deposits) in obsolete_seen_deposits {
                let first_deposit = seen_deposits.get(&ctip.outpoint).copied();
                remove_mempool_txs.extend(competing_deposits(&seen_deposits, first_deposit));
            }
            Ok(ConnectBlockRwTxnAction::Accept {
                event,
                remove_mempool_txs,
                rwtxns: parent_child_rwtxn,
            })
        }
        Err(jfyi) => {
            let header_rwtxn = parent_child_rwtxn.abort_child();
            Ok(ConnectBlockRwTxnAction::Reject {
                header_rwtxn,
                reason: RejectReason::ConnectBlock(jfyi),
            })
        }
    }
}

/// Used to specify commit/dry-run modes
trait ConnectBlockMode<'validator> {
    type Output;

    fn connect_block(
        self,
        validator: &'validator Validator,
        block: &Block,
    ) -> Result<Self::Output, ConnectBlockError>;
}

/// Used to implement `ConnectBlockMode`.
/// Connects and commits a block.
struct ConnectBlockCommit;

impl<'validator> ConnectBlockMode<'validator> for ConnectBlockCommit {
    type Output = ConnectBlockAction;

    fn connect_block(
        self,
        validator: &'validator Validator,
        block: &Block,
    ) -> Result<Self::Output, ConnectBlockError> {
        match connect_block_no_commit(validator, block)? {
            ConnectBlockRwTxnAction::Accept {
                event,
                remove_mempool_txs,
                rwtxns,
            } => {
                tracing::info!("accepted block");
                let rwtxn = rwtxns.commit_child()?;
                rwtxn.commit()?;
                // Events should only ever be sent after committing DB txs, see
                // https://github.com/LayerTwo-Labs/bip300301_enforcer/pull/185
                let _send_err: Result<Option<_>, TrySendError<_>> =
                    validator.events_tx.try_broadcast(event);
                Ok(ConnectBlockAction::Accept { remove_mempool_txs })
            }
            ConnectBlockRwTxnAction::Reject {
                header_rwtxn,
                reason,
            } => {
                tracing::info!("rejecting block: {:#}", ErrorChain::new(&reason));
                header_rwtxn.commit()?;
                Ok(ConnectBlockAction::Reject)
            }
        }
    }
}

/// Used to implement `ConnectBlockMode`.
/// Connects a block, but aborts the rwtxn.
/// If the block is accepted, the function is executed on the rwtxn state
/// before aborting, and the result of the function is returned.
/// If the block is rejected, the rejection reason is returned.
#[repr(transparent)]
struct ConnectBlockDryRun<F>(F);

impl<'validator, F, Output> ConnectBlockMode<'validator> for ConnectBlockDryRun<F>
where
    F: FnOnce(&RoTxn<'_>) -> Output,
{
    type Output = Result<Output, RejectReason>;

    #[tracing::instrument(name = "connect_block(dry run)", skip_all)]
    fn connect_block(
        self,
        validator: &'validator Validator,
        block: &Block,
    ) -> Result<Self::Output, ConnectBlockError> {
        let rwtxns = match connect_block_no_commit(validator, block)? {
            ConnectBlockRwTxnAction::Accept {
                event: _,
                rwtxns,
                remove_mempool_txs: _,
            } => rwtxns,
            ConnectBlockRwTxnAction::Reject {
                header_rwtxn,
                reason,
            } => {
                tracing::warn!("rejecting block: {:#}", ErrorChain::new(&reason));
                header_rwtxn.abort();
                return Ok(Err(reason));
            }
        };
        let res: Output = rwtxns.with_child(|child_rwtxn| self.0(child_rwtxn));
        let rwtxn = rwtxns.abort_child();
        rwtxn.abort(); // We don't want the effects of the block to be applied!
        Ok(Ok(res))
    }
}

/// An M5 deposit into an empty treasury does not spend a treasury UTXO, so
/// competing first deposits for the same slot are not double-spends of each
/// other, and all of them enter the mempool. Only the deposits of a single
/// chain can ever be connected, so the deposits of the other chains must be
/// reported as conflicts, and evicted once the treasury exists.
///
/// Returns the first deposit of the chain that `tx` extends, which is `txid`
/// itself if `tx` starts a new chain.
fn deposit_chain_root(seen_deposits: &SeenDeposits, tx: &Transaction, txid: Txid) -> Txid {
    tx.input
        .iter()
        .find_map(|input| seen_deposits.get(&input.previous_output).copied())
        .unwrap_or(txid)
}

/// Txids of the seen deposits that are not part of the chain rooted at
/// `first_deposit`. See [`deposit_chain_root`].
fn competing_deposits(seen_deposits: &SeenDeposits, first_deposit: Option<Txid>) -> HashSet<Txid> {
    seen_deposits
        .iter()
        .filter(|(_, root)| Some(**root) != first_deposit)
        .map(|(outpoint, _)| outpoint.txid)
        .collect()
}

impl CusfEnforcer for Validator {
    type InvalidBlockReason = InvalidBlockReason;
    type SyncError = SyncError;

    async fn sync_to_tip<Signal>(
        &mut self,
        shutdown_signal: Signal,
        tip: BlockHash,
    ) -> Result<(), SyncToTipError<Self::InvalidBlockReason, Self::SyncError>>
    where
        Signal: Future<Output = ()> + Send,
    {
        let cancel = CancellationToken::new();

        let header_sync_progress_tx = {
            let mut header_sync_progress_rx_write = self.header_sync_progress_rx.write();
            if header_sync_progress_rx_write.is_some() {
                return Err(SyncError::from(task::error::Sync::HeaderSyncInProgress).into());
            }
            let (header_sync_progress_tx, header_sync_progress_rx) =
                tokio::sync::watch::channel(HeaderSyncProgress {
                    current_height: None,
                });
            *header_sync_progress_rx_write = Some(header_sync_progress_rx);
            header_sync_progress_tx
        };
        tracing::debug!(block_hash = %tip, "Syncing to tip");

        let handler = BlockHandler::new(&self.dbs, self.network, self.network_params);
        let sync_future = handler.sync_to_tip(
            &self.mainchain_client,
            self.mainchain_rest_client.as_ref(),
            self.mainchain_blocks_dir.clone(),
            tip,
            task::SyncSignals {
                cancel: cancel.clone(),
                header_sync_progress_tx,
                event_tx: self.events_tx.clone(),
            },
        );

        tokio::select! {
            result = sync_future => {
                *self.header_sync_progress_rx.write() = None;
                match result {
                    Ok(None) => Ok(()),
                    Ok(Some(invalid_block)) => Err(SyncToTipError::InvalidBlock {
                        block_hash: invalid_block.block_hash,
                        reason: InvalidBlockReason(invalid_block.reason),
                    }),
                    Err(err) => Err(SyncError(err).into()),
                }
            }
            _ = shutdown_signal => {
                cancel.cancel();
                *self.header_sync_progress_rx.write() = None;
                Err(SyncError(crate::validator::task::error::Sync::Shutdown).into())
            }
        }
    }

    type ConnectBlockError = ConnectBlockError;

    async fn connect_block(
        &mut self,
        block: &Block,
    ) -> Result<ConnectBlockAction, Self::ConnectBlockError> {
        ConnectBlockCommit.connect_block(self, block)
    }

    type DisconnectBlockError = DisconnectBlockError;

    async fn disconnect_block(
        &mut self,
        block_hash: BlockHash,
    ) -> Result<DisconnectBlockAction, Self::DisconnectBlockError> {
        let mut rwtxn = self.dbs.write_txn()?;
        let handler = BlockHandler::new(&self.dbs, self.network, self.network_params);
        let mut events = Vec::new();
        let () = handler.disconnect_block(&mut rwtxn, &mut events, block_hash)?;
        rwtxn.commit()?;
        crate::validator::task::broadcast_events(&self.events_tx, events);
        Ok(DisconnectBlockAction::default())
    }

    type AcceptTxError = AcceptTxError;

    fn accept_tx(&mut self, tx: &Transaction) -> Result<TxAcceptAction, Self::AcceptTxError> {
        let mut rwtxn = self.dbs.write_txn()?;
        // A fatal error here isn't something that means we should
        // call out to the `invalidateblock` RPC. It simply means
        // the transaction will not be accepted into the mempool.
        let handler = BlockHandler::new(&self.dbs, self.network, self.network_params);
        let res = if handler.validate_tx(&mut rwtxn, tx)? {
            let (mut conflicts_with, weight_tweak) = if let Some(bmm_request) = parse_m8_tx(tx) {
                let txid = tx.compute_txid();
                let conflicts_with = {
                    let mut seen_bmm_request_txs = self
                        .dbs
                        .block_hashes
                        .get_seen_bmm_requests(
                            &rwtxn,
                            bmm_request.prev_mainchain_block_hash,
                            bmm_request.sidechain_number,
                        )?
                        .into_values()
                        .flatten()
                        .collect::<HashSet<_>>();
                    seen_bmm_request_txs.remove(&txid);
                    seen_bmm_request_txs
                };
                let () = self
                    .dbs
                    .block_hashes
                    .put_seen_bmm_request(
                        &mut rwtxn,
                        bmm_request.prev_mainchain_block_hash,
                        bmm_request.sidechain_number,
                        txid,
                        bmm_request.sidechain_block_hash,
                    )
                    .map_err(db::Error::from)?;

                /// Weight, in wu, of the BMM accept (M7) coinbase output that block
                /// production appends for an accepted BMM request (M8).
                /// The M7 txout is pure non-witness data, so its weight is
                /// `size * WITNESS_SCALE_FACTOR`, where the size in bytes is
                /// `value (8) + script_pubkey length prefix (1) + script_pubkey (39)`.
                const BMM_ACCEPT_OUTPUT_WEIGHT: i64 = {
                    let spk_size: i64 = 39;
                    (8 + 1 + spk_size) * bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR as i64
                };

                (conflicts_with, BMM_ACCEPT_OUTPUT_WEIGHT)
            } else {
                (HashSet::new(), 0)
            };
            // Treasury outputs that this tx creates for slots whose treasury
            // does not exist yet, ie. the M5 deposits that
            // `deposit_chain_root` must reconcile. A deposit into a slot that
            // already has a ctip must spend it, so competing deposits there
            // are ordinary double-spends that never coexist in the mempool,
            // and an inactive slot has no treasury at all.
            let active_sidechains = &self.dbs.active_sidechains;
            let mut new_treasury_vouts = Vec::new();
            for (vout, output) in tx.output.iter().enumerate() {
                let Ok((_input, sidechain_number)) =
                    parse_op_drivechain(output.script_pubkey.as_bytes())
                else {
                    continue;
                };
                if !active_sidechains
                    .sidechain()
                    .contains_key(&rwtxn, &sidechain_number)
                    .map_err(db::Error::from)?
                    || active_sidechains
                        .ctip()
                        .contains_key(&rwtxn, &sidechain_number)
                        .map_err(db::Error::from)?
                {
                    continue;
                }
                new_treasury_vouts.push((sidechain_number, vout as u32));
            }
            if !new_treasury_vouts.is_empty() {
                let txid = tx.compute_txid();
                for (sidechain_number, vout) in new_treasury_vouts {
                    let seen_deposits = self.dbs.get_seen_deposits(&rwtxn, sidechain_number)?;
                    let first_deposit = deposit_chain_root(&seen_deposits, tx, txid);
                    conflicts_with.extend(competing_deposits(&seen_deposits, Some(first_deposit)));
                    let () = self.dbs.put_seen_deposit(
                        &mut rwtxn,
                        sidechain_number,
                        OutPoint { txid, vout },
                        first_deposit,
                    )?;
                }
            }
            rwtxn.commit()?;
            TxAcceptAction::Accept {
                conflicts_with,
                weight_tweak,
            }
        } else {
            TxAcceptAction::Reject
        };
        Ok(res)
    }

    type ValidateBlockError = ConnectBlockError;

    fn validate_block(&self, block: &Block) -> Result<Option<String>, Self::ValidateBlockError> {
        match ConnectBlockDryRun(|_: &RoTxn<'_>| ()).connect_block(self, block)? {
            Ok(()) => Ok(None),
            Err(reason) => Ok(Some(format!("{:#}", ErrorChain::new(&reason)))),
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum GetCtipsAfterError {
    #[error(transparent)]
    ConnectBlock(#[from] ConnectBlockError),
    #[error(transparent)]
    DbIter(#[from] db::error::Iter),
}

/// Get ctips after (speculatively) applying a block.
/// Returns the rejection reason if the block would be rejected.
pub(crate) fn get_ctips_after(
    validator: &Validator,
    block: &Block,
) -> Result<Result<HashMap<SidechainNumber, Ctip>, String>, GetCtipsAfterError> {
    match ConnectBlockDryRun(|rotxn: &RoTxn<'_>| -> Result<_, _> {
        validator
            .dbs
            .active_sidechains
            .ctip()
            .iter(rotxn)
            .map_err(db::error::Iter::Init)?
            .collect()
            .map_err(db::error::Iter::Item)
    })
    .connect_block(validator, block)?
    {
        Ok(ctips) => Ok(Ok(ctips?)),
        Err(reason) => Ok(Err(format!("{:#}", ErrorChain::new(&reason)))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use bitcoin::{Amount, OutPoint, Transaction, Txid, hashes::Hash as _};
    use miette::IntoDiagnostic as _;

    use super::{SeenDeposits, competing_deposits, deposit_chain_root};
    use crate::{
        messages::CoinbaseBuilder,
        types::{BmmCommitment, Ctip, SidechainNumber},
        validator::test_utils::create_test_dbs,
    };

    /// `TxAcceptAction::weight_tweak` is specified in weight units, so the
    /// tweak reported for an M8 must be the weight of the M7 accept output
    /// that block production will append for it, not its size in bytes.
    #[test]
    fn bmm_accept_output_weight_matches_produced_txout() -> miette::Result<()> {
        let mut coinbase_txouts = Vec::new();
        let mut coinbase_builder = CoinbaseBuilder::new(&mut coinbase_txouts).into_diagnostic()?;
        coinbase_builder
            .bmm_accept(SidechainNumber(0), BmmCommitment([0; 32]))
            .into_diagnostic()?;
        let coinbase_txouts_suffix = coinbase_builder.build_extension().into_diagnostic()?;
        let [bmm_accept_txout] = coinbase_txouts_suffix.as_slice() else {
            return Err(miette::miette!(
                "expected exactly one BMM accept txout, got {}",
                coinbase_txouts_suffix.len()
            ));
        };
        assert_eq!(192, bmm_accept_txout.weight().to_wu() as i64);
        Ok(())
    }

    fn dummy_txid(byte: u8) -> Txid {
        Txid::from_byte_array([byte; 32])
    }

    /// Spend the treasury outpoints (`txid`, 0) of the provided txids.
    fn tx_spending(txids: &[Txid]) -> Transaction {
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: txids
                .iter()
                .map(|txid| bitcoin::TxIn {
                    previous_output: OutPoint {
                        txid: *txid,
                        vout: 0,
                    },
                    script_sig: bitcoin::ScriptBuf::new(),
                    sequence: bitcoin::Sequence::MAX,
                    witness: bitcoin::Witness::new(),
                })
                .collect(),
            output: Vec::new(),
        }
    }

    /// Seen deposits for a chain `a0 -> a1 -> a2` into an empty treasury, and
    /// a competing first deposit `b0`.
    fn competing_seen_deposits() -> (SeenDeposits, [Txid; 4]) {
        let [a0, a1, a2, b0] = [0xa0, 0xa1, 0xa2, 0xb0].map(dummy_txid);
        let seen_deposits = [(a0, a0), (a1, a0), (a2, a0), (b0, b0)]
            .into_iter()
            .map(|(txid, first_deposit)| (OutPoint { txid, vout: 0 }, first_deposit))
            .collect();
        (seen_deposits, [a0, a1, a2, b0])
    }

    /// A deposit that extends a seen chain of deposits into an empty treasury
    /// is compatible with every other deposit in that chain, however deep it
    /// is, and conflicts only with the deposits of competing chains.
    #[test]
    fn deposit_conflicts_are_limited_to_competing_chains() {
        let (seen_deposits, [a0, a1, a2, b0]) = competing_seen_deposits();
        // A deposit extending the tip of the `a0` chain is itself part of that
        // chain, and conflicts with `b0` only.
        let a3 = dummy_txid(0xa3);
        let first_deposit = deposit_chain_root(&seen_deposits, &tx_spending(&[a2]), a3);
        assert_eq!(first_deposit, a0);
        assert_eq!(
            competing_deposits(&seen_deposits, Some(first_deposit)),
            HashSet::from_iter([b0])
        );
        // A deposit that starts a new chain conflicts with every deposit of
        // both seen chains.
        let c0 = dummy_txid(0xc0);
        let first_deposit = deposit_chain_root(&seen_deposits, &tx_spending(&[]), c0);
        assert_eq!(first_deposit, c0);
        assert_eq!(
            competing_deposits(&seen_deposits, Some(first_deposit)),
            HashSet::from_iter([a0, a1, a2, b0])
        );
    }

    /// Competing deposits into the same empty treasury must all be retained,
    /// per slot, until a block creates the treasury. The deposits that lost
    /// are then evicted, and the slot is no longer tracked.
    #[test]
    fn obsolete_seen_deposits_are_taken_once_the_treasury_exists() -> miette::Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sidechain_number = SidechainNumber(1);
        let (seen_deposits, [a0, a1, _a2, b0]) = competing_seen_deposits();
        for (outpoint, first_deposit) in &seen_deposits {
            let () = dbs
                .put_seen_deposit(&mut rwtxn, sidechain_number, *outpoint, *first_deposit)
                .into_diagnostic()?;
        }
        assert_eq!(
            dbs.get_seen_deposits(&rwtxn, sidechain_number)
                .into_diagnostic()?,
            seen_deposits
        );
        // Deposits are not reported for other slots.
        assert!(
            dbs.get_seen_deposits(&rwtxn, SidechainNumber(2))
                .into_diagnostic()?
                .is_empty()
        );
        // While the treasury does not exist, the seen deposits are still
        // needed to reconcile further deposits.
        assert!(
            dbs.take_obsolete_seen_deposits(&mut rwtxn)
                .into_diagnostic()?
                .is_empty()
        );
        // Connecting `a1` creates the treasury, so the losers of the race are
        // reported for eviction, and the slot is dropped.
        let ctip = Ctip {
            outpoint: OutPoint { txid: a1, vout: 0 },
            value: Amount::ONE_BTC,
        };
        let _seq: u64 = dbs
            .active_sidechains
            .put_ctip(&mut rwtxn, sidechain_number, &ctip)
            .into_diagnostic()?;
        let obsolete = dbs
            .take_obsolete_seen_deposits(&mut rwtxn)
            .into_diagnostic()?;
        let [(obsolete_ctip, obsolete_seen_deposits)] = obsolete.as_slice() else {
            return Err(miette::miette!(
                "expected exactly one slot's seen deposits, got {}",
                obsolete.len()
            ));
        };
        assert_eq!(obsolete_ctip.outpoint, ctip.outpoint);
        assert_eq!(
            competing_deposits(
                obsolete_seen_deposits,
                obsolete_seen_deposits.get(&ctip.outpoint).copied()
            ),
            // Only the `b0` chain lost: `a0` and `a2` belong to the chain that
            // `a1` extends, and remain connectable.
            HashSet::from_iter([b0])
        );
        assert_eq!(a0, obsolete_seen_deposits[&ctip.outpoint]);
        assert!(
            dbs.get_seen_deposits(&rwtxn, sidechain_number)
                .into_diagnostic()?
                .is_empty()
        );
        Ok(())
    }
}
