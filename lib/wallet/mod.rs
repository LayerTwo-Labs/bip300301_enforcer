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
    cli::{Config, WalletConfig, WalletSyncSource, redact_embedded_credentials},
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
mod locks;
pub mod mnemonic;
mod seed_store;
mod sync;
mod sync_state;
mod thread_safe_connection;
mod util;

pub(crate) type Persistence = thread_safe_connection::ThreadSafeConnection;
type PersistenceError = <Persistence as bdk_wallet::AsyncWalletPersister>::Error;
type BdkWallet = bdk_wallet::PersistedWallet<Persistence>;

type ElectrumClient = BdkElectrumClient<bdk_electrum::electrum_client::Client>;
type EsploraClient = bdk_esplora::esplora_client::AsyncClient;

/// SLIP-44 coin type for the BIP 44 account path's second level. SLIP-44
/// registers `0` for Bitcoin and reserves `1` for *all* test networks.
const fn slip44_coin_type(network: Network) -> u32 {
    match network {
        Network::Bitcoin => 0,
        _ => 1,
    }
}

/// The coin type every wallet derived under before [`slip44_coin_type`] made
/// the choice network-aware. On mainnet it is not what this build derives, so
/// wallets persisted by those builds need
/// [`WalletInner::initialize_wallet_from_mnemonic`]'s fallback.
const LEGACY_COIN_TYPE: u32 = 1;

fn descriptor_coin_type(config: &WalletConfig, network: Network) -> u32 {
    config
        .derivation_coin_type
        .unwrap_or_else(|| slip44_coin_type(network))
}

fn is_descriptor_mismatch(err: &bdk_wallet::LoadWithPersistError<PersistenceError>) -> bool {
    matches!(
        err,
        bdk_wallet::LoadWithPersistError::InvalidChangeSet(bdk_wallet::LoadError::Mismatch(
            bdk_wallet::LoadMismatch::Descriptor { .. }
        ))
    )
}

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
    /// Weak, so that a wallet that has been dropped ends the retry loop.
    state: sync_state::WeakSyncState,
}

