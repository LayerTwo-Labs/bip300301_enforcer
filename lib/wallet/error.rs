use std::{fmt::Debug, path::PathBuf};

use bdk_esplora::esplora_client;
use bitcoin_jsonrpsee::jsonrpsee::core::client::Error as JsonRpcError;
use cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer;
use miette::Diagnostic;
use serde::Deserialize;
use thiserror::Error;

pub use crate::block_producer::error::{
    BitcoinCoreRPC, FinalizeBlockTemplate, GenerateCoinbaseTxouts, GetBundleProposals,
    InitDbConnection, InitialBlockTemplate,
};
use crate::{
    proto::{StatusBuilder, ToStatus},
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
    fn builder(&self) -> StatusBuilder<'_> {
        StatusBuilder::new(self).code(match self.code {
            // https://github.com/bitcoin/bitcoin/blob/e8f72aefd20049eac81b150e7f0d33709acd18ed/src/common/messages.cpp
            -25 => connectrpc::ErrorCode::InvalidArgument,
            _ => connectrpc::ErrorCode::Unknown,
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

/// Wallet not synced
#[derive(Debug, Diagnostic, Error)]
#[diagnostic(code(wallet_not_synced))]
#[error("enforcer wallet not synced")]
pub struct NotSynced;

impl ToStatus for NotSynced {
    fn builder(&self) -> StatusBuilder<'_> {
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
    fn builder(&self) -> StatusBuilder<'_> {
        StatusBuilder::new(self)
    }
}

/// Wallet data mismatch. The field is BDK's account of what differs, without
/// which the report cannot say whether the fix is a different data dir or a
/// different build.
#[derive(Debug, Diagnostic, Error)]
#[diagnostic(code(wallet_data_mismatch))]
#[error(
    "enforcer wallet data mismatch, data directory content does not line up \
     with wallet descriptor: {0}"
)]
pub struct DataMismatch(String);

impl ToStatus for DataMismatch {
    fn builder(&self) -> StatusBuilder<'_> {
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
    fn builder(&self) -> StatusBuilder<'_> {
        StatusBuilder::new(self).code(match self {
            Self::NotSynced(_) => connectrpc::ErrorCode::FailedPrecondition,
            Self::InvalidPassword => connectrpc::ErrorCode::InvalidArgument,
            Self::DataMismatch(_) => connectrpc::ErrorCode::Internal,
            Self::NotFound => connectrpc::ErrorCode::NotFound,
            Self::AlreadyExists => connectrpc::ErrorCode::AlreadyExists,
            Self::AlreadyUnlocked => connectrpc::ErrorCode::AlreadyExists,
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
    fn builder(&self) -> StatusBuilder<'_> {
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
    fn builder(&self) -> StatusBuilder<'_> {
        StatusBuilder::new(self)
    }
}

/// Opening the wallet's seed store (`seed.json`), including the automatic
/// migration of a legacy seed out of the pre-split `db.sqlite`.
#[derive(Debug, Diagnostic, Error)]
pub enum InitSeedStore {
    #[error(
        "invalid legacy wallet seed in `wallet_seeds` (plaintext_mnemonic=`{}`, iv=`{}`, ciphertext=`{}`, key_salt=`{}`)",
        Self::display_is_some(*.plaintext_mnemonic_is_some),
        Self::display_is_some(*.iv_is_some),
        Self::display_is_some(*.ciphertext_is_some),
        Self::display_is_some(*.key_salt_is_some),
    )]
    InvalidLegacySeed {
        plaintext_mnemonic_is_some: bool,
        iv_is_some: bool,
        ciphertext_is_some: bool,
        key_salt_is_some: bool,
    },
    #[error(
        "the wallet seed in the legacy `wallet_seeds` table of {} does not match the seed in {}",
        legacy_db.display(),
        seed_file.display(),
    )]
    #[diagnostic(help(
        "the wallet cannot tell which seed is the right one, \
         resolve manually before starting again"
    ))]
    LegacySeedMismatch {
        legacy_db: std::path::PathBuf,
        seed_file: std::path::PathBuf,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    ParseMnemonic(#[from] ParseMnemonic),
    #[error("failed to read seed file {}", seed_file.display())]
    ReadSeedFile {
        seed_file: std::path::PathBuf,
        #[source]
        source: ReadSeed,
    },
    #[error(transparent)]
    Rusqlite(#[from] rusqlite::Error),
    #[error(
        "the seed file {} does not read back as written",
        seed_file.display()
    )]
    VerifySeedFile { seed_file: std::path::PathBuf },
}

