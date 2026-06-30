use bitcoin_jsonrpsee::jsonrpsee;
use error_fatality::{Fatality, Split};
use sneed::{db, env, rwtxn};
use thiserror::Error;
use transitive::Transitive;

use crate::{
    errors::Splittable,
    messages::CoinbaseMessagesError,
    types::{M6id, SidechainNumber},
    validator::{dbs, main_rest_client::MainRestClientError, parse_block_files},
};

#[derive(Debug, Error, Fatality, Split)]
pub(in crate::validator) enum HandleM1ProposeSidechain {
    #[error(transparent)]
    #[fatal(true)]
    DbPut(#[from] db::error::Put),
    #[error(transparent)]
    #[fatal(true)]
    DbTryGet(#[from] db::error::TryGet),
}

#[derive(Debug, Error, Fatality, Split, Transitive)]
#[expect(clippy::duplicated_attributes)]
#[split(attrs(derive(Debug, Error)))]
#[transitive(
    from(db::error::Delete, db::Error),
    from(db::error::Put, db::Error),
    from(db::error::TryGet, db::Error)
)]
pub(in crate::validator) enum HandleM2AckSidechain {
    #[error(transparent)]
    #[fatal(true)]
    Db(Box<db::Error>),
}

impl From<db::Error> for HandleM2AckSidechain {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[derive(Debug, Error, Fatality, Split, Transitive)]
#[expect(clippy::duplicated_attributes)]
#[split(attrs(derive(Debug, Error)))]
#[transitive(
    from(db::error::Delete, db::Error),
    from(db::error::Iter, db::Error),
    from(db::error::TryGet, db::Error)
)]
pub(in crate::validator) enum HandleFailedSidechainProposals {
    #[error(transparent)]
    #[fatal(true)]
    Db(Box<db::Error>),
}

impl From<db::Error> for HandleFailedSidechainProposals {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[derive(Debug, Error, Fatality, Split, Transitive)]
#[expect(clippy::duplicated_attributes)]
#[split(attrs(derive(Debug, Error)))]
#[transitive(from(db::error::Get, db::Error), from(db::error::TryGet, db::Error))]
pub(in crate::validator) enum HandleM3ProposeBundle {
    /// BIP 300: an M6ID already proposed in a previous block and not yet paid
    /// out must not be re-proposed; re-proposing it would reset its ack count.
    #[error(
        "withdrawal bundle {m6id} for sidechain slot {} is already pending",
        .sidechain_number.0
    )]
    #[fatal(false)]
    BundleAlreadyPending {
        sidechain_number: SidechainNumber,
        m6id: M6id,
    },
    #[error(transparent)]
    #[fatal(true)]
    Db(Box<db::Error>),
    #[error(
        "Cannot propose bundle; sidechain slot {} is inactive",
        .sidechain_number.0
    )]
    #[fatal(false)]
    InactiveSidechain { sidechain_number: SidechainNumber },
}

impl From<db::Error> for HandleM3ProposeBundle {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[derive(Debug, Error, Fatality, Split, Transitive)]
#[expect(clippy::duplicated_attributes)]
#[split(attrs(derive(Debug, Error)))]
#[transitive(
    from(db::error::Get, db::Error),
    from(db::error::IterInit, db::Error),
    from(db::error::IterItem, db::Error)
)]
pub(in crate::validator) enum HandleM4Votes {
    #[error(transparent)]
    #[fatal(true)]
    Db(Box<db::Error>),
    #[error("Invalid votes: expected {expected}, but found {len}")]
    #[fatal(false)]
    InvalidVotes { expected: usize, len: usize },
    #[error(
        "No pending withdrawal for sidechain `{}` at index `{}`",
        .sidechain_number,
        .index
    )]
    #[fatal(false)]
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

