use std::{fmt::Debug, path::PathBuf};

use bdk_chain::CheckPoint;
use bdk_esplora::esplora_client;
use bitcoin_jsonrpsee::jsonrpsee::core::client::Error as JsonRpcError;
use cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer;
use either::Either;
use miette::{Diagnostic, diagnostic};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    cli::WalletSyncSource,
    errors::ErrorChain,
    messages::CoinbaseMessagesError,
    proto::{StatusBuilder, ToStatus},
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

impl ToStatus for Electrum {
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::new(self).code(match self.code {
            // https://github.com/bitcoin/bitcoin/blob/e8f72aefd20049eac81b150e7f0d33709acd18ed/src/common/messages.cpp
            -25 => tonic::Code::InvalidArgument,
            _ => tonic::Code::Unknown,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Diagnostic, Error)]
#[diagnostic(code(esplora_error))]
#[error("esplora error `{code}`: `{message}`")]
pub struct Esplora {
    code: i32,
    message: String,
}

// Add new TonicStatusError type
#[derive(Clone, Debug, Diagnostic, Error)]
#[error("tonic error: {0}")]
pub struct TonicStatus(#[from] tonic::Status);

// Add extension trait for tonic::Status
pub trait TonicStatusExt {
    fn into_diagnostic(self) -> miette::Result<()>;
}

impl TonicStatusExt for tonic::Status {
    fn into_diagnostic(self) -> miette::Result<()> {
        Err(TonicStatus(self).into())
    }
}

/// Wallet not synced
#[derive(Debug, Diagnostic, Error)]
#[diagnostic(code(wallet_not_synced))]
#[error("enforcer wallet not synced")]
pub struct NotSynced;

impl ToStatus for NotSynced {
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::new(self)
    }
}

/// Wallet not unlocked
#[derive(Debug, Diagnostic, Error)]
#[diagnostic(code(wallet_not_unlocked))]
#[error("enforcer wallet not unlocked")]
#[help("use the cusf.mainchain.v1.WalletService/UnlockWallet RPC to unlock the wallet")]
pub struct NotUnlocked;

impl ToStatus for NotUnlocked {
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::new(self)
    }
}

/// Wallet data mismatch
#[derive(Debug, Diagnostic, Error)]
#[diagnostic(code(wallet_data_mismatch))]
#[error(
    "enforcer wallet data mismatch, data directory content does not line up with wallet descriptor"
)]
pub struct DataMismatch;

