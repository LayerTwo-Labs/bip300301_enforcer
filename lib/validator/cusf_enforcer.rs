//! Implementation of [`cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer`]

use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    future::Future,
};

use async_broadcast::TrySendError;
use bitcoin::{Block, BlockHash, Transaction, Txid, hashes::Hash as _};
use cusf_enforcer_mempool::cusf_enforcer::{ConnectBlockAction, CusfEnforcer, TxAcceptAction};
use fallible_iterator::FallibleIterator;
use fatality::Nested as _;
use futures::TryFutureExt as _;
use miette::Diagnostic;
use ouroboros::self_referencing;
use sneed::{RoTxn, RwTxn, db, env, rwtxn};
use thiserror::Error;

use crate::{
    messages::parse_m8_tx,
    proto::mainchain::HeaderSyncProgress,
    types::{Ctip, Event, SidechainNumber},
    validator::{
        Validator,
        task::{self, error::ValidateTransaction as ValidateTransactionError},
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
    ConnectBlock(#[from] Box<<task::error::ConnectBlock as fatality::Split>::Fatal>),
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

impl From<<task::error::ConnectBlock as fatality::Split>::Fatal> for ConnectBlockErrorInner {
    fn from(err: <task::error::ConnectBlock as fatality::Split>::Fatal) -> Self {
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
    ConnectBlock(#[from] <task::error::ConnectBlock as fatality::Split>::Jfyi),
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
    match parent_child_rwtxn
        .with_child_mut(|child_rwtxn| task::connect_block(child_rwtxn, &validator.dbs, block))
        .into_nested()?
    {
        Ok(event) => {
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
                tracing::info!("rejecting block: {reason:#}");
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
#[repr(transparent)]
struct ConnectBlockDryRun<F>(F);

impl<'validator, F, Output> ConnectBlockMode<'validator> for ConnectBlockDryRun<F>
where
    F: FnOnce(&RoTxn<'_>) -> Output,
{
    type Output = Option<Output>;

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
                reason: _,
            } => {
                header_rwtxn.abort();
                return Ok(None);
            }
        };
        let res: Output = rwtxns.with_child(|child_rwtxn| self.0(child_rwtxn));
        let rwtxn = rwtxns.abort_child();
        rwtxn.abort(); // We don't want the effects of the block to be applied!
        Ok(Some(res))
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
        let () = task::sync_to_tip(
            &self.dbs,
            &self.events_tx,
            &header_sync_progress_tx,
            &self.mainchain_client,
            &self.mainchain_rest_client,
            tip,
            shutdown_signal,
        )
        .map_err(SyncError)
        .await?;
        *self.header_sync_progress_rx.write() = None;
        Ok(())
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
    ) -> Result<(), Self::DisconnectBlockError> {
        let mut rwtxn = self.dbs.write_txn()?;
        let () = task::disconnect_block(&mut rwtxn, &self.dbs, &self.events_tx, block_hash)?;
        rwtxn.commit()?;
        Ok(())
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
        let res = if task::validate_tx(&self.dbs, &mut rwtxn, tx)? {
            let conflicts_with = if let Some(bmm_request) = parse_m8_tx(tx) {
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
                conflicts_with
            } else {
                HashSet::new()
            };
            TxAcceptAction::Accept { conflicts_with }
        } else {
            TxAcceptAction::Reject
        };
        Ok(res)
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
/// Returns `None` if the block would be rejected.
pub(crate) fn get_ctips_after(
    validator: &Validator,
    block: &Block,
) -> Result<Option<HashMap<SidechainNumber, Ctip>>, GetCtipsAfterError> {
    let res = ConnectBlockDryRun(|rotxn: &RoTxn<'_>| -> Result<_, _> {
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
    .transpose()?;
    Ok(res)
}
