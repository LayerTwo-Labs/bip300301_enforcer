use std::{
    collections::HashMap,
    path::Path,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use bdk_chain::ChainPosition;
use bdk_electrum::{
    BdkElectrumClient,
    electrum_client::{self, ElectrumApi},
};
use bdk_esplora::esplora_client;
use bdk_wallet::{
    self, KeychainKind,
    keys::{DerivableKey as _, ExtendedKey, bip39::Mnemonic},
};
use bitcoin::{
    Amount, BlockHash, Network, Transaction, Txid,
    hashes::{Hash as _, HashEngine, sha256, sha256d},
    script::PushBytesBuf,
};
use bitcoin_jsonrpsee::{
    client::{GetRawTransactionClient, GetRawTransactionVerbose, MainClient as _},
    jsonrpsee::http_client::HttpClient,
};
use either::Either;
use fallible_iterator::{FallibleIterator as _, IteratorExt as _};
use futures::{TryFutureExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::instrument;
use uuid::Uuid;

use crate::{
    block_producer::BlockProducer,
    cli::{Config, WalletConfig, WalletSyncSource},
    convert,
    errors::ErrorChain,
    messages::{self, M8BmmRequest},
    types::{
        BDKWalletTransaction, BlindedM6, BmmCommitment, Ctip, M6id, SidechainNumber,
        SidechainProposal,
    },
    validator::{self, Validator},
    wallet::{
        error::WalletInitialization,
        mnemonic::{EncryptedMnemonic, KdfParams, new_mnemonic},
        seed_store::{Seed, SeedStore},
        util::{RwLockReadGuardSome, RwLockUpgradableReadGuardSome, RwLockWriteGuardSome},
    },
};

mod cusf_block_producer;
pub mod error;
pub mod mnemonic;
mod seed_store;
mod sync;
mod thread_safe_connection;
mod util;

pub(crate) type Persistence = thread_safe_connection::ThreadSafeConnection;
type BdkWallet = bdk_wallet::PersistedWallet<Persistence>;

type ElectrumClient = BdkElectrumClient<bdk_electrum::electrum_client::Client>;
type EsploraClient = bdk_esplora::esplora_client::AsyncClient;

#[non_exhaustive]
enum ChainSourceClient {
    Electrum(Box<ElectrumClient>),
    Esplora(EsploraClient),
}

/// Retry loop for a sync backend that was not reachable when the wallet was
/// created. Returned by [`Wallet::new`] for the caller to spawn.
pub struct ChainSourceInitTask {
    config: WalletConfig,
    network: Network,
    state_tx: tokio::sync::watch::Sender<Option<Arc<ChainSourceClient>>>,
}

impl ChainSourceInitTask {
    /// Publishes the client through the wallet's watch channel once the
    /// backend comes up. Returns without publishing on a non-transient error
    /// (the wallet then operates without a sync source), on `cancel`, or if
    /// every receiver is dropped.
    pub async fn run(self, cancel: CancellationToken) {
        const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
        const MAX_RETRY_DELAY: Duration = Duration::from_secs(10);
        let mut retry_delay = INITIAL_RETRY_DELAY;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = self.state_tx.closed() => return,
                () = tokio::time::sleep(retry_delay) => (),
            }
            match WalletInner::try_init_chain_source_client(&self.config, self.network).await {
                Ok(client) => {
                    tracing::info!("wallet sync backend became available");
                    let _send_res: Result<(), _> = self.state_tx.send(Some(Arc::new(client)));
                    return;
                }
                Err(err) if err.is_transient() => {
                    retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
                    tracing::warn!(
                        %err,
                        "wallet sync backend not ready, retrying in {retry_delay:?}",
                    );
                }
                Err(err) => {
                    tracing::error!(
                        "wallet sync backend initialization failed, the wallet will \
                         operate without a sync source: {:#}",
                        ErrorChain::new(&err)
                    );
                    return;
                }
            }
        }
    }
}

const fn default_esplora_url(network: Network) -> Option<&'static str> {
    match network {
        Network::Signet => Some("https://explorer.signet.drivechain.info/api"),
        Network::Bitcoin => Some("https://explorer.forknet.drivechain.info/api"),
        Network::Regtest => Some("http://localhost:3003"),
        _ => None,
    }
}

const fn default_electrum_host_port(network: Network) -> Option<(&'static str, u16)> {
    match network {
        Network::Signet => Some(("node.signet.drivechain.info", 50001)),
        Network::Bitcoin => Some(("node.forknet.drivechain.info", 50001)),
        Network::Regtest =>
        // Default for mempool/electrs
        {
            Some(("127.0.0.1", 60401))
        }
        _ => None,
    }
}

struct WalletInner {
    main_client: HttpClient,
    producer: BlockProducer,
    magic: bitcoin::p2p::Magic,
    // Unlocked, ready-to-go wallet: Some
    // Locked wallet: None
    bitcoin_wallet: async_lock::RwLock<Option<BdkWallet>>,
    /// Persistence for the BDK wallet.
    ///
    /// Lock order: when both are needed, take `bitcoin_wallet` before
    /// `bdk_db`
    bdk_db: tokio::sync::Mutex<Persistence>,
    seed_store: SeedStore,
    /// Handle to the configured chain source (Electrum/Esplora).
    ///
    /// The sync backend may not be reachable when the enforcer starts, and
    /// the wallet must not hold up the rest of the process while waiting for
    /// it. Holds `None` until a client exists.
    chain_source: tokio::sync::watch::Receiver<Option<Arc<ChainSourceClient>>>,
    last_sync: async_lock::RwLock<Option<SystemTime>>,
    config: Config,
}

impl WalletInner {
    fn validator(&self) -> &Validator {
        self.producer.validator()
    }

    /// The drivechain policy DB, owned by the producer. Policy only — the
    /// wallet's seed lives in its own [`SeedStore`], not in here.
    fn db(&self) -> &crate::block_producer::Db {
        self.producer.db()
    }
}

impl WalletInner {
    async fn init_esplora_client(
        config: &WalletConfig,
        network: Network,
    ) -> Result<EsploraClient, error::InitEsploraClient> {
        let esplora_url = match config.esplora_url.as_ref() {
            Some(url) => url,
            None => {
                let default_url = default_esplora_url(network)
                    .ok_or(error::InitEsploraClient::MissingUrl { network })?;
                &url::Url::parse(default_url)?
            }
        };

        tracing::info!(esplora_url = %esplora_url, "creating esplora client");

        // URLs with a port number at the end get a `/` when turned back into a string, for
        // some reason. The Esplora library doesn't like that! Remove it.
        let client = esplora_client::Builder::new(esplora_url.as_str().trim_end_matches("/"))
            .build_async()
            .map_err(error::InitEsploraClient::BuildEsploraClient)?;

        let height = client
            .get_height()
            .await
            .map_err(error::InitEsploraClient::EsploraClientHeight)?;

        tracing::info!(height = height, "esplora client initialized");
        Ok(client)
    }

    /// Initialize electrum client
    fn init_electrum_client(
        config: &WalletConfig,
        network: Network,
    ) -> Result<ElectrumClient, error::InitElectrumClient> {
        let (electrum_host, electrum_port) =
            match (config.electrum_host.as_deref(), config.electrum_port) {
                (Some(host), Some(port)) => (host, port),
                (host, port) => {
                    let (default_host, default_port) = default_electrum_host_port(network)
                        .ok_or(error::InitElectrumClient::MissingHostPort { network })?;
                    (host.unwrap_or(default_host), port.unwrap_or(default_port))
                }
            };
        let electrum_url = format!("{electrum_host}:{electrum_port}");

        tracing::debug!(%electrum_url, "creating electrum client");
        // Apply a reasonably short timeout to prevent the wallet from hanging
        let timeout = std::time::Duration::from_secs(5);
        let config = electrum_client::ConfigBuilder::new()
            .timeout(Some(timeout))
            .build();
        let electrum_client = electrum_client::Client::from_config(&electrum_url, config)
            .map_err(error::InitElectrumClient::CreateElectrumClient)?;
        let header = electrum_client
            .block_header(0)
            .map_err(error::InitElectrumClient::GetInitialBlockHeader)?;
        // Verify the Electrum server is on the same chain as we are.
        if header.block_hash().as_byte_array() != network.chain_hash().as_bytes() {
            return Err(error::InitElectrumClient::ChainMismatch {
                electrum_block_hash: header.block_hash(),
                wallet_chain_hash: network.chain_hash(),
            });
        }
        Ok(BdkElectrumClient::new(electrum_client))
    }