impl ToStatus for DataMismatch {
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::new(self)
    }
}

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
    #[error(transparent)]
    #[diagnostic(transparent)]
    DataMismatch(#[from] DataMismatch),
    #[error("invalid password for enforcer wallet")]
    #[diagnostic(code(wallet_invalid_password))]
    InvalidPassword,
    // These errors are strictly speaking not related to wallet initialization...
    // The wallet is not synced to the blockchain
    #[error(transparent)]
    #[diagnostic(transparent)]
    NotSynced(#[from] NotSynced),
}

impl ToStatus for WalletInitialization {
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::new(self).code(match self {
            Self::NotSynced(_) => tonic::Code::FailedPrecondition,
            Self::InvalidPassword => tonic::Code::InvalidArgument,
            Self::DataMismatch(_) => tonic::Code::Internal,
            Self::NotFound => tonic::Code::NotFound,
            Self::AlreadyExists => tonic::Code::AlreadyExists,
            Self::AlreadyUnlocked => tonic::Code::AlreadyExists,
        })
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum WalletSignTransaction {
    #[error(transparent)]
    NotUnlocked(#[from] NotUnlocked),
    #[error(transparent)]
    SignerError(#[from] bdk_wallet::signer::SignerError),
    #[error(transparent)]
    ExtractTx(#[from] bdk_wallet::bitcoin::psbt::ExtractTxError),
    #[error("unable to sign transaction")]
    #[diagnostic(code(unable_to_sign_transaction))]
    UnableToSign,
}

impl ToStatus for WalletSignTransaction {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::NotUnlocked(err) => err.builder(),
            Self::SignerError(err) => StatusBuilder::new(err),
            Self::ExtractTx(err) => StatusBuilder::new(err),
            Self::UnableToSign => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[diagnostic(code(bdk_wallet_persist))]
#[error("unable to persist BDK wallet DB `{file_path}`")]
pub struct BdkWalletPersist {
    pub file_path: PathBuf,
    pub source: tokio_rusqlite::Error,
}

#[derive(Debug, Diagnostic, Error)]
pub enum WalletSync {
    #[error(transparent)]
    #[diagnostic(code(bdk_wallet_connect))]
    BdkWalletConnect(#[from] bdk_wallet::chain::local_chain::CannotConnectError),
    #[error(transparent)]
    #[diagnostic(transparent)]
    BdkWalletPersist(#[from] BdkWalletPersist),
    #[error(transparent)]
    #[diagnostic(code(electrum_sync))]
    ElectrumSync(#[from] bdk_electrum::electrum_client::Error),
    #[error(transparent)]
    #[diagnostic(code(esplora_sync))]
    EsploraSync(#[from] Box<bdk_esplora::esplora_client::Error>),
    #[error(transparent)]
    #[diagnostic(code(wallet_not_unlocked))]
    WalletNotUnlocked(#[from] NotUnlocked),
}

#[derive(Debug, Diagnostic, Error)]
#[error("failed to parse mnemonic")]
#[repr(transparent)]
pub struct ParseMnemonic(#[from] bdk_wallet::bip39::Error);

impl ToStatus for ParseMnemonic {
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::new(self)
    }
}

#[derive(Debug, Diagnostic, Error)]
pub(crate) enum ReadDbMnemonicInner {
    #[error(
        "invalid mnemonic DB state (plaintext_mnemonic=`{}`, iv=`{}`, ciphertext=`{}`, key_salt=`{}`)",
        Self::display_is_some(*.plaintext_mnemonic_is_some),
        Self::display_is_some(*.iv_is_some),
        Self::display_is_some(*.ciphertext_is_some),
        Self::display_is_some(*.key_salt_is_some),
    )]
    InvalidDbState {
        plaintext_mnemonic_is_some: bool,
        iv_is_some: bool,
        ciphertext_is_some: bool,
        key_salt_is_some: bool,
    },
    #[error(transparent)]
    ParseMnemonic(#[from] ParseMnemonic),
    #[error("failed to read mnemonic from DB")]
    ReadMnemonic(#[source] rusqlite::Error),
    #[error("rusqlite error")]
    Rusqlite(#[source] rusqlite::Error),
}

impl ReadDbMnemonicInner {
    /// Display a bool that indicates if an `Option<_>` is `Some` or `None`.
    fn display_is_some(is_some: bool) -> &'static str {
        if is_some { "Some" } else { "None" }
    }
}

impl ToStatus for ReadDbMnemonicInner {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::ParseMnemonic(err) => err.builder(),
            Self::InvalidDbState { .. } | Self::ReadMnemonic(_) | Self::Rusqlite(_) => {
                StatusBuilder::new(self)
            }
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("failed to read mnemonic seed phrase from DB")]
#[repr(transparent)]
pub struct ReadDbMnemonic(#[source] ReadDbMnemonicInner);

impl<T> From<T> for ReadDbMnemonic
where
    ReadDbMnemonicInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

impl ToStatus for ReadDbMnemonic {
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::with_code(self, self.0.builder())
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("failed to stretch password into AES key")]
#[repr(transparent)]
pub struct StretchPassword(#[from] argon2::Error);

impl ToStatus for StretchPassword {
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::new(self)
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum EncryptMnemonic {
    #[error("failed to encrypt mnemonic")]
    Encrypt(#[from] aes_gcm::Error),
    #[error(transparent)]
    StretchPassword(#[from] StretchPassword),
}

impl ToStatus for EncryptMnemonic {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::Encrypt(_) => StatusBuilder::new(self),
            Self::StretchPassword(err) => err.builder(),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum DecryptMnemonic {
    #[error("failed to decrypt mnemonic")]
    Decrypt(#[from] aes_gcm::Error),
    #[error(transparent)]
    ParseMnemonic(#[from] ParseMnemonic),
    #[error(transparent)]
    StretchPassword(#[from] StretchPassword),
    #[error("failed to convert mnemonic to string")]
    Utf8(#[from] std::string::FromUtf8Error),
}

#[derive(Debug, Diagnostic, Error)]
pub enum UnlockExistingWallet {
    #[error(transparent)]
    InitWalletFromMnemonic(Box<InitWalletFromMnemonic>),
    #[error("wallet is not encrypted")]
    NotEncrypted,
    #[error(transparent)]
    ReadDbMnemonic(#[from] ReadDbMnemonic),
    #[error(transparent)]
    WalletInitialization(#[from] WalletInitialization),
}

impl From<InitWalletFromMnemonic> for UnlockExistingWallet {
    fn from(err: InitWalletFromMnemonic) -> Self {
        Self::InitWalletFromMnemonic(Box::new(err))
    }
}

impl ToStatus for UnlockExistingWallet {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::InitWalletFromMnemonic(err) => err.builder(),
            Self::NotEncrypted => StatusBuilder::new(self),
            Self::ReadDbMnemonic(err) => err.builder(),
            Self::WalletInitialization(err) => err.builder(),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("unsupported network (`{0}`)")]
pub struct UnsupportedNetwork(pub bitcoin::Network);

#[derive(Debug, Diagnostic, Error)]
pub enum InitEsploraClient {
    #[error("failed to build esplora client")]
    BuildEsploraClient(#[source] esplora_client::Error),
    #[error("failed to get esplora height")]
    EsploraClientHeight(#[source] esplora_client::Error),
    #[error(transparent)]
    ParseUrl(#[from] url::ParseError),
    #[error(transparent)]
    UnsupportedNetwork(#[from] UnsupportedNetwork),
}

#[derive(Debug, Diagnostic, Error)]
pub enum InitElectrumClient {
    #[error(
        "electrum server (`{}`) is not on the same chain as the wallet (`{}`)",
        .electrum_block_hash,
        .wallet_chain_hash
    )]
    ChainMismatch {
        electrum_block_hash: bitcoin::BlockHash,
        wallet_chain_hash: bitcoin::blockdata::constants::ChainHash,
    },
    #[error("failed to create electrum client")]
    CreateElectrumClient(#[source] bdk_electrum::electrum_client::Error),
    #[error("failed to get initial block header")]
    GetInitialBlockHeader(#[source] bdk_electrum::electrum_client::Error),
    #[error(transparent)]
    UnsupportedNetwork(#[from] UnsupportedNetwork),
}

#[derive(Debug, Diagnostic, Error)]
pub enum InitDbConnection {
    #[error(transparent)]
    Migration(#[from] rusqlite_migration::Error),
    #[error(transparent)]
    Rusqlite(#[from] rusqlite::Error),
}

type Persistence = <crate::wallet::Persistence as bdk_wallet::AsyncWalletPersister>::Error;

#[derive(Debug, Diagnostic, Error)]
pub enum InitWalletFromMnemonic {
    #[error("failed to create wallet")]
    CreateWallet(#[from] bdk_wallet::CreateWithPersistError<Persistence>),
    #[diagnostic(transparent)]
    #[error("wallet data mismatch (wipe your data directory and try again)")]
    DataMismatch(#[source] DataMismatch),
    #[error("unable to derive xpriv from extended key")]
    DeriveXpriv,
    #[error("failed to load wallet")]
    LoadWallet(#[source] bdk_wallet::LoadWithPersistError<Persistence>),
    #[error("mnemonic key error")]
    MnemonicKey(#[from] bdk_wallet::keys::KeyError),
}

impl From<bdk_wallet::LoadWithPersistError<Persistence>> for InitWalletFromMnemonic {
    fn from(err: bdk_wallet::LoadWithPersistError<Persistence>) -> Self {
        if err.to_string().contains("data mismatch") {
            Self::DataMismatch(DataMismatch)
        } else {
            Self::LoadWallet(err)
        }
    }
}

impl ToStatus for InitWalletFromMnemonic {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::DataMismatch(source) => StatusBuilder::with_code(self, source.builder()),
            Self::CreateWallet(_)
            | Self::DeriveXpriv
            | Self::LoadWallet(_)
            | Self::MnemonicKey(_) => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum CreateNewWallet {
    #[error(transparent)]
    EncryptMnemonic(#[from] EncryptMnemonic),
    #[error("failed to generate mnemonic")]
    GenerateMnemonic(#[from] bdk_wallet::bip39::Error),
    #[error(transparent)]
    InitFromMnemonic(Box<InitWalletFromMnemonic>),
    #[error(transparent)]
    ReadDbMnemonic(#[from] ReadDbMnemonic),
    #[error("rusqlite error")]
    Rusqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    WalletInitialization(#[from] WalletInitialization),
}

impl From<InitWalletFromMnemonic> for CreateNewWallet {
    fn from(err: InitWalletFromMnemonic) -> Self {
        Self::InitFromMnemonic(Box::new(err))
    }
}

impl ToStatus for CreateNewWallet {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::EncryptMnemonic(err) => err.builder(),
            Self::GenerateMnemonic(_) => StatusBuilder::new(self),
            Self::InitFromMnemonic(err) => err.builder(),
            Self::ReadDbMnemonic(err) => err.builder(),
            Self::Rusqlite(_) => StatusBuilder::new(self),
            Self::WalletInitialization(err) => err.builder(),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum InitWallet {
    #[error("failed to initialize DB connection")]
    InitDbConnection(#[from] InitDbConnection),
    #[error("failed to initialize electrum client")]
    InitElectrumClient(#[from] InitElectrumClient),
    #[error("failed to initialize esplora client")]
    InitEsploraClient(#[from] InitEsploraClient),
    #[error("failed to initialize wallet from mnemonic")]
    InitFromMnemonic(Box<InitWalletFromMnemonic>),
    #[error("failed to open connection to wallet DB")]
    OpenConnection(#[source] tokio_rusqlite::Error),
    #[error(transparent)]
    ParseNetwork(#[from] bitcoin::network::ParseNetworkError),
    #[error(transparent)]
    ReadDbMnemonic(#[from] ReadDbMnemonic),
}

impl From<InitWalletFromMnemonic> for InitWallet {
    fn from(err: InitWalletFromMnemonic) -> Self {
        Self::InitFromMnemonic(Box::new(err))
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum FullScan {
    #[error(transparent)]
    WalletNotUnlocked(#[from] NotUnlocked),

    #[error("failed to check for bitcoin address transactions")]
    #[diagnostic(code(check_address_transactions))]
    CheckAddressTransactions {
        address: bitcoin::Address,
        error: Either<bdk_electrum::electrum_client::Error, bdk_esplora::esplora_client::Error>,
    },

    #[error(transparent)]
    ListHeaders(#[from] crate::validator::ListHeadersError),

    #[error("unable to create checkpoint from headers{}", .last_successful_header.as_ref().map_or(String::new(), |cp| format!(", last successful header at height {}", cp.height())))]
    #[diagnostic(code(create_checkpoint_from_headers))]
    CreateCheckPointFromHeaders {
        last_successful_header: Option<CheckPoint>,
    },

    #[error(transparent)]
    EsploraSync(#[from] bdk_esplora::esplora_client::Error),

    #[error(transparent)]
    ElectrumSync(#[from] bdk_electrum::electrum_client::Error),

    #[error(transparent)]
    CannotConnect(#[from] bdk_wallet::chain::local_chain::CannotConnectError),

    #[error("unable to persist wallet post scan")]
    PersistWallet(#[source] SqliteError),

    #[error("chain sync source does not support full scan: {:?}", .sync_source)]
    #[diagnostic(code(invalid_sync_source))]
    InvalidSyncSource { sync_source: WalletSyncSource },
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
    fn builder(&self) -> StatusBuilder {
        const BITCOIN_CORE_RPC_ERROR_H_NOT_FOUND: i32 = -18;
        match &self.error {
            jsonrpsee::core::client::Error::Call(err)
                if err.code() == BITCOIN_CORE_RPC_ERROR_H_NOT_FOUND
                    && err.message().contains("No wallet is loaded") =>
            {
                // Try being super precise here. Easy to confuse the /enforcer/ wallet not being
                // loaded with the /bitcoin core/ wallet not being loaded.
                let err_msg = "the underlying Bitcoin Core node has no loaded wallet (fix this: `bitcoin-cli loadwallet WALLET_NAME`)";
                StatusBuilder {
                    code: tonic::Code::FailedPrecondition,
                    fmt_message: Box::new(|f| err_msg.fmt(f)),
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
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::new(self)
    }
}

#[derive(Debug, Diagnostic, Error)]
enum GetBundleProposalsInner {
    #[error(transparent)]
    BlindedM6(#[from] crate::types::BlindedM6Error),
    #[error(transparent)]
    ConsensusEncoding(#[from] bitcoin::consensus::encode::Error),
    #[error(transparent)]
    GetPendingWithdrawals(#[from] crate::validator::GetPendingWithdrawalsError),
    #[error("rusqlite error")]
    Rusqlite(#[from] rusqlite::Error),
}

impl ToStatus for GetBundleProposalsInner {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::BlindedM6(err) => err.builder(),
            Self::GetPendingWithdrawals(err) => err.builder(),
            Self::ConsensusEncoding(err) => StatusBuilder::new(err),
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
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::with_code(self, self.0.builder())
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum FetchTransaction {
    #[error(transparent)]
    BitcoinCoreRPC(#[from] BitcoinCoreRPC),
    #[error(transparent)]
    Convert(#[from] bitcoin::consensus::encode::Error),
    #[error(transparent)]
    DeserializeHex(#[from] bitcoin::consensus::encode::FromHexError),
}

impl ToStatus for FetchTransaction {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::BitcoinCoreRPC(err) => err.builder(),
            Self::Convert(err) => StatusBuilder::new(err),
            Self::DeserializeHex(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum CreateDepositPsbt {
    #[error(transparent)]
    AddForeignUtxo(#[from] bdk_wallet::AddForeignUtxoError),
    #[error("failed to create tx")]
    CreateTx(#[from] bdk_wallet::error::CreateTxError),
    #[error("failed to fetch transaction (`{txid}`)")]
    FetchTransaction {
        txid: bitcoin::Txid,
        source: FetchTransaction,
    },
    #[error(transparent)]
    NotUnlocked(#[from] NotUnlocked),
    #[error("failed to parse sidechain number")]
    ParseSidechainNumber,
}

impl ToStatus for CreateDepositPsbt {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::AddForeignUtxo(err) => StatusBuilder::new(err),
            Self::FetchTransaction { source, .. } => {
                StatusBuilder::with_code(self, source.builder())
            }
            Self::NotUnlocked(err) => err.builder(),
            Self::CreateTx(_) | Self::ParseSidechainNumber => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum CreateDeposit {
    #[error("failed to broadcast tx")]
    BroadcastTx(#[source] jsonrpsee::core::ClientError),
    #[error("failed to broadcast nonstandard tx")]
    BroadcastNonstandardTx(#[source] bitcoin_send_tx_p2p::Error),
    #[error("broadcast deposit transaction failed: {txid}")]
    BroadcastUnsuccessful { txid: bitcoin::Txid },
    #[error("failed to convert sidechain address to PushBytesBuf")]
    ConvertSidechainAddress(#[source] bitcoin::script::PushBytesError),
    #[error(transparent)]
    Psbt(#[from] CreateDepositPsbt),
    #[error(transparent)]
    SignTransaction(#[from] WalletSignTransaction),
    #[error(transparent)]
    TryGetCtip(#[from] validator::TryGetCtipError),
    #[error(transparent)]
    TryGetMainchainTipHeight(#[from] validator::TryGetMainchainTipHeightError),
}

impl ToStatus for CreateDeposit {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::BroadcastTx(_)
            | Self::BroadcastNonstandardTx(_)
            | Self::BroadcastUnsuccessful { .. }
            | Self::ConvertSidechainAddress(_) => StatusBuilder::new(self),
            Self::Psbt(err) => err.builder(),
            Self::SignTransaction(err) => err.builder(),
            Self::TryGetCtip(err) => err.builder(),
            Self::TryGetMainchainTipHeight(err) => err.builder(),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GenerateCoinbaseTxouts {
    #[error(transparent)]
    CoinbaseMessages(#[from] crate::messages::CoinbaseMessagesError),
    #[error("transparent")]
    GetBundleProposals(#[from] crate::wallet::error::GetBundleProposals),
    #[error(transparent)]
    GetPendingWithdrawals(#[from] crate::validator::GetPendingWithdrawalsError),
    #[error(transparent)]
    GetSidechains(#[from] crate::validator::GetSidechainsError),
    #[error(transparent)]
    PushBytes(#[from] bitcoin::script::PushBytesError),
    #[error("rusqlite error")]
    Rusqlite(#[from] rusqlite::Error),
}

impl ToStatus for GenerateCoinbaseTxouts {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::CoinbaseMessages(err) => err.builder(),
            Self::GetBundleProposals(err) => err.builder(),
            Self::GetPendingWithdrawals(err) => err.builder(),
            Self::GetSidechains(err) => err.builder(),
            Self::PushBytes(err) => StatusBuilder::new(err),
            Self::Rusqlite(_) => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GenerateSuffixTxs {
    #[error(transparent)]
    GetBundleProposals(#[from] crate::wallet::error::GetBundleProposals),
    #[error(transparent)]
    M6(#[from] crate::types::AmountUnderflowError),
    #[error("Missing ctip for sidechain {sidechain_id}")]
    MissingCtip { sidechain_id: SidechainNumber },
}

impl ToStatus for GenerateSuffixTxs {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::GetBundleProposals(err) => err.builder(),
            Self::M6(err) => err.builder(),
            Self::MissingCtip { .. } => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum SelectBlockTxs {
    #[error(transparent)]
    BitcoinCoreRPC(#[from] BitcoinCoreRPC),
    #[error(transparent)]
    FetchTransaction(#[from] FetchTransaction),
    #[error(transparent)]
    GenerateSuffixTxs(#[from] GenerateSuffixTxs),
    #[error(transparent)]
    GetCtips(#[from] validator::GetCtipsError),
}

impl ToStatus for SelectBlockTxs {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::BitcoinCoreRPC(err) => err.builder(),
            Self::FetchTransaction(err) => err.builder(),
            Self::GenerateSuffixTxs(err) => err.builder(),
            Self::GetCtips(err) => err.builder(),
        }
    }
}

#[derive(Debug, Error)]
pub enum SqliteError {
    #[error(transparent)]
    Rusqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    TokioRusqlite(#[from] tokio_rusqlite::Error),
}

#[derive(Debug, Diagnostic, Error)]
pub enum ConnectBlock {
    #[error("failed connecting block to BDK chain")]
    #[diagnostic(code(connect_block_error))]
    BdkConnect(#[from] bdk_wallet::chain::local_chain::CannotConnectError),
    #[error(transparent)]
    ConnectBlock(#[from] <Validator as CusfEnforcer>::ConnectBlockError),
    #[error(transparent)]
    GetBlockInfo(#[from] validator::GetBlockInfoError),
    #[error(transparent)]
    GetBlockInfos(#[from] validator::GetBlockInfosError),
    #[error("unable to fetch block from mainchain")]
    #[diagnostic(code(fetch_block_error))]
    GetBlock(#[source] BitcoinCoreRPC),
    #[error("unable to fetch block hash from mainchain")]
    #[diagnostic(code(fetch_block_hash_error))]
    GetBlockHash(#[source] BitcoinCoreRPC),
    #[error("unable to fetch missing block")]
    #[diagnostic(code(fetch_block_error))]
    FetchBlock(#[source] BitcoinCoreRPC),
    #[error(transparent)]
    GetHeaderInfo(#[from] validator::GetHeaderInfoError),
    #[error("rusqlite error")]
    Sqlite(#[from] SqliteError),
    #[error(transparent)]
    WalletNotUnlocked(#[from] NotUnlocked),
}

impl From<tokio_rusqlite::Error> for ConnectBlock {
    fn from(error: tokio_rusqlite::Error) -> Self {
        Self::Sqlite(SqliteError::TokioRusqlite(error))
    }
}

impl From<rusqlite::Error> for ConnectBlock {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(SqliteError::Rusqlite(error))
    }
}

#[derive(Debug, Diagnostic, Error)]
pub(in crate::wallet) enum InitialBlockTemplateInner {
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
    #[error(transparent)]
    CoinbaseMessages(#[from] CoinbaseMessagesError),
    #[error("Failed to apply initial block template")]
    InitialBlockTemplate,
    #[error("Failed to generate coinbase txouts suffix")]
    GenerateSuffixCoinbaseTxouts(#[source] bitcoin::script::PushBytesError),
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

#[derive(Debug, Diagnostic, Error)]
pub enum FinalizeBlock {
    #[error(transparent)]
    GetHeaderInfo(#[from] validator::GetHeaderInfoError),
    #[error(transparent)]
    GetMainchainTip(#[from] validator::GetMainchainTipError),
    #[error(transparent)]
    GetNewAddress(#[from] GetNewAddress),
    #[error(transparent)]
    Script(#[from] bitcoin::script::PushBytesError),
    #[error(transparent)]
    SystemTime(#[from] std::time::SystemTimeError),
}

impl ToStatus for FinalizeBlock {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::GetHeaderInfo(err) => err.builder(),
            Self::GetMainchainTip(err) => err.builder(),
            Self::GetNewAddress(err) => err.builder(),
            Self::Script(err) => StatusBuilder::new(err),
            Self::SystemTime(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum Mine {
    #[error(transparent)]
    BitcoinCoreRPC(#[from] BitcoinCoreRPC),
    #[error(transparent)]
    EncodeBlock(#[from] EncodeBlock),
    #[error(transparent)]
    FinalizeBlock(#[from] FinalizeBlock),
}

impl ToStatus for Mine {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::BitcoinCoreRPC(err) => err.builder(),
            Self::EncodeBlock(err) => err.builder(),
            Self::FinalizeBlock(err) => err.builder(),
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
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::new(self).code(if self.source.is_some() {
            tonic::Code::Internal
        } else {
            tonic::Code::FailedPrecondition
        })
    }
}

#[derive(Diagnostic, Debug, Error)]
pub enum VerifyCanMine {
    #[error(transparent)]
    BitcoinCoreRPC(#[from] BitcoinCoreRPC),
    #[error(transparent)]
    MissingBinary(#[from] MissingBinary),
    #[error("cannot generate more than one block on signet")]
    MultipleBlocksOnSignet,
    #[error("cannot generate blocks on network (`{0}`)")]
    Network(bitcoin::Network),
    #[error("no signet challenge found")]
    NoSignetChallengeFound,
    #[error("unable to parse signet challenge")]
    ParseSignetChallenge(#[from] bitcoin::address::FromScriptError),
    #[error("signet challenge address (`{0}`) is not in mainchain wallet")]
    SignetChallengeAddressMissing(bitcoin::Address),
}

impl ToStatus for VerifyCanMine {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::BitcoinCoreRPC(err) => err.builder(),
            Self::MissingBinary(err) => err.builder(),
            Self::MultipleBlocksOnSignet => {
                StatusBuilder::new(self).code(tonic::Code::InvalidArgument)
            }
            Self::Network(_) | Self::SignetChallengeAddressMissing(_) => {
                StatusBuilder::new(self).code(tonic::Code::FailedPrecondition)
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
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::CreateSignetMinerDir(err) | Self::DownloadSignetMiner(err) => {
                StatusBuilder::with_code(self, err.builder())
            }
        }
    }
}

#[derive(Diagnostic, Debug, Error)]
pub enum GenerateSignetBlock {
    #[error("failed to fetch most recent block hash")]
    FetchMostRecentBlockHash(#[source] BitcoinCoreRPC),
    #[error(transparent)]
    GetHeaderInfo(#[from] validator::GetHeaderInfoError),
    #[error(transparent)]
    GetMainchainTip(#[from] validator::GetMainchainTipError),
    #[error(transparent)]
    GetSignetMinerPath(#[from] GetSignetMinerPath),
    #[error(transparent)]
    Mine(#[from] crate::bins::CommandError),
}

impl ToStatus for GenerateSignetBlock {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::FetchMostRecentBlockHash(err) => StatusBuilder::with_code(self, err.builder()),
            Self::GetHeaderInfo(err) => err.builder(),
            Self::GetMainchainTip(err) => err.builder(),
            Self::GetSignetMinerPath(err) => err.builder(),
            Self::Mine(err) => err.builder(),
        }
    }
}

#[derive(Diagnostic, Debug, Error)]
pub enum GenerateBlock {
    #[error(transparent)]
    CoinbaseBuilder(#[from] CoinbaseMessagesError),
    #[error("failed to delete BMM requests")]
    DeleteBmmRequests(#[source] rusqlite::Error),
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
    TryGetMainchainTip(#[from] validator::TryGetMainchainTipError),
    #[error("validator is not synced")]
    ValidatorNotSynced,
}

impl ToStatus for GenerateBlock {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::CoinbaseBuilder(err) => err.builder(),
            Self::GenerateCoinbaseTxouts(err) => err.builder(),
            Self::GenerateSignetBlock(err) => err.builder(),
            Self::Mine(err) => err.builder(),
            Self::SelectBlockTxs(err) => err.builder(),
            Self::TryGetMainchainTip(err) => err.builder(),
            Self::DeleteBmmRequests(_) | Self::PushBytesBuf(_) | Self::ValidatorNotSynced => {
                StatusBuilder::new(self)
            }
        }
    }
}

#[derive(Diagnostic, Debug, Error)]
pub enum GetWalletBalance {
    #[error(transparent)]
    NotSynced(#[from] NotSynced),
    #[error(transparent)]
    NotUnlocked(#[from] NotUnlocked),
}

impl ToStatus for GetWalletBalance {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::NotSynced(err) => err.builder(),
            Self::NotUnlocked(err) => err.builder(),
        }
    }
}

#[derive(Diagnostic, Debug, Error)]
pub enum ListWalletTransactions {
    #[error("unable to fetch wallet transaction")]
    FetchTransaction {
        txid: bitcoin::Txid,
        source: BitcoinCoreRPC,
    },
    #[error(transparent)]
    DeserializeHex(#[from] bitcoin::consensus::encode::FromHexError),
    #[error(transparent)]
    NotUnlocked(#[from] NotUnlocked),
}

impl ToStatus for ListWalletTransactions {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::FetchTransaction { txid, source } => StatusBuilder::new(source)
                .message(move |f| write!(f, "unable to fetch wallet transaction `{txid:?}`")),
            Self::DeserializeHex(err) => StatusBuilder::new(err),
            Self::NotUnlocked(err) => err.builder(),
        }
    }
}

#[derive(Diagnostic, Debug, Error)]
pub enum ListSidechainDepositTransactions {
    #[error(transparent)]
    GetTreasuryUtxo(#[from] validator::GetTreasuryUtxoError),
    #[error(transparent)]
    ListWalletTransactions(#[from] ListWalletTransactions),
    #[error(transparent)]
    TryGetCtip(#[from] validator::TryGetCtipError),
    #[error(transparent)]
    TryGetCtipValueSeq(#[from] validator::TryGetCtipValueSeqError),
}

impl ToStatus for ListSidechainDepositTransactions {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::GetTreasuryUtxo(err) => err.builder(),
            Self::ListWalletTransactions(err) => err.builder(),
            Self::TryGetCtip(err) => err.builder(),
            Self::TryGetCtipValueSeq(err) => err.builder(),
        }
    }
}

#[derive(Diagnostic, Debug, Error)]
pub enum CreateSendPsbt {
    #[error(transparent)]
    CreateTx(#[from] bdk_wallet::error::CreateTxError),
    #[error(transparent)]
    NotUnlocked(#[from] NotUnlocked),
    #[error(transparent)]
    Script(#[from] bitcoin::script::PushBytesError),
    #[diagnostic(code(create_send_transaction_add_utxo))]
    #[error("UTXO is not in wallet (`{}:{}`)", .0.txid, .0.vout)]
    UnknownUTXO(bitcoin::OutPoint),
}

impl ToStatus for CreateSendPsbt {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::UnknownUTXO(_) => StatusBuilder::new(self).code(tonic::Code::InvalidArgument),
            Self::NotUnlocked(err) => err.builder(),
            Self::CreateTx(err) => StatusBuilder::new(err),
            Self::Script(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Diagnostic, Debug, Error)]
pub enum SendWalletTransaction {
    #[error("failed to broadcast tx")]
    BroadcastTx(#[source] jsonrpsee::core::ClientError),
    #[error(transparent)]
    CreateSendPsbt(#[from] CreateSendPsbt),
    #[error(transparent)]
    SignTransaction(#[from] WalletSignTransaction),
    #[error(
        "failed to broadcast OP_DRIVECHAIN transaction (make sure your node has 'acceptnonstdtxn=1' in its configuration)"
    )]
    OpDrivechainNotSupported,
    #[error(transparent)]
    NotUnlocked(#[from] NotUnlocked),
    #[error(transparent)]
    Persistence(#[from] Persistence),
}

impl ToStatus for SendWalletTransaction {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::CreateSendPsbt(err) => err.builder(),
            Self::SignTransaction(err) => err.builder(),
            Self::BroadcastTx(_) | Self::OpDrivechainNotSupported => StatusBuilder::new(self),
            Self::NotUnlocked(err) => err.builder(),
            Self::Persistence(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum BuildBmmTx {
    #[error(transparent)]
    CreateTx(#[from] bdk_wallet::error::CreateTxError),
    #[error(transparent)]
    NotUnlocked(#[from] NotUnlocked),
    #[error(transparent)]
    Script(#[from] bitcoin::script::PushBytesError),
}

impl ToStatus for BuildBmmTx {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::CreateTx(err) => StatusBuilder::new(err),
            Self::NotUnlocked(err) => err.builder(),
            Self::Script(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
enum CreateBmmRequestInner {
    #[error("failed to build BMM tx")]
    BuildBmmTx(#[from] BuildBmmTx),
    #[error("rusqlite error")]
    Rusqlite(#[from] rusqlite::Error),
    #[error("failed to sign BMM tx")]
    SignTx(#[from] WalletSignTransaction),
}

impl ToStatus for CreateBmmRequestInner {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::BuildBmmTx(err) => StatusBuilder::with_code(self, err.builder()),
            Self::Rusqlite(_) => StatusBuilder::new(self),
            Self::SignTx(err) => StatusBuilder::with_code(self, err.builder()),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("error creating BMM request")]
#[repr(transparent)]
pub struct CreateBmmRequest(#[source] CreateBmmRequestInner);

impl<T> From<T> for CreateBmmRequest
where
    CreateBmmRequestInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

impl ToStatus for CreateBmmRequest {
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::with_code(self, self.0.builder())
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetNewAddress {
    #[error(transparent)]
    NotUnlocked(#[from] NotUnlocked),
    #[error(transparent)]
    Persistence(#[from] Persistence),
}

impl ToStatus for GetNewAddress {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::NotUnlocked(err) => err.builder(),
            Self::Persistence(err) => StatusBuilder::new(err),
        }
    }
}
