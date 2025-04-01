//! Implementation of [`cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer`]

use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
};

use async_broadcast::TrySendError;
use bitcoin::{hashes::Hash as _, Block, BlockHash, Transaction, Txid};
use cusf_enforcer_mempool::cusf_enforcer::{ConnectBlockAction, CusfEnforcer};
use fallible_iterator::FallibleIterator;
use fatality::Nested as _;
use futures::TryFutureExt as _;
use heed::RoTxn;
use miette::Diagnostic;
use ouroboros::self_referencing;
use thiserror::Error;

use crate::{
    types::{Ctip, Event, SidechainNumber},
    validator::{
        db_error,
        dbs::{self, RwTxn},
        task, Validator,
    },
};

pub use task::error::ValidateTransaction as ValidateTransactionError;

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct SyncError(#[from] task::error::Sync);

#[derive(Debug, Diagnostic, Error)]
enum ConnectBlockErrorInner {
    #[error(transparent)]
    CommitWriteTxn(#[from] dbs::CommitWriteTxnError),
    #[error(transparent)]
    ConnectBlock(#[from] Box<<task::error::ConnectBlock as fatality::Split>::Fatal>),
    #[error(transparent)]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
    #[error(transparent)]
    NestedWriteTxn(#[from] dbs::NestedWriteTxnError),
    #[error(transparent)]
    WriteTxn(#[from] dbs::WriteTxnError),
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
    CommitWriteTxn(#[from] dbs::CommitWriteTxnError),
    #[error(transparent)]
    DisconnectBlock(#[from] task::error::DisconnectBlock),
    #[error(transparent)]
    WriteTxn(#[from] dbs::WriteTxnError),
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
    fn commit_child(self) -> Result<RwTxn<'a>, dbs::CommitWriteTxnError> {
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
        tracing::debug!(%block_hash, "Storing header");
        validator
            .dbs
            .block_hashes
            .put_header(&mut parent_rwtxn, &block.header, height)?;
    }
    // Commit on block accept, abort on block reject
    let mut parent_child_rwtxn = ParentChildRwTxnTryBuilder {
        parent: parent_rwtxn,
        child_builder: |parent: &mut RwTxn| validator.dbs.nested_write_txn(parent),
    }
    .try_build()?;
    tracing::debug!(%block_hash, "Connecting block");
    match parent_child_rwtxn
        .with_child_mut(|child_rwtxn| task::connect_block(child_rwtxn, &validator.dbs, block))
        .into_nested()?
    {
        Ok(event) => {
            // FIXME: implement
            let remove_mempool_txs = HashSet::new();
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
        let block_hash = block.block_hash();
        match connect_block_no_commit(validator, block)? {
            ConnectBlockRwTxnAction::Accept {
                event,
                remove_mempool_txs,
                rwtxns,
            } => {
                tracing::info!(%block_hash, "Accepted block");
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
                tracing::info!(%block_hash, "rejecting block: {reason:#}");
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

    async fn sync_to_tip(&mut self, tip: BlockHash) -> Result<(), Self::SyncError> {
        task::sync_to_tip(
            &self.dbs,
            &self.events_tx,
            &self.header_sync_progress_channel,
            &self.mainchain_client,
            tip,
        )
        .map_err(SyncError)
        .await
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

    type AcceptTxError = ValidateTransactionError;

    fn accept_tx<TxRef>(
        &mut self,
        tx: &Transaction,
        _tx_inputs: &HashMap<bitcoin::Txid, TxRef>,
    ) -> Result<bool, Self::AcceptTxError>
    where
        TxRef: Borrow<Transaction>,
    {
        let res = task::validate_tx(&self.dbs, tx)?;
        Ok(res)
    }
}

#[derive(Debug, Error)]
pub(crate) enum GetCtipsAfterError {
    #[error(transparent)]
    ConnectBlock(#[from] ConnectBlockError),
    #[error(transparent)]
    DbIter(#[from] db_error::Iter),
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
            .map_err(db_error::Iter::Init)?
            .collect()
            .map_err(db_error::Iter::Item)
    })
    .connect_block(validator, block)?
    .transpose()?;
    Ok(res)
}
