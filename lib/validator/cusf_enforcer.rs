//! Implementation of [`cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer`]

use std::{
    collections::{HashMap, HashSet},
    future::Future,
};

use bitcoin::{Block, BlockHash, Transaction, Txid, hashes::Hash as _};
use cusf_enforcer_mempool::cusf_enforcer::{
    ConnectBlockAction, CusfEnforcer, DisconnectBlockAction, SyncToTipError, TxAcceptAction,
};
use error_fatality::{Nested as _, Split};
use fallible_iterator::FallibleIterator;
use miette::Diagnostic;
use sneed::{RoTxn, RwTxn, db, env, rwtxn};
use thiserror::Error;
use tokio::sync::broadcast::error::SendError;
use tokio_util::sync::CancellationToken;

use crate::{
    errors::ErrorChain,
    messages::parse_m8_tx,
    proto::mainchain::HeaderSyncProgress,
    types::{Ctip, Event, SidechainNumber},
    validator::{
        Validator,
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

/// Connect a block, leaving `mode` to commit or abort the result.
/// The block connect happens in a child rwtxn nested in the rwtxn that
/// stores its header, so that in commit mode a rejected block still keeps
/// its header. A dry run aborts both.
fn connect_block_with_mode<'validator, Mode>(
    mode: Mode,
    validator: &'validator Validator,
    block: &Block,
) -> Result<Mode::Output, ConnectBlockError>
where
    Mode: ConnectBlockMode<'validator>,
{
    let block_hash = block.block_hash();
    let parent = block.header.prev_blockhash;
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
            return Mode::reject(parent_rwtxn, reject_reason);
        };
        tracing::trace!("Storing header");
        validator
            .dbs
            .block_hashes
            .put_headers(&mut parent_rwtxn, &[(block.header, height)])?;
    }
    let mut child_rwtxn = validator.dbs.nested_write_txn(&mut parent_rwtxn)?;
    let handler = BlockHandler::new(&validator.dbs, validator.network, validator.network_params);
    match handler
        .connect_block(&mut child_rwtxn, block)
        .into_nested()?
    {
        Ok(event) => {
            let remove_mempool_txs = validator
                .dbs
                .block_hashes
                .get_seen_bmm_requests_for_parent_block(&child_rwtxn, parent)?
                .into_values()
                .flat_map(|bmm_requests| bmm_requests.into_values().flatten())
                .collect();
            let accepted = mode.finish_child(child_rwtxn, event, remove_mempool_txs)?;
            Mode::finish_parent(validator, parent_rwtxn, accepted)
        }
        Err(jfyi) => {
            child_rwtxn.abort();
            Mode::reject(parent_rwtxn, RejectReason::ConnectBlock(jfyi))
        }
    }
}

/// Used to specify commit/dry-run modes
trait ConnectBlockMode<'validator>: Sized {
    type Output;
    /// Carried from finishing the child rwtxn to finishing the parent
    type Accepted;

    fn connect_block(
        self,
        validator: &'validator Validator,
        block: &Block,
    ) -> Result<Self::Output, ConnectBlockError>;

    /// The block was accepted in `child_rwtxn`. Commit or abort it.
    fn finish_child(
        self,
        child_rwtxn: RwTxn<'_>,
        event: Event,
        remove_mempool_txs: HashSet<Txid>,
    ) -> Result<Self::Accepted, ConnectBlockError>;

    /// Commit or abort the parent rwtxn, once the child is finished.
    fn finish_parent(
        validator: &'validator Validator,
        parent_rwtxn: RwTxn<'_>,
        accepted: Self::Accepted,
    ) -> Result<Self::Output, ConnectBlockError>;

    /// The block was rejected. `header_rwtxn` holds its header, if newly
    /// stored.
    fn reject(
        header_rwtxn: RwTxn<'_>,
        reason: RejectReason,
    ) -> Result<Self::Output, ConnectBlockError>;
}

/// Used to implement `ConnectBlockMode`.
/// Connects and commits a block.
struct ConnectBlockCommit;

impl<'validator> ConnectBlockMode<'validator> for ConnectBlockCommit {
    type Output = ConnectBlockAction;
    type Accepted = (Event, HashSet<Txid>);

    fn connect_block(
        self,
        validator: &'validator Validator,
        block: &Block,
    ) -> Result<Self::Output, ConnectBlockError> {
        connect_block_with_mode(self, validator, block)
    }

    fn finish_child(
        self,
        child_rwtxn: RwTxn<'_>,
        event: Event,
        remove_mempool_txs: HashSet<Txid>,
    ) -> Result<Self::Accepted, ConnectBlockError> {
        tracing::info!("accepted block");
        child_rwtxn.commit()?;
        Ok((event, remove_mempool_txs))
    }