    async fn try_init_chain_source_client(
        config: &WalletConfig,
        network: Network,
    ) -> Result<ChainSourceClient, error::InitChainSourceClient> {
        match config.sync_source {
            WalletSyncSource::Electrum => {
                // The electrum client does synchronous socket I/O (with a 5s
                // timeout), which must not stall a runtime worker thread.
                let config = config.clone();
                tokio::task::spawn_blocking(move || Self::init_electrum_client(&config, network))
                    .await
                    .expect("electrum client init task panicked")
                    .map(|client| ChainSourceClient::Electrum(Box::new(client)))
                    .map_err(error::InitChainSourceClient::from)
            }
            WalletSyncSource::Esplora => Self::init_esplora_client(config, network)
                .await
                .map(ChainSourceClient::Esplora)
                .map_err(error::InitChainSourceClient::from),
            WalletSyncSource::Disabled => {
                unreachable!("callers check for a disabled sync source")
            }
        }
    }

    /// The sync backend may not be reachable yet when the enforcer starts.
    /// Rather than blocking startup, a transient connection failure hands the
    /// retrying off to a [`ChainSourceInitTask`], returned here for the
    /// caller to spawn.
    async fn init_chain_source(
        config: &WalletConfig,
        network: Network,
    ) -> Result<
        (
            tokio::sync::watch::Receiver<Option<Arc<ChainSourceClient>>>,
            Option<ChainSourceInitTask>,
        ),
        error::InitChainSourceClient,
    > {
        if config.sync_source == WalletSyncSource::Disabled {
            let (_state_tx, state_rx) = tokio::sync::watch::channel(None);
            return Ok((state_rx, None));
        }
        match Self::try_init_chain_source_client(config, network).await {
            Ok(client) => {
                let (_state_tx, state_rx) = tokio::sync::watch::channel(Some(Arc::new(client)));
                Ok((state_rx, None))
            }
            Err(err) if err.is_transient() => {
                tracing::warn!(
                    %err,
                    "wallet sync backend not reachable at startup, \
                     continuing without it and retrying in the background",
                );
                let (state_tx, state_rx) = tokio::sync::watch::channel(None);
                let init_task = ChainSourceInitTask {
                    config: config.clone(),
                    network,
                    state_tx,
                };
                Ok((state_rx, Some(init_task)))
            }
            Err(err) => Err(err),
        }
    }

    fn chain_source_client(&self) -> Option<Arc<ChainSourceClient>> {
        self.chain_source.borrow().clone()
    }

    async fn initialize_wallet_from_mnemonic(
        mnemonic: &Mnemonic,
        network: bdk_wallet::bitcoin::Network,
        wallet_database: &mut Persistence,
    ) -> Result<BdkWallet, error::InitWalletFromMnemonic> {
        let extended_key: ExtendedKey = mnemonic.clone().into_extended_key()?;

        let xpriv = extended_key
            .into_xprv(network.into())
            .ok_or(error::InitWalletFromMnemonic::DeriveXpriv)?;

        // Create a BDK wallet structure using BIP 84 descriptor ("m/84h/1h/0h/0" and "m/84h/1h/0h/1")
        let external_desc = format!("wpkh({xpriv}/84'/1'/0'/0/*)");
        let internal_desc = format!("wpkh({xpriv}/84'/1'/0'/1/*)");

        tracing::debug!("Attempting load of existing BDK wallet");
        let bitcoin_wallet = bdk_wallet::Wallet::load()
            .descriptor(KeychainKind::External, Some(external_desc.clone()))
            .descriptor(KeychainKind::Internal, Some(internal_desc.clone()))
            .extract_keys()
            .check_network(network)
            .load_wallet_async(wallet_database)
            .await?;

        let bitcoin_wallet = match bitcoin_wallet {
            Some(wallet) => {
                tracing::info!("Loaded existing BDK wallet");
                wallet
            }

            None => {
                tracing::info!("Creating new BDK wallet");

                bdk_wallet::Wallet::create(external_desc, internal_desc)
                    .network(network)
                    .create_wallet_async(wallet_database)
                    .await?
            }
        };

        Ok(bitcoin_wallet)
    }

    async fn new(
        data_dir: &Path,
        config: &Config,
        main_client: HttpClient,
        producer: BlockProducer,
        magic: bitcoin::p2p::Magic,
    ) -> Result<(Self, Option<ChainSourceInitTask>), error::InitWallet> {
        let network = {
            let validator_network = producer.validator().network();
            bdk_wallet::bitcoin::Network::from_str(validator_network.to_string().as_str())?
        };
        if network == bdk_wallet::bitcoin::Network::Signet && producer.signet_challenge().is_none()
        {
            return Err(error::InitWallet::NoSignetChallengeFound);
        }

        let database_path = data_dir.join("wallet.sqlite.db");

        tracing::info!(
            data_dir = %data_dir.display(),
            database_path = %database_path.display(),
            "Instantiating {} wallet",
            network,
        );

        let mut wallet_database = thread_safe_connection::ThreadSafeConnection::open(database_path)
            .await
            .map_err(error::InitWallet::OpenConnection)?;

        let (chain_source, chain_source_init) =
            Self::init_chain_source(&config.wallet_opts, network).await?;

        // If we:
        // 1. Already have an initialized wallet
        // 2. It's plaintext
        //
        // We can just go ahead and unlock the wallet right away.
        let seed_store = SeedStore::new(data_dir)?;

        let bitcoin_wallet = match seed_store.read_mnemonic().await? {
            Some(Either::Left(mnemonic)) => {
                tracing::debug!("found plaintext mnemonic, going straight to initialization");
                let initialized = WalletInner::initialize_wallet_from_mnemonic(
                    &mnemonic,
                    network,
                    &mut wallet_database,
                )
                .await?;

                Some(initialized)
            }
            _ => None,
        };

        tracing::debug!(
            message = "wallet inner: wired together components",
            wallet_initialized = bitcoin_wallet.is_some()
        );

        let inner = Self {
            config: config.clone(),
            main_client,
            producer,
            magic,
            bitcoin_wallet: async_lock::RwLock::new(bitcoin_wallet),
            bdk_db: tokio::sync::Mutex::new(wallet_database),
            seed_store,
            chain_source,
            last_sync: async_lock::RwLock::new(None),
        };
        Ok((inner, chain_source_init))
    }

    /// Warn if lock takes this long to acquire
    const LOCK_WARN_DURATION: Duration = Duration::from_secs(1);

    /// Await a lock on the inner wallet, warning if acquisition takes longer
    /// than [`Self::LOCK_WARN_DURATION`].
    async fn acquire_lock_warn_slow<Guard>(
        lock: impl std::future::Future<Output = Guard>,
        what: &str,
    ) -> Guard {
        use futures::future::{Either, select};
        tracing::trace!("wallet: acquiring {what}");
        match select(
            std::pin::pin!(lock),
            std::pin::pin!(tokio::time::sleep(Self::LOCK_WARN_DURATION)),
        )
        .await
        {
            Either::Left((guard, _sleep)) => guard,
            Either::Right(((), acquiring_lock)) => {
                tracing::warn!(
                    "wallet: waiting over {} to acquire {what}",
                    jiff::SignedDuration::try_from(Self::LOCK_WARN_DURATION).unwrap(),
                );
                acquiring_lock.await
            }
        }
    }