#[derive(Debug, Error, Fatality, Split, Transitive)]
#[expect(clippy::duplicated_attributes)]
#[split(attrs(derive(Debug, Error)))]
#[transitive(from(db::error::Get, db::Error), from(db::error::TryGet, db::Error))]
pub(in crate::validator) enum HandleM4AckBundles {
    #[error("Error handling M4 Votes")]
    #[fatal(forward)]
    Votes(#[from] HandleM4Votes),
    #[error(transparent)]
    #[fatal(true)]
    Db(Box<db::Error>),
    #[error(
        "M4 RepeatPrevious upvotes bundle {m6id} for sidechain {sidechain_number} that is no longer pending"
    )]
    #[fatal(false)]
    RepeatPreviousUpvotesMissingBundle {
        sidechain_number: SidechainNumber,
        m6id: M6id,
    },
    #[error("M4 TwoBytes encoding with no element > 253")]
    #[fatal(false)]
    TwoBytesWithinByteRange,
}

impl From<db::Error> for HandleM4AckBundles {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[derive(Debug, Error, Fatality, Split, Transitive)]
#[expect(clippy::duplicated_attributes)]
#[split(attrs(derive(Debug, Error)))]
#[transitive(
    from(db::error::Get, db::Error),
    from(db::error::IterInit, db::Error),
    from(db::error::IterItem, db::Error),
    from(db::error::TryGet, db::Error)
)]
pub(in crate::validator) enum HandleFailedM6Ids {
    #[error(transparent)]
    #[fatal(true)]
    Db(Box<db::Error>),
}

impl From<db::Error> for HandleFailedM6Ids {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[derive(Debug, Error)]
pub(in crate::validator) enum InvalidM6 {
    #[error(
        "invalid input count ({}); M6 withdrawals must have exactly 1 input",
        n_inputs
    )]
    InputCount { n_inputs: usize },
    #[error(
        "vote count ({}) is below threshold ({}) for withdrawal bundle {}",
        .vote_count,
        .threshold,
        .m6id,
    )]
    InsufficientVoteCount {
        m6id: M6id,
        threshold: u16,
        vote_count: u16,
    },
    #[error(
        "M6ID {} does not correspond to a pending withdrawal for sidechain {}",
        .m6id,
        .sidechain_number,
    )]
    MissingPendingWithdrawal {
        m6id: M6id,
        sidechain_number: SidechainNumber,
    },
    #[error(
        "invalid treasury output count ({}); must create exactly 1 ctip",
        .n_treasury_outputs,
    )]
    TreasuryOutputCount { n_treasury_outputs: usize },
    #[error(
        "invalid treasury output index ({}); must be at index 0 in tx outputs",
        .treasury_vout
    )]
    TreasuryOutputIndex { treasury_vout: u32 },
}

#[derive(Debug, Error, Fatality, Split, Transitive)]
#[split(attrs(derive(Debug, Error)))]
#[transitive(from(db::error::TryGet, db::Error))]
pub(in crate::validator) enum HandleM5M6 {
    #[error("Cannot be both M5 deposit and M6 withdrawal")]
    #[fatal(false)]
    Ambiguous,
    #[error(
        "Ctip for sidechain {} exists in {} but not {}",
        .sidechain,
        .db_exists_in,
        .db_missing_in,
    )]
    #[fatal(true)]
    CtipDbsInconsistent {
        sidechain: SidechainNumber,
        db_exists_in: String,
        db_missing_in: String,
    },
    #[error(transparent)]
    #[fatal(true)]
    Db(Box<db::Error>),
    #[error("Invalid M6 withdrawal")]
    #[fatal(false)]
    InvalidM6(#[from] InvalidM6),
    #[error(transparent)]
    #[fatal(false)]
    M6id(#[from] crate::messages::M6idError),
    #[error("Multiple OP_DRIVECHAIN outputs for sidechain {0}")]
    #[fatal(false)]
    MultipleOpDrivechainOutputs(SidechainNumber),
    /// BIP 300 M5: If the treasury UTXO for sidechain slot `S` exists and
    /// a transaction creates a new treasury UTXO for `S` without spending
    /// the already existing treasury UTXO, then this transaction MUST be
    /// considered invalid.
    #[error("Old Ctip for sidechain {} is unspent", .sidechain_number.0)]
    #[fatal(false)]
    OldCtipUnspent { sidechain_number: SidechainNumber },
    /// BIP 300: a transaction that spends a treasury UTXO as one of its inputs
    /// and does not create a new treasury UTXO as one of its outputs is
    /// invalid.
    #[error(
        "treasury UTXO for sidechain {} is spent without creating a new treasury UTXO",
        .sidechain_number.0
    )]
    #[fatal(false)]
    TreasurySpentWithoutNewCtip { sidechain_number: SidechainNumber },
    #[error("cannot deposit or withdraw zero sats from sidechain")]
    #[fatal(false)]
    ZeroDiff,
}

