use bitcoin_jsonrpsee::jsonrpsee::core::client::Error as JsonRpcError;
use cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer;
use miette::Diagnostic;
use thiserror::Error;

use crate::{
    errors::ErrorChain,
    messages::CoinbaseMessagesError,
    proto::{StatusBuilder, ToStatus},
    validator::Validator,
};

#[derive(Debug, Diagnostic, Error)]
pub enum InitDbConnection {
    #[error(transparent)]
    Migration(#[from] rusqlite_migration::Error),
    #[error(transparent)]
    Rusqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Diagnostic, Error)]
enum GetBundleProposalsInner {
    #[error(transparent)]
    DecodeBlindedM6(#[from] crate::types::BlindedM6DecodeError),
    #[error(transparent)]
    GetPendingWithdrawals(#[from] crate::validator::GetPendingWithdrawalsError),
    #[error(transparent)]
    GetSidechains(#[from] crate::validator::GetSidechainsError),
    #[error("rusqlite error")]
    Rusqlite(#[from] rusqlite::Error),
}

impl ToStatus for GetBundleProposalsInner {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DecodeBlindedM6(err) => err.builder(),
            Self::GetPendingWithdrawals(err) => err.builder(),
            Self::GetSidechains(err) => err.builder(),
            Self::Rusqlite(_) => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("failed to get bundle proposals")]
#[repr(transparent)]
pub struct GetBundleProposals(#[source] GetBundleProposalsInner);

impl<T> From<T> for GetBundleProposals
where
    GetBundleProposalsInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

impl ToStatus for GetBundleProposals {
    fn builder(&self) -> StatusBuilder<'_> {
        StatusBuilder::with_code(self, self.0.builder())
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GenerateCoinbaseTxouts {
    #[error(transparent)]
    CoinbaseMessages(#[from] crate::messages::CoinbaseMessagesError),
    #[error("transparent")]
    GetBundleProposals(#[from] GetBundleProposals),
    #[error(transparent)]
    GetHeaderInfo(#[from] crate::validator::GetHeaderInfoError),
    #[error(transparent)]
    GetPendingWithdrawals(#[from] crate::validator::GetPendingWithdrawalsError),
    #[error(transparent)]
    GetSidechains(#[from] crate::validator::GetSidechainsError),
    #[error(transparent)]
    PushBytes(#[from] bitcoin::script::PushBytesError),
    #[error("rusqlite error")]
    Rusqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    TryGetCtip(#[from] crate::validator::TryGetCtipError),
}

impl ToStatus for GenerateCoinbaseTxouts {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::CoinbaseMessages(err) => err.builder(),
            Self::GetBundleProposals(err) => err.builder(),
            Self::GetHeaderInfo(err) => err.builder(),
            Self::GetPendingWithdrawals(err) => err.builder(),
            Self::GetSidechains(err) => err.builder(),
            Self::PushBytes(err) => StatusBuilder::new(err),
            Self::TryGetCtip(err) => err.builder(),
            Self::Rusqlite(_) => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("Bitcoin Core RPC error (`{method}`)")]
#[diagnostic(code(bitcoin_core_rpc_error))]
pub struct BitcoinCoreRPC {
    pub method: String,
    #[source]
    pub error: JsonRpcError,
}

impl ToStatus for BitcoinCoreRPC {
    fn builder(&self) -> StatusBuilder<'_> {
        const BITCOIN_CORE_RPC_ERROR_H_NOT_FOUND: i32 = -18;
        match &self.error {
            JsonRpcError::Call(err)
                if err.code() == BITCOIN_CORE_RPC_ERROR_H_NOT_FOUND
                    && err.message().contains("No wallet is loaded") =>
            {
                // Try being super precise here. Easy to confuse the /enforcer/ wallet not being
                // loaded with the /bitcoin core/ wallet not being loaded.
                let err_msg = "the underlying Bitcoin Core node has no loaded wallet (fix this: `bitcoin-cli loadwallet WALLET_NAME`)";
                StatusBuilder {
                    code: connectrpc::ErrorCode::FailedPrecondition,
                    fmt_message: Box::new(|f| std::fmt::Display::fmt(err_msg, f)),
                    source: None,
                }
            }
            err => {
                tracing::error!(err_msg = %ErrorChain::new(&err), "unexpected bitcoin core RPC error");
                StatusBuilder::new(err)
            }
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("failed to consensus encode block")]
#[diagnostic(code(encode_block_error))]
pub struct EncodeBlock(#[from] pub bitcoin::io::Error);

impl ToStatus for EncodeBlock {
    fn builder(&self) -> StatusBuilder<'_> {
        StatusBuilder::new(self)
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum FetchTransaction {
    #[error(transparent)]
    BitcoinCoreRPC(#[from] BitcoinCoreRPC),
    #[error(transparent)]
    DeserializeHex(#[from] bitcoin::consensus::encode::FromHexError),
}

impl ToStatus for FetchTransaction {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::BitcoinCoreRPC(err) => err.builder(),
            Self::DeserializeHex(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error(
    "failed to fetch block template{}",
    .template_error.as_deref().map(|err| format!(": {err}")).unwrap_or_default()
)]
pub struct GetBlockTemplate {
    #[source]
    pub source: JsonRpcError,
    /// From `BlockProducer::last_gbt_error`
    pub template_error: Option<String>,
}

impl ToStatus for GetBlockTemplate {
    fn builder(&self) -> StatusBuilder<'_> {
        match &self.source {
            // The enforcer's own template server only comes up once the
            // initial mempool sync is done, so a transport error usually
            // means it is still starting.
            JsonRpcError::Transport(_) => {
                StatusBuilder::new(self).code(connectrpc::ErrorCode::Unavailable)
            }
            _ => StatusBuilder::new(self).code(connectrpc::ErrorCode::Internal),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum SelectBlockTxs {
    #[error(transparent)]
    GenerateSuffixTxs(#[from] GetBundleProposals),
    #[error(transparent)]
    GetBlockTemplate(#[from] GetBlockTemplate),
    #[error(transparent)]
    GetCtips(#[from] crate::validator::GetCtipsError),
    #[error("failed to decode transaction `{txid}` from the block template")]
    DecodeTemplateTransaction {
        txid: bitcoin::Txid,
        source: bitcoin::consensus::encode::Error,
    },
    #[error("negative fee `{fee}` for transaction `{txid}` from the block template")]
    NegativeTemplateTransactionFee {
        txid: bitcoin::Txid,
        fee: bitcoin::SignedAmount,
    },
    /// The block template was built on a different tip than the one the
    /// validator has caught up to. Building on the validator's tip while using
    /// the template's tx set produces a block Core rejects as `inconclusive`,
    /// so bail out and let the caller retry once the two tips agree.
    #[error(
        "block template is built on `{template_tip}`, but the validator tip is `{validator_tip}`"
    )]
    TemplateTipMismatch {
        template_tip: bitcoin::BlockHash,
        validator_tip: bitcoin::BlockHash,
    },
}

impl ToStatus for SelectBlockTxs {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::GenerateSuffixTxs(err) => err.builder(),
            Self::GetBlockTemplate(err) => err.builder(),
            Self::GetCtips(err) => err.builder(),
            Self::DecodeTemplateTransaction { .. }
            | Self::NegativeTemplateTransactionFee { .. } => {
                StatusBuilder::new(self).code(connectrpc::ErrorCode::Internal)
            }
            // Retryable: whichever side is behind just needs to catch up.
            Self::TemplateTipMismatch { .. } => {
                StatusBuilder::new(self).code(connectrpc::ErrorCode::FailedPrecondition)
            }
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum FinalizeBlock {
    #[error(transparent)]
    GetHeaderInfo(#[from] crate::validator::GetHeaderInfoError),
    #[error(transparent)]
    GetMainchainTip(#[from] crate::validator::GetMainchainTipError),
    #[error(transparent)]
    Script(#[from] bitcoin::script::PushBytesError),
    #[error(transparent)]
    SystemTime(#[from] std::time::SystemTimeError),
}

impl ToStatus for FinalizeBlock {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::GetHeaderInfo(err) => err.builder(),
            Self::GetMainchainTip(err) => err.builder(),
            Self::Script(err) => StatusBuilder::new(err),
            Self::SystemTime(err) => StatusBuilder::new(err),
        }
    }
}

/// Waiting for the validator's connect-block pipeline to process a block
/// that was just submitted to Bitcoin Core.
#[derive(Debug, Diagnostic, Error)]
pub enum AwaitBlockConnection {
    #[error(
        "timed out waiting for validator to connect block `{block_hash}` after {timeout:?}: \
         the enforcer has fallen behind Bitcoin Core, or rejected the block"
    )]
    Timeout {
        block_hash: bitcoin::BlockHash,
        timeout: std::time::Duration,
    },
    #[error(transparent)]
    TryGetBlockInfos(#[from] crate::validator::TryGetBlockInfosError),
}

impl ToStatus for AwaitBlockConnection {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            err @ Self::Timeout { .. } => {
                StatusBuilder::new(err).code(connectrpc::ErrorCode::DeadlineExceeded)
            }
            Self::TryGetBlockInfos(_) => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum Mine {
    #[error(transparent)]
    AwaitBlockConnection(#[from] AwaitBlockConnection),
    #[error(transparent)]
    BitcoinCoreRPC(#[from] BitcoinCoreRPC),
    #[error(transparent)]
    EncodeBlock(#[from] EncodeBlock),
    #[error(transparent)]
    FinalizeBlock(#[from] FinalizeBlock),

    #[error("block rejected: `{reason}`")]
    BlockRejected { reason: String },
}

impl ToStatus for Mine {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::AwaitBlockConnection(err) => err.builder(),
            Self::BitcoinCoreRPC(err) => err.builder(),
            Self::EncodeBlock(err) => err.builder(),
            Self::FinalizeBlock(err) => err.builder(),
            err @ Self::BlockRejected { .. } => {
                StatusBuilder::new(err).message(move |f| write!(f, "{err}"))
            }
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("{name} is required for mining on signet")]
pub struct MissingBinary {
    pub name: String,
    #[source]
    pub source: Option<std::io::Error>,
}

impl ToStatus for MissingBinary {
    fn builder(&self) -> StatusBuilder<'_> {
        StatusBuilder::new(self).code(if self.source.is_some() {
            connectrpc::ErrorCode::Internal
        } else {
            connectrpc::ErrorCode::FailedPrecondition
        })
    }
}

#[derive(Diagnostic, Debug, Error)]
pub enum VerifyCanMine {
    #[error(transparent)]
    BitcoinCoreRPC(#[from] BitcoinCoreRPC),
    #[error(transparent)]
    GetBlockTemplate(#[from] GetBlockTemplate),
    #[error(transparent)]
    MissingBinary(#[from] MissingBinary),
    #[error("cannot generate more than one block on signet")]
    MultipleBlocksOnSignet,
    #[error("cannot generate blocks on network (`{0}`)")]
    Network(bitcoin::Network),
    #[error(
        "generating blocks on signet requires the block template server: restart the enforcer \
         with `--enable-mempool --enable-block-template-server`"
    )]
    NoBlockTemplateServerOnSignet,
    #[error("no signet challenge found")]
    NoSignetChallengeFound,
    #[error("unable to parse signet challenge")]
    ParseSignetChallenge(#[from] bitcoin::address::FromScriptError),
    #[error("signet challenge address (`{0}`) is not in mainchain wallet")]
    SignetChallengeAddressMissing(bitcoin::Address),
}

impl ToStatus for VerifyCanMine {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::BitcoinCoreRPC(err) => err.builder(),
            Self::GetBlockTemplate(err) => err.builder(),
            Self::MissingBinary(err) => err.builder(),
            Self::MultipleBlocksOnSignet => {
                StatusBuilder::new(self).code(connectrpc::ErrorCode::InvalidArgument)
            }
            Self::Network(_)
            | Self::NoBlockTemplateServerOnSignet
            | Self::SignetChallengeAddressMissing(_) => {
                StatusBuilder::new(self).code(connectrpc::ErrorCode::FailedPrecondition)
            }
            Self::NoSignetChallengeFound | Self::ParseSignetChallenge(_) => {
                StatusBuilder::new(self)
            }
        }
    }
}

#[derive(Diagnostic, Debug, Error)]
pub enum GetSignetMinerPath {
    #[error("failed to create signet miner directory")]
    CreateSignetMinerDir(#[source] crate::bins::CommandError),
    #[error("failed to download signet miner")]
    DownloadSignetMiner(#[source] crate::bins::CommandError),
}

impl ToStatus for GetSignetMinerPath {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::CreateSignetMinerDir(err) | Self::DownloadSignetMiner(err) => {
                StatusBuilder::with_code(self, err.builder())
            }
        }
    }
}

#[derive(Diagnostic, Debug, Error)]
pub enum GenerateSignetBlock {
    #[error(transparent)]
    AwaitBlockConnection(#[from] AwaitBlockConnection),
    #[error("failed to fetch most recent block hash")]
    FetchMostRecentBlockHash(#[source] BitcoinCoreRPC),
    #[error(transparent)]
    GetHeaderInfo(#[from] crate::validator::GetHeaderInfoError),
    #[error(transparent)]
    GetMainchainTip(#[from] crate::validator::GetMainchainTipError),
    #[error(transparent)]
    GetSignetMinerPath(#[from] GetSignetMinerPath),
    #[error(transparent)]
    Mine(#[from] crate::bins::CommandError),
    #[error("signet miner subprocess timed out")]
    Timeout { duration: tokio::time::Duration },
}

impl ToStatus for GenerateSignetBlock {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::AwaitBlockConnection(err) => err.builder(),
            Self::FetchMostRecentBlockHash(err) => StatusBuilder::with_code(self, err.builder()),
            Self::GetHeaderInfo(err) => err.builder(),
            Self::GetMainchainTip(err) => err.builder(),
            Self::GetSignetMinerPath(err) => err.builder(),
            Self::Mine(err) => err.builder(),
            Self::Timeout { .. } => StatusBuilder::new(self),
        }
    }
}

/// Building and mining a single block via the producer: built and ground
/// locally on regtest, or via the signet miner script on signet.
#[derive(Debug, Diagnostic, Error)]
pub enum GenerateBlock {
    #[error(transparent)]
    CoinbaseBuilder(#[from] CoinbaseMessagesError),
    #[error(transparent)]
    GenerateCoinbaseTxouts(#[from] GenerateCoinbaseTxouts),
    #[error(transparent)]
    GenerateSignetBlock(#[from] GenerateSignetBlock),
    #[error(transparent)]
    Mine(#[from] Mine),
    #[error(transparent)]
    PushBytesBuf(#[from] bitcoin::script::PushBytesError),
    #[error(transparent)]
    SelectBlockTxs(#[from] SelectBlockTxs),
    #[error(transparent)]
    TryGetMainchainTip(#[from] crate::validator::TryGetMainchainTipError),
    #[error("validator is not synced")]
    ValidatorNotSynced,
}

impl ToStatus for GenerateBlock {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::CoinbaseBuilder(err) => err.builder(),
            Self::GenerateCoinbaseTxouts(err) => err.builder(),
            Self::GenerateSignetBlock(err) => err.builder(),
            Self::Mine(err) => err.builder(),
            Self::SelectBlockTxs(err) => err.builder(),
            Self::TryGetMainchainTip(err) => err.builder(),
            Self::PushBytesBuf(_) | Self::ValidatorNotSynced => StatusBuilder::new(self),
        }
    }
}

/// `connect_block` for the producer: the validator's error, plus the policy-table
/// maintenance that follows an accepted block. Wallet's `ConnectBlock` wraps this
/// and adds the BDK failures on top.
#[derive(Debug, Diagnostic, Error)]
pub enum ConnectBlock {
    #[error(transparent)]
    Validator(#[from] <Validator as CusfEnforcer>::ConnectBlockError),
    #[error(transparent)]
    GetBlockInfos(#[from] crate::validator::GetBlockInfosError),
    #[error("rusqlite error")]
    Rusqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Diagnostic, Error)]
pub(in crate::block_producer) enum InitialBlockTemplateInner {
    #[error(transparent)]
    CoinbaseMessages(#[from] CoinbaseMessagesError),
    #[error(transparent)]
    GetMainchainTip(#[from] crate::validator::GetMainchainTipError),
    #[error(transparent)]
    GetSeenBmmRequestsForParentBlock(
        #[from] crate::validator::GetSeenBmmRequestsForParentBlockError,
    ),
    #[error(transparent)]
    GenerateCoinbaseTxouts(#[from] GenerateCoinbaseTxouts),
    #[error(transparent)]
    GenerateSuffixTxs(#[from] GetBundleProposals),
    #[error("Failed to read the ACK-all-proposals setting")]
    GetAckAllProposals(#[source] rusqlite::Error),
    #[error("the `coinbasetxn` GBT capability is required")]
    NoCoinbaseTxn,
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct InitialBlockTemplate(InitialBlockTemplateInner);

impl<Err> From<Err> for InitialBlockTemplate
where
    InitialBlockTemplateInner: From<Err>,
{
    fn from(err: Err) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub(in crate::block_producer) enum FinalizeBlockTemplateInner {
    #[error(transparent)]
    CoinbaseMessages(#[from] CoinbaseMessagesError),
    #[error("Failed to apply initial block template: {reason}")]
    InitialBlockTemplate { reason: String },
    #[error("Failed to generate coinbase txouts suffix")]
    GenerateSuffixCoinbaseTxouts(#[source] bitcoin::script::PushBytesError),
    #[error(transparent)]
    GenerateSuffixTxs(#[from] GetBundleProposals),
    #[error(transparent)]
    GetCtipsAfter(#[from] crate::validator::cusf_enforcer::GetCtipsAfterError),
    #[error(transparent)]
    GetHeaderInfo(#[from] crate::validator::GetHeaderInfoError),
    #[error(transparent)]
    TryGetMainchainTip(#[from] crate::validator::TryGetMainchainTipError),
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct FinalizeBlockTemplate(FinalizeBlockTemplateInner);

impl<Err> From<Err> for FinalizeBlockTemplate
where
    FinalizeBlockTemplateInner: From<Err>,
{
    fn from(err: Err) -> Self {
        Self(err.into())
    }
}