    async fn read_wallet(&self) -> Result<RwLockReadGuardSome<'_, BdkWallet>, error::NotUnlocked> {
        let read_guard =
            Self::acquire_lock_warn_slow(self.bitcoin_wallet.read(), "read lock").await;
        RwLockReadGuardSome::new(read_guard).ok_or(error::NotUnlocked)
    }

    /// Obtain an upgradable read lock on the inner wallet
    async fn read_wallet_upgradable(
        &self,
    ) -> Result<RwLockUpgradableReadGuardSome<'_, BdkWallet>, error::NotUnlocked> {
        let read_guard = Self::acquire_lock_warn_slow(
            self.bitcoin_wallet.upgradable_read(),
            "upgradable read lock",
        )
        .await;
        RwLockUpgradableReadGuardSome::new(read_guard).ok_or(error::NotUnlocked)
    }

    async fn write_wallet(
        &self,
    ) -> Result<RwLockWriteGuardSome<'_, BdkWallet>, error::NotUnlocked> {
        let write_guard =
            Self::acquire_lock_warn_slow(self.bitcoin_wallet.write(), "write lock").await;
        RwLockWriteGuardSome::new(write_guard).ok_or(error::NotUnlocked)
    }

    pub async fn create_new_wallet(
        &self,
        mnemonic: Option<Mnemonic>,
        password: Option<&str>,
    ) -> Result<(), error::CreateNewWallet> {
        let (mnemonic, generated) = match mnemonic {
            Some(mnemonic) => (mnemonic, false),
            None => {
                tracing::info!("create new wallet: no mnemonic provided, generating fresh");
                (new_mnemonic()?, true)
            }
        };

        let birthday_height = if generated {
            let info = self
                .main_client
                .get_blockchain_info()
                .await
                .map_err(|err| {
                    error::CreateNewWallet::FetchBirthdayHeight(error::BitcoinCoreRPC {
                        method: "getblockchaininfo".to_owned(),
                        error: err,
                    })
                })?;
            Some(info.blocks)
        } else {
            None
        };

        match password {
            Some(password) => {
                tracing::info!("create new wallet: persisting encrypted mnemonic");
                let encrypted =
                    EncryptedMnemonic::encrypt(&mnemonic, password, KdfParams::CURRENT)?;
                self.seed_store
                    .insert_seed(Seed::Encrypted(&encrypted), birthday_height)
                    .await?;
            }
            None => {
                tracing::info!(
                    "create new wallet: no password provided, persisting plaintext mnemonic"
                );
                self.seed_store
                    .insert_seed(Seed::Plaintext(&mnemonic), birthday_height)
                    .await?;
            }
        }

        let mut database = self.bdk_db.lock().await;
        let network = self.validator().network();
        let wallet =
            WalletInner::initialize_wallet_from_mnemonic(&mnemonic, network, &mut database).await?;
        drop(database);

        let mut write_guard = self.bitcoin_wallet.write().await;
        *write_guard = Some(wallet);
        drop(write_guard);
        Ok(())
    }

    pub async fn unlock_existing_wallet(
        &self,
        password: &str,
    ) -> Result<(), error::UnlockExistingWallet> {
        if self.bitcoin_wallet.read().await.is_some() {
            return Err(WalletInitialization::AlreadyUnlocked.into());
        }

        // Read the mnemonic from the database.
        let read = self.seed_store.read_mnemonic().await?;

        tracing::debug!("unlock wallet: read from DB");

        // Verify that it is encrypted!
        let encrypted = match read {
            None => {
                return Err(WalletInitialization::NotFound.into());
            }
            // Plaintext!
            Some(Either::Left(_)) => {
                return Err(error::UnlockExistingWallet::NotEncrypted);
            }
            Some(Either::Right(encrypted)) => encrypted,
        };

        tracing::debug!("unlock wallet: decrypting mnemonic");

        let mnemonic = encrypted.decrypt(password).map_err(|err| {
            tracing::error!("failed to decrypt mnemonic: {:#}", ErrorChain::new(&err));
            WalletInitialization::InvalidPassword
        })?;

        let mut database = self.bdk_db.lock().await;
        let network = self.validator().network();

        tracing::debug!("unlock wallet: initializing BDK wallet struct");
        let wallet =
            WalletInner::initialize_wallet_from_mnemonic(&mnemonic, network, &mut database).await?;
        drop(database);

        let mut write_guard = self.bitcoin_wallet.write().await;
        *write_guard = Some(wallet);
        drop(write_guard);

        tracing::info!("unlock wallet: initialized wallet");
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SidechainDepositTransaction {
    pub sidechain_number: SidechainNumber,
    pub deposit_amount: Amount,
    #[serde(with = "hex::serde")]
    pub destination_address: Vec<u8>,
    pub wallet_tx: BDKWalletTransaction,
}

/// Optional parameters for sending a wallet transaction
#[derive(Debug, Default)]
pub struct CreateTransactionParams {
    /// Optional fee policy to use for the transaction
    pub fee_policy: Option<crate::types::FeePolicy>,
    /// Optional OP_RETURN message to include in the transaction
    pub op_return_message: Option<Vec<u8>>,
    /// Optional UTXOs that must be included in the transaction
    pub required_utxos: Vec<bdk_wallet::bitcoin::OutPoint>,
    // If set, sends ALL UTXOs in the wallet to this address.
    // Incompatible with `required_utxos`.
    pub drain_wallet_to: Option<bdk_wallet::bitcoin::Address>,
}

pub struct WalletInfo {
    // Public (i.e. without private keys) descriptors for the wallet
    pub keychain_descriptors: std::collections::HashMap<
        bdk_wallet::KeychainKind,
        bdk_wallet::descriptor::ExtendedDescriptor,
    >,
    pub network: bdk_wallet::bitcoin::Network,
    pub transaction_count: usize,
    pub unspent_output_count: usize,
    pub tip: (BlockHash, u32),
}

/// Cheap to clone, since it uses Arc internally
#[derive(Clone)]
pub struct Wallet {
    inner: Arc<WalletInner>,
}

impl Wallet {
    /// Also returns the retry task for a sync backend that was unreachable at
    /// startup (if any), which the caller is responsible for spawning.
    pub async fn new(
        data_dir: &Path,
        config: &Config,
        main_client: HttpClient,
        producer: BlockProducer,
        magic: bitcoin::p2p::Magic,
    ) -> Result<(Self, Option<ChainSourceInitTask>), error::InitWallet> {
        let (inner, chain_source_init) =
            WalletInner::new(data_dir, config, main_client, producer, magic).await?;
        let wallet = Self {
            inner: Arc::new(inner),
        };
        Ok((wallet, chain_source_init))
    }

    /// The keyless block producer underneath this wallet.
    pub fn producer(&self) -> &BlockProducer {
        &self.inner.producer
    }

    pub async fn propose_sidechain(
        &self,
        proposal: &SidechainProposal,
    ) -> Result<(), rusqlite::Error> {
        self.inner.db().propose_sidechain(proposal).await
    }

    pub async fn ack_sidechain(
        &self,
        sidechain_number: SidechainNumber,
        data_hash: sha256d::Hash,
    ) -> Result<(), rusqlite::Error> {
        self.inner
            .db()
            .ack_sidechain(sidechain_number, data_hash)
            .await
    }

    pub async fn nack_sidechain(
        &self,
        sidechain_number: u8,
        data_hash: &[u8; 32],
    ) -> Result<(), rusqlite::Error> {
        self.inner
            .db()
            .nack_sidechain(sidechain_number, data_hash)
            .await
    }

    pub async fn put_withdrawal_bundle(
        &self,
        sidechain_number: SidechainNumber,
        blinded_m6: &BlindedM6<'static>,
    ) -> Result<M6id, rusqlite::Error> {
        self.inner
            .db()
            .put_withdrawal_bundle(sidechain_number, blinded_m6)
            .await
    }

    pub async fn sync_task(&self, cancel: CancellationToken) -> Result<(), miette::Report> {
        const SYNC_INTERVAL: Duration = Duration::from_secs(15);
        tracing::debug!(
            interval = %jiff::SignedDuration::try_from(SYNC_INTERVAL).unwrap(),
            "wallet sync task: starting"
        );

        // Needed so we can use `tokio::select!`
        let shutdown_signal = cancel.cancelled();
        futures::pin_mut!(shutdown_signal);

        let mut sleep = tokio::time::sleep(SYNC_INTERVAL).boxed();
        loop {
            tokio::select! {
                biased;  // Prioritize shutdown

                _ = &mut shutdown_signal => {
                    tracing::info!("shutting down sync task");
                    return Ok(());
                }
                _ = &mut sleep => {
                    let tick = Uuid::new_v4().simple();
                    let span = tracing::span!(tracing::Level::DEBUG,
                        "wallet_sync",
                        %tick,
                    );
                    let guard = span.enter();
                    if self.inner.last_sync.read().await.is_none() {
                        // Initial sync is incomplete, nothing to do
                        tracing::debug!(
                            "waiting for initial wallet sync to complete"
                        );
                    } else if let Err(err) = self.inner.sync().await {
                        tracing::error!("wallet sync error: {:#}", ErrorChain::new(&err));
                    }
                    drop(guard);
                    sleep = tokio::time::sleep(SYNC_INTERVAL).boxed();
                }
            }
        }
    }

    pub(crate) fn parse_checked_address(
        &self,
        address: &str,
    ) -> Result<bitcoin::Address, connectrpc::ConnectError> {
        let network = self.validator().network();
        let address = bdk_wallet::bitcoin::Address::from_str(address).map_err(|err| {
            connectrpc::ConnectError::invalid_argument(format!("invalid bitcoin address: {err:#}"))
        })?;

        let address = address.require_network(network).map_err(|_| {
            connectrpc::ConnectError::invalid_argument(format!(
                "bitcoin address is not valid for network `{network}`",
            ))
        })?;

        Ok(address)
    }

    pub async fn is_initialized(&self) -> bool {
        self.inner.bitcoin_wallet.read().await.is_some()
    }

    pub fn validator(&self) -> &Validator {
        self.inner.validator()
    }

    fn create_deposit_op_drivechain_output(
        sidechain_number: SidechainNumber,
        sidechain_ctip_amount: Amount,
        value: Amount,
    ) -> Result<bdk_wallet::bitcoin::TxOut, crate::types::AmountOverflowError> {
        let deposit_txout =
            messages::create_m5_deposit_output(sidechain_number, sidechain_ctip_amount, value)?;

        Ok(bdk_wallet::bitcoin::TxOut {
            script_pubkey: bdk_wallet::bitcoin::ScriptBuf::from_bytes(
                deposit_txout.script_pubkey.to_bytes(),
            ),
            value: deposit_txout.value,
        })
    }

    fn create_op_return_output<Msg>(
        msg: Msg,
    ) -> Result<bdk_wallet::bitcoin::TxOut, <bitcoin::script::PushBytesBuf as TryFrom<Msg>>::Error>
    where
        PushBytesBuf: TryFrom<Msg>,
    {
        let op_return_txout = messages::create_op_return_output(msg)?;
        Ok(bdk_wallet::bitcoin::TxOut {
            script_pubkey: bdk_wallet::bitcoin::ScriptBuf::from_bytes(
                op_return_txout.script_pubkey.to_bytes(),
            ),
            value: op_return_txout.value,
        })
    }

    async fn fetch_transaction(
        &self,
        txid: Txid,
    ) -> Result<bdk_wallet::bitcoin::Transaction, error::FetchTransaction> {
        let block_hash = None;

        let transaction_hex = self
            .inner
            .main_client
            .get_raw_transaction(txid, GetRawTransactionVerbose::<false>, block_hash)
            .await
            .map_err(|err| error::BitcoinCoreRPC {
                method: "getrawtransaction".to_string(),
                error: err,
            })?;

        let transaction =
            bitcoin::consensus::encode::deserialize_hex::<Transaction>(&transaction_hex)?;

        convert::bitcoin_tx_to_bdk_tx(transaction).map_err(error::FetchTransaction::Convert)
    }

    /// [`bdk_wallet::TxOrdering`] for deposit txs
    fn deposit_txordering(
        sidechain_addrs: HashMap<Vec<u8>, SidechainNumber>,
    ) -> bdk_wallet::TxOrdering {
        use std::cmp::Ordering;

        use bitcoin::hashes::{Hash, Hmac, HmacEngine};
        let hmac_engine = || {
            let key = {
                use rand::Rng;
                let mut bytes = vec![0u8; <sha256::Hash as Hash>::Engine::BLOCK_SIZE];
                rand::rng().fill_bytes(&mut bytes);
                bytes
            };
            HmacEngine::<sha256::Hash>::new(&key)
        };
        fn hmac_sha256<T>(mut engine: HmacEngine<sha256::Hash>, value: &T) -> Hmac<sha256::Hash>
        where
            T: bitcoin::consensus::Encodable,
        {
            value
                .consensus_encode(&mut engine)
                .expect("should encode correctly");
            Hmac::<sha256::Hash>::from_engine(engine)
        }
        let input_sort = {
            let hmac_engine = hmac_engine();
            move |txin_l: &bdk_wallet::bitcoin::TxIn, txin_r: &bdk_wallet::bitcoin::TxIn| {
                let txin_l_hmac = hmac_sha256(hmac_engine.clone(), txin_l);
                let txin_r_hmac = hmac_sha256(hmac_engine.clone(), txin_r);
                txin_l_hmac.cmp(&txin_r_hmac)
            }
        };
        enum TxOutKind {
            OpDrivechain(SidechainNumber),
            OpReturnAddress(SidechainNumber),
            Other,
        }
        // classify as an op_drivechain output or an
        // op_return address
        fn classify_txout(
            sidechain_addrs: &HashMap<Vec<u8>, SidechainNumber>,
            txout: &bdk_wallet::bitcoin::TxOut,
        ) -> TxOutKind {
            if let Ok((_, sidechain_id)) =
                crate::messages::parse_op_drivechain(txout.script_pubkey.as_bytes())
            {
                return TxOutKind::OpDrivechain(sidechain_id);
            }
            if let Some(address) =
                crate::messages::try_parse_op_return_address(&txout.script_pubkey)
                && let Some(sidechain_id) = sidechain_addrs.get(&address)
            {
                return TxOutKind::OpReturnAddress(*sidechain_id);
            }
            TxOutKind::Other
        }
        let output_sort = {
            let hmac_engine = hmac_engine();
            move |txout_l: &bdk_wallet::bitcoin::TxOut, txout_r: &bdk_wallet::bitcoin::TxOut| match (
                classify_txout(&sidechain_addrs, txout_l),
                classify_txout(&sidechain_addrs, txout_r),
            ) {
                (TxOutKind::OpDrivechain(_) | TxOutKind::OpReturnAddress(_), TxOutKind::Other) => {
                    Ordering::Less
                }
                (TxOutKind::Other, TxOutKind::OpDrivechain(_) | TxOutKind::OpReturnAddress(_)) => {
                    Ordering::Greater
                }
                (
                    TxOutKind::OpDrivechain(sidechain_id_l),
                    TxOutKind::OpDrivechain(sidechain_id_r),
                )
                | (
                    TxOutKind::OpReturnAddress(sidechain_id_l),
                    TxOutKind::OpReturnAddress(sidechain_id_r),
                ) => sidechain_id_l.cmp(&sidechain_id_r),
                (
                    TxOutKind::OpDrivechain(sidechain_id_l),
                    TxOutKind::OpReturnAddress(sidechain_id_r),
                ) => match sidechain_id_l.cmp(&sidechain_id_r) {
                    Ordering::Equal => Ordering::Less,
                    ordering => ordering,
                },
                (
                    TxOutKind::OpReturnAddress(sidechain_id_l),
                    TxOutKind::OpDrivechain(sidechain_id_r),
                ) => match sidechain_id_l.cmp(&sidechain_id_r) {
                    Ordering::Equal => Ordering::Greater,
                    ordering => ordering,
                },
                (TxOutKind::Other, TxOutKind::Other) => {
                    let txout_l_hmac = hmac_sha256(hmac_engine.clone(), txout_l);
                    let txout_r_hmac = hmac_sha256(hmac_engine.clone(), txout_r);
                    txout_l_hmac.cmp(&txout_r_hmac)
                }
            }
        };
        bdk_wallet::TxOrdering::Custom {
            input_sort: Arc::new(input_sort),
            output_sort: Arc::new(output_sort),
        }
    }

    async fn create_deposit_psbt(
        &self,
        op_drivechain_output: bdk_wallet::bitcoin::TxOut,
        sidechain_address_data: bdk_wallet::bitcoin::script::PushBytesBuf,
        sidechain_ctip: Option<&Ctip>,
        fee: Option<Amount>,
    ) -> Result<bdk_wallet::bitcoin::psbt::Psbt, error::CreateDepositPsbt> {
        let sidechain_number = match crate::messages::parse_op_drivechain(
            op_drivechain_output.script_pubkey.as_bytes(),
        ) {
            Ok((_, sidechain_number)) => sidechain_number,
            Err(_) => return Err(error::CreateDepositPsbt::ParseSidechainNumber),
        };
        // If the sidechain has a Ctip (i.e. treasury UTXO), the BIP300 rules mandate that we spend the previous
        // Ctip.
        let ctip_foreign_utxo = match sidechain_ctip {
            Some(sidechain_ctip) => {
                let outpoint = bdk_wallet::bitcoin::OutPoint {
                    txid: convert::bitcoin_txid_to_bdk_txid(sidechain_ctip.outpoint.txid),
                    vout: sidechain_ctip.outpoint.vout,
                };

                let ctip_transaction =
                    self.fetch_transaction(sidechain_ctip.outpoint.txid)
                        .await
                        .map_err(|err| error::CreateDepositPsbt::FetchTransaction {
                            txid: sidechain_ctip.outpoint.txid,
                            source: err,
                        })?;

                let psbt_input = bdk_wallet::bitcoin::psbt::Input {
                    non_witness_utxo: Some(ctip_transaction),
                    final_script_sig: Some(bitcoin::ScriptBuf::new()),
                    ..bdk_wallet::bitcoin::psbt::Input::default()
                };

                Some((psbt_input, outpoint))
            }
            None => None,
        };

        let psbt = {
            let mut wallet_write = self.inner.write_wallet().await?;
            tokio::task::block_in_place(|| {
                wallet_write.with_mut(|wallet| {
                    let mut builder = wallet.build_tx();
                    builder
                        // important: the M5 OP_DRIVECHAIN output must come directly before the OP_RETURN sidechain address output.
                        .add_recipient(
                            op_drivechain_output.script_pubkey,
                            op_drivechain_output.value,
                        )
                        .add_data(&sidechain_address_data);

                    if let Some(fee) = fee {
                        builder.fee_absolute(fee);
                    }

                    if let Some((ctip_psbt_input, outpoint)) = ctip_foreign_utxo {
                        // This might be wrong. Seems to work!
                        let satisfaction_weight = bdk_wallet::bitcoin::Weight::ZERO;

                        builder.add_foreign_utxo(outpoint, ctip_psbt_input, satisfaction_weight)?;
                    }

                    builder.ordering(Self::deposit_txordering(
                        [(
                            sidechain_address_data.as_bytes().to_owned(),
                            sidechain_number,
                        )]
                        .into_iter()
                        .collect(),
                    ));

                    builder.finish().map_err(error::CreateDepositPsbt::CreateTx)
                })
            })?
        };
        Ok(psbt)
    }

    fn p2p_broadcast_addrs(&self) -> Box<dyn Iterator<Item = crate::p2p::BroadcastAddr> + '_> {
        let network = self.inner.validator().network();
        let magic = self.inner.magic;
        let p2p_broadcast_addrs = self.inner.config.p2p_broadcast_addr.iter().cloned();
        match crate::p2p::default_p2p_broadcast_addr(network, magic.to_bytes()) {
            Some(default_addr) => {
                if magic.to_bytes() == crate::p2p::SIGNET_MAGIC_BYTES {
                    tracing::debug!(
                        "Using default P2P broadcast address for signet: {default_addr:?}"
                    );
                    let res = std::iter::once(default_addr.into()).chain(p2p_broadcast_addrs);
                    Box::new(res)
                } else {
                    tracing::debug!(
                        %network,
                        %magic,
                        "No default P2P broadcast addresses for signet with non-matching magic",
                    );
                    Box::new(p2p_broadcast_addrs)
                }
            }
            None => {
                tracing::debug!(
                    %network,
                    "No default P2P broadcast addresses for network",
                );
                Box::new(p2p_broadcast_addrs)
            }
        }
    }

    /// Creates a deposit transaction, persists it to the database, and returns the TXID.
    /// This is also known as a M5 message, in BIP300 nomenclature.
    ///
    /// https://github.com/bitcoin/bips/blob/master/bip-0300.mediawiki#m5----deposit-btc-from-l1-to-l2
    pub async fn create_deposit(
        &self,
        sidechain_number: SidechainNumber,
        sidechain_address: String,
        value: Amount,
        fee: Option<Amount>,
    ) -> Result<bitcoin::Txid, error::CreateDeposit> {
        let block_height = self
            .inner
            .validator()
            .try_get_block_height()?
            .unwrap_or_default();
        // If this is None, there's been no deposit to this sidechain yet. We're the first one!
        let sidechain_ctip = self.inner.validator().try_get_ctip(sidechain_number)?;
        let sidechain_ctip = sidechain_ctip.as_ref();
        let sidechain_ctip_amount = sidechain_ctip
            .map(|ctip| ctip.value)
            .unwrap_or(Amount::ZERO);
        let op_drivechain_output = Self::create_deposit_op_drivechain_output(
            sidechain_number,
            sidechain_ctip_amount,
            value,
        )?;
        tracing::debug!(
            value = %op_drivechain_output.value,
            spk = %op_drivechain_output.script_pubkey.to_asm_string(),
            "Created OP_DRIVECHAIN output",
        );
        let sidechain_address_data =
            bdk_wallet::bitcoin::script::PushBytesBuf::try_from(sidechain_address.into_bytes())
                .map_err(error::CreateDeposit::ConvertSidechainAddress)?;
        let psbt = self
            .create_deposit_psbt(
                op_drivechain_output,
                sidechain_address_data,
                sidechain_ctip,
                fee,
            )
            .await?;
        tracing::debug!("Created deposit PSBT: {psbt}");
        let tx = self.sign_transaction(psbt).await?;
        let txid = tx.compute_txid();
        tracing::info!(%txid, "Signed deposit transaction");
        tracing::debug!("Serialized deposit transaction: {}", {
            let tx_bytes = bdk_wallet::bitcoin::consensus::serialize(&tx);
            hex::encode(tx_bytes)
        });
        tracing::debug!(%txid, "Attempting to broadcast deposit transaction via RPC...");
        let mut broadcast_successfully: bool =
            crate::rpc_client::broadcast_transaction(&self.inner.main_client, &tx)
                .await
                .map_err(error::CreateDeposit::BroadcastTx)?
                .is_some();

        if self.p2p_broadcast_addrs().count() > 0 {
            tracing::debug!(%txid, "Attempting to broadcast deposit transaction via P2P to {} peer(s)...", self.p2p_broadcast_addrs().count());
        } else {
            tracing::warn!(%txid, "No P2P peers configured, skipping P2P attempt of failed deposit transaction broadcast");
        }

        let mut broadcast_results_stream = self
            .p2p_broadcast_addrs()
            .map(|peer_addr| {
                crate::p2p::broadcast_nonstandard_tx(
                    peer_addr.clone(),
                    block_height as i32,
                    self.inner.magic,
                    tx.clone(),
                )
                .map_ok({
                    let peer_addr = peer_addr.clone();
                    move |result| (peer_addr, result)
                })
                .map_err(move |source| {
                    error::CreateDeposit::BroadcastNonstandardTx { peer_addr, source }
                })
            })
            .collect::<futures::stream::FuturesUnordered<_>>();
        while let Some((peer_addr, broadcast_success)) = broadcast_results_stream.try_next().await?
        {
            tracing::debug!(%txid, "Broadcast deposit transaction via P2P to {peer_addr} successfully: {broadcast_success}");
            broadcast_successfully |= broadcast_success
        }
        if broadcast_successfully {
            tracing::info!(%txid, "Broadcast deposit transaction successfully");

            // Apply the unconfirmed deposit to the wallet so its funding input
            // is marked spent. Otherwise a deposit created before this tx
            // confirms can reselect the same UTXO, which Bitcoin Core rejects
            // as an RBF replacement. Mirrors `send_wallet_transaction`.
            let last_seen = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            let applied_changes = {
                // Lock order: wallet before `bdk_db`, see the `bdk_db` field
                // docs
                let mut wallet_write = self.inner.write_wallet().await?;
                let mut bdk_db_lock = self.inner.bdk_db.lock().await;
                wallet_write
                    .with_mut(|wallet| {
                        wallet.apply_unconfirmed_txs(vec![(tx, last_seen.as_secs())]);
                        wallet.persist_async(&mut bdk_db_lock)
                    })
                    .await?
            };
            if applied_changes {
                tracing::debug!(%txid, "Applied unconfirmed deposit transaction to wallet");
            } else {
                // A deposit can be funded entirely by the sidechain's CTIP
                // foreign UTXO, with no wallet-owned input or change, in which
                // case there is nothing for the wallet to track.
                tracing::warn!(
                    %txid,
                    "No wallet changes after applying unconfirmed deposit transaction",
                );
            }

            Ok(convert::bdk_txid_to_bitcoin_txid(txid))
        } else {
            Err(error::CreateDeposit::BroadcastUnsuccessful { txid })
        }
    }

    #[instrument(skip_all)]
    /// Returns the balance of the wallet, alongside a bool indicating whether the wallet is synced.
    pub async fn get_wallet_balance(
        &self,
    ) -> Result<(bdk_wallet::Balance, bool), error::GetWalletBalance> {
        let has_synced = self.inner.last_sync.read().await.is_some();

        let balance = self.inner.read_wallet().await?.balance();

        Ok((balance, has_synced))
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "false positive for `bitcoin_wallet`"
    )]
    #[instrument(skip_all)]
    pub async fn list_wallet_transactions(
        &self,
    ) -> Result<Vec<BDKWalletTransaction>, error::ListWalletTransactions> {
        // Massage the wallet data into a format that we can use to calculate fees, etc.
        let wallet_data = {
            let wallet_read = self.inner.read_wallet().await?;
            let transactions = wallet_read.transactions();

            transactions
                .into_iter()
                .map(|tx| {
                    let txid = tx.tx_node.txid;
                    let chain_position = tx.chain_position;
                    let tx = tx.tx_node.tx.clone();

                    let output_ownership: Vec<_> = tx
                        .output
                        .iter()
                        .map(|output| {
                            (
                                output.value,
                                wallet_read.is_mine(output.script_pubkey.clone()),
                            )
                        })
                        .collect();

                    // Just collect the inputs - we'll get their values using getrawtransaction later
                    let inputs = tx.input.clone();

                    (txid, tx, chain_position, output_ownership, inputs)
                })
                .collect::<Vec<_>>()
        };

        // Calculate fees, received, and sent amounts
        let mut txs = Vec::new();
        for (txid, tx, chain_position, output_ownership, inputs) in wallet_data {
            let mut input_value = Amount::ZERO;
            let mut output_value = Amount::ZERO;
            let mut received = Amount::ZERO;
            let mut sent = Amount::ZERO;

            // Calculate output value and received amount
            for (value, is_mine) in output_ownership {
                output_value += value;
                if is_mine {
                    received += value;
                }
            }

            // Get input values using getrawtransaction
            for input in inputs {
                // Coinbase transactions have an empty prev output txid, which we'll be unable to fetch
                if input.previous_output.txid == bitcoin::Txid::all_zeros() {
                    continue;
                }

                let transaction_hex = self
                    .inner
                    .main_client
                    // TODO: get rid of this. It's kind of absurd that we're calling out to getrawtransaction for every input.
                    // Both from a performance point of view, as well as requiring txindex. Would be better to somehow
                    // persist the relevant values in the wallet DB
                    .get_raw_transaction(
                        input.previous_output.txid,
                        GetRawTransactionVerbose::<false>,
                        None,
                    )
                    .await
                    .map_err(|err| error::ListWalletTransactions::FetchTransaction {
                        txid: input.previous_output.txid,
                        source: error::BitcoinCoreRPC {
                            method: "getrawtransaction".to_string(),
                            error: err,
                        },
                    })?;

                let prev_output =
                    bitcoin::consensus::encode::deserialize_hex::<Transaction>(&transaction_hex)?;

                let value = prev_output.output[input.previous_output.vout as usize].value;
                if self.inner.read_wallet().await?.is_mine(
                    prev_output.output[input.previous_output.vout as usize]
                        .script_pubkey
                        .clone(),
                ) {
                    sent += value;
                }
                input_value += value;
            }

            let fee = input_value
                .checked_sub(output_value)
                .unwrap_or(Amount::ZERO);
            // Calculate net wallet change (excluding fee)
            // We need to handle received and sent separately since Amount can't be negative
            let (final_received, final_sent) = if received >= sent {
                (received - sent, Amount::from_sat(0)) // Net gain to wallet
            } else {
                (Amount::from_sat(0), sent - received - fee) // Net loss from wallet
            };

            txs.push(BDKWalletTransaction {
                txid,
                tx,
                chain_position,
                fee,
                received: final_received,
                sent: final_sent,
            });
        }

        // Make sure that the transaction list is in chronological order.
        txs.sort_by(|a, b| match (a.chain_position, b.chain_position) {
            (
                ChainPosition::Confirmed {
                    anchor: a_anchor, ..
                },
                ChainPosition::Confirmed {
                    anchor: b_anchor, ..
                },
            ) => a_anchor.confirmation_time.cmp(&b_anchor.confirmation_time),
            (
                ChainPosition::Confirmed { anchor, .. },
                ChainPosition::Unconfirmed {
                    last_seen: Some(last_seen),
                    first_seen: _,
                },
            ) => anchor.confirmation_time.cmp(&last_seen),
            (
                ChainPosition::Unconfirmed {
                    last_seen: Some(last_seen),
                    first_seen: _,
                },
                ChainPosition::Confirmed { anchor, .. },
            ) => last_seen.cmp(&anchor.confirmation_time),
            (
                ChainPosition::Unconfirmed {
                    last_seen: Some(a_last_seen),
                    first_seen: _,
                },
                ChainPosition::Unconfirmed {
                    last_seen: Some(b_last_seen),
                    first_seen: _,
                },
            ) => a_last_seen.cmp(&b_last_seen),

            // Fallback to comparing TXIDs
            (_, _) => a.txid.cmp(&b.txid),
        });
        Ok(txs)
    }

    pub async fn list_sidechain_deposit_transactions(
        &self,
    ) -> Result<Vec<SidechainDepositTransaction>, error::ListSidechainDepositTransactions> {
        self.list_wallet_transactions()
            .await?
            .into_iter()
            .map(Ok::<_, error::ListSidechainDepositTransactions>)
            .transpose_into_fallible()
            .filter_map(|bdk_wallet_tx| {
                let Some(treasury_output) = bdk_wallet_tx.tx.output.first() else {
                    return Ok(None);
                };
                let Ok((_, sidechain_number)) =
                    crate::messages::parse_op_drivechain(&treasury_output.script_pubkey.to_bytes())
                else {
                    return Ok(None);
                };
                let treasury_outpoint = bitcoin::OutPoint {
                    txid: bdk_wallet_tx.txid,
                    vout: 0,
                };
                let spent_ctip = match self
                    .validator()
                    .try_get_ctip_value_seq(&treasury_outpoint)?
                {
                    // `seq == 0` is the sidechain's first deposit, which created
                    // the first treasury UTXO, so there is no previous spent ctip.
                    Some((_, _, seq)) => match seq.checked_sub(1) {
                        Some(prev_seq) => {
                            let spent_treasury_utxo = self
                                .validator()
                                .get_treasury_utxo(sidechain_number, prev_seq)?;
                            Some(crate::types::Ctip {
                                outpoint: spent_treasury_utxo.outpoint,
                                value: spent_treasury_utxo.total_value,
                            })
                        }
                        None => None,
                    },
                    None => {
                        // May be unconfirmed
                        // check if current ctip in inputs
                        match self.validator().try_get_ctip(sidechain_number)? {
                            Some(ctip) => {
                                if bdk_wallet_tx.tx.input.iter().any(|txin: &bitcoin::TxIn| {
                                    txin.previous_output == ctip.outpoint
                                }) {
                                    Some(ctip)
                                } else {
                                    return Ok(None);
                                }
                            }
                            None => None,
                        }
                    }
                };
                if let Some(spent_ctip) = spent_ctip
                    && spent_ctip.value > treasury_output.value
                {
                    return Ok(None);
                }
                let deposit_amount = if let Some(spent_ctip) = spent_ctip {
                    match treasury_output.value.checked_sub(spent_ctip.value) {
                        Some(deposit_amount) => deposit_amount,
                        None => return Ok(None),
                    }
                } else {
                    treasury_output.value
                };
                let Some(destination_address_output) = bdk_wallet_tx.tx.output.get(1) else {
                    return Ok(None);
                };
                let Some(destination_address) = crate::messages::try_parse_op_return_address(
                    &destination_address_output.script_pubkey,
                ) else {
                    return Ok(None);
                };
                let deposit_tx = SidechainDepositTransaction {
                    sidechain_number,
                    deposit_amount,
                    destination_address,
                    wallet_tx: bdk_wallet_tx,
                };
                Ok(Some(deposit_tx))
            })
            .collect()
    }

    fn build_send_psbt(
        wallet: &mut bdk_wallet::Wallet,
        destinations: HashMap<bitcoin::Address, Amount>,
        params: CreateTransactionParams,
    ) -> Result<bdk_wallet::bitcoin::psbt::Psbt, error::CreateSendPsbt> {
        let mut timestamp = Instant::now();
        let mut builder = wallet.build_tx();

        if let Some(op_return_message) = params.op_return_message {
            let op_return_output = Self::create_op_return_output(op_return_message)?;
            // BIP300/BIP301 messages are only read from output 0, and BDK
            // shuffles outputs by default.
            builder.ordering(bdk_wallet::TxOrdering::Untouched);
            builder.add_recipient(op_return_output.script_pubkey, op_return_output.value);

            tracing::debug!("Added OP_RETURN output in {:?}", timestamp.elapsed());
            timestamp = Instant::now();
        }

        let destinations_len = destinations.len();

        // Add outputs for each destination address
        for (address, value) in destinations {
            builder.add_recipient(address.script_pubkey(), value);
        }

        tracing::debug!(
            "Added {} destinations in {:?}",
            destinations_len,
            timestamp.elapsed()
        );
        timestamp = Instant::now();

        if let Some(drain_wallet_to) = params.drain_wallet_to {
            tracing::debug!("Draining wallet to {}", drain_wallet_to);
            builder
                .drain_to(drain_wallet_to.script_pubkey())
                .drain_wallet();
        }

        if !params.required_utxos.is_empty() {
            builder
                // TODO: this does not work at all for wallets past a certain scale....
                // 25s pr. UTXO for a wallet with 40k UTXOs in total
                .add_utxos(&params.required_utxos)
                .map_err(|err| match err {
                    bdk_wallet::tx_builder::AddUtxoError::UnknownUtxo(outpoint) => {
                        error::CreateSendPsbt::UnknownUTXO(outpoint)
                    }
                })?;

            builder.manually_selected_only();

            tracing::debug!(
                "Added {} required UTXOs in {:?}",
                params.required_utxos.len(),
                timestamp.elapsed()
            );
            timestamp = Instant::now();
        }

        match params.fee_policy {
            Some(crate::types::FeePolicy::Absolute(fee)) => {
                builder.fee_absolute(fee);
            }
            Some(crate::types::FeePolicy::Rate(rate)) => {
                builder.fee_rate(rate);
            }
            None => (),
        }

        tracing::debug!("Set fee policy in {:?}", timestamp.elapsed());
        timestamp = Instant::now();

        builder
            .finish()
            .inspect(|_| {
                tracing::debug!("Finished transaction builder in {:?}", timestamp.elapsed());
            })
            .map_err(error::CreateSendPsbt::CreateTx)
    }

    async fn create_send_psbt(
        &self,
        destinations: HashMap<bitcoin::Address, Amount>,
        params: CreateTransactionParams,
    ) -> Result<bdk_wallet::bitcoin::psbt::Psbt, error::CreateSendPsbt> {
        let mut wallet_write = self.inner.write_wallet().await?;
        tokio::task::block_in_place(|| {
            wallet_write.with_mut(|wallet| Self::build_send_psbt(wallet, destinations, params))
        })
    }

    /// Creates a transaction, sends it, and returns the TXID.
    pub async fn send_wallet_transaction(
        &self,
        destinations: HashMap<bdk_wallet::bitcoin::Address, Amount>,
        params: CreateTransactionParams,
    ) -> Result<bitcoin::Txid, error::SendWalletTransaction> {
        tracing::debug!(
            destinations = destinations.len(),
            required_utxos = params.required_utxos.len(),
            drain_wallet = params.drain_wallet_to.is_some(),
            "Sending wallet transaction",
        );
        let mut timestamp = Instant::now();
        let psbt = self.create_send_psbt(destinations, params).await?;

        tracing::debug!("Created send PSBT in {:?}", timestamp.elapsed());
        timestamp = Instant::now();

        let tx = self.sign_transaction(psbt).await?;
        let txid = tx.compute_txid();

        tracing::info!(
            %txid,
            "Signed send transaction in {:?}, {} bytes",
            timestamp.elapsed(),
            {
                let tx_bytes = bdk_wallet::bitcoin::consensus::serialize(&tx);
                tx_bytes.len()
            },
        );
        timestamp = Instant::now();

        if crate::rpc_client::broadcast_transaction(&self.inner.main_client, &tx)
            .await
            .map_err(error::SendWalletTransaction::BroadcastTx)?
            .is_none()
        {
            let err = error::SendWalletTransaction::OpDrivechainNotSupported;
            tracing::error!(%txid, "{:#}", ErrorChain::new(&err));
            return Err(err);
        }
        tracing::info!(%txid, "Broadcast send transaction in {:?}", timestamp.elapsed());

        // Apply the unconfirmed transaction to the wallet
        let last_seen = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();

        let applied_changes = {
            // Lock order: wallet before `bdk_db`, see the `bdk_db` field docs
            let mut wallet_write = self.inner.write_wallet().await?;
            let mut bdk_db_lock = self.inner.bdk_db.lock().await;
            wallet_write
                .with_mut(|wallet| {
                    wallet.apply_unconfirmed_txs(vec![(tx, last_seen.as_secs())]);
                    wallet.persist_async(&mut bdk_db_lock)
                })
                .await?
        };

        // We used to do a sanity check here that changes were applied. However,
        // `applied_changes` may be false if the transaction was already
        // applied to the wallet by the mempool `accept_tx` hook, which runs in
        // a background task once bitcoind accepts the broadcast.
        if applied_changes {
            tracing::debug!(%txid, "Applied unconfirmed transaction to wallet");
        } else {
            tracing::debug!(
                %txid,
                "Unconfirmed transaction already applied to wallet (likely by mempool accept_tx)"
            );
        }

        Ok(convert::bdk_txid_to_bitcoin_txid(txid))
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "false positive for `bitcoin_wallet`"
    )]
    #[instrument(skip_all)]
    pub async fn get_utxos(&self) -> Result<Vec<bdk_wallet::LocalOutput>, error::NotUnlocked> {
        let wallet_read = self.inner.read_wallet().await?;
        let utxos = wallet_read.list_unspent().collect::<Vec<_>>();

        Ok(utxos)
    }

    #[expect(dead_code)]
    async fn get_sidechain_ctip(
        &self,
        sidechain_number: SidechainNumber,
    ) -> Result<Option<(bitcoin::OutPoint, Amount, u64)>, miette::Report> {
        let ctip = self.inner.validator().try_get_ctip(sidechain_number)?;

        let sequence_number = self
            .inner
            .validator()
            .get_ctip_sequence_number(sidechain_number)?
            .unwrap();

        if let Some(ctip) = ctip {
            let value = ctip.value;
            Ok(Some((ctip.outpoint, value, sequence_number)))
        } else {
            Ok(None)
        }
    }

    pub fn is_sidechain_active(
        &self,
        sidechain_number: SidechainNumber,
    ) -> Result<bool, validator::GetSidechainsError> {
        let sidechains = self.inner.validator().get_active_sidechains()?;
        let active = sidechains
            .iter()
            .any(|sc| sc.proposal.sidechain_number == sidechain_number);

        Ok(active)
    }

    #[instrument(skip_all, err)]
    async fn sign_transaction(
        &self,
        psbt: bdk_wallet::bitcoin::psbt::Psbt,
    ) -> Result<bdk_wallet::bitcoin::Transaction, error::WalletSignTransaction> {
        self.sign_transaction_inner(psbt, false).await
    }

    /// [`Self::sign_transaction`], without the PSBT fee-rate sanity limit.
    /// A BMM bid is paid as the transaction fee, so it may deliberately
    /// exceed any fee-rate threshold that guards ordinary transactions
    /// against fat-fingered fees.
    #[instrument(skip_all, err)]
    async fn sign_bmm_request_transaction(
        &self,
        psbt: bdk_wallet::bitcoin::psbt::Psbt,
    ) -> Result<bdk_wallet::bitcoin::Transaction, error::WalletSignTransaction> {
        self.sign_transaction_inner(psbt, true).await
    }

    async fn sign_transaction_inner(
        &self,
        mut psbt: bdk_wallet::bitcoin::psbt::Psbt,
        skip_fee_rate_check: bool,
    ) -> Result<bdk_wallet::bitcoin::Transaction, error::WalletSignTransaction> {
        let mut timestamp = Instant::now();

        if !self
            .inner
            .read_wallet()
            .await
            .map_err(error::WalletSignTransaction::NotUnlocked)?
            .sign(&mut psbt, bdk_wallet::SignOptions::default())
            .map_err(error::WalletSignTransaction::SignerError)?
        {
            return Err(error::WalletSignTransaction::UnableToSign);
        }

        tracing::debug!("Signed transaction in {:?}", timestamp.elapsed());
        timestamp = Instant::now();

        let tx = if skip_fee_rate_check {
            psbt.extract_tx_unchecked_fee_rate()
        } else {
            psbt.extract_tx()
                .map_err(error::WalletSignTransaction::ExtractTx)?
        };

        tracing::debug!("Extracted transaction in {:?}", timestamp.elapsed());
        Ok(tx)
    }

    fn build_bmm_psbt(
        wallet: &mut bdk_wallet::Wallet,
        message: &PushBytesBuf,
        bid_amount: bdk_wallet::bitcoin::Amount,
        locktime: bdk_wallet::bitcoin::absolute::LockTime,
    ) -> Result<bdk_wallet::bitcoin::psbt::Psbt, bdk_wallet::error::CreateTxError> {
        let mut builder = wallet.build_tx();
        // BIP301 requires the M8 OP_RETURN at output zero.
        builder.ordering(bdk_wallet::TxOrdering::Untouched);
        builder.nlocktime(locktime);
        builder.add_data(message);
        builder.fee_absolute(bid_amount);
        builder.finish()
    }

    /// Evict every unconfirmed BMM request whose `prev_mainchain_block_hash`
    /// is not `mainchain_tip` from the wallet's canonical set, returning the
    /// evicted txids. Such requests can never confirm (BIP300 rejects an M8
    /// whose prev hash is not the block's parent), but they linger in Bitcoin
    /// Core's mempool, where chain-source syncs keep re-adopting them into
    /// the wallet and locking their inputs. Callers must persist the wallet
    /// afterwards, or the eviction is lost on restart.
    fn evict_stale_bmm_requests(
        wallet: &mut bdk_wallet::Wallet,
        mainchain_tip: bitcoin::BlockHash,
    ) -> Vec<bdk_wallet::bitcoin::Txid> {
        let stale: Vec<bdk_wallet::bitcoin::Txid> = wallet
            .transactions()
            .filter(|wallet_tx| !wallet_tx.chain_position.is_confirmed())
            .filter_map(|wallet_tx| {
                let output = wallet_tx.tx_node.tx.output.first()?;
                let script = output.script_pubkey.to_bytes();
                let (_rest, m8) = M8BmmRequest::parse(&script).ok()?;
                (m8.prev_mainchain_block_hash != mainchain_tip).then_some(wallet_tx.tx_node.txid)
            })
            .collect();
        if !stale.is_empty() {
            tracing::info!(stale_bmm_requests = ?stale, "evicting stale BMM requests from wallet");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            wallet.apply_evicted_txs(stale.iter().map(|txid| (*txid, now)));
        }
        stale
    }

    async fn build_bmm_tx(
        &self,
        sidechain_number: SidechainNumber,
        prev_mainchain_block_hash: bdk_wallet::bitcoin::BlockHash,
        sidechain_block_hash: BmmCommitment,
        bid_amount: bdk_wallet::bitcoin::Amount,
        locktime: bdk_wallet::bitcoin::absolute::LockTime,
    ) -> Result<bdk_wallet::bitcoin::psbt::Psbt, error::BuildBmmTx> {
        let message = M8BmmRequest::data(
            sidechain_number,
            sidechain_block_hash,
            prev_mainchain_block_hash,
        )?;

        let mut wallet_write = self.inner.write_wallet().await?;
        let psbt = tokio::task::block_in_place(|| {
            wallet_write
                .with_mut(|wallet| Self::build_bmm_psbt(wallet, &message, bid_amount, locktime))
        })?;

        Ok(psbt)
    }

    /// Creates a BMM request transaction and broadcasts it via the p2p
    /// whitelist and RPC to our own node.
    /// The bid amount is paid as transaction fee, so miners collect it by
    /// selecting the request rather than the requester burning it in the M8
    /// output.
    pub async fn create_bmm_request(
        &self,
        sidechain_number: SidechainNumber,
        prev_mainchain_block_hash: bdk_wallet::bitcoin::BlockHash,
        sidechain_block_hash: BmmCommitment,
        bid_amount: bdk_wallet::bitcoin::Amount,
        locktime: bdk_wallet::bitcoin::absolute::LockTime,
    ) -> Result<bdk_wallet::bitcoin::Transaction, error::CreateBmmRequest> {
        tracing::debug!("create_bmm_request: building transaction");
        let psbt = self
            .build_bmm_tx(
                sidechain_number,
                prev_mainchain_block_hash,
                sidechain_block_hash,
                bid_amount,
                locktime,
            )
            .await?;
        let tx = self.sign_bmm_request_transaction(psbt).await?;
        tracing::info!("BMM request: PSBT signed successfully");
        let txid = tx.compute_txid();
        let block_height = self
            .inner
            .validator()
            .get_header_info(&prev_mainchain_block_hash)?
            .height;
        tracing::debug!(%txid, "Broadcasting BMM request transaction...");
        let mut broadcast_results_stream = self
            .p2p_broadcast_addrs()
            .map(|peer_addr| {
                crate::p2p::broadcast_nonstandard_tx(
                    peer_addr,
                    block_height as i32,
                    self.inner.magic,
                    tx.clone(),
                )
                .map_err(error::CreateBmmRequestInner::BroadcastNonstandardTx)
            })
            .collect::<futures::stream::FuturesUnordered<_>>();
        let mut broadcast_successfully = None;
        while let Some(broadcast_success) = broadcast_results_stream.try_next().await? {
            broadcast_successfully = match broadcast_successfully {
                Some(broadcast_successfully) => Some(broadcast_successfully || broadcast_success),
                None => Some(broadcast_success),
            }
        }
        match broadcast_successfully {
            Some(true) => {
                tracing::info!(%txid, "Broadcast BMM request transaction successfully");
            }
            Some(false) => {
                let err = error::CreateBmmRequestInner::BroadcastUnsuccessful { txid };
                return Err(err.into());
            }
            None => {}
        }
        // Submit to our own node via RPC as well. This must happen after the
        // p2p broadcast: if a peer received the tx via relay from our node
        // first, the p2p broadcast to it would time out. The bid is paid as
        // the fee, so broadcast without Bitcoin Core's RPC fee-rate cap.
        tracing::debug!(%txid, "Broadcasting BMM request transaction to own node via RPC...");
        match crate::rpc_client::broadcast_transaction_no_fee_limit(&self.inner.main_client, &tx)
            .await
            .map_err(error::CreateBmmRequestInner::BroadcastTxRpc)?
        {
            Some(_) => {
                tracing::info!(%txid, "Broadcast BMM request transaction via RPC to own node");
            }
            None => {
                tracing::info!(%txid, "Own node rejected BMM request transaction from its mempool");
            }
        }
        Ok(tx)
    }

    pub async fn get_wallet_info(&self) -> Result<WalletInfo, error::NotUnlocked> {
        let w = self.inner.read_wallet().await?;
        let mut keychain_descriptors = std::collections::HashMap::new();
        for (kind, _) in w.keychains() {
            keychain_descriptors.insert(kind, w.public_descriptor(kind).clone());
        }

        let tip = w.local_chain().tip();

        Ok(WalletInfo {
            keychain_descriptors,
            network: w.network(),
            transaction_count: w.transactions().count(),
            unspent_output_count: w.list_unspent().count(),
            tip: (tip.hash(), tip.height()),
        })
    }

    #[expect(clippy::significant_drop_tightening)]
    pub async fn get_new_address(
        &self,
    ) -> Result<bdk_wallet::bitcoin::Address, error::GetNewAddress> {
        // Using next_unused_address here means that we get a new address
        // when funds are received. Without this we'd need to take care not
        // to cross the wallet scan gap.
        let mut wallet_write = self.inner.write_wallet().await?;

        let mut bdk_db_lock = self.inner.bdk_db.lock().await;
        let address = wallet_write
            .with_mut(|wallet| {
                let info = wallet.next_unused_address(bdk_wallet::KeychainKind::External);
                wallet
                    .persist_async(&mut bdk_db_lock)
                    .map_ok(|_: bool| info.address)
            })
            .await?;
        Ok(address)
    }

    pub async fn unlock_existing_wallet(
        &self,
        password: &str,
    ) -> Result<(), error::UnlockExistingWallet> {
        self.inner.unlock_existing_wallet(password).await
    }

    // Creates a new wallet with a given mnemonic and encryption password.
    // Note that the password is NOT a BIP39 passphrase, but is only used to
    // encrypt the mnemonic in storage.
    pub async fn create_wallet(
        &self,
        mnemonic: Option<Mnemonic>,
        password: Option<&str>,
    ) -> Result<(), error::CreateNewWallet> {
        self.inner.create_new_wallet(mnemonic, password).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bdk_wallet::{
        bitcoin::{Amount, ScriptBuf, script::PushBytesBuf},
        test_utils::get_funded_wallet_wpkh,
    };
    use bitcoin::{BlockHash, hashes::Hash as _};

    use super::{CreateTransactionParams, M8BmmRequest, Wallet};
    use crate::types::{BmmCommitment, SidechainNumber};

    #[test]
    fn bmm_bid_is_a_fee_not_a_burn() {
        let (mut wallet, _) = get_funded_wallet_wpkh();
        let bid = Amount::from_sat(10_000);
        let message = M8BmmRequest::data(
            SidechainNumber(13),
            BmmCommitment([0x11; 32]),
            BlockHash::from_byte_array([0x22; 32]),
        )
        .unwrap();

        let psbt = Wallet::build_bmm_psbt(
            &mut wallet,
            &message,
            bid,
            bitcoin::absolute::LockTime::ZERO,
        )
        .unwrap();

        assert_eq!(wallet.calculate_fee(&psbt.unsigned_tx).unwrap(), bid);
        assert_eq!(psbt.unsigned_tx.output[0].value, Amount::ZERO);
        M8BmmRequest::parse(&psbt.unsigned_tx.output[0].script_pubkey.to_bytes())
            .expect("output zero must contain the M8 request");
    }

    /// A BMM request that no longer bids on the tip can never confirm, so
    /// evicting it must free its inputs; a request still bidding on the tip
    /// must be left alone.
    #[test]
    fn stale_bmm_request_is_evicted_and_frees_inputs() {
        let (mut wallet, _) = get_funded_wallet_wpkh();
        let tip = BlockHash::from_byte_array([0xAA; 32]);
        let message =
            M8BmmRequest::data(SidechainNumber(3), BmmCommitment([0x33; 32]), tip).unwrap();
        let psbt = Wallet::build_bmm_psbt(
            &mut wallet,
            &message,
            Amount::from_sat(1_000),
            bitcoin::absolute::LockTime::ZERO,
        )
        .unwrap();
        let tx = psbt.unsigned_tx.clone();
        let txid = tx.compute_txid();
        let spent: Vec<_> = tx.input.iter().map(|input| input.previous_output).collect();
        assert!(!spent.is_empty());
        wallet.apply_unconfirmed_txs([(tx, 100)]);
        assert!(
            wallet
                .list_unspent()
                .all(|utxo| !spent.contains(&utxo.outpoint)),
            "an unconfirmed bid must spend its inputs"
        );

        assert!(
            Wallet::evict_stale_bmm_requests(&mut wallet, tip).is_empty(),
            "a bid on the current tip must not be evicted"
        );
        let moved_tip = BlockHash::from_byte_array([0xBB; 32]);
        assert_eq!(
            Wallet::evict_stale_bmm_requests(&mut wallet, moved_tip),
            vec![txid]
        );
        assert!(
            spent
                .iter()
                .all(|outpoint| wallet.list_unspent().any(|utxo| utxo.outpoint == *outpoint)),
            "evicting the stale bid must free its inputs"
        );
    }

    /// A BMM request only counts as an M8 when its OP_RETURN sits at output 0,
    /// and a shuffle lands it there half the time, so one build proves nothing.
    #[test]
    fn op_return_is_always_output_zero() {
        const ROUNDS: usize = 64;

        let message = [
            &M8BmmRequest::TAG[..],
            &[13],
            &[0x11; 32][..],
            &[0x22; 32][..],
        ]
        .concat();
        let expected = ScriptBuf::new_op_return(PushBytesBuf::try_from(message.clone()).unwrap());

        for _ in 0..ROUNDS {
            let (mut wallet, _) = get_funded_wallet_wpkh();

            let psbt = Wallet::build_send_psbt(
                &mut wallet,
                HashMap::new(),
                CreateTransactionParams {
                    op_return_message: Some(message.clone()),
                    ..Default::default()
                },
            )
            .unwrap();

            let outputs = psbt.unsigned_tx.output;
            assert_eq!(outputs.len(), 2, "want an OP_RETURN and change");
            assert_eq!(outputs[0].script_pubkey, expected);
            assert_eq!(outputs[0].value, Amount::ZERO);
            M8BmmRequest::parse(&outputs[0].script_pubkey.to_bytes())
                .expect("output 0 must parse as an M8 BMM request");
        }
    }
}