    fn finish_parent(
        validator: &'validator Validator,
        parent_rwtxn: RwTxn<'_>,
        (event, remove_mempool_txs): Self::Accepted,
    ) -> Result<Self::Output, ConnectBlockError> {
        parent_rwtxn.commit()?;
        // Events should only ever be sent after committing DB txs, see
        // https://github.com/LayerTwo-Labs/bip300301_enforcer/pull/185
        let _send_err: Result<usize, SendError<_>> = validator.events_tx.send(event);
        Ok(ConnectBlockAction::Accept { remove_mempool_txs })
    }

    fn reject(
        header_rwtxn: RwTxn<'_>,
        reason: RejectReason,
    ) -> Result<Self::Output, ConnectBlockError> {
        tracing::info!("rejecting block: {:#}", ErrorChain::new(&reason));
        header_rwtxn.commit()?;
        Ok(ConnectBlockAction::Reject)
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
    type Accepted = Output;

    #[tracing::instrument(name = "connect_block(dry run)", skip_all)]
    fn connect_block(
        self,
        validator: &'validator Validator,
        block: &Block,
    ) -> Result<Self::Output, ConnectBlockError> {
        connect_block_with_mode(self, validator, block)
    }

    fn finish_child(
        self,
        child_rwtxn: RwTxn<'_>,
        _event: Event,
        _remove_mempool_txs: HashSet<Txid>,
    ) -> Result<Self::Accepted, ConnectBlockError> {
        let res = self.0(&child_rwtxn);
        child_rwtxn.abort();
        Ok(res)
    }

    fn finish_parent(
        _validator: &'validator Validator,
        parent_rwtxn: RwTxn<'_>,
        res: Self::Accepted,
    ) -> Result<Self::Output, ConnectBlockError> {
        parent_rwtxn.abort(); // We don't want the effects of the block to be applied!
        Ok(Ok(res))
    }

    fn reject(
        header_rwtxn: RwTxn<'_>,
        reason: RejectReason,
    ) -> Result<Self::Output, ConnectBlockError> {
        tracing::warn!("rejecting block: {:#}", ErrorChain::new(&reason));
        header_rwtxn.abort();
        Ok(Err(reason))
    }
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
            let (conflicts_with, weight_tweak) = if let Some(bmm_request) = parse_m8_tx(tx) {
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
                rwtxn.commit()?;

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
    use miette::IntoDiagnostic as _;

    use crate::{
        messages::CoinbaseBuilder,
        types::{BmmCommitment, SidechainNumber},
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

    /// Both modes, on accepted, rejected and orphan blocks: what each leaves
    /// in the DB, and which events it sends.
    mod connect_block_modes {
        use bitcoin::{Amount, Block, BlockHash, OutPoint, Txid, hashes::Hash as _};
        use cusf_enforcer_mempool::cusf_enforcer::ConnectBlockAction;
        use miette::{IntoDiagnostic as _, Result};
        use sneed::RoTxn;
        use tokio::sync::broadcast;

        use super::super::{ConnectBlockCommit, ConnectBlockDryRun, ConnectBlockMode as _};
        use crate::{
            types::{Ctip, Event, SidechainNumber},
            validator::{
                Validator,
                test_utils::{
                    TestBlockParts, build_m5_deposit_tx, build_test_block, dummy_validator,
                    test_sidechain,
                },
            },
        };

        const SIDECHAIN: SidechainNumber = SidechainNumber(1);

        /// Validator tracking a CTIP for [`SIDECHAIN`], so that
        /// [`rejected_block`] is invalid
        fn validator(dir: &temp_dir::TempDir) -> Result<Validator> {
            let validator = dummy_validator(dir.path());
            let mut rwtxn = validator.dbs.write_txn().into_diagnostic()?;
            validator
                .dbs
                .active_sidechains
                .put_sidechain(&mut rwtxn, &SIDECHAIN, &test_sidechain(SIDECHAIN.0, 0))
                .into_diagnostic()?;
            validator
                .dbs
                .active_sidechains
                .put_ctip(
                    &mut rwtxn,
                    SIDECHAIN,
                    &Ctip {
                        outpoint: OutPoint {
                            txid: Txid::from_byte_array([0x11; 32]),
                            vout: 0,
                        },
                        value: Amount::from_sat(5_000),
                    },
                )
                .into_diagnostic()?;
            rwtxn.commit().into_diagnostic()?;
            Ok(validator)
        }

        fn accepted_block() -> Block {
            build_test_block(BlockHash::all_zeros(), TestBlockParts::default())
        }

        /// Deposits without spending the tracked CTIP
        fn rejected_block() -> Block {
            build_test_block(
                BlockHash::all_zeros(),
                TestBlockParts {
                    extra_txs: vec![build_m5_deposit_tx(
                        SIDECHAIN,
                        OutPoint::default(),
                        Amount::from_sat(5_000),
                        Amount::from_sat(1_000),
                    )],
                    ..Default::default()
                },
            )
        }

        fn orphan_block() -> Block {
            build_test_block(
                BlockHash::from_byte_array([0x22; 32]),
                TestBlockParts::default(),
            )
        }

        fn tip(validator: &Validator) -> Result<Option<BlockHash>> {
            let rotxn = validator.dbs.read_txn().into_diagnostic()?;
            validator
                .dbs
                .current_chain_tip
                .try_get(&rotxn, &())
                .into_diagnostic()
        }

        fn has_header(validator: &Validator, block: &Block) -> Result<bool> {
            let rotxn = validator.dbs.read_txn().into_diagnostic()?;
            validator
                .dbs
                .block_hashes
                .contains_header(&rotxn, &block.block_hash())
                .into_diagnostic()
        }

        fn no_event(events: &mut broadcast::Receiver<Event>) -> bool {
            matches!(
                events.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            )
        }

        #[test]
        fn commit_accept_persists_block_then_sends_event() -> Result<()> {
            let dir = temp_dir::TempDir::new().into_diagnostic()?;
            let validator = validator(&dir)?;
            let mut events = validator.events_tx.subscribe();
            let block = accepted_block();

            let action = ConnectBlockCommit.connect_block(&validator, &block)?;

            assert!(matches!(action, ConnectBlockAction::Accept { .. }));
            assert_eq!(tip(&validator)?, Some(block.block_hash()));
            assert!(has_header(&validator, &block)?);
            let event = events.try_recv().into_diagnostic()?;
            assert!(matches!(
                event,
                Event::ConnectBlock { header_info, .. }
                    if header_info.block_hash == block.block_hash()
            ));
            Ok(())
        }

        #[test]
        fn dry_run_accept_sees_block_then_leaves_no_trace() -> Result<()> {
            let dir = temp_dir::TempDir::new().into_diagnostic()?;
            let validator = validator(&dir)?;
            let mut events = validator.events_tx.subscribe();
            let block = accepted_block();

            let seen_tip = ConnectBlockDryRun(|rotxn: &RoTxn<'_>| {
                validator.dbs.current_chain_tip.try_get(rotxn, &())
            })
            .connect_block(&validator, &block)?
            .expect("block must be accepted")
            .into_diagnostic()?;

            assert_eq!(
                seen_tip,
                Some(block.block_hash()),
                "the closure must run against the connected block"
            );
            assert_eq!(tip(&validator)?, None, "a dry run must not move the tip");
            assert!(
                !has_header(&validator, &block)?,
                "a dry run must not keep the header"
            );
            assert!(no_event(&mut events), "a dry run must not send events");
            Ok(())
        }

        #[test]
        fn commit_reject_keeps_only_header() -> Result<()> {
            let dir = temp_dir::TempDir::new().into_diagnostic()?;
            let validator = validator(&dir)?;
            let mut events = validator.events_tx.subscribe();
            let block = rejected_block();

            let action = ConnectBlockCommit.connect_block(&validator, &block)?;

            assert!(matches!(action, ConnectBlockAction::Reject));
            assert_eq!(tip(&validator)?, None);
            assert!(
                has_header(&validator, &block)?,
                "a rejected block must keep its header"
            );
            assert!(no_event(&mut events));
            Ok(())
        }

        #[test]
        fn dry_run_reject_reports_reason_and_leaves_no_trace() -> Result<()> {
            let dir = temp_dir::TempDir::new().into_diagnostic()?;
            let validator = validator(&dir)?;
            let mut events = validator.events_tx.subscribe();
            let block = rejected_block();

            let reason = ConnectBlockDryRun(|_: &RoTxn<'_>| ())
                .connect_block(&validator, &block)?
                .expect_err("block must be rejected");

            let reason = format!("{:#}", crate::errors::ErrorChain::new(&reason));
            assert!(
                reason.contains("Old Ctip for sidechain 1 is unspent"),
                "unexpected rejection reason `{reason}`"
            );
            assert_eq!(tip(&validator)?, None);
            assert!(
                !has_header(&validator, &block)?,
                "a dry run must not keep the header"
            );
            assert!(no_event(&mut events));
            Ok(())
        }

        #[test]
        fn orphan_is_rejected_in_both_modes_without_writes() -> Result<()> {
            let dir = temp_dir::TempDir::new().into_diagnostic()?;
            let validator = validator(&dir)?;
            let mut events = validator.events_tx.subscribe();
            let block = orphan_block();

            let action = ConnectBlockCommit.connect_block(&validator, &block)?;
            assert!(matches!(action, ConnectBlockAction::Reject));
            let dry_run =
                ConnectBlockDryRun(|_: &RoTxn<'_>| ()).connect_block(&validator, &block)?;
            assert!(matches!(
                dry_run,
                Err(super::super::RejectReason::MissingParentHeight { .. })
            ));

            assert_eq!(tip(&validator)?, None);
            assert!(!has_header(&validator, &block)?);
            assert!(no_event(&mut events));
            Ok(())
        }
    }
}