impl From<db::Error> for HandleM5M6 {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[derive(Debug, Error, Fatality, Split)]
pub(in crate::validator) enum HandleM8 {
    #[error("BMM request expired")]
    #[fatal(false)]
    BmmRequestExpired,
    #[error("Cannot include BMM request; not accepted by miners")]
    #[fatal(false)]
    NotAcceptedByMiners,
}

#[derive(Debug, Error, Fatality, Split)]
pub(in crate::validator) enum HandleTransaction {
    #[error("Error handling M5/M6")]
    #[fatal(forward)]
    M5M6(#[from] HandleM5M6),
    #[error("Error handling M8")]
    #[fatal(forward)]
    M8(#[from] HandleM8),
}

#[derive(Debug, Error, Transitive)]
#[expect(clippy::duplicated_attributes)]
#[transitive(from(db::error::Get, db::Error), from(db::error::TryGet, db::Error))]
pub(in crate::validator::task) enum ValidateTransactionInner {
    #[error(transparent)]
    Db(Box<db::Error>),
    #[error(transparent)]
    NestedWriteTxn(#[from] env::error::NestedWriteTxn),
    #[error("No chain tip")]
    NoChainTip,
    #[error(transparent)]
    Transaction(#[from] <HandleTransaction as Split>::Fatal),
}

impl From<db::Error> for ValidateTransactionInner {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
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

#[derive(Debug, Error, Fatality, Split, Transitive)]
#[expect(clippy::duplicated_attributes)]
#[split(attrs(derive(Debug, Error)))]
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
    #[fatal(false)]
    BlockParent {
        parent: bitcoin::BlockHash,
        tip: bitcoin::BlockHash,
        tip_height: u32,
    },
    #[error(transparent)]
    #[fatal(false)]
    CoinbaseMessages(#[from] CoinbaseMessagesError),
    #[error(transparent)]
    #[fatal(true)]
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
    #[fatal(false)]
    MultipleBmmBlocks { sidechain_number: SidechainNumber },
    #[error("Block has no transactions (missing coinbase)")]
    #[fatal(false)]
    NoCoinbase,
    #[error(transparent)]
    #[fatal(true)]
    PutBlockInfo(#[from] dbs::block_hash_dbs_error::PutBlockInfo),
    #[error("Error handling transaction `{txid}` in block `{block_hash}`")]
    #[fatal(forward)]
    Transaction {
        txid: bitcoin::Txid,
        block_hash: bitcoin::BlockHash,
        #[source]
        source: HandleTransaction,
    },
}

impl From<db::Error> for ConnectBlock {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[derive(Debug, Error, Transitive)]
#[expect(clippy::duplicated_attributes)]
#[transitive(
    from(db::error::Delete, db::Error),
    from(db::error::Get, db::Error),
    from(db::error::Put, db::Error),
    from(db::error::TryGet, db::Error)
)]
pub(in crate::validator) enum DisconnectBlock {
    #[error(transparent)]
    Db(Box<db::Error>),
    #[error(transparent)]
    GetHeaderInfo(#[from] dbs::block_hash_dbs_error::GetHeaderInfo),
    #[error("Block hash `{block_hash}` does not match tip `{tip_hash}`")]
    TipHash {
        block_hash: bitcoin::BlockHash,
        tip_hash: bitcoin::BlockHash,
    },
    #[error(transparent)]
    Undo(#[from] dbs::diff::UndoError),
}

impl From<db::Error> for DisconnectBlock {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[derive(Debug, Error, Fatality, Split, Transitive)]
#[expect(clippy::duplicated_attributes)]
#[split(attrs(derive(Debug, Error)))]
#[transitive(
    from(db::error::Get, db::Error),
    from(db::error::Put, db::Error),
    from(db::error::TryGet, db::Error),
    from(env::error::ReadTxn, env::Error),
    from(env::error::WriteTxn, env::Error)
)]
pub(in crate::validator) enum Sync {
    #[error("Batch JSON RPC error: {}", .errors.join(", "))]
    #[fatal(true)]
    BatchJsonRpc { errors: Vec<String> },
    #[error(transparent)]
    #[fatal(true)]
    BlockDirectoryParser(#[from] parse_block_files::BlockDirectoryParserError),
    #[error("failed to set byte offset in block file parser")]
    #[fatal(true)]
    BlockFileParserSetOffset(#[source] std::io::Error),
    #[error("Block not in active chain: `{block_hash}`")]
    #[fatal(true)]
    BlockNotInActiveChain { block_hash: bitcoin::BlockHash },
    // A block the mainchain node accepted but that violates BIP300/301. This is
    // NOT a fatal sync failure: the sync loop invalidates the block on the node
    // and re-syncs, mirroring the live `ConnectBlockAction::Reject` path.
    #[error("Consensus-invalid block `{block_hash}` rejected during sync")]
    #[fatal(false)]
    BlockRejected {
        block_hash: bitcoin::BlockHash,
        #[source]
        source: Box<ConnectBlock>,
    },
    #[error(transparent)]
    #[fatal(true)]
    CommitWriteTxn(#[from] rwtxn::error::Commit),
    #[error(transparent)]
    #[fatal(forward)]
    ConnectBlock(Box<Splittable<ConnectBlock>>),
    #[error(transparent)]
    #[fatal(true)]
    Db(#[from] db::Error),
    #[error(transparent)]
    #[fatal(false)]
    Decode(#[from] bitcoin::consensus::encode::Error),
    #[error(transparent)]
    #[fatal(true)]
    DisconnectBlock(#[from] DisconnectBlock),
    #[error(transparent)]
    #[fatal(true)]
    Env(#[from] env::Error),
    #[error(transparent)]
    #[fatal(true)]
    FetchBlockIndex(#[from] parse_block_files::FetchBlockIndexError),
    #[error(transparent)]
    #[fatal(true)]
    GetHeaderInfo(#[from] dbs::block_hash_dbs_error::GetHeaderInfo),
    #[error("Header sync already in progress")]
    #[fatal(false)]
    HeaderSyncInProgress,
    #[error(transparent)]
    #[fatal(false)]
    Hex(#[from] hex::FromHexError),
    #[error("JSON RPC error (`{method}`)")]
    #[fatal(true)]
    JsonRpc {
        method: String,
        source: jsonrpsee::core::ClientError,
    },
    #[error("JSON serialization error")]
    #[fatal(true)]
    JsonSerialize(#[source] serde_json::Error),
    #[error(transparent)]
    #[fatal(true)]
    LastCommonAncestor(#[from] dbs::block_hash_dbs_error::LastCommonAncestor),
    #[error(transparent)]
    #[fatal(true)]
    ParseBlockFiles(#[from] parse_block_files::ParseBlockFileError),
    #[error(transparent)]
    #[fatal(true)]
    Rest(#[from] MainRestClientError),
    #[error("Shutdown signal received")]
    #[fatal(true)]
    Shutdown,
}

impl From<ConnectBlock> for Sync {
    fn from(err: ConnectBlock) -> Self {
        Self::ConnectBlock(Box::new(Splittable(err)))
    }
}
