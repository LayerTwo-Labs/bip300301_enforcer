use bitcoin::hashes::sha256d;
use bitcoin_jsonrpsee::jsonrpsee;
use fatality::fatality;
use sneed::{db, env, rwtxn};
use thiserror::Error;
use transitive::Transitive;

use crate::{
    errors::Splittable,
    messages::CoinbaseMessagesError,
    types::SidechainNumber,
    validator::{dbs, main_rest_client::MainRestClientError},
};

#[fatality(splitable)]
pub(in crate::validator) enum HandleM1ProposeSidechain {
    #[error(transparent)]
    #[fatal]
    DbPut(#[from] db::error::Put),
    #[error(transparent)]
    #[fatal]
    DbTryGet(#[from] db::error::TryGet),
}

#[derive(Transitive)]
#[fatality(splitable)]
#[transitive(
    from(db::error::Delete, db::Error),
    from(db::error::Put, db::Error),
    from(db::error::TryGet, db::Error)
)]
pub(in crate::validator) enum HandleM2AckSidechain {
    #[error(transparent)]
    #[fatal]
    Db(Box<db::Error>),
    #[error("Missing sidechain proposal for slot `{sidechain_slot}`: `{description_hash}`")]
    MissingProposal {
        sidechain_slot: SidechainNumber,
        description_hash: sha256d::Hash,
    },
}

impl From<db::Error> for HandleM2AckSidechain {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[allow(clippy::enum_variant_names)]
#[derive(Transitive)]
#[fatality(splitable)]
#[transitive(
    from(db::error::Delete, db::Error),
    from(db::error::Iter, db::Error),
    from(db::error::TryGet, db::Error)
)]
pub(in crate::validator) enum HandleFailedSidechainProposals {
    #[error(transparent)]
    #[fatal]
    Db(Box<db::Error>),
}

impl From<db::Error> for HandleFailedSidechainProposals {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[fatality(splitable)]
pub(in crate::validator) enum HandleM3ProposeBundle {
    #[error(transparent)]
    #[fatal]
    Db(Box<db::Error>),
    #[error(
        "Cannot propose bundle; sidechain slot {} is inactive",
        .sidechain_number.0
    )]
    InactiveSidechain { sidechain_number: SidechainNumber },
}

impl From<db::Error> for HandleM3ProposeBundle {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[derive(Transitive)]
#[fatality(splitable)]
#[transitive(
    from(db::error::IterInit, db::Error),
    from(db::error::IterItem, db::Error)
)]
pub(in crate::validator) enum HandleM4Votes {
    #[error(transparent)]
    #[fatal]
    Db(Box<db::Error>),
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
}

impl From<db::Error> for HandleM4Votes {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
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
    Db(Box<db::Error>),
}

impl From<db::Error> for HandleFailedM6Ids {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[derive(Transitive)]
#[fatality(splitable)]
#[transitive(from(db::error::TryGet, db::Error))]
pub(in crate::validator) enum HandleM5M6 {
    #[error(transparent)]
    #[fatal]
    Db(Box<db::Error>),
    #[error("Invalid M6")]
    InvalidM6,
    #[error(transparent)]
    M6id(#[from] crate::messages::M6idError),
    #[error("Old Ctip for sidechain {} is unspent", .sidechain_number.0)]
    OldCtipUnspent { sidechain_number: SidechainNumber },
}

impl From<db::Error> for HandleM5M6 {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
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
    DbTryGet(#[from] db::error::TryGet),
    #[error(transparent)]
    NestedWriteTxn(#[from] env::error::NestedWriteTxn),
    #[error("No chain tip")]
    NoChainTip,
    #[error(transparent)]
    Transaction(#[from] <HandleTransaction as fatality::Split>::Fatal),
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

#[derive(Transitive)]
#[fatality(splitable)]
#[transitive(
    from(db::error::Delete, db::Error),
    from(db::error::First, db::Error),
    from(db::error::Get, db::Error),
    from(db::error::Len, db::Error),
    from(db::error::Put, db::Error),
    from(db::error::TryGet, db::Error)
)]
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
    Db(Box<db::Error>),
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
    #[fatal]
    PutBlockInfo(#[from] dbs::block_hash_dbs_error::PutBlockInfo),
    #[error(transparent)]
    #[fatal(forward)]
    Transaction(#[from] HandleTransaction),
}

impl From<db::Error> for ConnectBlock {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[derive(Debug, Error)]
pub(in crate::validator) enum DisconnectBlock {}

impl fatality::Fatality for DisconnectBlock {
    fn is_fatal(&self) -> bool {
        true
    }
}

#[derive(Transitive)]
#[fatality(splitable)]
#[transitive(
    from(db::error::Get, db::Error),
    from(db::error::Put, db::Error),
    from(db::error::TryGet, db::Error),
    from(env::error::ReadTxn, env::Error),
    from(env::error::WriteTxn, env::Error)
)]
pub(in crate::validator) enum Sync {
    #[error("Block not in active chain: `{block_hash}`")]
    #[fatal]
    BlockNotInActiveChain { block_hash: bitcoin::BlockHash },
    #[error(transparent)]
    #[fatal]
    CommitWriteTxn(#[from] rwtxn::error::Commit),
    #[error(transparent)]
    #[fatal(forward)]
    ConnectBlock(Box<Splittable<ConnectBlock>>),
    #[error(transparent)]
    #[fatal]
    Db(#[from] db::Error),
    #[error(transparent)]
    #[fatal(forward)]
    DisconnectBlock(#[from] DisconnectBlock),
    #[error(transparent)]
    #[fatal]
    Env(#[from] env::Error),
    #[error(transparent)]
    #[fatal]
    GetHeaderInfo(#[from] dbs::block_hash_dbs_error::GetHeaderInfo),
    #[error("Header sync already in progress")]
    HeaderSyncInProgress,
    #[error("JSON RPC error (`{method}`)")]
    #[fatal]
    JsonRpc {
        method: String,
        source: jsonrpsee::core::ClientError,
    },
    #[error(transparent)]
    #[fatal]
    LastCommonAncestor(#[from] dbs::block_hash_dbs_error::LastCommonAncestor),
    #[error(transparent)]
    #[fatal]
    Rest(#[from] MainRestClientError),
    #[error("Shutdown signal received")]
    #[fatal]
    Shutdown,
}

impl From<ConnectBlock> for Sync {
    fn from(err: ConnectBlock) -> Self {
        Self::ConnectBlock(Box::new(Splittable(err)))
    }
}

#[derive(Debug, Error)]
pub(in crate::validator::task) enum FatalInner {
    #[error(transparent)]
    DisconnectBlock(#[from] DisconnectBlock),
    #[error(transparent)]
    Sync(#[from] <Sync as fatality::Split>::Fatal),
    #[error(transparent)]
    WriteTxn(#[from] env::error::WriteTxn),
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