impl InitSeedStore {
    /// Display a bool that indicates if an `Option<_>` is `Some` or `None`.
    fn display_is_some(is_some: bool) -> &'static str {
        if is_some { "Some" } else { "None" }
    }
}

/// Persisting the seed of a newly created wallet.
#[derive(Debug, Diagnostic, Error)]
pub enum InsertSeed {
    #[error("a wallet seed already exists")]
    AlreadyExists,
    #[error("failed to write the seed file")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Diagnostic, Error)]
pub(crate) enum ReadSeedInner {
    #[error("failed to read the seed file")]
    Io(#[source] std::io::Error),
    #[error("failed to parse the seed file")]
    Json(#[source] serde_json::Error),
    #[error(transparent)]
    ParseMnemonic(#[from] ParseMnemonic),
}

impl ToStatus for ReadSeedInner {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::ParseMnemonic(err) => err.builder(),
            Self::Io(_) | Self::Json(_) => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("failed to read wallet seed")]
#[repr(transparent)]
pub struct ReadSeed(#[source] ReadSeedInner);

impl<T> From<T> for ReadSeed
where
    ReadSeedInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

impl ToStatus for ReadSeed {
    fn builder(&self) -> StatusBuilder<'_> {
        StatusBuilder::with_code(self, self.0.builder())
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("failed to stretch password into AES key")]
#[repr(transparent)]
pub struct StretchPassword(#[from] argon2::Error);

impl ToStatus for StretchPassword {
    fn builder(&self) -> StatusBuilder<'_> {
        StatusBuilder::new(self)
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum EncryptMnemonic {
    #[error("failed to encrypt mnemonic")]
    Encrypt(#[from] aes_gcm::Error),
    #[error("failed to read from OS RNG")]
    SysRng(#[from] rand::rngs::SysError),
    #[error(transparent)]
    StretchPassword(#[from] StretchPassword),
}

impl ToStatus for EncryptMnemonic {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::Encrypt(_) | Self::SysRng(_) => StatusBuilder::new(self),
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
    ReadSeed(#[from] ReadSeed),
    #[error(transparent)]
    WalletInitialization(#[from] WalletInitialization),
}

impl From<InitWalletFromMnemonic> for UnlockExistingWallet {
    fn from(err: InitWalletFromMnemonic) -> Self {
        Self::InitWalletFromMnemonic(Box::new(err))
    }
}

impl ToStatus for UnlockExistingWallet {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::InitWalletFromMnemonic(err) => err.builder(),
            Self::NotEncrypted => StatusBuilder::new(self),
            Self::ReadSeed(err) => err.builder(),
            Self::WalletInitialization(err) => err.builder(),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum InitEsploraClient {
    #[error("failed to build esplora client")]
    BuildEsploraClient(#[source] esplora_client::Error),
    #[error("failed to get esplora height")]
    EsploraClientHeight(#[source] esplora_client::Error),
    #[error(
        "esplora URL must be explicitly provided for {}", 
        if network == &bitcoin::Network::Bitcoin {"mainnet".to_string()} else {network.to_string()}
    )]
    MissingUrl { network: bitcoin::Network },
    #[error(transparent)]
    ParseUrl(#[from] url::ParseError),
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
    #[error("electrum host and port must be explicitly provided for {network}")]
    MissingHostPort { network: bitcoin::Network },
}

impl InitElectrumClient {
    /// Whether the error is expected to clear up once electrs is reachable
    /// (vs. a config / wrong-chain error that retrying will never fix).
    fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::CreateElectrumClient(_) | Self::GetInitialBlockHeader(_)
        )
    }
}

impl InitEsploraClient {
    /// Whether the error is expected to clear up once the esplora server is
    /// reachable (vs. a config error that retrying will never fix).
    fn is_transient(&self) -> bool {
        matches!(self, Self::EsploraClientHeight(_))
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum InitChainSourceClient {
    #[error("failed to initialize electrum client")]
    Electrum(#[from] InitElectrumClient),
    #[error("failed to initialize esplora client")]
    Esplora(#[from] InitEsploraClient),
}

impl InitChainSourceClient {
    /// A transient error means the sync backend isn't reachable yet (e.g. electrs
    /// still starting up) and the connection should be retried.
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            Self::Electrum(err) => err.is_transient(),
            Self::Esplora(err) => err.is_transient(),
        }
    }
}

type Persistence = <crate::wallet::Persistence as bdk_wallet::AsyncWalletPersister>::Error;

#[derive(Debug, Diagnostic, Error)]
pub enum InitWalletFromMnemonic {
    #[error("failed to create wallet")]
    CreateWallet(#[source] Box<bdk_wallet::CreateWithPersistError<Persistence>>),
    #[diagnostic(transparent)]
    #[error("wallet data mismatch (wipe your data directory and try again)")]
    DataMismatch(#[source] DataMismatch),
    #[error("unable to derive xpriv from extended key")]
    DeriveXpriv,
    #[error("failed to load wallet")]
    LoadWallet(#[source] Box<bdk_wallet::LoadWithPersistError<Persistence>>),
    #[error("mnemonic key error")]
    MnemonicKey(#[from] bdk_wallet::keys::KeyError),
}

impl From<bdk_wallet::CreateWithPersistError<Persistence>> for InitWalletFromMnemonic {
    fn from(err: bdk_wallet::CreateWithPersistError<Persistence>) -> Self {
        Self::CreateWallet(Box::new(err))
    }
}

impl From<bdk_wallet::LoadWithPersistError<Persistence>> for InitWalletFromMnemonic {
    fn from(err: bdk_wallet::LoadWithPersistError<Persistence>) -> Self {
        // Matched on the variant, not the rendered message: BDK has no one
        // phrase common to its mismatches, so a substring test recognizes
        // none of them.
        match &err {
            bdk_wallet::LoadWithPersistError::InvalidChangeSet(
                bdk_wallet::LoadError::Mismatch(mismatch),
            ) => Self::DataMismatch(DataMismatch(mismatch.to_string())),
            _ => Self::LoadWallet(Box::new(err)),
        }
    }
}

impl ToStatus for InitWalletFromMnemonic {
    fn builder(&self) -> StatusBuilder<'_> {
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
    #[error("failed to fetch the node tip for the wallet birthday")]
    FetchBirthdayHeight(#[source] BitcoinCoreRPC),
    #[error("failed to generate mnemonic")]
    GenerateMnemonic(#[from] bdk_wallet::bip39::Error),
    #[error(transparent)]
    InitFromMnemonic(Box<InitWalletFromMnemonic>),
    #[error("failed to persist wallet seed")]
    InsertSeed(#[source] InsertSeed),
    #[error(transparent)]
    ReadSeed(#[from] ReadSeed),
    #[error("rusqlite error")]
    Rusqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    WalletInitialization(#[from] WalletInitialization),
}

impl From<InsertSeed> for CreateNewWallet {
    fn from(err: InsertSeed) -> Self {
        // Preserve the `AlreadyExists` gRPC status: a second CreateWallet call
        // must still report that a wallet is already there, not an opaque
        // DB error.
        match err {
            InsertSeed::AlreadyExists => {
                Self::WalletInitialization(WalletInitialization::AlreadyExists)
            }
            err => Self::InsertSeed(err),
        }
    }
}

impl From<InitWalletFromMnemonic> for CreateNewWallet {
    fn from(err: InitWalletFromMnemonic) -> Self {
        Self::InitFromMnemonic(Box::new(err))
    }
}

impl ToStatus for CreateNewWallet {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::EncryptMnemonic(err) => err.builder(),
            Self::FetchBirthdayHeight(err) => err.builder(),
            Self::GenerateMnemonic(_) => StatusBuilder::new(self),
            Self::InitFromMnemonic(err) => err.builder(),
            // `AlreadyExists` never lands here: `From<InsertSeed>` maps it to
            // `WalletInitialization`, keeping its status code.
            Self::InsertSeed(_) => StatusBuilder::new(self),
            Self::ReadSeed(err) => err.builder(),
            Self::Rusqlite(_) => StatusBuilder::new(self),
            Self::WalletInitialization(err) => err.builder(),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum InitWallet {
    #[error("failed to initialize the wallet seed store")]
    InitSeedStore(#[from] InitSeedStore),
    #[error(transparent)]
    InitChainSourceClient(#[from] InitChainSourceClient),
    // `#[source]`: a bare `Box<_>` is not a source to thiserror, and without
    // it the report says initialization failed and not a word of why.
    #[error("failed to initialize wallet from mnemonic")]
    InitFromMnemonic(#[source] Box<InitWalletFromMnemonic>),
    #[error("no signet challenge found")]
    NoSignetChallengeFound,
    #[error("failed to open connection to wallet DB")]
    OpenConnection(#[source] tokio_rusqlite::Error),
    #[error(transparent)]
    ParseNetwork(#[from] bitcoin::network::ParseNetworkError),
    #[error(transparent)]
    ReadSeed(#[from] ReadSeed),
}

impl From<InitWalletFromMnemonic> for InitWallet {
    fn from(err: InitWalletFromMnemonic) -> Self {
        Self::InitFromMnemonic(Box::new(err))
    }
}

#[derive(Debug, Diagnostic, Error)]
#[non_exhaustive]
pub enum ChainSourceClient {
    #[error("electrum client error")]
    Electrum(#[from] bdk_electrum::electrum_client::Error),
    #[error("esplora client error")]
    Esplora(#[from] bdk_esplora::esplora_client::Error),
}

pub mod full_scan {
    use miette::Diagnostic;
    use thiserror::Error;

    use crate::{
        proto::{StatusBuilder, ToStatus},
        wallet::{
            WalletSyncSource,
            error::{ChainSourceClient, NotUnlocked, SqliteError},
        },
    };

    #[derive(Debug, Diagnostic, Error)]
    pub enum Error {
        #[error(transparent)]
        WalletNotUnlocked(#[from] NotUnlocked),

        #[error(transparent)]
        ChainSourceClient(#[from] ChainSourceClient),

        #[error(transparent)]
        ListHeaders(#[from] crate::validator::ListHeadersError),

        #[error("unable to create checkpoint from headers{}", .last_successful_header.as_ref().map_or(String::new(), |cp| format!(", last successful header at height {}", cp.height())))]
        #[diagnostic(code(create_checkpoint_from_headers))]
        CreateCheckPointFromHeaders {
            last_successful_header: Option<bdk_chain::CheckPoint>,
        },

        #[error(transparent)]
        CannotConnect(#[from] bdk_wallet::chain::local_chain::CannotConnectError),

        #[error("unable to persist wallet post scan")]
        PersistWallet(#[source] SqliteError),

        #[error("no chain source client available for a full scan (sync source: {:?})", .sync_source)]
        #[diagnostic(code(invalid_sync_source))]
        InvalidSyncSource { sync_source: WalletSyncSource },

        #[error("a wallet full scan is already in progress")]
        #[diagnostic(code(full_scan_in_progress))]
        ScanInProgress,

        #[error(transparent)]
        TryGetMainchainTip(#[from] crate::validator::TryGetMainchainTipError),

        #[error("no mainchain tip: the validator has not synced a chain tip yet")]
        #[diagnostic(code(no_mainchain_tip))]
        NoMainchainTip,
    }

    impl ToStatus for Error {
        fn builder(&self) -> StatusBuilder<'_> {
            match self {
                Self::WalletNotUnlocked(err) => err.builder(),
                Self::InvalidSyncSource { .. } | Self::NoMainchainTip => {
                    StatusBuilder::new(self).code(connectrpc::ErrorCode::FailedPrecondition)
                }
                // A concurrency conflict rather than a bad request: the same
                // call succeeds once the running scan finishes.
                Self::ScanInProgress => {
                    StatusBuilder::new(self).code(connectrpc::ErrorCode::Aborted)
                }
                Self::TryGetMainchainTip(err) => err.builder(),
                Self::ChainSourceClient(err) => {
                    StatusBuilder::new(err).code(connectrpc::ErrorCode::Unavailable)
                }
                Self::ListHeaders(err) => {
                    StatusBuilder::new(err).code(connectrpc::ErrorCode::Internal)
                }
                Self::CannotConnect(err) => {
                    StatusBuilder::new(err).code(connectrpc::ErrorCode::Internal)
                }
                Self::CreateCheckPointFromHeaders { .. } | Self::PersistWallet(_) => {
                    StatusBuilder::new(self).code(connectrpc::ErrorCode::Internal)
                }
            }
        }
    }
}

pub use self::full_scan::Error as FullScan;

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
    fn builder(&self) -> StatusBuilder<'_> {
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
    fn builder(&self) -> StatusBuilder<'_> {
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

/// Applying an unconfirmed transaction to the wallet and persisting it
#[derive(Debug, Diagnostic, Error)]
pub enum ApplyUnconfirmedTx {
    #[error(transparent)]
    NotUnlocked(#[from] NotUnlocked),
    #[error("failed to persist wallet after applying unconfirmed tx")]
    Persist(#[from] Persistence),
}

impl ToStatus for ApplyUnconfirmedTx {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::NotUnlocked(err) => err.builder(),
            Self::Persist(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum CreateDeposit {
    #[error("failed to broadcast tx")]
    BroadcastTx(#[source] jsonrpsee::core::ClientError),
    #[error("failed to broadcast nonstandard tx to {peer_addr}")]
    BroadcastNonstandardTx {
        peer_addr: crate::p2p::BroadcastAddr,
        source: crate::p2p::BroadcastNonstandardTxError,
    },
    #[error("broadcast deposit transaction failed: {txid}")]
    BroadcastUnsuccessful { txid: bitcoin::Txid },
    #[error("failed to convert sidechain address to PushBytesBuf")]
    ConvertSidechainAddress(#[source] bitcoin::script::PushBytesError),
    #[error(transparent)]
    OutputAmountOverflow(#[from] crate::types::AmountOverflowError),
    #[error(transparent)]
    ApplyUnconfirmedTx(#[from] ApplyUnconfirmedTx),
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
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::BroadcastTx(_)
            | Self::BroadcastNonstandardTx { .. }
            | Self::BroadcastUnsuccessful { .. }
            | Self::ConvertSidechainAddress(_) => StatusBuilder::new(self),
            Self::OutputAmountOverflow(err) => err.builder(),
            Self::ApplyUnconfirmedTx(err) => err.builder(),
            Self::Psbt(err) => err.builder(),
            Self::SignTransaction(err) => err.builder(),
            Self::TryGetCtip(err) => err.builder(),
            Self::TryGetMainchainTipHeight(err) => err.builder(),
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
pub(in crate::wallet) enum HandleConnectBlockInner {
    #[error("rusqlite error")]
    Sqlite(#[from] SqliteError),
    #[error(transparent)]
    WalletNotUnlocked(#[from] NotUnlocked),
}

impl From<tokio_rusqlite::Error> for HandleConnectBlockInner {
    fn from(error: tokio_rusqlite::Error) -> Self {
        Self::Sqlite(SqliteError::TokioRusqlite(error))
    }
}

impl From<rusqlite::Error> for HandleConnectBlockInner {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(SqliteError::Rusqlite(error))
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct HandleConnectBlock(HandleConnectBlockInner);

impl<Err> From<Err> for HandleConnectBlock
where
    HandleConnectBlockInner: From<Err>,
{
    fn from(err: Err) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Diagnostic, Error)]
pub(in crate::wallet) enum ConnectMissingBlockInner {
    #[error("failed connecting block at height {block_height} to BDK chain")]
    #[diagnostic(code(connect_block_error))]
    BdkConnect {
        block_height: u32,
        source: bdk_wallet::chain::local_chain::CannotConnectError,
    },
    #[error("unable to fetch block from mainchain")]
    #[diagnostic(code(fetch_block_error))]
    GetBlock(#[source] BitcoinCoreRPC),
    #[error(transparent)]
    GetBlockInfos(#[from] validator::GetBlockInfosError),
    #[error("unable to fetch block hash from mainchain")]
    #[diagnostic(code(fetch_block_hash_error))]
    GetBlockHash(#[source] BitcoinCoreRPC),
    #[error("failed to handle connecting block to wallet")]
    HandleConnectBlock(#[from] HandleConnectBlock),
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct ConnectMissingBlock(ConnectMissingBlockInner);

impl<Err> From<Err> for ConnectMissingBlock
where
    ConnectMissingBlockInner: From<Err>,
{
    fn from(err: Err) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Diagnostic, Error)]
pub(in crate::wallet) enum SyncWalletToTipInner {
    #[error("failed to connect missing block to wallet")]
    ConnectMissingBlock(#[from] ConnectMissingBlock),
    #[error("failed to catch wallet chain up to tip")]
    ChainCatchUp(#[from] FullScan),
    #[error("unable to fetch block from mainchain")]
    #[diagnostic(code(fetch_block_error))]
    GetBlock(#[source] BitcoinCoreRPC),
    #[error(transparent)]
    GetBlockInfos(#[from] validator::GetBlockInfosError),
    #[error(transparent)]
    GetHeaderInfo(#[from] validator::GetHeaderInfoError),
    #[error("failed to handle connecting block to wallet")]
    HandleConnectBlock(#[from] HandleConnectBlock),
    #[error(transparent)]
    WalletNotUnlocked(#[from] NotUnlocked),
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct SyncWalletToTip(SyncWalletToTipInner);

impl<Err> From<Err> for SyncWalletToTip
where
    SyncWalletToTipInner: From<Err>,
{
    fn from(err: Err) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum InitialSync {
    #[error(transparent)]
    GetMainchainTip(#[from] validator::GetMainchainTipError),
    #[error("received shutdown signal")]
    Shutdown,
    #[error(transparent)]
    Validator(#[from] <Validator as CusfEnforcer>::SyncError),
    #[error(transparent)]
    Wallet(#[from] SyncWalletToTip),
}

#[derive(Debug, Diagnostic, Error)]
pub enum ConnectBlock {
    #[error(transparent)]
    NotUnlocked(#[from] NotUnlocked),
    #[error("failed to persist wallet after evicting BMM requests")]
    PersistBmmEviction(#[source] SqliteError),
    #[error(transparent)]
    Validator(#[from] <Validator as CusfEnforcer>::ConnectBlockError),
    #[error(transparent)]
    SyncWalletToTip(#[from] SyncWalletToTip),
}

#[derive(Diagnostic, Debug, Error)]
pub enum GetWalletBalance {
    #[error(transparent)]
    NotSynced(#[from] NotSynced),
    #[error(transparent)]
    NotUnlocked(#[from] NotUnlocked),
}

impl ToStatus for GetWalletBalance {
    fn builder(&self) -> StatusBuilder<'_> {
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
    fn builder(&self) -> StatusBuilder<'_> {
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
    fn builder(&self) -> StatusBuilder<'_> {
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
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::UnknownUTXO(_) => {
                StatusBuilder::new(self).code(connectrpc::ErrorCode::InvalidArgument)
            }
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
    ApplyUnconfirmedTx(#[from] ApplyUnconfirmedTx),
}

impl ToStatus for SendWalletTransaction {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::CreateSendPsbt(err) => err.builder(),
            Self::SignTransaction(err) => err.builder(),
            Self::BroadcastTx(_) | Self::OpDrivechainNotSupported => StatusBuilder::new(self),
            Self::ApplyUnconfirmedTx(err) => err.builder(),
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
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::CreateTx(err) => StatusBuilder::new(err),
            Self::NotUnlocked(err) => err.builder(),
            Self::Script(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub(in crate::wallet) enum CreateBmmRequestInner {
    #[error("failed to build BMM tx")]
    BuildBmmTx(#[from] BuildBmmTx),
    #[error("failed to broadcast nonstandard tx")]
    BroadcastNonstandardTx(#[source] crate::p2p::BroadcastNonstandardTxError),
    #[error("failed to broadcast BMM request tx via RPC")]
    BroadcastTxRpc(#[source] JsonRpcError),
    #[error("broadcast deposit transaction failed: {txid}")]
    BroadcastUnsuccessful { txid: bitcoin::Txid },
    #[error(transparent)]
    GetHeaderInfo(#[from] validator::GetHeaderInfoError),
    #[error("failed to sign BMM tx")]
    SignTx(#[from] WalletSignTransaction),
}

impl ToStatus for CreateBmmRequestInner {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::BuildBmmTx(err) => StatusBuilder::with_code(self, err.builder()),
            Self::GetHeaderInfo(err) => err.builder(),
            Self::SignTx(err) => StatusBuilder::with_code(self, err.builder()),
            Self::BroadcastNonstandardTx(_)
            | Self::BroadcastTxRpc(_)
            | Self::BroadcastUnsuccessful { .. } => StatusBuilder::new(self),
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
    fn builder(&self) -> StatusBuilder<'_> {
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
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::NotUnlocked(err) => err.builder(),
            Self::Persistence(err) => StatusBuilder::new(err),
        }
    }
}
