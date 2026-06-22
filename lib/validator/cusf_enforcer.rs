//! Implementation of [`cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer`]

use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    future::Future,
};

use async_broadcast::TrySendError;
use bitcoin::{Block, BlockHash, Transaction, Txid, hashes::Hash as _};
use cusf_enforcer_mempool::cusf_enforcer::{
    ConnectBlockAction, CusfEnforcer, DisconnectBlockAction, TxAcceptAction,
};
use error_fatality::{Nested as _, Split};
use fallible_iterator::FallibleIterator;
use futures::TryFutureExt as _;
use miette::Diagnostic;
use ouroboros::self_referencing;
use sneed::{RoTxn, RwTxn, db, env, rwtxn};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    errors::ErrorChain,
    messages::{parse_m8_tx, parse_op_drivechain},
    proto::mainchain::HeaderSyncProgress,
    types::{
        BlockEvent, BlockInfo, Ctip, Event, SidechainNumber, WithdrawalBundleEvent,
        WithdrawalBundleEventKind,
    },
    validator::{
        Validator,
        dbs::Dbs,
        task::{self, BlockHandler, error::ValidateTransaction as ValidateTransactionError},
    },
};

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct SyncError(#[from] task::error::Sync);

#[derive(Debug, Diagnostic, Error)]
enum ConnectBlockErrorInner {
    #[error(transparent)]
    CommitWriteTxn(#[from] rwtxn::error::Commit),
    #[error(transparent)]
    ConnectBlock(#[from] Box<<task::error::ConnectBlock as Split>::Fatal>),
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
    let handler = BlockHandler::new(&validator.dbs, validator.network);
    match parent_child_rwtxn
        .with_child_mut(|child_rwtxn| handler.connect_block(child_rwtxn, block))
        .into_nested()?
    {
        Ok((header_info, block_info)) => {
            let remove_mempool_txs = parent_child_rwtxn
                .with_child(|child_rotxn| {
                    validator
                        .dbs
                        .block_hashes
                        .get_seen_bmm_requests_for_parent_block(child_rotxn, parent)
                })?
                .into_values()
                .flat_map(|bmm_requests| bmm_requests.into_values().flatten())
                .collect();
            let event = Event::ConnectBlock {
                header_info,
                block_info,
            };
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

/// Record `tx` as the sole unconfirmed deposit for each active treasury slot
/// it creates a UTXO for. Two deposits into the same treasury cannot both be
/// mined. BIP300 M5/M6 require spending the treasury's current Ctip, so the
/// second to be included is invalid). If a slot already holds a different
/// unconfirmed deposit this tx competes with it: `Some(existing)` is returned
/// and nothing is recorded, signalling that `tx` must be rejected.
fn track_deposit_tx(
    dbs: &Dbs,
    rotxn: &RoTxn,
    seen_deposit_txs: &mut HashMap<SidechainNumber, Txid>,
    tx: &Transaction,
    txid: Txid,
) -> Result<Option<Txid>, db::Error> {
    // Sidechains for which this tx creates a treasury UTXO.
    let mut treasury_slots = Vec::new();
    for output in &tx.output {
        let Ok((_, sidechain_number)) = parse_op_drivechain(output.script_pubkey.as_bytes()) else {
            continue;
        };
        // An OP_DRIVECHAIN output for an inactive slot is an ordinary
        // output, not a treasury UTXO
        if !dbs
            .active_sidechains
            .sidechain()
            .contains_key(rotxn, &sidechain_number)?
        {
            continue;
        }
        // A different unconfirmed deposit already holds this treasury: `tx`
        // competes with it and must be rejected.
        if let Some(existing) = seen_deposit_txs.get(&sidechain_number)
            && *existing != txid
        {
            return Ok(Some(*existing));
        }
        treasury_slots.push(sidechain_number);
    }
    for sidechain_number in treasury_slots {
        seen_deposit_txs.insert(sidechain_number, txid);
        tracing::debug!(
            %txid,
            sidechain = sidechain_number.0,
            "deposit conflict tracking: recorded sole treasury deposit"
        );
    }
    Ok(None)
}

/// Clear the recorded unconfirmed deposit for every treasury that this block
/// updates, so that a later deposit spending the new Ctip is accepted rather
/// than rejected as a competitor of the now-confirmed one.
fn clear_confirmed_deposit_slots(
    seen_deposit_txs: &mut HashMap<SidechainNumber, Txid>,
    block_info: &BlockInfo,
) {
    let updated_treasuries: HashSet<SidechainNumber> = block_info
        .events
        .iter()
        .flat_map(|block_event| match block_event {
            BlockEvent::Deposits(deposits) => deposits.keys().copied().collect(),
            BlockEvent::WithdrawalBundle(WithdrawalBundleEvent {
                sidechain_id,
                kind: WithdrawalBundleEventKind::Succeeded { .. },
                ..
            }) => vec![*sidechain_id],
            BlockEvent::SidechainProposal { .. } | BlockEvent::WithdrawalBundle(_) => Vec::new(),
        })
        .collect();
    for sidechain_number in updated_treasuries {
        if let Some(txid) = seen_deposit_txs.remove(&sidechain_number) {
            tracing::debug!(
                sidechain = sidechain_number.0,
                %txid,
                "deposit conflict tracking: treasury updated, cleared recorded deposit"
            );
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
                // The block's treasury updates are now final, so clear the
                // recorded unconfirmed deposit for each updated treasury. Done
                // post-commit so a failed commit or a dry run leaves the set
                // untouched.
                if let Event::ConnectBlock { block_info, .. } = &event {
                    clear_confirmed_deposit_slots(
                        &mut validator.seen_deposit_txs.write(),
                        block_info,
                    );
                }
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

impl CusfEnforcer for Validator {
    type SyncError = SyncError;

    async fn sync_to_tip<Signal>(
        &mut self,
        shutdown_signal: Signal,
        tip: BlockHash,
    ) -> Result<(), Self::SyncError>
    where
        Signal: Future<Output = ()> + Send,
    {
        let cancel = CancellationToken::new();

        let header_sync_progress_tx = {
            let mut header_sync_progress_rx_write = self.header_sync_progress_rx.write();
            if header_sync_progress_rx_write.is_some() {
                return Err(task::error::Sync::HeaderSyncInProgress.into());
            }
            let (header_sync_progress_tx, header_sync_progress_rx) =
                tokio::sync::watch::channel(HeaderSyncProgress {
                    current_height: None,
                });
            *header_sync_progress_rx_write = Some(header_sync_progress_rx);
            header_sync_progress_tx
        };
        tracing::debug!(block_hash = %tip, "Syncing to tip");

        let handler = BlockHandler::new(&self.dbs, self.network);
        let sync_future = handler
            .sync_to_tip(
                &self.mainchain_client,
                &self.mainchain_rest_client,
                self.mainchain_blocks_dir.clone(),
                tip,
                task::SyncSignals {
                    cancel: cancel.clone(),
                    header_sync_progress_tx,
                    event_tx: self.events_tx.clone(),
                },
            )
            .map_err(SyncError);

        tokio::select! {
            result = sync_future => {
                *self.header_sync_progress_rx.write() = None;
                result
            }
            _ = shutdown_signal => {
                cancel.cancel();
                *self.header_sync_progress_rx.write() = None;
                Err(SyncError(crate::validator::task::error::Sync::Shutdown))
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
        let handler = BlockHandler::new(&self.dbs, self.network);
        let () = handler.disconnect_block(&mut rwtxn, &self.events_tx, block_hash)?;
        rwtxn.commit()?;
        Ok(DisconnectBlockAction::default())
    }

    type AcceptTxError = AcceptTxError;

    fn accept_tx<TxRef>(
        &mut self,
        tx: &Transaction,
        _tx_inputs: &HashMap<bitcoin::Txid, TxRef>,
    ) -> Result<TxAcceptAction, Self::AcceptTxError>
    where
        TxRef: Borrow<Transaction>,
    {
        let mut rwtxn = self.dbs.write_txn()?;
        // A fatal error here isn't something that means we should
        // call out to the `invalidateblock` RPC. It simply means
        // the transaction will not be accepted into the mempool.
        let handler = BlockHandler::new(&self.dbs, self.network);
        if !handler.validate_tx(&mut rwtxn, tx)? {
            return Ok(TxAcceptAction::Reject);
        }
        let txid = tx.compute_txid();
        let mut conflicts_with = HashSet::new();
        let mut weight_tweak = 0;
        if let Some(bmm_request) = parse_m8_tx(tx) {
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
            conflicts_with.extend(seen_bmm_request_txs);
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
            // Size in bytes of a BMM accept output
            const BMM_ACCEPT_OUTPUT_SIZE: i64 = {
                let spk_size: i64 = 39;
                spk_size + 1 + 8
            };
            weight_tweak = BMM_ACCEPT_OUTPUT_SIZE;
        }
        {
            let mut seen_deposit_txs = self.seen_deposit_txs.write();
            // A deposit competing with another for the same treasury can never
            // be mined alongside it, so reject it rather than letting the block
            // template contain two. Dropping the open rwtxn here also discards
            // any BMM tracking above, which is correct since the tx is rejected.
            if let Some(competitor) =
                track_deposit_tx(&self.dbs, &rwtxn, &mut seen_deposit_txs, tx, txid)?
            {
                tracing::debug!(
                    %txid, %competitor,
                    "rejecting deposit: competes with an existing deposit for the same treasury"
                );
                return Ok(TxAcceptAction::Reject);
            }
        }
        rwtxn.commit()?;
        Ok(TxAcceptAction::Accept {
            conflicts_with,
            weight_tweak,
        })
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
    use bitcoin::{Amount, OutPoint, TxIn, TxOut, hashes::Hash as _};
    use miette::{IntoDiagnostic, Result};

    use super::*;
    use crate::{
        messages::create_m5_deposit_output,
        types::{BmmCommitments, Deposit},
        validator::test_utils::{create_test_dbs, test_m6id, test_sidechain},
    };

    const ACTIVE_SLOT: SidechainNumber = SidechainNumber(1);

    /// A tx with a treasury (OP_DRIVECHAIN) output for the given slot,
    /// spending `input` to make the txid unique
    fn treasury_tx(slot: SidechainNumber, input: u8) -> Transaction {
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::from_byte_array([input; 32]),
                    vout: 0,
                },
                ..TxIn::default()
            }],
            output: vec![create_m5_deposit_output(
                slot,
                Amount::ZERO,
                Amount::from_sat(1_000),
            )],
        }
    }

    /// Run the tracker; returns the competing deposit if `tx` must be rejected,
    /// or `None` if it was recorded.
    fn track(
        dbs: &Dbs,
        rotxn: &RoTxn,
        seen: &mut HashMap<SidechainNumber, Txid>,
        tx: &Transaction,
    ) -> Result<Option<Txid>> {
        track_deposit_tx(dbs, rotxn, seen, tx, tx.compute_txid()).into_diagnostic()
    }

    /// A treasury (OP_DRIVECHAIN) deposit for `slot` that spends `parent`'s
    /// (non-treasury) output — i.e. a deposit funded from a competing deposit's
    /// change.
    fn child_treasury_tx(slot: SidechainNumber, parent: Txid) -> Transaction {
        Transaction {
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: parent,
                    vout: 1,
                },
                ..TxIn::default()
            }],
            ..treasury_tx(slot, 0)
        }
    }

    fn block_info(events: Vec<BlockEvent>) -> BlockInfo {
        BlockInfo {
            bmm_commitments: BmmCommitments::new(),
            coinbase_txid: Txid::all_zeros(),
            events,
        }
    }

    fn deposits_event(slot: SidechainNumber) -> BlockEvent {
        let deposit = Deposit {
            sequence_number: 0,
            outpoint: OutPoint::default(),
            address: Vec::new(),
            value: Amount::from_sat(1_000),
        };
        BlockEvent::Deposits(HashMap::from([(slot, deposit)]))
    }

    fn withdrawal_bundle_event(
        slot: SidechainNumber,
        kind: WithdrawalBundleEventKind,
    ) -> BlockEvent {
        BlockEvent::WithdrawalBundle(WithdrawalBundleEvent {
            sidechain_id: slot,
            m6id: test_m6id(0x33),
            kind,
        })
    }

    #[test]
    fn competing_deposit_is_rejected() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let other_slot = SidechainNumber(2);
        for slot in [ACTIVE_SLOT, other_slot] {
            dbs.active_sidechains
                .put_sidechain(&mut rwtxn, &slot, &test_sidechain(slot.0, 0))
                .into_diagnostic()?;
        }
        let mut seen = HashMap::new();

        let tx1 = treasury_tx(ACTIVE_SLOT, 1);
        let tx1_txid = tx1.compute_txid();
        let sibling = treasury_tx(ACTIVE_SLOT, 2);
        let child = child_treasury_tx(ACTIVE_SLOT, tx1_txid);
        let other_slot_tx = treasury_tx(other_slot, 3);

        assert_eq!(
            track(&dbs, &rwtxn, &mut seen, &tx1)?,
            None,
            "the first deposit into a treasury is accepted"
        );
        assert_eq!(
            track(&dbs, &rwtxn, &mut seen, &tx1)?,
            None,
            "re-accepting the same deposit is not a conflict"
        );
        assert_eq!(
            track(&dbs, &rwtxn, &mut seen, &other_slot_tx)?,
            None,
            "a deposit into a different treasury is accepted"
        );
        assert_eq!(
            track(&dbs, &rwtxn, &mut seen, &sibling)?,
            Some(tx1_txid),
            "an independent deposit competing for the same treasury is rejected"
        );
        assert_eq!(
            track(&dbs, &rwtxn, &mut seen, &child)?,
            Some(tx1_txid),
            "a competing deposit funded from the first's change is rejected"
        );
        assert_eq!(
            seen.get(&ACTIVE_SLOT).copied(),
            Some(tx1_txid),
            "rejected competitors must not be recorded"
        );
        assert_eq!(
            seen.get(&other_slot).copied(),
            Some(other_slot_tx.compute_txid())
        );
        Ok(())
    }

    #[test]
    fn inactive_slot_and_plain_txs_are_ignored() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let rwtxn = dbs.write_txn().into_diagnostic()?;
        let inactive_slot = SidechainNumber(42);
        let mut seen = HashMap::new();

        let tx1 = treasury_tx(inactive_slot, 1);
        let tx2 = treasury_tx(inactive_slot, 2);
        let plain_tx = Transaction {
            output: vec![TxOut {
                script_pubkey: bitcoin::ScriptBuf::new(),
                value: Amount::from_sat(1_000),
            }],
            ..treasury_tx(inactive_slot, 3)
        };

        for tx in [&tx1, &tx2, &plain_tx] {
            assert_eq!(
                track(&dbs, &rwtxn, &mut seen, tx)?,
                None,
                "txs for inactive slots or without treasury outputs are accepted"
            );
        }
        assert!(
            !seen.contains_key(&inactive_slot),
            "txs for an inactive slot must not be recorded"
        );
        Ok(())
    }

    #[test]
    fn confirmed_deposit_slot_is_cleared() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let other_slot = SidechainNumber(2);
        for slot in [ACTIVE_SLOT, other_slot] {
            dbs.active_sidechains
                .put_sidechain(&mut rwtxn, &slot, &test_sidechain(slot.0, 0))
                .into_diagnostic()?;
        }
        let mut seen = HashMap::new();

        let tx1 = treasury_tx(ACTIVE_SLOT, 1);
        let other_slot_tx = treasury_tx(other_slot, 3);
        track(&dbs, &rwtxn, &mut seen, &tx1)?;
        track(&dbs, &rwtxn, &mut seen, &other_slot_tx)?;

        clear_confirmed_deposit_slots(&mut seen, &block_info(vec![deposits_event(ACTIVE_SLOT)]));
        assert!(
            !seen.contains_key(&ACTIVE_SLOT),
            "the updated treasury's recorded deposit must be cleared"
        );
        assert_eq!(
            seen.get(&other_slot).copied(),
            Some(other_slot_tx.compute_txid()),
            "other treasuries must be unaffected"
        );

        // With the slot cleared, a subsequent deposit into it is accepted.
        let tx2 = treasury_tx(ACTIVE_SLOT, 2);
        assert_eq!(track(&dbs, &rwtxn, &mut seen, &tx2)?, None);
        Ok(())
    }

    #[test]
    fn deposit_slot_only_cleared_on_successful_withdrawal_bundle() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &ACTIVE_SLOT, &test_sidechain(ACTIVE_SLOT.0, 0))
            .into_diagnostic()?;
        let mut seen = HashMap::new();

        let tx = treasury_tx(ACTIVE_SLOT, 1);
        let tx_txid = tx.compute_txid();
        track(&dbs, &rwtxn, &mut seen, &tx)?;

        for kind in [
            WithdrawalBundleEventKind::Submitted,
            WithdrawalBundleEventKind::Failed,
        ] {
            clear_confirmed_deposit_slots(
                &mut seen,
                &block_info(vec![withdrawal_bundle_event(ACTIVE_SLOT, kind)]),
            );
            assert_eq!(
                seen.get(&ACTIVE_SLOT).copied(),
                Some(tx_txid),
                "a bundle event that does not spend the treasury must not clear the deposit"
            );
        }

        let succeeded = WithdrawalBundleEventKind::Succeeded {
            sequence_number: 0,
            transaction: treasury_tx(ACTIVE_SLOT, 2),
        };
        clear_confirmed_deposit_slots(
            &mut seen,
            &block_info(vec![withdrawal_bundle_event(ACTIVE_SLOT, succeeded)]),
        );
        assert!(
            !seen.contains_key(&ACTIVE_SLOT),
            "a successful bundle spends the treasury and must clear the deposit"
        );
        Ok(())
    }
}
