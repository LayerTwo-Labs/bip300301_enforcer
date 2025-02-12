use bip300301::jsonrpsee::core::client::Error as JsonRpcError;
use cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer;
use miette::{diagnostic, Diagnostic};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    types::SidechainNumber,
    validator::{self, Validator},
};

#[derive(Clone, Debug, Deserialize, Diagnostic, Error)]
#[diagnostic(
    code(electrum_error),
    help("The error is from the Electrum server. Check the message for more details.")
)]
#[error("electrum error `{code}`: `{message}`")]
pub struct Electrum {
    code: i32,
    message: String,
}

// Add new TonicStatusError type
#[derive(Debug, Diagnostic, Error, Clone)]
#[error("tonic error: {0}")]
pub struct TonicStatus(#[from] tonic::Status);

impl TonicStatus {
    pub fn into_status(&self) -> tonic::Status {
        self.0.clone()
    }
}

// Add extension trait for tonic::Status
pub trait TonicStatusExt {
    fn into_diagnostic(self) -> miette::Result<()>;
}

impl TonicStatusExt for tonic::Status {
    fn into_diagnostic(self) -> miette::Result<()> {
        Err(TonicStatus(self).into())
    }
}

impl From<Electrum> for tonic::Status {
    fn from(error: Electrum) -> Self {
        let code = match error.code {
            // https://github.com/bitcoin/bitcoin/blob/e8f72aefd20049eac81b150e7f0d33709acd18ed/src/common/messages.cpp
            -25 => tonic::Code::InvalidArgument,
            _ => tonic::Code::Unknown,
        };
        Self::new(code, error.to_string())
    }
}

/// Wallet not unlocked
#[derive(Debug, Diagnostic, Error)]
#[diagnostic(code(wallet_not_unlocked))]
#[error("Enforcer wallet not unlocked")]
pub struct NotUnlocked;

// Errors related to creating/unlocking wallets.
#[derive(Debug, Diagnostic, Error)]
pub enum WalletInitialization {
    #[error("enforcer wallet already unlocked")]
    #[diagnostic(code(wallet_already_unlocked))]
    AlreadyUnlocked,
    #[error("enforcer wallet not found (can be created with CreateWallet RPC)")]
    #[diagnostic(code(wallet_not_found))]
    NotFound,
    #[error("enforcer wallet already exists (but might not be initialized)")]
    #[diagnostic(code(wallet_already_exists))]
    AlreadyExists,
    /// This means you've been fooling around with different mnemonics and data directories!
    /// Wallet directory probably needs to be wiped.
    #[error(
        "enforcer wallet data mismatch, data directory content does not line up with wallet descriptor"
    )]
    #[diagnostic(code(wallet_data_mismatch))]
    DataMismatch,
    #[error("invalid password for enforcer wallet")]
    #[diagnostic(code(wallet_invalid_password))]
    InvalidPassword,
    // These errors are strictly speaking not related to wallet initialization...
    // The wallet is not synced to the blockchain
    #[error("enforcer wallet not synced")]
    #[diagnostic(code(wallet_not_synced))]
    NotSynced,
}

#[derive(Debug, Diagnostic, Error)]
pub enum WalletSync {
    #[error(transparent)]
    BdkWalletConnect(#[from] bdk_wallet::chain::local_chain::CannotConnectError),
    #[error(transparent)]
    BdkWalletPersist(#[from] super::PersistenceError),
    #[error(transparent)]
    ElectrumSync(#[from] bdk_electrum::electrum_client::Error),
    #[error(transparent)]
    EsploraSync(#[from] Box<bdk_esplora::esplora_client::Error>),
    #[error(transparent)]
    WalletNotUnlocked(#[from] NotUnlocked),
}

#[derive(Debug, Diagnostic, Error)]
#[error("Bitcoin Core RPC error `{method}`: {error}")]
#[diagnostic(code(bitcoin_core_rpc_error))]
pub struct BitcoinCoreRPC {
    pub method: String,
    #[source]
    pub error: JsonRpcError,
}

#[derive(Debug, Diagnostic, Error)]
#[error("failed to consensus encode block")]
#[diagnostic(code(encode_block_error))]
pub struct EncodeBlock(#[from] pub bitcoin::io::Error);

#[derive(Debug, Diagnostic, Error)]
pub(in crate::wallet) enum GetBundleProposals {
    #[error(transparent)]
    BlindedM6(#[from] crate::types::BlindedM6Error),
    #[error(transparent)]
    ConsensusEncoding(#[from] bitcoin::consensus::encode::Error),
    #[error(transparent)]
    GetPendingWithdrawals(#[from] crate::validator::GetPendingWithdrawalsError),
    #[error(transparent)]
    Rustqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Diagnostic, Error)]
pub(in crate::wallet) enum GenerateCoinbaseTxouts {
    #[error(transparent)]
    CoinbaseMessages(#[from] crate::messages::CoinbaseMessagesError),
    #[error(transparent)]
    GetBundleProposals(#[from] crate::wallet::error::GetBundleProposals),
    #[error(transparent)]
    GetPendingWithdrawals(#[from] crate::validator::GetPendingWithdrawalsError),
    #[error(transparent)]
    GetSidechains(#[from] crate::validator::GetSidechainsError),
    #[error(transparent)]
    PushBytes(#[from] bitcoin::script::PushBytesError),
    #[error(transparent)]
    Rustqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Diagnostic, Error)]
pub(in crate::wallet) enum GenerateSuffixTxs {
    #[error(transparent)]
    GetBundleProposals(#[from] crate::wallet::error::GetBundleProposals),
    #[error(transparent)]
    M6(#[from] crate::types::AmountUnderflowError),
    #[error("Missing ctip for sidechain {sidechain_id}")]
    MissingCtip { sidechain_id: SidechainNumber },
}

#[derive(Debug, Error)]
pub enum ConnectBlock {
    #[error(transparent)]
    BdkConnect(#[from] bdk_wallet::chain::local_chain::CannotConnectError),
    #[error(transparent)]
    ConnectBlock(#[from] <Validator as CusfEnforcer>::ConnectBlockError),
    #[error(transparent)]
    GetBlockInfo(#[from] validator::GetBlockInfoError),
    #[error(transparent)]
    GetHeaderInfo(#[from] validator::GetHeaderInfoError),
    #[error(transparent)]
    Rustqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    WalletNotUnlocked(#[from] NotUnlocked),
}

#[derive(Debug, Diagnostic, Error)]
pub(in crate::wallet) enum InitialBlockTemplateInner {
    #[error(transparent)]
    GetMainchainTip(#[from] crate::validator::GetMainchainTipError),
    #[error(transparent)]
    GenerateCoinbaseTxouts(#[from] GenerateCoinbaseTxouts),
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
pub(in crate::wallet) enum SuffixTxsInner {
    #[error("Failed to apply initial block template")]
    InitialBlockTemplate,
    #[error(transparent)]
    GenerateSuffixTxs(#[from] GenerateSuffixTxs),
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
pub struct SuffixTxs(SuffixTxsInner);

impl<Err> From<Err> for SuffixTxs
where
    SuffixTxsInner: From<Err>,
{
    fn from(err: Err) -> Self {
        Self(err.into())
    }
}

#[derive(Diagnostic, Debug, Error)]
pub enum SendTransaction {
    #[diagnostic(code(send_transaction_add_utxo))]
    #[error("UTXO is not in wallet: {}:{}", .0.txid, .0.vout)]
    UnknownUTXO(bdk_wallet::bitcoin::OutPoint),
}