impl ChainSourceInitTask {
    /// Publishes the client into the wallet's sync state once the backend
    /// comes up. Returns without publishing on a non-transient error,
    /// on `cancel`, or once the wallet itself has been dropped.
    pub async fn run(self, cancel: CancellationToken) {
        const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
        const MAX_RETRY_DELAY: Duration = Duration::from_secs(10);
        let mut retry_delay = INITIAL_RETRY_DELAY;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(retry_delay) => (),
            }
            let Some(state) = self.state.upgrade() else {
                return;
            };
            match WalletInner::try_init_chain_source_client(&self.config, self.network).await {
                Ok(client) => {
                    tracing::info!("wallet sync backend became available");
                    state.publish_client(client);
                    return;
                }
                Err(err) if err.is_transient() => {
                    state.set_last_error(&err);
                    retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
                    tracing::warn!(
                        %err,
                        "wallet sync backend not ready, retrying in {retry_delay:?}",
                    );
                }
                Err(err) => {
                    state.set_last_error(&err);
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

/// The effective esplora URL, applying the per-network default when unset.
fn esplora_endpoint(
    config: &WalletConfig,
    network: Network,
) -> Result<url::Url, error::InitEsploraClient> {
    match config.esplora_url.as_ref() {
        Some(url) => Ok(url.clone()),
        None => {
            let default_url = default_esplora_url(network)
                .ok_or(error::InitEsploraClient::MissingUrl { network })?;
            Ok(url::Url::parse(default_url)?)
        }
    }
}

/// The effective electrum endpoint (`host:port`), applying per-network
/// defaults for whichever of host and port is unset.
fn electrum_endpoint(
    config: &WalletConfig,
    network: Network,
) -> Result<String, error::InitElectrumClient> {
    let (electrum_host, electrum_port) =
        match (config.electrum_host.as_deref(), config.electrum_port) {
            (Some(host), Some(port)) => (host, port),
            (host, port) => {
                let (default_host, default_port) = default_electrum_host_port(network)
                    .ok_or(error::InitElectrumClient::MissingHostPort { network })?;
                (host.unwrap_or(default_host), port.unwrap_or(default_port))
            }
        };
    Ok(format!("{electrum_host}:{electrum_port}"))
}

/// The configured sync backend endpoint, for reporting in wallet info:
/// `host:port` for electrum, a URL with any embedded credentials redacted for
/// esplora. `None` when the sync source is disabled, or when no endpoint is
/// known for the network.
fn sync_backend_endpoint(config: &WalletConfig, network: Network) -> Option<String> {
    match config.sync_source {
        WalletSyncSource::Electrum => electrum_endpoint(config, network).ok(),
        WalletSyncSource::Esplora => {
            let url = esplora_endpoint(config, network).ok()?.to_string();
            Some(redact_embedded_credentials(&url).unwrap_or(url))
        }
        WalletSyncSource::Disabled => None,
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
    /// The BDK wallet and its persistence. Held together so that we
    /// ensure at build time that the correct order is applied.
    locks: locks::WalletLocks,
    seed_store: SeedStore,
    sync_state: sync_state::SharedSyncState,
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
        let esplora_url = esplora_endpoint(config, network)?;

        // Redacted: an esplora URL may carry credentials
        tracing::info!(
            esplora_url = %redact_embedded_credentials(esplora_url.as_str())
                .unwrap_or_default(),
            "creating esplora client"
        );

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
        let electrum_url = electrum_endpoint(config, network)?;

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
    /// caller to spawn. The returned state is seeded with the startup failure
    /// (if any), so wallet info can report it meanwhile.
    async fn init_chain_source(
        config: &WalletConfig,
        network: Network,
    ) -> Result<
        (sync_state::SharedSyncState, Option<ChainSourceInitTask>),
        error::InitChainSourceClient,
    > {
        if config.sync_source == WalletSyncSource::Disabled {
            return Ok((sync_state::SharedSyncState::default(), None));
        }
        match Self::try_init_chain_source_client(config, network).await {
            Ok(client) => Ok((sync_state::SharedSyncState::with_client(client), None)),
            Err(err) if err.is_transient() => {
                tracing::warn!(
                    %err,
                    "wallet sync backend not reachable at startup, \
                     continuing without it and retrying in the background",
                );
                let state = sync_state::SharedSyncState::with_last_error(Some(format!(
                    "{:#}",
                    ErrorChain::new(&err)
                )));
                let init_task = ChainSourceInitTask {
                    config: config.clone(),
                    network,
                    state: state.downgrade(),
                };
                Ok((state, Some(init_task)))
            }
            Err(err) => Err(err),
        }
    }

    fn chain_source_client(&self) -> Option<Arc<ChainSourceClient>> {
        self.sync_state.client()
    }

    /// Record the outcome of an interaction with the sync backend, so wallet
    /// info can report a backend that is currently failing. Passes the result
    /// through unchanged.
    pub(in crate::wallet) fn record_sync_backend_result<T, E>(
        &self,
        result: Result<T, E>,
    ) -> Result<T, E>
    where
        E: std::error::Error,
    {
        self.sync_state.record_result(result)
    }

    fn bip84_descriptors(
        mnemonic: &Mnemonic,
        network: bdk_wallet::bitcoin::Network,
        coin_type: u32,
    ) -> Result<(String, String), error::InitWalletFromMnemonic> {
        let extended_key: ExtendedKey = mnemonic.clone().into_extended_key()?;

        let xpriv = extended_key
            .into_xprv(network.into())
            .ok_or(error::InitWalletFromMnemonic::DeriveXpriv)?;

        Ok((
            format!("wpkh({xpriv}/84'/{coin_type}'/0'/0/*)"),
            format!("wpkh({xpriv}/84'/{coin_type}'/0'/1/*)"),
        ))
    }

    /// Insists on exactly these descriptors. `Ok(None)` if the database holds
    /// no wallet yet.
    async fn load_bdk_wallet(
        external_desc: &str,
        internal_desc: &str,
        network: bdk_wallet::bitcoin::Network,
        wallet_database: &mut Persistence,
    ) -> Result<Option<BdkWallet>, bdk_wallet::LoadWithPersistError<PersistenceError>> {
        bdk_wallet::Wallet::load()
            .descriptor(KeychainKind::External, Some(external_desc.to_owned()))
            .descriptor(KeychainKind::Internal, Some(internal_desc.to_owned()))
            .extract_keys()
            .check_network(network)
            .load_wallet_async(wallet_database)
            .await
    }

    async fn initialize_wallet_from_mnemonic(
        mnemonic: &Mnemonic,
        network: bdk_wallet::bitcoin::Network,
        config: &WalletConfig,
        wallet_database: &mut Persistence,
    ) -> Result<BdkWallet, error::InitWalletFromMnemonic> {
        let coin_type = descriptor_coin_type(config, network);
        let (external_desc, internal_desc) = Self::bip84_descriptors(mnemonic, network, coin_type)?;

        tracing::debug!(%coin_type, "Attempting load of existing BDK wallet");
        let loaded =
            Self::load_bdk_wallet(&external_desc, &internal_desc, network, wallet_database).await;

        let bitcoin_wallet = match loaded {
            Ok(Some(wallet)) => {
                tracing::info!("Loaded existing BDK wallet");
                wallet
            }

            Ok(None) => {
                tracing::info!(%coin_type, "Creating new BDK wallet");

                bdk_wallet::Wallet::create(external_desc, internal_desc)
                    .network(network)
                    .create_wallet_async(wallet_database)
                    .await?
            }

            // A wallet persisted before the coin type became network-aware
            // sits at `LEGACY_COIN_TYPE` on every network. Adopt it rather
            // than refusing to start: this node's funds are on those
            // addresses, and re-deriving would lose sight of them.
            Err(err) if is_descriptor_mismatch(&err) && coin_type != LEGACY_COIN_TYPE => {
                let (legacy_external, legacy_internal) =
                    Self::bip84_descriptors(mnemonic, network, LEGACY_COIN_TYPE)?;
                match Self::load_bdk_wallet(
                    &legacy_external,
                    &legacy_internal,
                    network,
                    wallet_database,
                )
                .await
                {
                    Ok(Some(wallet)) => {
                        tracing::warn!(
                            %coin_type,
                            legacy_coin_type = %LEGACY_COIN_TYPE,
                            "Loaded existing BDK wallet under the legacy coin type. It was \
                             created before the derivation path became network-aware, and \
                             keeps deriving addresses under the path it was created with.",
                        );
                        wallet
                    }
                    // Not a legacy wallet either. Report the original
                    // mismatch, against the descriptor this build wanted.
                    _ => return Err(err.into()),
                }
            }

            Err(err) => return Err(err.into()),
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

        let (sync_state, chain_source_init) =
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
                    &config.wallet_opts,
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
            locks: locks::WalletLocks::new(bitcoin_wallet, wallet_database),
            seed_store,
            sync_state,
        };
        Ok((inner, chain_source_init))
    }

    async fn read_wallet(&self) -> Result<RwLockReadGuardSome<'_, BdkWallet>, error::NotUnlocked> {
        self.locks.read().await
    }

    /// Obtain an upgradable read lock on the inner wallet
    async fn read_wallet_upgradable(
        &self,
    ) -> Result<RwLockUpgradableReadGuardSome<'_, BdkWallet>, error::NotUnlocked> {
        self.locks.upgradable_read().await
    }

    async fn write_wallet(
        &self,
    ) -> Result<RwLockWriteGuardSome<'_, BdkWallet>, error::NotUnlocked> {
        self.locks.write().await
    }

    /// Apply an unconfirmed transaction to the wallet, marking the wallet
    /// UTXOs it spends as spent, and persist. Returns whether anything
    /// changed.
    async fn apply_unconfirmed_tx(
        &self,
        tx: bdk_wallet::bitcoin::Transaction,
    ) -> Result<bool, error::ApplyUnconfirmedTx> {
        let last_seen = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut wallet_write = self.write_wallet().await?;
        let mut bdk_db = self.locks.db(&wallet_write).await;
        let changed = wallet_write
            .with_mut(|wallet| {
                wallet.apply_unconfirmed_txs([(tx, last_seen)]);
                wallet.persist_async(&mut bdk_db)
            })
            .await?;
        drop(bdk_db);
        drop(wallet_write);
        Ok(changed)
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

        // Same lock order as other places
        let mut write_guard = self.locks.write_slot().await;
        let mut database = self.locks.db(&write_guard).await;
        let network = self.validator().network();
        let wallet = WalletInner::initialize_wallet_from_mnemonic(
            &mnemonic,
            network,
            &self.config.wallet_opts,
            &mut database,
        )
        .await?;
        drop(database);
        *write_guard = Some(wallet);
        drop(write_guard);
        Ok(())
    }

    pub async fn unlock_existing_wallet(
        &self,
        password: &str,
    ) -> Result<(), error::UnlockExistingWallet> {
        if self.locks.read_slot().await.is_some() {
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

        let mut write_guard = self.locks.write_slot().await;
        let mut database = self.locks.db(&write_guard).await;
        let network = self.validator().network();

        tracing::debug!("unlock wallet: initializing BDK wallet struct");
        let wallet = WalletInner::initialize_wallet_from_mnemonic(
            &mnemonic,
            network,
            &self.config.wallet_opts,
            &mut database,
        )
        .await?;
        drop(database);
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
    pub sync_source: SyncSourceInfo,
}

/// The wallet's sync source and its observed status.
pub struct SyncSourceInfo {
    pub kind: WalletSyncSource,
    /// The effective backend endpoint, with credentials redacted. `None`
    /// when the sync source is disabled.
    pub endpoint: Option<String>,
    /// Whether a connection to the backend has been established. A backend
    /// that is unreachable at startup is retried in the background.
    pub connected: bool,
    /// The most recent error from the backend -- a failed connection attempt
    /// or a failed sync. `None` while the backend is healthy.
    pub last_error: Option<String>,
    /// Time of the last successful wallet sync.
    pub last_synced_at: Option<SystemTime>,
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

        loop {
            tokio::select! {
                biased;  // Prioritize shutdown

                () = cancel.cancelled() => {
                    tracing::info!("shutting down sync task");
                    return Ok(());
                }
                // A fresh sleep per iteration: the idle gap is measured from
                // the end of the previous sync, so a sync that outlasts the
                // interval never causes back-to-back runs.
                () = tokio::time::sleep(SYNC_INTERVAL) => {
                    let tick = Uuid::new_v4().simple();
                    let span = tracing::span!(tracing::Level::DEBUG,
                        "wallet_sync",
                        %tick,
                    );
                    let guard = span.enter();
                    if !self.inner.sync_state.has_synced() {
                        // Initial sync is incomplete, nothing to do
                        tracing::debug!(
                            "waiting for initial wallet sync to complete"
                        );
                    } else if let Err(err) = self.inner.sync().await {
                        tracing::error!("wallet sync error: {:#}", ErrorChain::new(&err));
                    }
                    drop(guard);
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

    /// Full scan the wallet against the chain source. Fails with
    /// [`error::FullScan::ScanInProgress`] rather than queueing if a scan is
    /// already running.
    pub async fn full_scan(&self) -> miette::Result<BlockHash, error::FullScan> {
        self.inner.try_full_scan().await
    }

    pub async fn is_initialized(&self) -> bool {
        self.inner.locks.read_slot().await.is_some()
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
            let applied_changes = self.inner.apply_unconfirmed_tx(tx).await?;
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
        let has_synced = self.inner.sync_state.has_synced();

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
            let mut prev_txouts = Vec::new();
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

                let prev_txout = prev_output.output[input.previous_output.vout as usize].clone();
                input_value += prev_txout.value;
                prev_txouts.push(prev_txout);
            }

            // One wallet read covers every input of this tx, instead of
            // re-acquiring the lock per input.
            {
                let wallet_read = self.inner.read_wallet().await?;
                for prev_txout in prev_txouts {
                    if wallet_read.is_mine(prev_txout.script_pubkey) {
                        sent += prev_txout.value;
                    }
                }
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

    /// The unconfirmed wallet transaction that has to be replaced for
    /// `required_utxos` to be spendable, if there is exactly one.
    ///
    /// `TxBuilder::add_utxos` resolves outpoints against the wallet's unspent
    /// outputs alone, so requiring an output that an unconfirmed wallet
    /// transaction already spends fails as unknown. Requiring such an output
    /// is a request to replace that transaction -- raising a BMM bid, for
    /// instance -- which BDK models as a fee bump of it rather than as a
    /// fresh build.
    ///
    /// `None` unless *every* required outpoint is an input of one and the same
    /// unconfirmed transaction. A replacement is confined to that
    /// transaction's inputs, so it cannot also honour a required output that
    /// is merely unspent; a mix of the two, like anything else ambiguous, is
    /// left to fail as an unknown UTXO exactly as before.
    fn replaced_tx(
        wallet: &bdk_wallet::Wallet,
        required_utxos: &[bdk_wallet::bitcoin::OutPoint],
    ) -> Option<bdk_wallet::bitcoin::Txid> {
        if required_utxos.is_empty()
            || required_utxos
                .iter()
                .any(|outpoint| wallet.list_unspent().any(|utxo| utxo.outpoint == *outpoint))
        {
            return None;
        }
        let mut spenders = wallet
            .transactions()
            .filter(|wallet_tx| !wallet_tx.chain_position.is_confirmed())
            .filter(|wallet_tx| {
                wallet_tx
                    .tx_node
                    .tx
                    .input
                    .iter()
                    .any(|txin| required_utxos.contains(&txin.previous_output))
            });
        let replaced = spenders.next()?;
        if spenders.next().is_some() {
            return None;
        }
        required_utxos
            .iter()
            .all(|outpoint| {
                replaced
                    .tx_node
                    .tx
                    .input
                    .iter()
                    .any(|txin| txin.previous_output == *outpoint)
            })
            .then_some(replaced.tx_node.txid)
    }

    fn build_send_psbt(
        wallet: &mut bdk_wallet::Wallet,
        destinations: HashMap<bitcoin::Address, Amount>,
        params: CreateTransactionParams,
    ) -> Result<bdk_wallet::bitcoin::psbt::Psbt, error::CreateSendPsbt> {
        let replaced_txid = Self::replaced_tx(wallet, &params.required_utxos);

        let mut timestamp = Instant::now();
        // Nothing is applied to the wallet here: a fee bump leaves the
        // transaction it replaces in the canonical set, which only gives way
        // once the replacement is broadcast and applied as unconfirmed. BDK
        // enforces the BIP125 fee increase against the replaced transaction.
        let mut builder = match replaced_txid {
            Some(txid) => {
                tracing::info!(%txid, "Replacing unconfirmed transaction");
                let mut builder = wallet
                    .build_fee_bump(txid)
                    .map_err(error::CreateSendPsbt::BuildFeeBump)?;
                // A fee bump seeds the builder with the replaced transaction's
                // outputs, but the outputs of this one come from the caller,
                // exactly as they do for a fresh build.
                builder.set_recipients(Vec::new());
                builder
            }
            None => wallet.build_tx(),
        };

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
            // A fee bump already requires the replaced transaction's inputs,
            // which is where the required UTXOs went. `manually_selected_only`
            // below then confines the replacement to those inputs, so it
            // cannot pull in an unconfirmed one and break BIP125's rule
            // against a replacement adding unconfirmed inputs.
            if replaced_txid.is_none() {
                builder
                    // TODO: this does not work at all for wallets past a certain scale....
                    // 25s pr. UTXO for a wallet with 40k UTXOs in total
                    .add_utxos(&params.required_utxos)
                    .map_err(|err| match err {
                        bdk_wallet::tx_builder::AddUtxoError::UnknownUtxo(outpoint) => {
                            error::CreateSendPsbt::UnknownUTXO(outpoint)
                        }
                    })?;
            }

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
        let applied_changes = self.inner.apply_unconfirmed_tx(tx).await?;

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
        let sync_state::SyncStateReport {
            connected,
            last_error,
            last_synced_at,
        } = self.inner.sync_state.report();

        let w = self.inner.read_wallet().await?;
        let mut keychain_descriptors = std::collections::HashMap::new();
        for (kind, _) in w.keychains() {
            keychain_descriptors.insert(kind, w.public_descriptor(kind).clone());
        }

        let tip = w.local_chain().tip();

        let wallet_opts = &self.inner.config.wallet_opts;
        let sync_source = SyncSourceInfo {
            kind: wallet_opts.sync_source,
            endpoint: sync_backend_endpoint(wallet_opts, w.network()),
            connected,
            last_error,
            last_synced_at,
        };

        Ok(WalletInfo {
            keychain_descriptors,
            network: w.network(),
            transaction_count: w.transactions().count(),
            unspent_output_count: w.list_unspent().count(),
            tip: (tip.hash(), tip.height()),
            sync_source,
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

        let mut bdk_db_lock = self.inner.locks.db(&wallet_write).await;
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
        bip39::Language,
        bitcoin::{Amount, ScriptBuf, script::PushBytesBuf},
        test_utils::get_funded_wallet_wpkh,
    };
    use bitcoin::{BlockHash, hashes::Hash as _};

    use super::{
        BdkWallet, CreateTransactionParams, KeychainKind, LEGACY_COIN_TYPE, M8BmmRequest, Mnemonic,
        Network, Persistence, Wallet, WalletConfig, WalletInner, WalletSyncSource, error,
        slip44_coin_type,
    };
    use crate::{
        errors::ErrorChain,
        types::{BmmCommitment, SidechainNumber},
    };

    /// The BIP 39 test mnemonic that BIP 84's published vectors derive from.
    const TEST_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon \
                                 abandon abandon abandon abandon abandon about";

    /// Every network other than mainnet is a test network, and SLIP-44
    /// reserves coin type `1` for all of them.
    #[test]
    fn coin_type_is_zero_only_on_mainnet() {
        assert_eq!(slip44_coin_type(Network::Bitcoin), 0);
        for network in [
            Network::Testnet,
            Network::Testnet4,
            Network::Signet,
            Network::Regtest,
        ] {
            assert_eq!(slip44_coin_type(network), 1, "{network}");
        }
    }

    fn test_mnemonic() -> Mnemonic {
        Mnemonic::parse_in_normalized(Language::English, TEST_MNEMONIC).unwrap()
    }

    fn descriptors(network: Network) -> (String, String) {
        WalletInner::bip84_descriptors(&test_mnemonic(), network, slip44_coin_type(network))
            .unwrap()
    }

    #[test]
    fn descriptors_carry_the_network_coin_type() {
        let (external, internal) = descriptors(Network::Bitcoin);
        assert!(external.starts_with("wpkh(xprv"), "{external}");
        assert!(external.ends_with("/84'/0'/0'/0/*)"), "{external}");
        assert!(internal.ends_with("/84'/0'/0'/1/*)"), "{internal}");

        for network in [
            Network::Testnet,
            Network::Testnet4,
            Network::Signet,
            Network::Regtest,
        ] {
            let (external, internal) = descriptors(network);
            assert!(external.starts_with("wpkh(tprv"), "{network}: {external}");
            assert!(
                external.ends_with("/84'/1'/0'/0/*)"),
                "{network}: {external}"
            );
            assert!(
                internal.ends_with("/84'/1'/0'/1/*)"),
                "{network}: {internal}"
            );
        }
    }

    /// First external and internal address for `network`, derived through BDK
    /// rather than from the descriptor string, so the assertions cover the
    /// path the wallet actually takes.
    fn first_addresses(network: Network) -> (String, String) {
        let (external, internal) = descriptors(network);
        let wallet = bdk_wallet::Wallet::create(external, internal)
            .network(network)
            .create_wallet_no_persist()
            .unwrap();
        (
            wallet.peek_address(KeychainKind::External, 0).to_string(),
            wallet.peek_address(KeychainKind::Internal, 0).to_string(),
        )
    }

    /// Pins mainnet against BIP 84's published test vectors. A mainnet wallet
    /// built before the coin type became network-aware derived under the
    /// test-network `1'` and produced neither of these addresses.
    #[test]
    fn mainnet_matches_bip84_test_vectors() {
        let (receive, change) = first_addresses(Network::Bitcoin);
        assert_eq!(receive, "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu");
        assert_eq!(change, "bc1q8c6fshw2dlwun7ekn9qwf37cu2rn755upcp6el");
    }

    /// The test networks keep the derivation they already had, and stay
    /// distinct from mainnet's.
    #[test]
    fn test_networks_derive_under_the_test_coin_type() {
        let (mainnet, _) = first_addresses(Network::Bitcoin);
        let (signet, _) = first_addresses(Network::Signet);
        let (regtest, _) = first_addresses(Network::Regtest);

        assert!(signet.starts_with("tb1"), "{signet}");
        assert!(regtest.starts_with("bcrt1"), "{regtest}");
        assert_ne!(mainnet, signet);
    }

    fn wallet_config(derivation_coin_type: Option<u32>) -> WalletConfig {
        WalletConfig {
            auto_create: false,
            esplora_url: None,
            electrum_host: None,
            electrum_port: None,
            skip_periodic_sync: false,
            sync_source: WalletSyncSource::Disabled,
            max_block_by_block_replay: 2_000,
            derivation_coin_type,
            mnemonic_path: None,
        }
    }

    /// Open the wallet the way the enforcer does.
    async fn open_wallet(
        database: &mut Persistence,
        network: Network,
        coin_type: Option<u32>,
    ) -> Result<BdkWallet, error::InitWalletFromMnemonic> {
        WalletInner::initialize_wallet_from_mnemonic(
            &test_mnemonic(),
            network,
            &wallet_config(coin_type),
            database,
        )
        .await
    }

    /// Leave `database` holding a wallet persisted under `coin_type`, and
    /// return its first receive address.
    async fn persist_wallet_under(
        database: &mut Persistence,
        network: Network,
        coin_type: u32,
    ) -> String {
        let wallet = open_wallet(database, network, Some(coin_type))
            .await
            .expect("an empty database must yield a freshly created wallet");
        wallet.peek_address(KeychainKind::External, 0).to_string()
    }

    async fn empty_database(dir: &temp_dir::TempDir) -> Persistence {
        Persistence::open(dir.path().join("wallet.sqlite.db"))
            .await
            .expect("must open a wallet database")
    }

    /// The upgrade the fallback exists for: a mainnet wallet persisted under
    /// [`LEGACY_COIN_TYPE`] has to open under a build that derives `0`, and
    /// keep deriving where its funds are.
    #[tokio::test]
    async fn a_legacy_mainnet_wallet_still_loads() {
        let dir = temp_dir::TempDir::new().unwrap();
        let mut database = empty_database(&dir).await;
        let legacy_address =
            persist_wallet_under(&mut database, Network::Bitcoin, LEGACY_COIN_TYPE).await;

        let wallet = open_wallet(&mut database, Network::Bitcoin, None)
            .await
            .expect("a legacy mainnet wallet must still load");

        assert_eq!(
            wallet.peek_address(KeychainKind::External, 0).to_string(),
            legacy_address,
            "the wallet must keep deriving under the path it was created with",
        );
        // Guard the premise: the two derivations must genuinely differ.
        assert_ne!(
            legacy_address, "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu",
            "the legacy derivation must not coincide with the BIP 84 one",
        );
    }

    /// An empty database is unaffected by the fallback.
    #[tokio::test]
    async fn a_fresh_mainnet_wallet_is_created_under_the_network_coin_type() {
        let dir = temp_dir::TempDir::new().unwrap();
        let mut database = empty_database(&dir).await;

        let wallet = open_wallet(&mut database, Network::Bitcoin, None)
            .await
            .expect("an empty database must yield a freshly created wallet");

        assert_eq!(
            wallet.peek_address(KeychainKind::External, 0).to_string(),
            "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu",
        );
    }

    /// Neither this build's descriptor nor the legacy one: a foreign wallet,
    /// which must fail as a data mismatch rather than an opaque load failure.
    #[tokio::test]
    async fn an_unrecognized_descriptor_is_a_data_mismatch() {
        let dir = temp_dir::TempDir::new().unwrap();
        let mut database = empty_database(&dir).await;
        let _foreign_address = persist_wallet_under(&mut database, Network::Bitcoin, 7).await;

        let err = open_wallet(&mut database, Network::Bitcoin, None)
            .await
            .expect_err("a foreign descriptor must not load");

        assert!(
            matches!(err, error::InitWalletFromMnemonic::DataMismatch(_)),
            "expected a data mismatch, got: {err:#}",
        );
        // The report has to name what differs; not doing so is what left the
        // original bug report with nothing to go on.
        let rendered = format!("{:#}", ErrorChain::new(&err));
        assert!(
            rendered.contains("Descriptor mismatch"),
            "the mismatch BDK reported must survive into the error: {rendered}",
        );
    }

    /// The mapping used to be a substring test for "data mismatch", a phrase
    /// BDK never emits, so every mismatch fell through to the opaque
    /// `LoadWallet` variant. Pins why the mapping must stay on the variant.
    #[tokio::test]
    async fn bdk_does_not_call_a_mismatch_a_data_mismatch() {
        let dir = temp_dir::TempDir::new().unwrap();
        let mut database = empty_database(&dir).await;
        let _foreign_address = persist_wallet_under(&mut database, Network::Bitcoin, 7).await;

        let (external, internal) = descriptors(Network::Bitcoin);
        let err =
            WalletInner::load_bdk_wallet(&external, &internal, Network::Bitcoin, &mut database)
                .await
                .expect_err("a foreign descriptor must not load");

        assert!(
            !err.to_string().contains("data mismatch"),
            "BDK renders its mismatch as `{err}`. If it now says `data \
             mismatch`, the old substring test would work again -- but the \
             mapping is matched on the variant, so only this pin is stale.",
        );
    }

    /// The test networks' coin type never changed, so there is nothing to
    /// fall back to and a mismatch is exactly that.
    #[tokio::test]
    async fn a_test_network_has_no_legacy_descriptor_to_fall_back_to() {
        let dir = temp_dir::TempDir::new().unwrap();
        let mut database = empty_database(&dir).await;
        assert_eq!(slip44_coin_type(Network::Regtest), LEGACY_COIN_TYPE);
        // The mainnet coin type, on a regtest wallet.
        let _foreign_address = persist_wallet_under(&mut database, Network::Regtest, 0).await;

        let err = open_wallet(&mut database, Network::Regtest, None)
            .await
            .expect_err("a foreign descriptor must not load");

        assert!(
            matches!(err, error::InitWalletFromMnemonic::DataMismatch(_)),
            "expected a data mismatch, got: {err:#}",
        );
    }

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

    /// Raising a BMM bid means replacing the unconfirmed request, so the
    /// raised bid must respend the very inputs that request already spends.
    /// Those inputs are gone from `list_unspent`, so requiring them has to
    /// build a fee bump of the request that holds them rather than fail as
    /// unknown -- while leaving that request in place, since only a broadcast
    /// replacement actually displaces it.
    #[test]
    fn raised_bmm_bid_is_a_fee_bump_of_the_unconfirmed_bid() {
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
        let tx = psbt.unsigned_tx;
        let bid_txid = tx.compute_txid();
        let spent: Vec<_> = tx.input.iter().map(|input| input.previous_output).collect();
        assert!(!spent.is_empty());
        wallet.apply_unconfirmed_txs([(tx, 100)]);

        let raise = Amount::from_sat(25_000);
        let raised_psbt = Wallet::build_send_psbt(
            &mut wallet,
            HashMap::new(),
            CreateTransactionParams {
                op_return_message: Some(message.as_bytes().to_vec()),
                required_utxos: spent.clone(),
                fee_policy: Some(crate::types::FeePolicy::Absolute(raise)),
                ..Default::default()
            },
        )
        .expect("a raised bid must respend the unconfirmed bid's inputs");
        let raised = raised_psbt.unsigned_tx;

        let respent: Vec<_> = raised
            .input
            .iter()
            .map(|input| input.previous_output)
            .collect();
        assert_eq!(respent, spent, "must respend the bid's inputs");
        assert!(
            raised.input.iter().all(|input| input.sequence.is_rbf()),
            "the raised bid must signal BIP125 replaceability"
        );
        assert_eq!(
            wallet.calculate_fee(&raised).unwrap(),
            raise,
            "the raise is paid as the fee"
        );
        assert_eq!(
            raised
                .output
                .iter()
                .filter(|output| output.script_pubkey.is_op_return())
                .count(),
            1,
            "the replaced bid's OP_RETURN must not be carried over"
        );
        M8BmmRequest::parse(&raised.output[0].script_pubkey.to_bytes())
            .expect("output zero must contain the M8 request");
        assert!(
            wallet.transactions().any(|tx| tx.tx_node.txid == bid_txid),
            "building a replacement must not evict the bid it replaces"
        );
    }

    /// A replacement can only spend what the transaction it replaces spends,
    /// so a required UTXO from outside that transaction cannot be honoured.
    /// Requiring one alongside the replaced inputs must keep failing loudly
    /// rather than quietly build a transaction without it.
    #[test]
    fn required_utxo_outside_the_replaced_bid_is_still_unknown() {
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
        let tx = psbt.unsigned_tx;
        let spent: Vec<_> = tx.input.iter().map(|input| input.previous_output).collect();
        wallet.apply_unconfirmed_txs([(tx, 100)]);

        let mut required_utxos = spent.clone();
        required_utxos.push(
            wallet
                .list_unspent()
                .next()
                .expect("the bid must leave an unspent output")
                .outpoint,
        );

        let err = Wallet::build_send_psbt(
            &mut wallet,
            HashMap::new(),
            CreateTransactionParams {
                op_return_message: Some(message.as_bytes().to_vec()),
                required_utxos,
                fee_policy: Some(crate::types::FeePolicy::Absolute(Amount::from_sat(25_000))),
                ..Default::default()
            },
        )
        .expect_err("a required UTXO outside the replaced bid must not be dropped");
        let error::CreateSendPsbt::UnknownUTXO(outpoint) = &err else {
            panic!("unexpected error: {err}");
        };
        assert!(
            spent.contains(outpoint),
            "the bid's own inputs are the unknown ones"
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

    /// A password embedded in `--wallet-esplora-url` reaches the wallet
    /// config verbatim, and wallet info is served over the API, so the
    /// endpoint reported there must be redacted the same way the startup
    /// config dump redacts it.
    #[test]
    fn reported_esplora_endpoint_redacts_credentials() {
        let config = WalletConfig {
            auto_create: false,
            esplora_url: Some(
                url::Url::parse("https://alice:hunter2@esplora.example/api").unwrap(),
            ),
            electrum_host: None,
            electrum_port: None,
            skip_periodic_sync: false,
            sync_source: WalletSyncSource::Esplora,
            max_block_by_block_replay: 2_000,
            derivation_coin_type: None,
            mnemonic_path: None,
        };

        let endpoint = super::sync_backend_endpoint(&config, Network::Regtest)
            .expect("an esplora URL is configured");
        assert!(
            !endpoint.contains("hunter2"),
            "the esplora password must not be reported over the API: {endpoint}"
        );
        assert_eq!(endpoint, "https://alice:redacted@esplora.example/api");
    }

    /// Host and port default independently, so a config that sets only one of
    /// them must still report a complete endpoint.
    #[test]
    fn reported_electrum_endpoint_fills_in_defaults() {
        let config = WalletConfig {
            auto_create: false,
            esplora_url: None,
            electrum_host: None,
            electrum_port: Some(50009),
            skip_periodic_sync: false,
            sync_source: WalletSyncSource::Electrum,
            max_block_by_block_replay: 2_000,
            derivation_coin_type: None,
            mnemonic_path: None,
        };

        assert_eq!(
            super::sync_backend_endpoint(&config, Network::Regtest).as_deref(),
            Some("127.0.0.1:50009"),
        );
    }

    /// A disabled sync source has no backend, so there is no endpoint to
    /// report rather than a misleading default one.
    #[test]
    fn disabled_sync_source_reports_no_endpoint() {
        let config = WalletConfig {
            auto_create: false,
            esplora_url: None,
            electrum_host: None,
            electrum_port: None,
            skip_periodic_sync: false,
            sync_source: WalletSyncSource::Disabled,
            max_block_by_block_replay: 2_000,
            derivation_coin_type: None,
            mnemonic_path: None,
        };

        assert_eq!(
            super::sync_backend_endpoint(&config, Network::Regtest),
            None
        );
    }
}
