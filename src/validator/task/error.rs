use bip300301::jsonrpsee;
use bitcoin::hashes::sha256d;
use fatality::fatality;
use thiserror::Error;

use crate::{
    messages::CoinbaseMessagesError,
    types::SidechainNumber,
    validator::dbs::{self, db_error},
};

#[fatality(splitable)]
pub(in crate::validator) enum HandleM1ProposeSidechain {
    #[error(transparent)]
    #[fatal]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    #[fatal]
    DbTryGet(#[from] db_error::TryGet),
}

#[allow(clippy::enum_variant_names)]
#[fatality(splitable)]
pub(in crate::validator) enum HandleM2AckSidechain {
    #[error(transparent)]
    #[fatal]
    DbDelete(#[from] db_error::Delete),
    #[error(transparent)]
    #[fatal]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    #[fatal]
    DbTryGet(#[from] db_error::TryGet),
    #[error(transparent)]
    #[fatal]
    PutActiveSidechain(#[from] dbs::PutActiveSidechainError),
    #[error("Missing sidechain proposal for slot `{sidechain_slot}`: `{description_hash}`")]
    MissingProposal {
        sidechain_slot: SidechainNumber,
        description_hash: sha256d::Hash,
    },
}

#[allow(clippy::enum_variant_names)]
#[fatality(splitable)]
pub(in crate::validator) enum HandleFailedSidechainProposals {
    #[error(transparent)]
    #[fatal]
    DbDelete(#[from] db_error::Delete),
    #[error(transparent)]
    #[fatal]
    DbIter(#[from] db_error::Iter),
    #[error(transparent)]
    #[fatal]
    DbTryGet(#[from] db_error::TryGet),
}

#[fatality(splitable)]
pub(in crate::validator) enum HandleM3ProposeBundle {
    #[error(transparent)]
    #[fatal]
    TryPutPendingM6id(#[from] dbs::TryWithPendingWithdrawalsError),
    #[error(
        "Cannot propose bundle; sidechain slot {} is inactive",
        .sidechain_number.0
    )]
    InactiveSidechain { sidechain_number: SidechainNumber },
}

#[fatality(splitable)]
pub(in crate::validator) enum HandleM4Votes {
    #[error(transparent)]
    DbIter(#[from] db_error::Iter),
    #[error("Invalid votes: expected {expected}, but found {len}")]
    InvalidVotes { expected: usize, len: usize },
    #[error(
        "No pending withdrawal for sidechain `{}` at index `{}`",
        .sidechain_number,
        .index
    )]
    UpvoteFailed {
        sidechain_number: SidechainNumber,
        index: u16,
    },
    #[error(transparent)]
    #[fatal]
    TryAlarmPendingM6ids(dbs::TryWithPendingWithdrawalsError),
    #[error(transparent)]
    #[fatal]
    TryUpvotePendingM6id(#[from] dbs::TryUpvotePendingWithdrawalError),
}

#[fatality(splitable)]
pub(in crate::validator) enum HandleM4AckBundles {
    #[error("Error handling M4 Votes")]
    #[fatal(forward)]
    Votes(#[from] HandleM4Votes),
}

#[fatality(splitable)]
pub(in crate::validator) enum HandleFailedM6Ids {
    #[error(transparent)]
    #[fatal]
    RetainPendingWithdrawals(#[from] dbs::RetainPendingWithdrawalsError),
}

#[fatality(splitable)]
pub(in crate::validator) enum HandleM5M6 {
    #[error(transparent)]
    #[fatal]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    #[fatal]
    DbTryGet(#[from] db_error::TryGet),
    #[error("Invalid M6")]
    InvalidM6,
    #[error(transparent)]
    M6id(#[from] crate::messages::M6idError),
    #[error("Old Ctip for sidechain {} is unspent", .sidechain_number.0)]
    OldCtipUnspent { sidechain_number: SidechainNumber },
    #[error(transparent)]
    #[fatal]
    TryWithPendingWithdrawals(#[from] dbs::TryWithPendingWithdrawalsError),
}

#[fatality(splitable)]
pub(in crate::validator) enum HandleM8 {
    #[error("BMM request expired")]
    BmmRequestExpired,
    #[error("Cannot include BMM request; not accepted by miners")]
    NotAcceptedByMiners,
}

#[fatality(splitable)]
pub(in crate::validator) enum HandleTransaction {
    #[error("Error handling M5/M6")]
    #[fatal(forward)]
    M5M6(#[from] HandleM5M6),
    #[error("Error handling M8")]
    #[fatal(forward)]
    M8(#[from] HandleM8),
}

#[derive(Debug, Error)]
pub(in crate::validator::task) enum ValidateTransactionInner {
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
    #[error("No chain tip")]
    NoChainTip,
    #[error(transparent)]
    Transaction(#[from] <HandleTransaction as fatality::Split>::Fatal),
    #[error(transparent)]
    WriteTxn(#[from] dbs::WriteTxnError),
}

#[derive(Debug, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct ValidateTransaction(ValidateTransactionInner);

impl<Err> From<Err> for ValidateTransaction
where
    ValidateTransactionInner: From<Err>,
{
    fn from(err: Err) -> Self {
        Self(err.into())
    }
}

#[fatality(splitable)]
pub(in crate::validator) enum ConnectBlock {
    #[error("Block parent `{parent}` does not match tip `{tip}` at height {tip_height}")]
    BlockParent {
        parent: bitcoin::BlockHash,
        tip: bitcoin::BlockHash,
        tip_height: u32,
    },
    #[error(transparent)]
    CoinbaseMessages(#[from] CoinbaseMessagesError),
    #[error(transparent)]
    #[fatal]
    PutBlockInfo(#[from] dbs::block_hash_dbs_error::PutBlockInfo),
    #[error(transparent)]
    #[fatal]
    DbDelete(#[from] db_error::Delete),
    #[error(transparent)]
    #[fatal]
    DbFirst(#[from] db_error::First),
    #[error(transparent)]
    #[fatal]
    DbGet(#[from] db_error::Get),
    #[error(transparent)]
    #[fatal]
    DbLen(#[from] db_error::Len),
    #[error(transparent)]
    #[fatal]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    #[fatal]
    DbTryGet(#[from] db_error::TryGet),
    #[error("Error handling failed M6IDs")]
    #[fatal(forward)]
    FailedM6Ids(#[from] HandleFailedM6Ids),
    #[error("Error handling failed sidechain proposals")]
    #[fatal(forward)]
    FailedSidechainProposals(#[from] HandleFailedSidechainProposals),
    #[error("Error handling M1 (propose sidechain)")]
    #[fatal(forward)]
    M1ProposeSidechain(#[from] HandleM1ProposeSidechain),
    #[error("Error handling M2 (ack sidechain)")]
    #[fatal(forward)]
    M2AckSidechain(#[from] HandleM2AckSidechain),
    #[error("Error handling M3 (propose bundle)")]
    #[fatal(forward)]
    M3ProposeBundle(#[from] HandleM3ProposeBundle),
    #[error("Error handling M4 (ack bundles)")]
    #[fatal(forward)]
    M4AckBundles(#[from] HandleM4AckBundles),
    #[error("Multiple blocks BMM'd in sidechain slot {}", .sidechain_number.0)]
    MultipleBmmBlocks { sidechain_number: SidechainNumber },
    #[error(transparent)]
    #[fatal(forward)]
    Transaction(#[from] HandleTransaction),
}

#[derive(Debug, Error)]
pub(in crate::validator) enum DisconnectBlock {}

#[fatality(splitable)]
pub(in crate::validator) enum Sync {
    #[error(transparent)]
    #[fatal]
    CommitWriteTxn(#[from] dbs::CommitWriteTxnError),
    #[error(transparent)]
    #[fatal(forward)]
    ConnectBlock(#[from] ConnectBlock),
    #[error(transparent)]
    #[fatal]
    DbGet(#[from] db_error::Get),
    #[error(transparent)]
    #[fatal]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    #[fatal]
    DbTryGet(#[from] db_error::TryGet),
    #[error("JSON RPC error (`{method}`)")]
    #[fatal]
    JsonRpc {
        method: String,
        source: jsonrpsee::core::ClientError,
    },
    #[error(transparent)]
    #[fatal]
    ReadTxn(#[from] dbs::ReadTxnError),
    #[error(transparent)]
    #[fatal]
    WriteTxn(#[from] dbs::WriteTxnError),
}

#[derive(Debug, Error)]
pub(in crate::validator::task) enum FatalInner {
    #[error(transparent)]
    DisconnectBlock(#[from] DisconnectBlock),
    #[error(transparent)]
    Sync(#[from] <Sync as fatality::Split>::Fatal),
    #[error(transparent)]
    WriteTxn(#[from] dbs::WriteTxnError),
    #[error(transparent)]
    Zmq(#[from] zeromq::ZmqError),
    #[error(transparent)]
    ZmqSequenceStream(#[from] cusf_enforcer_mempool::zmq::SequenceStreamError),
}

#[derive(Debug, Error)]
#[error(transparent)]
pub struct Fatal(FatalInner);

impl<E> From<E> for Fatal
where
    FatalInner: From<E>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}
