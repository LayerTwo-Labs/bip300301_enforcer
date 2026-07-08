use std::{
    collections::{HashMap, HashSet},
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
    keys::{
        DerivableKey as _, ExtendedKey,
        bip39::{Language, Mnemonic},
    },
};
use bitcoin::{
    Amount, BlockHash, Network, Transaction, Txid,
    hashes::{Hash as _, HashEngine, sha256, sha256d},
    script::PushBytesBuf,
};
use bitcoin_jsonrpsee::{
    client::{GetRawTransactionClient, GetRawTransactionVerbose},
    jsonrpsee::http_client::HttpClient,
};
use either::Either;
use fallible_iterator::{FallibleIterator as _, IteratorExt as _};
use futures::{FutureExt, TryFutureExt, TryStreamExt};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::instrument;
use uuid::Uuid;

use crate::{
    cli::{Config, WalletConfig, WalletSyncSource},
    convert,
    errors::ErrorChain,
    messages::{self, M8BmmRequest},
    types::{
        BDKWalletTransaction, BlindedM6, BmmCommitment, Ctip, M6id, PendingM6idInfo, SidechainAck,
        SidechainNumber, SidechainProposal, SidechainProposalId,
    },
    validator::{self, Validator},
    wallet::{
        error::WalletInitialization,
        mnemonic::{EncryptedMnemonic, new_mnemonic},
        util::{RwLockReadGuardSome, RwLockUpgradableReadGuardSome, RwLockWriteGuardSome},
    },
};

mod cusf_block_producer;
pub mod error;
mod mine;
pub mod mnemonic;
pub mod reusable_payments;
mod sync;
mod thread_safe_connection;
mod util;

type BundleProposals = Vec<(M6id, BlindedM6<'static>, Option<PendingM6idInfo>)>;

pub(crate) type Persistence = thread_safe_connection::ThreadSafeConnection;
type BdkWallet = bdk_wallet::PersistedWallet<Persistence>;

type ElectrumClient = BdkElectrumClient<bdk_electrum::electrum_client::Client>;
type EsploraClient = bdk_esplora::esplora_client::AsyncClient;

#[non_exhaustive]
enum ChainSourceClient {
    Electrum(Box<ElectrumClient>),
    Esplora(EsploraClient),
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
    validator: Validator,
    magic: bitcoin::p2p::Magic,
    // Always Some(_) on signets
    signet_challenge: Option<bitcoin::ScriptBuf>,
    // Unlocked, ready-to-go wallet: Some
    // Locked wallet: None
    bitcoin_wallet: async_lock::RwLock<Option<BdkWallet>>,
    /// Persistence for the BDK wallet
    bdk_db: tokio::sync::Mutex<Persistence>,
    // Persistence for things /we/ care about. Wallet seed, M* messages, ++.
    self_db: tokio::sync::Mutex<rusqlite::Connection>,
    chain_source_client: Option<ChainSourceClient>,
    last_sync: async_lock::RwLock<Option<SystemTime>>,
    config: Config,
    /// Limits GenerateBlocks to one concurrent call at a time.
    generate_blocks_semaphore: Arc<tokio::sync::Semaphore>,
    /// Error from the most recent failed block template build, cleared on
    /// success. The GBT server reports template failures to its JSON-RPC
    /// client in a field that `bitcoin-cli` (and thus the signet miner's
    /// stderr) drops, so GenerateBlocks attaches this to its own error to
    /// surface the root cause.
    last_gbt_error: parking_lot::RwLock<Option<String>>,
    /// One-permit semaphores serializing sends per recipient payment code.
    bip47_send_locks:
        tokio::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Semaphore>>>,
    /// Limits reusable-payments scanning to one task at a time.
    scanner_lock: tokio::sync::Semaphore,
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

    async fn init_chain_source_client(
        config: &WalletConfig,
        network: Network,
    ) -> Result<Option<ChainSourceClient>, error::InitChainSourceClient> {
        if config.sync_source == WalletSyncSource::Disabled {
            return Ok(None);
        }
        // The sync backend (electrs / esplora) may not be reachable yet when the
        // enforcer starts -- e.g. a freshly bootstrapped network where electrs is
        // still coming up. Rather than aborting startup, retry transient
        // connection failures with capped backoff, mirroring how we wait for
        // Bitcoin Core to become ready. Config and chain-mismatch errors are not
        // transient and fail immediately.
        const INITIAL_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
        const MAX_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
        let mut retry_delay = INITIAL_RETRY_DELAY;
        loop {
            let result = match config.sync_source {
                WalletSyncSource::Electrum => Self::init_electrum_client(config, network)
                    .map(|client| ChainSourceClient::Electrum(Box::new(client)))
                    .map_err(error::InitChainSourceClient::from),
                WalletSyncSource::Esplora => Self::init_esplora_client(config, network)
                    .await
                    .map(ChainSourceClient::Esplora)
                    .map_err(error::InitChainSourceClient::from),
                WalletSyncSource::Disabled => unreachable!("handled above"),
            };
            match result {
                Ok(client) => return Ok(Some(client)),
                Err(err) if err.is_transient() => {
                    tracing::warn!(
                        %err,
                        "wallet sync backend not ready, retrying in {retry_delay:?}",
                    );
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn init_db_connection(
        data_dir: &Path,
    ) -> Result<rusqlite::Connection, error::InitDbConnection> {
        use rusqlite_migration::{M, Migrations};
        // 1️⃣ Define migrations
        let migrations = Migrations::new(vec![
            M::up(
                "CREATE TABLE sidechain_proposals
               (sidechain_number INTEGER NOT NULL,
                data_hash BLOB NOT NULL,
                data BLOB NOT NULL,
                UNIQUE(sidechain_number, data_hash));",
            ),
            M::up(
                "CREATE TABLE sidechain_acks
               (number INTEGER NOT NULl,
                data_hash BLOB NOT NULL,
                UNIQUE(number, data_hash));",
            ),
            M::up(
                "CREATE TABLE bundle_proposals
               (sidechain_number INTEGER NOT NULL,
                bundle_hash BLOB NOT NULL,
                bundle_tx BLOB NOT NULL,
                UNIQUE(sidechain_number, bundle_hash));",
            ),
            M::up(
                "CREATE TABLE bundle_acks
               (sidechain_number INTEGER NOT NULL,
                bundle_hash BLOB NOT NULL,
                UNIQUE(sidechain_number, bundle_hash));",
            ),
            M::up(
                "CREATE TABLE bmm_requests
                (sidechain_number INTEGER NOT NULL,
                 prev_block_hash BLOB NOT NULL,
                 side_block_hash BLOB NOT NULL,
                 UNIQUE(sidechain_number, prev_block_hash));",
            ),
            M::up(
                "CREATE TABLE wallet_seeds
                (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 plaintext_mnemonic TEXT, 

                 -- encryption values 
                 initialization_vector BLOB, 
                 ciphertext_mnemonic BLOB, 
                 key_salt BLOB,

                 -- boolean that indicates if the wallet uses a BIP39 passphrase
                 needs_passphrase BOOLEAN NOT NULL DEFAULT FALSE, 

                 -- timestamp of the creation of the seed
                 creation_time DATETIME NOT NULL DEFAULT (DATETIME('now'))
                );",
            ),
            M::up(
                "CREATE TABLE bmm_requests_undo
                (block_hash BLOB NOT NULL,
                 sidechain_number INTEGER NOT NULL,
                 prev_block_hash BLOB NOT NULL,
                 side_block_hash BLOB NOT NULL);",
            ),
            M::up(
                "CREATE TABLE reusable_scan_state (
                    wallet_id INTEGER PRIMARY KEY,
                    birthday_height INTEGER NOT NULL,
                    last_scanned_height INTEGER NOT NULL,
                    last_scanned_block_hash BLOB
                );",
            ),
            M::up(
                "CREATE TABLE bip47_send_state (
                    recipient_payment_code TEXT PRIMARY KEY,
                    -- 1 = v1 (80-byte payload), 3 = v3 (35-byte payload).
                    version INTEGER NOT NULL,
                    notification_txid BLOB,
                    notification_broadcast_at INTEGER,
                    next_send_index INTEGER NOT NULL DEFAULT 0
                );",
            ),
            M::up(
                "CREATE TABLE bip47_inbound_payers (
                    sender_payment_code TEXT PRIMARY KEY,
                    version INTEGER NOT NULL,
                    notification_txid BLOB NOT NULL,
                    notification_height INTEGER NOT NULL,
                    next_receive_index INTEGER NOT NULL DEFAULT 0,
                    first_seen_unix INTEGER NOT NULL
                );",
            ),
            M::up(
                "CREATE TABLE silent_payment_labels (
                    -- 0 reserved for change; user labels start at 1.
                    m INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );",
            ),
            M::up(
                "CREATE TABLE silent_payment_received (
                    txid BLOB NOT NULL,
                    vout INTEGER NOT NULL,
                    output_pubkey BLOB NOT NULL,
                    amount_sats INTEGER NOT NULL,
                    tweak_k INTEGER NOT NULL,
                    -- NULL = base address, 0 = change, >=1 = user label.
                    label_m INTEGER,
                    height INTEGER NOT NULL,
                    spent_in_txid BLOB,
                    PRIMARY KEY (txid, vout)
                );",
            ),
            M::up("ALTER TABLE bip47_send_state ADD COLUMN notification_height INTEGER;"),
            M::up(
                "CREATE TABLE bip47_received (
                    txid BLOB NOT NULL,
                    vout INTEGER NOT NULL,
                    sender_payment_code TEXT NOT NULL,
                    sender_index INTEGER NOT NULL,
                    version INTEGER NOT NULL,
                    amount_sats INTEGER NOT NULL,
                    script_pubkey BLOB NOT NULL,
                    height INTEGER NOT NULL,
                    PRIMARY KEY (txid, vout)
                );",
            ),
            M::up(
                "CREATE INDEX bip47_received_sender ON bip47_received (sender_payment_code, sender_index);",
            ),
            M::up("ALTER TABLE bip47_received ADD COLUMN spent_in_txid BLOB;"),
        ]);

        let db_name = "db.sqlite";
        let path = data_dir.join(db_name);
        let mut db_connection = Connection::open(path.clone())?;
        tracing::info!("Created database connection to {}", path.display());
        migrations.to_latest(&mut db_connection)?;
        tracing::debug!("Ran migrations on {}", path.display());
        Ok(db_connection)
    }

    /// Derive the master `Xpriv` used to build the wallet's descriptors, and
    /// by the reusable-payments (BIP47 / silent payments) key hierarchy.
    pub(in crate::wallet) fn mnemonic_to_xpriv(
        mnemonic: &Mnemonic,
        network: Network,
    ) -> Result<bitcoin::bip32::Xpriv, error::MnemonicToXpriv> {
        let extended_key: ExtendedKey = mnemonic.clone().into_extended_key()?;
        extended_key
            .into_xprv(network.into())
            .ok_or(error::MnemonicToXpriv::DeriveXpriv)
    }

    async fn initialize_wallet_from_mnemonic(
        mnemonic: &Mnemonic,
        network: bdk_wallet::bitcoin::Network,
        wallet_database: &mut Persistence,
    ) -> Result<BdkWallet, error::InitWalletFromMnemonic> {
        let xpriv = Self::mnemonic_to_xpriv(mnemonic, network)?;

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
        validator: Validator,
        magic: bitcoin::p2p::Magic,
        signet_challenge: Option<bitcoin::ScriptBuf>,
    ) -> Result<Self, error::InitWallet> {
        let network = {
            let validator_network = validator.network();
            bdk_wallet::bitcoin::Network::from_str(validator_network.to_string().as_str())?
        };
        if network == bdk_wallet::bitcoin::Network::Signet && signet_challenge.is_none() {
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

        let chain_source_client =
            Self::init_chain_source_client(&config.wallet_opts, network).await?;
        let db_connection = Self::init_db_connection(data_dir)?;

        // If we:
        // 1. Already have an initialized wallet
        // 2. It's plaintext
        //
        // We can just go ahead and unlock the wallet right away.
        let bitcoin_wallet =
            if let Some(Either::Left(mnemonic)) = WalletInner::read_db_mnemonic(&db_connection)? {
                tracing::debug!("found plaintext mnemonic, going straight to initialization");
                let initialized = WalletInner::initialize_wallet_from_mnemonic(
                    &mnemonic,
                    network,
                    &mut wallet_database,
                )
                .await?;

                Some(initialized)
            } else {
                None
            };

        tracing::debug!(
            message = "wallet inner: wired together components",
            wallet_initialized = bitcoin_wallet.is_some()
        );

        Ok(Self {
            config: config.clone(),
            main_client,
            validator,
            magic,
            signet_challenge,
            bitcoin_wallet: async_lock::RwLock::new(bitcoin_wallet),
            bdk_db: tokio::sync::Mutex::new(wallet_database),
            self_db: tokio::sync::Mutex::new(db_connection),
            chain_source_client,
            last_sync: async_lock::RwLock::new(None),
            generate_blocks_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            last_gbt_error: parking_lot::RwLock::new(None),
            bip47_send_locks: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            scanner_lock: tokio::sync::Semaphore::new(1),
        })
    }

    /// Warn if lock takes this long to acquire
    const LOCK_WARN_DURATION: Duration = Duration::from_secs(1);

    async fn read_wallet(&self) -> Result<RwLockReadGuardSome<'_, BdkWallet>, error::NotUnlocked> {
        use futures::future::{Either, select};
        tracing::trace!("wallet: acquiring read lock");
        let read_guard = match select(
            self.bitcoin_wallet.read().boxed(),
            tokio::time::sleep(Self::LOCK_WARN_DURATION).boxed(),
        )
        .await
        {
            Either::Left((read_guard, _sleep)) => read_guard,
            Either::Right(((), acquiring_read_lock)) => {
                tracing::warn!(
                    "wallet: waiting over {} to acquire read lock",
                    jiff::SignedDuration::try_from(Self::LOCK_WARN_DURATION).unwrap(),
                );
                acquiring_read_lock.await
            }
        };
        RwLockReadGuardSome::new(read_guard).ok_or(error::NotUnlocked)
    }

    /// Obtain an upgradable read lock on the inner wallet
    async fn read_wallet_upgradable(
        &self,
    ) -> Result<RwLockUpgradableReadGuardSome<'_, BdkWallet>, error::NotUnlocked> {
        use futures::future::{Either, select};
        tracing::trace!("wallet: acquiring upgradable read lock");
        let read_guard = match select(
            self.bitcoin_wallet.upgradable_read().boxed(),
            tokio::time::sleep(Self::LOCK_WARN_DURATION).boxed(),
        )
        .await
        {
            Either::Left((read_guard, _sleep)) => read_guard,
            Either::Right(((), acquiring_read_lock)) => {
                tracing::warn!(
                    "waiting over {} to acquire read lock",
                    jiff::SignedDuration::try_from(Self::LOCK_WARN_DURATION).unwrap(),
                );
                acquiring_read_lock.await
            }
        };
        RwLockUpgradableReadGuardSome::new(read_guard).ok_or(error::NotUnlocked)
    }

    async fn write_wallet(
        &self,
    ) -> Result<RwLockWriteGuardSome<'_, BdkWallet>, error::NotUnlocked> {
        use futures::future::{Either, select};
        let start = SystemTime::now();
        let span = tracing::span!(tracing::Level::TRACE, "acquire_write_lock");
        let _guard = span.enter();
        tracing::trace!("acquiring write lock");
        let write_guard = match select(
            self.bitcoin_wallet.write().boxed(),
            tokio::time::sleep(Self::LOCK_WARN_DURATION).boxed(),
        )
        .await
        {
            Either::Left((write_guard, _sleep)) => write_guard,
            Either::Right(((), acquiring_write_lock)) => {
                tracing::warn!(
                    "waiting over {} to acquire write lock",
                    jiff::SignedDuration::try_from(Self::LOCK_WARN_DURATION).unwrap()
                );
                acquiring_write_lock.await
            }
        };
        tracing::trace!(
            "wallet: acquired write lock successfully in {:?}",
            start.elapsed().unwrap_or_default()
        );
        RwLockWriteGuardSome::new(write_guard).ok_or(error::NotUnlocked)
    }

    fn read_db_mnemonic(
        connection: &Connection,
    ) -> Result<Option<Either<Mnemonic, EncryptedMnemonic>>, error::ReadDbMnemonic> {
        let mut statement = connection
            .prepare(
                "SELECT plaintext_mnemonic, initialization_vector, 
                            ciphertext_mnemonic, key_salt FROM wallet_seeds",
            )
            .map_err(error::ReadDbMnemonicInner::Rusqlite)?;

        let statement_result = statement.query_row([], |row| {
            let plaintext_mnemonic: Option<String> = row.get("plaintext_mnemonic")?;
            let iv: Option<Vec<u8>> = row.get("initialization_vector")?;
            let ciphertext: Option<Vec<u8>> = row.get("ciphertext_mnemonic")?;
            let key_salt: Option<Vec<u8>> = row.get("key_salt")?;
            Ok((plaintext_mnemonic, iv, ciphertext, key_salt))
        });
        let res = match statement_result {
            Ok(row) => row,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(err) => return Err(error::ReadDbMnemonicInner::ReadMnemonic(err).into()),
        };

        match res {
            (Some(plaintext_mnemonic), None, None, None) => {
                let mnemonic =
                    Mnemonic::parse_in_normalized(Language::English, plaintext_mnemonic.as_str())
                        .map_err(error::ParseMnemonic::from)?;

                Ok(Some(Either::Left(mnemonic)))
            }
            (None, Some(iv), Some(ciphertext), Some(key_salt)) => {
                Ok(Some(Either::Right(EncryptedMnemonic {
                    initialization_vector: iv,
                    ciphertext_mnemonic: ciphertext,
                    key_salt,
                })))
            }

            // This is a sanity check, and should never really happen.
            // Don't print out the actual contents, just indicate which values are set/not set.
            (plaintext_mnemonic, iv, ciphertext, key_salt) => {
                Err(error::ReadDbMnemonicInner::InvalidDbState {
                    plaintext_mnemonic_is_some: plaintext_mnemonic.is_some(),
                    iv_is_some: iv.is_some(),
                    ciphertext_is_some: ciphertext.is_some(),
                    key_salt_is_some: key_salt.is_some(),
                }
                .into())
            }
        }
    }

    pub async fn create_new_wallet(
        &self,
        mnemonic: Option<Mnemonic>,
        password: Option<&str>,
    ) -> Result<(), error::CreateNewWallet> {
        let connection = self.self_db.lock().await;
        if WalletInner::read_db_mnemonic(&connection)?.is_some() {
            return Err(WalletInitialization::AlreadyExists.into());
        }
        drop(connection);

        let mnemonic = match mnemonic {
            Some(mnemonic) => mnemonic,
            None => {
                tracing::info!("create new wallet: no mnemonic provided, generating fresh");
                new_mnemonic()?
            }
        };

        match password {
            // Encrypt the mnemonic and insert
            Some(password) => {
                tracing::info!("create new wallet: persisting encrypted mnemonic");
                let encrypted = EncryptedMnemonic::encrypt(&mnemonic, password)?;

                // Satisfy clippy with a single function call per lock
                let with_connection = |connection: &Connection| -> Result<_, rusqlite::Error> {
                    let mut statement = connection.prepare(
                        "INSERT INTO wallet_seeds (initialization_vector, 
                            ciphertext_mnemonic, key_salt) VALUES (?, ?, ?)",
                    )?;

                    statement.execute((
                        encrypted.initialization_vector,
                        encrypted.ciphertext_mnemonic,
                        encrypted.key_salt,
                    ))?;

                    Ok(())
                };

                let connection = self.self_db.lock().await;
                with_connection(&connection)?
            }
            None => {
                tracing::info!(
                    "create new wallet: no password provided, persisting plaintext mnemonic"
                );

                // Satisfy clippy with a single function call per lock
                let with_connection = |connection: &Connection| -> Result<_, rusqlite::Error> {
                    let mut statement = connection
                        .prepare("INSERT INTO wallet_seeds (plaintext_mnemonic) VALUES (?)")?;

                    statement.execute([mnemonic.to_string()])?;
                    Ok(())
                };

                let connection = self.self_db.lock().await;
                with_connection(&connection)?
            }
        }

        let mut database = self.bdk_db.lock().await;
        let network = self.validator.network();
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
        let connection = self.self_db.lock().await;
        let read = WalletInner::read_db_mnemonic(&connection)?;
        drop(connection);

        tracing::debug!("unlock wallet: read from DB");

        // Verify that it is encrypted!
        let encrypted = match read {
            None => {
                return Err(WalletInitialization::NotFound(error::NotFound).into());
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
        let network = self.validator.network();

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

    // Gets wiped upon generating a new block.
    async fn delete_bundle_proposals<I>(&self, iter: I) -> Result<(), rusqlite::Error>
    where
        I: IntoIterator<Item = (SidechainNumber, M6id)>,
    {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<usize, rusqlite::Error> {
            let mut total_deleted = 0;
            for (sidechain_number, m6id) in iter {
                let deleted = connection.execute(
                    "DELETE FROM bundle_proposals where sidechain_number = ?1 AND bundle_hash = ?2;",
                    (sidechain_number.0, m6id.0.as_byte_array())
                )?;
                total_deleted += deleted;
            }
            Ok(total_deleted)
        };
        let total_deleted = {
            let connection = self.self_db.lock().await;
            with_connection(&connection)?
        };

        if total_deleted > 0 {
            tracing::debug!(
                "deleted {} bundle proposal(s) from SQLite DB",
                total_deleted
            );
        }
        Ok(())
    }

    // Gets wiped upon generating a new block.
    async fn delete_pending_sidechain_proposals<I>(
        &self,
        proposals: I,
    ) -> Result<(), rusqlite::Error>
    where
        I: IntoIterator<Item = SidechainProposalId>,
    {
        let with_connection = |connection: &Connection| -> Result<usize, rusqlite::Error> {
            let mut total_deleted = 0;
            for proposal_id in proposals {
                let deleted = connection.execute(
                    "DELETE FROM sidechain_proposals where sidechain_number = ?1 AND data_hash = ?2;",
                    (proposal_id.sidechain_number.0, proposal_id.description_hash.as_byte_array())
                )?;
                total_deleted += deleted;
            }
            Ok(total_deleted)
        };
        let connection = self.self_db.lock().await;
        let total_deleted = with_connection(&connection)?;
        drop(connection);

        if total_deleted > 0 {
            tracing::debug!(
                "deleted {} pending sidechain proposal(s) from SQLite DB",
                total_deleted
            );
        }
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
    pub async fn new(
        data_dir: &Path,
        config: &Config,
        main_client: HttpClient,
        validator: Validator,
        magic: bitcoin::p2p::Magic,
        signet_challenge: Option<bitcoin::ScriptBuf>,
    ) -> Result<Self, error::InitWallet> {
        let inner = Arc::new(
            WalletInner::new(
                data_dir,
                config,
                main_client,
                validator,
                magic,
                signet_challenge,
            )
            .await?,
        );
        Ok(Self { inner })
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

    pub async fn full_scan(&self) -> miette::Result<BlockHash, error::FullScan> {
        self.inner.full_scan().await
    }

    pub async fn is_initialized(&self) -> bool {
        self.inner.bitcoin_wallet.read().await.is_some()
    }

    pub fn validator(&self) -> &Validator {
        &self.inner.validator
    }

    pub fn generate_blocks_semaphore(&self) -> &Arc<tokio::sync::Semaphore> {
        &self.inner.generate_blocks_semaphore
    }

    /// Returns pending sidechain proposals from the wallet. These are not yet
    /// active on the chain, and not possible to vote on.
    async fn get_our_sidechain_proposals(&self) -> Result<Vec<SidechainProposal>, rusqlite::Error> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, rusqlite::Error> {
            let mut statement =
                connection.prepare("SELECT sidechain_number, data FROM sidechain_proposals")?;

            let proposals = statement
                .query_map([], |row| {
                    let data: Vec<u8> = row.get(1)?;
                    let sidechain_number: u8 = row.get::<_, u8>(0)?;
                    Ok(SidechainProposal {
                        sidechain_number: sidechain_number.into(),
                        description: data.into(),
                    })
                })?
                .collect::<Result<_, _>>()?;

            Ok(proposals)
        };
        let connection = self.inner.self_db.lock().await;
        with_connection(&connection)
    }

    async fn get_sidechain_acks(&self) -> Result<Vec<SidechainAck>, rusqlite::Error> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, _> {
            let mut statement =
                connection.prepare("SELECT number, data_hash FROM sidechain_acks")?;
            let rows = statement
                .query_map([], |row| {
                    let description_hash: [u8; 32] = row.get(1)?;
                    Ok(SidechainAck {
                        sidechain_number: SidechainNumber(row.get(0)?),
                        description_hash: sha256d::Hash::from_byte_array(description_hash),
                    })
                })?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        };
        let connection = self.inner.self_db.lock().await;
        with_connection(&connection)
    }

    async fn get_bundle_proposals(
        &self,
    ) -> Result<HashMap<SidechainNumber, BundleProposals>, error::GetBundleProposals> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, error::GetBundleProposals> {
            let mut statement = connection
                .prepare("SELECT sidechain_number, bundle_hash, bundle_tx FROM bundle_proposals")?;
            let mut bundle_proposals = HashMap::<_, Vec<_>>::new();
            let () = statement
                .query_map([], |row| {
                    let sidechain_number = SidechainNumber(row.get(0)?);
                    let m6id_bytes: [u8; 32] = row.get(1)?;
                    let m6id = M6id::from(m6id_bytes);
                    let bundle_tx_bytes: Vec<u8> = row.get(2)?;
                    Ok((sidechain_number, m6id, bundle_tx_bytes))
                })?
                .transpose_into_fallible()
                .map_err(error::GetBundleProposals::from)
                .for_each(|(sidechain_number, m6id, bundle_tx_bytes)| {
                    let bundle_proposal_tx = BlindedM6::deserialize(&bundle_tx_bytes)?;
                    bundle_proposals
                        .entry(sidechain_number)
                        .or_default()
                        .push((m6id, bundle_proposal_tx));
                    Ok(())
                })?;
            Ok(bundle_proposals)
        };
        let bundle_proposals = {
            let connection = self.inner.self_db.lock().await;
            with_connection(&connection)?
        };
        // Per BIP300 M3, a bundle proposed for a sidechain slot that is not active
        // is interpreted as an ordinary script (a no-op), so there is nothing to
        // gain by proposing it.
        //
        // Bundles for an inactive sidechain can sneak in if we're activating a sidechain
        // and then reorging it out of existence. The wallet currently doesn't handle reorgs
        // properly, so we would then end up with a bundle proposal for an inactive sidechain
        // in our DB.
        let active_sidechain_numbers: HashSet<SidechainNumber> = self
            .inner
            .validator
            .get_active_sidechains()?
            .into_iter()
            .map(|sidechain| sidechain.proposal.sidechain_number)
            .collect();
        // Filter out proposals that have already been created
        let res = bundle_proposals
            .into_iter()
            .map(Ok::<_, error::GetBundleProposals>)
            .transpose_into_fallible()
            .filter_map(|(sidechain_id, m6ids)| {
                if !active_sidechain_numbers.contains(&sidechain_id) {
                    return Ok(None);
                }
                let pending_m6ids = self
                    .inner
                    .validator
                    .get_pending_withdrawals(&sidechain_id)?;
                let res: Vec<_> = m6ids
                    .into_iter()
                    .map(|(m6id, blinded_m6)| (m6id, blinded_m6, pending_m6ids.get(&m6id).copied()))
                    .collect();
                if res.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some((sidechain_id, res)))
                }
            })
            .collect()?;
        Ok(res)
    }

    /// Fetches sidechain proposals from the validator. Returns proposals that
    /// are already included into a block, and possible to vote on.
    fn get_active_sidechain_proposals(
        &self,
    ) -> Result<HashMap<SidechainNumber, SidechainProposal>, crate::validator::GetSidechainsError>
    {
        let pending_proposals = self
            .inner
            .validator
            .get_sidechains()?
            .into_iter()
            .map(|(_, sidechain)| (sidechain.proposal.sidechain_number, sidechain.proposal))
            .collect();
        Ok(pending_proposals)
    }

    pub async fn ack_sidechain(
        &self,
        sidechain_number: SidechainNumber,
        data_hash: sha256d::Hash,
    ) -> Result<(), rusqlite::Error> {
        let sidechain_number: u8 = sidechain_number.into();
        let data_hash: &[u8; 32] = data_hash.as_byte_array();
        let connection = self.inner.self_db.lock().await;
        connection.execute(
            "INSERT INTO sidechain_acks (number, data_hash) VALUES (?1, ?2)",
            (sidechain_number, data_hash),
        )?;
        drop(connection);
        Ok(())
    }

    fn validate_sidechain_ack(
        &self,
        ack: &SidechainAck,
        pending_proposals: &HashMap<SidechainNumber, SidechainProposal>,
    ) -> bool {
        let Some(sidechain_proposal) = pending_proposals.get(&ack.sidechain_number) else {
            tracing::error!(
                "Handle sidechain ACK: could not find proposal: {}",
                ack.sidechain_number
            );
            return false;
        };
        let description_hash = sidechain_proposal.description.sha256d_hash();
        if description_hash == ack.description_hash {
            true
        } else {
            tracing::error!(
                "Handle sidechain ACK: invalid actual hash vs. ACK hash: {} != {}",
                description_hash,
                ack.description_hash,
            );
            false
        }
    }

    async fn delete_sidechain_ack(&self, ack: &SidechainAck) -> Result<(), rusqlite::Error> {
        let connection = self.inner.self_db.lock().await;
        connection.execute(
            "DELETE FROM sidechain_acks WHERE number = ?1 AND data_hash = ?2",
            (ack.sidechain_number.0, ack.description_hash.as_byte_array()),
        )?;
        drop(connection);
        Ok(())
    }

    /// Get BMM requests with the specified previous blockhash.
    /// Returns pairs of sidechain numbers and side blockhash.
    async fn get_bmm_requests(
        &self,
        prev_blockhash: &bitcoin::BlockHash,
    ) -> Result<Vec<(SidechainNumber, BmmCommitment)>, rusqlite::Error> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, _> {
            let mut statement = connection
                .prepare(
                    "SELECT sidechain_number, side_block_hash FROM bmm_requests WHERE prev_block_hash = ?"
                )?;

            let queried = statement
                .query_map([prev_blockhash.as_byte_array()], |row| {
                    let sidechain_number = SidechainNumber(row.get(0)?);
                    let side_blockhash = BmmCommitment(row.get(1)?);
                    Ok((sidechain_number, side_blockhash))
                })?
                .collect::<Result<_, _>>()?;

            Ok(queried)
        };
        let connection = self.inner.self_db.lock().await;
        with_connection(&connection)
    }

    /// Consume the BMM requests for `prev_blockhash` when a block is generated
    /// on top of it. The deleted rows are first snapshotted into
    /// `bmm_requests_undo`, keyed by the mined `block_hash`, so they can be
    /// restored (see `restore_bmm_requests`) if that block is later
    /// disconnected by a reorg — otherwise the operator would have to re-issue
    /// `create_bmm_request` for the new mainchain tip.
    async fn delete_bmm_requests(
        &self,
        prev_blockhash: &bitcoin::BlockHash,
        block_hash: &bitcoin::BlockHash,
    ) -> Result<(), rusqlite::Error> {
        let mut connection = self.inner.self_db.lock().await;
        snapshot_and_delete_bmm_requests(&mut connection, prev_blockhash, block_hash)
    }

    /// Restore the BMM requests that were consumed when `block_hash` was
    /// generated, moving them back out of `bmm_requests_undo`. Called when
    /// `block_hash` is disconnected by a reorg, so the operator's queued BMM
    /// requests can be re-emitted against the new mainchain tip.
    async fn restore_bmm_requests(
        &self,
        block_hash: &bitcoin::BlockHash,
    ) -> Result<(), rusqlite::Error> {
        let restored = {
            let mut connection = self.inner.self_db.lock().await;
            restore_bmm_requests_from_undo(&mut connection, block_hash)?
        };
        if restored > 0 {
            tracing::info!(
                %block_hash,
                "restored {restored} BMM request(s) from disconnected block",
            );
        }
        Ok(())
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

    fn p2p_broadcast_addrs(&self) -> Box<dyn Iterator<Item = std::net::SocketAddr> + '_> {
        let network = self.inner.validator.network();
        let magic = self.inner.magic;
        let p2p_broadcast_addrs = self.inner.config.p2p_broadcast_addr.iter().copied();
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
            .validator
            .try_get_block_height()?
            .unwrap_or_default();
        // If this is None, there's been no deposit to this sidechain yet. We're the first one!
        let sidechain_ctip = self.inner.validator.try_get_ctip(sidechain_number)?;
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
                    peer_addr,
                    block_height as i32,
                    self.inner.magic,
                    tx.clone(),
                )
                .map_ok(move |result| (peer_addr, result))
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
            let mut bdk_db_lock = self.inner.bdk_db.lock().await;
            let last_seen = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            let applied_changes = self
                .inner
                .write_wallet()
                .await?
                .with_mut(|wallet| {
                    wallet.apply_unconfirmed_txs(vec![(tx, last_seen.as_secs())]);
                    wallet.persist_async(&mut bdk_db_lock)
                })
                .await?;
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

    async fn create_send_psbt(
        &self,
        destinations: HashMap<bitcoin::Address, Amount>,
        params: CreateTransactionParams,
    ) -> Result<bdk_wallet::bitcoin::psbt::Psbt, error::CreateSendPsbt> {
        self.create_send_psbt_with_foreign(destinations, params, &[])
            .await
    }

    async fn create_send_psbt_with_foreign(
        &self,
        destinations: HashMap<bitcoin::Address, Amount>,
        params: CreateTransactionParams,
        foreign: &[(
            bdk_wallet::bitcoin::OutPoint,
            bdk_wallet::bitcoin::Transaction,
        )],
    ) -> Result<bdk_wallet::bitcoin::psbt::Psbt, error::CreateSendPsbt> {
        let mut timestamp = Instant::now();
        let psbt = {
            let mut wallet_write = self.inner.write_wallet().await?;
            tokio::task::block_in_place(|| {
                wallet_write.with_mut(|wallet| {
                    let mut builder = wallet.build_tx();

                    if let Some(op_return_message) = params.op_return_message {
                        let op_return_output = Self::create_op_return_output(op_return_message)?;
                        builder
                            .add_recipient(op_return_output.script_pubkey, op_return_output.value);

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

                    for (outpoint, funding_tx) in foreign {
                        let txout = funding_tx
                            .output
                            .get(outpoint.vout as usize)
                            .cloned()
                            .ok_or_else(|| {
                                error::CreateSendPsbt::ForeignUtxo(format!(
                                    "funding tx for {outpoint} has no output {}",
                                    outpoint.vout
                                ))
                            })?;
                        let weight = reusable_payments::spend::satisfaction_weight(
                            txout.script_pubkey.as_script(),
                        );
                        // Legacy (P2PKH) foreign inputs require the full prev tx;
                        // segwit/taproot use witness_utxo. Provide both.
                        let input = bdk_wallet::bitcoin::psbt::Input {
                            witness_utxo: Some(txout),
                            non_witness_utxo: Some(funding_tx.clone()),
                            ..Default::default()
                        };
                        builder
                            .add_foreign_utxo(*outpoint, input, weight)
                            .map_err(|err| error::CreateSendPsbt::ForeignUtxo(err.to_string()))?;
                    }

                    let foreign_set: std::collections::HashSet<_> =
                        foreign.iter().map(|(op, _)| *op).collect();
                    let bdk_required: Vec<_> = params
                        .required_utxos
                        .iter()
                        .copied()
                        .filter(|op| !foreign_set.contains(op))
                        .collect();

                    if !bdk_required.is_empty() {
                        builder
                            // TODO: this does not work at all for wallets past a certain scale....
                            // 25s pr. UTXO for a wallet with 40k UTXOs in total
                            .add_utxos(&bdk_required)
                            .map_err(|err| match err {
                                bdk_wallet::tx_builder::AddUtxoError::UnknownUtxo(outpoint) => {
                                    error::CreateSendPsbt::UnknownUTXO(outpoint)
                                }
                            })?;
                    }

                    // The caller supplies a fee-covering input set, so restrict
                    // selection to exactly the pinned inputs (BDK only adds change).
                    if !foreign.is_empty() || !bdk_required.is_empty() {
                        builder.manually_selected_only();
                    }

                    if !bdk_required.is_empty() || !foreign.is_empty() {
                        tracing::debug!(
                            "Added {} required + {} foreign UTXOs in {:?}",
                            bdk_required.len(),
                            foreign.len(),
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
                            tracing::debug!(
                                "Finished transaction builder in {:?}",
                                timestamp.elapsed()
                            );
                        })
                        .map_err(error::CreateSendPsbt::CreateTx)
                })
            })?
        };

        Ok(psbt)
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

        // Recover keys for any reusable-payments outputs the caller pinned in
        // `required_utxos`; these are added as foreign inputs we sign ourselves.
        let mut reusable_keyed: Vec<(
            reusable_payments::ReusableOwnedOutput,
            bitcoin::secp256k1::SecretKey,
        )> = Vec::new();
        if !params.required_utxos.is_empty() {
            for owned in self.inner.read_reusable_owned_outputs().await? {
                if params.required_utxos.contains(&owned.outpoint) {
                    let key = self.inner.recover_reusable_spend_key(&owned).await?;
                    reusable_keyed.push((owned, key));
                }
            }
        }
        let mut foreign: Vec<(
            bdk_wallet::bitcoin::OutPoint,
            bdk_wallet::bitcoin::Transaction,
        )> = Vec::with_capacity(reusable_keyed.len());
        for (owned, _) in &reusable_keyed {
            let funding_tx = self.fetch_transaction(owned.outpoint.txid).await?;
            foreign.push((owned.outpoint, funding_tx));
        }

        let mut psbt = self
            .create_send_psbt_with_foreign(destinations, params, &foreign)
            .await?;

        tracing::debug!("Created send PSBT in {:?}", timestamp.elapsed());
        timestamp = Instant::now();

        let keys_by_outpoint: Vec<(bitcoin::OutPoint, bitcoin::secp256k1::SecretKey)> =
            reusable_keyed
                .iter()
                .map(|(o, k)| (o.outpoint, *k))
                .collect();
        let secp = bitcoin::secp256k1::Secp256k1::new();
        reusable_payments::spend::sign_psbt_inputs(&mut psbt, &keys_by_outpoint, &secp)?;

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
        let mut bdk_db_lock = self.inner.bdk_db.lock().await;

        let last_seen = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();

        let applied_changes = self
            .inner
            .write_wallet()
            .await?
            .with_mut(|wallet| {
                wallet.apply_unconfirmed_txs(vec![(tx, last_seen.as_secs())]);
                wallet.persist_async(&mut bdk_db_lock)
            })
            .await?;

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

        let bitcoin_txid = convert::bdk_txid_to_bitcoin_txid(txid);
        for (owned, _) in &reusable_keyed {
            self.inner
                .mark_reusable_spent(owned.outpoint, bitcoin_txid)
                .await?;
        }

        Ok(bitcoin_txid)
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

    /// Persists a sidechain proposal into our database.
    /// On regtest: picked up by the next block generation.
    /// On signet: TBD, but needs some way of getting communicated to the miner.
    pub async fn propose_sidechain(
        &self,
        proposal: &SidechainProposal,
    ) -> Result<(), rusqlite::Error> {
        let sidechain_number: u8 = proposal.sidechain_number.into();
        self.inner.self_db.lock().await.execute(
            "INSERT INTO sidechain_proposals (sidechain_number, data_hash, data) VALUES (?1, ?2, ?3)",
            (sidechain_number, proposal.description.sha256d_hash().to_byte_array(), &proposal.description.0),
        )?;
        Ok(())
    }

    pub async fn nack_sidechain(
        &self,
        sidechain_number: u8,
        data_hash: &[u8; 32],
    ) -> Result<(), rusqlite::Error> {
        self.inner.self_db.lock().await.execute(
            "DELETE FROM sidechain_acks WHERE number = ?1 AND data_hash = ?2",
            (sidechain_number, data_hash),
        )?;
        Ok(())
    }

    #[expect(dead_code)]
    async fn get_sidechain_ctip(
        &self,
        sidechain_number: SidechainNumber,
    ) -> Result<Option<(bitcoin::OutPoint, Amount, u64)>, miette::Report> {
        let ctip = self.inner.validator.try_get_ctip(sidechain_number)?;

        let sequence_number = self
            .inner
            .validator
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
        let sidechains = self.inner.validator.get_active_sidechains()?;
        let active = sidechains
            .iter()
            .any(|sc| sc.proposal.sidechain_number == sidechain_number);

        Ok(active)
    }

    #[instrument(skip_all, err)]
    async fn sign_transaction(
        &self,
        mut psbt: bdk_wallet::bitcoin::psbt::Psbt,
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

        let tx = psbt
            .extract_tx()
            .map_err(error::WalletSignTransaction::ExtractTx)?;

        tracing::debug!("Extracted transaction in {:?}", timestamp.elapsed());
        Ok(tx)
    }

    fn bmm_request_message(
        sidechain_number: SidechainNumber,
        prev_mainchain_block_hash: bdk_wallet::bitcoin::BlockHash,
        sidechain_block_hash: BmmCommitment,
    ) -> Result<bdk_wallet::bitcoin::ScriptBuf, bitcoin::script::PushBytesError> {
        M8BmmRequest::script_pubkey(
            sidechain_number,
            sidechain_block_hash,
            prev_mainchain_block_hash,
        )
    }

    async fn build_bmm_tx(
        &self,
        sidechain_number: SidechainNumber,
        prev_mainchain_block_hash: bdk_wallet::bitcoin::BlockHash,
        sidechain_block_hash: BmmCommitment,
        bid_amount: bdk_wallet::bitcoin::Amount,
        locktime: bdk_wallet::bitcoin::absolute::LockTime,
    ) -> Result<bdk_wallet::bitcoin::psbt::Psbt, error::BuildBmmTx> {
        tracing::trace!("build_bmm_tx: constructing request message");
        // https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip301.md#m8-bmm-request
        let message = Self::bmm_request_message(
            sidechain_number,
            prev_mainchain_block_hash,
            sidechain_block_hash,
        )?;

        let psbt = {
            tracing::trace!("build_bmm_tx: acquiring wallet write lock");
            let mut wallet_write = self.inner.write_wallet().await?;
            tokio::task::block_in_place(|| {
                wallet_write.with_mut(|wallet| {
                    tracing::trace!("build_bmm_tx: creating transaction builder");
                    let mut builder = wallet.build_tx();
                    // OP_RETURN message MUST be first output
                    builder.ordering(bdk_wallet::TxOrdering::Untouched);

                    tracing::trace!("build_bmm_tx: adding locktime {locktime}");
                    builder.nlocktime(locktime);

                    tracing::trace!("build_bmm_tx: adding recipient");
                    builder.add_recipient(message, bid_amount);

                    tracing::trace!("build_bmm_tx: finishing transaction builder");
                    let res = builder.finish();

                    tracing::trace!("build_bmm_tx: built transaction");

                    res
                })
            })?
        };

        Ok(psbt)
    }

    /// Returns `true` if a BMM request was inserted, `false` if a BMM request
    /// already exists for that sidechain and previous blockhash
    async fn insert_new_bmm_request(
        &self,
        sidechain_number: SidechainNumber,
        prev_blockhash: bdk_wallet::bitcoin::BlockHash,
        side_block_hash: BmmCommitment,
    ) -> Result<bool, rusqlite::Error> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<bool, rusqlite::Error> {
            connection
                .prepare(
                    "INSERT OR ABORT INTO bmm_requests (sidechain_number, prev_block_hash, side_block_hash) VALUES (?1, ?2, ?3)",
                )?
                .execute((
                    u8::from(sidechain_number),
                    prev_blockhash.to_byte_array(),
                    side_block_hash.0,
                ))
                .map_or_else(
                    |err| if err.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
                        Ok(false)
                    } else {
                        Err(err)
                    },
                    |_| Ok(true)
                )
        };
        let connection = self.inner.self_db.lock().await;
        with_connection(&connection)
    }

    /// Creates a BMM request transaction. Broadcasts via p2p whitelist only.
    /// Returns `Some(tx)` if the BMM request was stored, `None` if the BMM
    /// request was not stored due to pre-existing request with the same
    /// `sidechain_number` and `prev_mainchain_block_hash`.
    pub async fn create_bmm_request(
        &self,
        sidechain_number: SidechainNumber,
        prev_mainchain_block_hash: bdk_wallet::bitcoin::BlockHash,
        sidechain_block_hash: BmmCommitment,
        bid_amount: bdk_wallet::bitcoin::Amount,
        locktime: bdk_wallet::bitcoin::absolute::LockTime,
    ) -> Result<Option<bdk_wallet::bitcoin::Transaction>, error::CreateBmmRequest> {
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
        let tx = self.sign_transaction(psbt).await?;
        tracing::info!("BMM request: PSBT signed successfully");
        let txid = tx.compute_txid();
        let stored = self
            .insert_new_bmm_request(
                sidechain_number,
                prev_mainchain_block_hash,
                sidechain_block_hash,
            )
            .await?;
        if stored {
            tracing::info!("BMM request: inserted new bmm request into db");
        } else {
            tracing::warn!(
                "BMM request: Ignored, request exists with same sidechain slot and previous block hash"
            );
        }
        let block_height = self
            .inner
            .validator
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
        if stored { Ok(Some(tx)) } else { Ok(None) }
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

    pub async fn put_withdrawal_bundle(
        &self,
        sidechain_number: SidechainNumber,
        blinded_m6: &BlindedM6<'static>,
    ) -> Result<M6id, rusqlite::Error> {
        let m6id = blinded_m6.compute_m6id();
        // Always encode with rust-bitcoin. A zero-input bundle round-trips
        // because `BlindedM6::deserialize` reads this encoding back, and a
        // finalized M6 has a treasury input anyway.
        let tx_bytes = blinded_m6.serialize();
        self.inner.self_db
            .lock()
            .await
            .execute(
                "INSERT OR IGNORE INTO bundle_proposals (sidechain_number, bundle_hash, bundle_tx) VALUES (?1, ?2, ?3)",
                (sidechain_number.0, m6id.0.as_byte_array(), tx_bytes),
            )?;
        Ok(m6id)
    }

    /// Connect missing blocks to the BDK chain. Retries if we get a 'nested'
    /// alert from BDK, about further missing ancestors.
    async fn connect_missing_block(
        &mut self,
        try_include_height: u32,
    ) -> std::result::Result<(), error::ConnectMissingBlock> {
        use bitcoin_jsonrpsee::{
            MainClient as _,
            client::{GetBlockClient as _, U8Witness},
        };

        struct TryInclude {
            block_height: u32,
            block: Option<bitcoin::Block>,
        }

        // stack of block heights / blocks to connect
        let mut try_includes = vec![TryInclude {
            block_height: try_include_height,
            block: None,
        }];

        while let Some(try_include) = try_includes.last_mut() {
            let TryInclude {
                block_height,
                block,
            } = try_include;
            let block = match block {
                Some(block) => block,
                None => {
                    let block_hash = self
                        .inner
                        .main_client
                        .getblockhash(try_include_height as usize)
                        .await
                        .map_err(|err| {
                            error::ConnectMissingBlockInner::GetBlockHash(error::BitcoinCoreRPC {
                                method: "getblockhash".to_string(),
                                error: err,
                            })
                        })?;
                    block.insert(
                        self.inner
                            .main_client
                            .get_block(block_hash, U8Witness::<0>)
                            .await
                            .map_err(|err| {
                                error::ConnectMissingBlockInner::GetBlock(error::BitcoinCoreRPC {
                                    method: "getblock".to_string(),
                                    error: err,
                                })
                            })?
                            .0,
                    )
                }
            };
            let block_hash = block.block_hash();
            let infos = self.inner.validator.get_block_infos(&block_hash, 0)?;
            assert_eq!(infos.len(), 1);
            let (_header_info, block_info) = infos.head;
            tracing::debug!(
                "connecting missing block {} at height {}",
                block_hash,
                block_height,
            );
            match self
                .inner
                .handle_connect_block(block, *block_height, &block_info)
                .await?
            {
                Ok(()) => {
                    tracing::debug!(
                        "connected missing block {} at height {}",
                        block_hash,
                        block_height
                    );
                    try_includes.pop();
                }
                // We can receive 'nested' alerts from BDK, about further missing ancestors. We therefore
                // recurse, but make sure to only do so if the recommended try_include_height is /below/
                // what we just tried. Otherwise we'll just loop forever.
                Err(
                    err @ bdk_wallet::chain::local_chain::CannotConnectError { try_include_height },
                ) => {
                    if try_include_height < *block_height {
                        tracing::debug!(
                            "adding missing block at height {} to stack",
                            try_include_height
                        );
                        try_includes.push(TryInclude {
                            block_height: try_include_height,
                            block: None,
                        });
                    } else {
                        return Err(error::ConnectMissingBlockInner::BdkConnect {
                            block_height: *block_height,
                            source: err,
                        }
                        .into());
                    }
                }
            };
        }

        Ok(())
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

    pub async fn bip47_payment_code(
        &self,
        version: reusable_payments::Bip47Version,
    ) -> Result<reusable_payments::PaymentCode, error::ReusablePayments> {
        self.inner.ensure_scanner_activated().await?;
        self.inner.bip47_payment_code_inner(version).await
    }

    pub async fn silent_payment_address(
        &self,
        label: Option<u32>,
    ) -> Result<reusable_payments::SilentPaymentAddress, error::ReusablePayments> {
        self.inner.ensure_scanner_activated().await?;
        self.inner.silent_payment_address_inner(label).await
    }

    pub async fn create_silent_payment_label(
        &self,
        name: &str,
    ) -> Result<u32, error::ReusablePayments> {
        self.inner.ensure_wallet_unlocked().await?;
        self.inner.ensure_scanner_activated().await?;
        let connection = self.inner.self_db.lock().await;
        let next_m: i64 = connection
            .query_row(
                "SELECT COALESCE(MAX(m), 0) + 1 FROM silent_payment_labels WHERE m >= 1",
                [],
                |row| row.get(0),
            )
            .map_err(error::ReusablePayments::db)?;
        let m: u32 = u32::try_from(next_m).unwrap_or(1);
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        connection
            .execute(
                "INSERT INTO silent_payment_labels (m, name, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![m, name, now_unix],
            )
            .map_err(error::ReusablePayments::db)?;
        drop(connection);

        if let Some(cursor) = self.inner.load_scan_cursor_if_present().await? {
            self.rescan_reusable_payments(cursor.birthday_height)
                .await?;
        }

        Ok(m)
    }

    pub async fn list_silent_payment_labels(
        &self,
    ) -> Result<Vec<(u32, String)>, error::ReusablePayments> {
        self.inner.ensure_wallet_unlocked().await?;
        let connection = self.inner.self_db.lock().await;
        let mut stmt = connection
            .prepare("SELECT m, name FROM silent_payment_labels ORDER BY m ASC")
            .map_err(error::ReusablePayments::db)?;
        let rows_result: Result<Vec<(u32, String)>, _> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
            })
            .and_then(|iter| iter.collect());
        drop(stmt);
        drop(connection);
        let out = rows_result.map_err(error::ReusablePayments::db)?;
        Ok(out)
    }

    pub async fn list_bip47_inbound_payers(
        &self,
    ) -> Result<Vec<reusable_payments::Bip47InboundPayer>, error::ReusablePayments> {
        self.inner.ensure_wallet_unlocked().await?;
        let connection = self.inner.self_db.lock().await;
        let mut stmt = connection
            .prepare(
                "SELECT p.sender_payment_code, p.next_receive_index, p.first_seen_unix, \
                        COALESCE(SUM(r.amount_sats), 0) \
                 FROM bip47_inbound_payers p \
                 LEFT JOIN bip47_received r \
                     ON r.sender_payment_code = p.sender_payment_code \
                 GROUP BY p.sender_payment_code \
                 ORDER BY p.first_seen_unix ASC",
            )
            .map_err(error::ReusablePayments::db)?;
        let rows_result: Result<Vec<(String, u32, i64, i64)>, _> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .and_then(|iter| iter.collect());
        drop(stmt);
        drop(connection);
        let raw_rows = rows_result.map_err(error::ReusablePayments::db)?;
        let mut out = Vec::new();
        for (code_str, next_idx, first_seen, total_sats) in raw_rows {
            let sender = std::str::FromStr::from_str(&code_str)?;
            out.push(reusable_payments::Bip47InboundPayer {
                sender_payment_code: sender,
                next_receive_index: next_idx,
                total_received_sats: total_sats.max(0) as u64,
                first_seen_unix: first_seen,
            });
        }
        Ok(out)
    }

    pub async fn list_silent_payment_receives(
        &self,
        min_confirmations: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Vec<reusable_payments::SilentPaymentReceive>, u32), error::ReusablePayments> {
        self.inner.ensure_wallet_unlocked().await?;
        let last_scanned_height = self
            .inner
            .load_scan_cursor_if_present()
            .await?
            .map(|c| c.last_scanned_height)
            .unwrap_or(0);
        let connection = self.inner.self_db.lock().await;

        let min_conf = min_confirmations.unwrap_or(0);
        let max_height_for_filter = last_scanned_height
            .saturating_sub(min_conf)
            .saturating_add(1);
        let limit_val = limit.unwrap_or(1000).min(10_000) as i64;

        let mut stmt = connection
            .prepare(
                "SELECT r.txid, r.vout, r.output_pubkey, r.amount_sats, r.tweak_k, \
                        r.label_m, l.name, r.height, r.spent_in_txid \
                 FROM silent_payment_received r \
                 LEFT JOIN silent_payment_labels l ON l.m = r.label_m \
                 WHERE r.height < ?1 \
                 ORDER BY r.height ASC, r.vout ASC \
                 LIMIT ?2",
            )
            .map_err(error::ReusablePayments::db)?;
        type RawReceiveRow = (
            Vec<u8>,
            u32,
            Vec<u8>,
            i64,
            u32,
            Option<u32>,
            Option<String>,
            u32,
            Option<Vec<u8>>,
        );
        let rows_result: Result<Vec<RawReceiveRow>, _> = stmt
            .query_map(rusqlite::params![max_height_for_filter, limit_val], |row| {
                let txid: Vec<u8> = row.get(0)?;
                let vout: u32 = row.get(1)?;
                let output_pk: Vec<u8> = row.get(2)?;
                let amount: i64 = row.get(3)?;
                let tweak_k: u32 = row.get(4)?;
                let label_m: Option<u32> = row.get(5)?;
                let label_name: Option<String> = row.get(6)?;
                let height: u32 = row.get(7)?;
                let spent_in: Option<Vec<u8>> = row.get(8)?;
                Ok((
                    txid, vout, output_pk, amount, tweak_k, label_m, label_name, height, spent_in,
                ))
            })
            .and_then(|iter| iter.collect());
        drop(stmt);
        drop(connection);
        let raw_rows = rows_result.map_err(error::ReusablePayments::db)?;

        let mut out = Vec::new();
        for (txid_b, vout, pk_b, amt, tk, lm, ln, height, spent_b) in raw_rows {
            let txid = bitcoin::Txid::from_slice(&txid_b).map_err(|_| {
                error::ReusablePayments::Bip47Parse(
                    reusable_payments::bip47::ParseError::BadLength { got: txid_b.len() },
                )
            })?;
            let output_xonly = bitcoin::XOnlyPublicKey::from_slice(&pk_b).map_err(|e| {
                error::ReusablePayments::SilentPaymentParse(
                    reusable_payments::silent_payments::ParseError::BadPubkey(e),
                )
            })?;
            let spent_txid = spent_b
                .as_ref()
                .and_then(|b| bitcoin::Txid::from_slice(b).ok());
            out.push(reusable_payments::SilentPaymentReceive {
                txid,
                vout,
                output_pubkey: output_xonly,
                amount: bitcoin::Amount::from_sat(amt as u64),
                tweak_k: tk,
                label_m: lm,
                label_name: ln,
                height,
                spent_in_txid: spent_txid,
            });
        }
        Ok((out, last_scanned_height))
    }

    pub async fn send_to_payment_code(
        &self,
        recipient: reusable_payments::PaymentCode,
        amount: bitcoin::Amount,
        fee_sat_per_vbyte: u64,
    ) -> Result<reusable_payments::Bip47SendResult, error::ReusablePayments> {
        use bitcoin::FeeRate;

        self.inner.ensure_scanner_activated().await?;
        let version = recipient.version();
        let recipient_code_str = recipient.to_string();

        let recipient_lock: Arc<tokio::sync::Semaphore> = {
            let mut locks = self.inner.bip47_send_locks.lock().await;
            locks
                .entry(recipient_code_str.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
                .clone()
        };
        let _send_guard = recipient_lock
            .acquire_owned()
            .await
            .expect("BIP47 send semaphore is never closed");

        let (mut existing_notif_txid, current_index) = self
            .inner
            .load_bip47_send_state(&recipient_code_str)
            .await?;

        if let Some(prev_notif) = existing_notif_txid {
            let still_live = self
                .inner
                .notification_in_chain_or_mempool(prev_notif)
                .await?;
            if !still_live {
                tracing::warn!(
                    txid = %prev_notif,
                    "BIP47 notification no longer in chain/mempool, re-notifying"
                );
                self.inner
                    .rollback_bip47_notification(&recipient_code_str)
                    .await?;
                existing_notif_txid = None;
            }
        }

        let fee_rate = FeeRate::from_sat_per_vb(fee_sat_per_vbyte)
            .ok_or_else(|| error::ReusablePayments::Rpc("invalid fee_sat_per_vbyte".to_string()))?;

        let notification_txid: Option<bitcoin::Txid> = if existing_notif_txid.is_none() {
            use bitcoin_jsonrpsee::client::MainClient;
            let tip_height: u32 = self
                .inner
                .main_client
                .getblockcount()
                .await
                .map(|h| h as u32)
                .map_err(|e| {
                    error::ReusablePayments::Rpc(format!("tip height for notification: {e}"))
                })?;
            let txid = self
                .broadcast_bip47_notification(&recipient, version, fee_rate)
                .await?;
            self.inner
                .persist_bip47_notification(&recipient_code_str, version, txid, tip_height)
                .await?;
            Some(txid)
        } else {
            None
        };

        let payment_addr = self
            .inner
            .compute_bip47_send_address(&recipient, current_index)
            .await?;

        self.inner
            .advance_bip47_next_index(&recipient_code_str, current_index + 1)
            .await?;

        let mut destinations = HashMap::new();
        destinations.insert(payment_addr, amount);
        let payment_result = self
            .send_wallet_transaction(
                destinations,
                CreateTransactionParams {
                    fee_policy: Some(crate::types::FeePolicy::Rate(fee_rate)),
                    op_return_message: None,
                    required_utxos: vec![],
                    drain_wallet_to: None,
                },
            )
            .await;

        let payment_txid = match payment_result {
            Ok(txid) => txid,
            Err(err) => {
                if let Err(rollback_err) = self
                    .inner
                    .rollback_bip47_index_advance(&recipient_code_str, current_index)
                    .await
                {
                    tracing::error!(
                        ?rollback_err,
                        "failed to roll back BIP47 next_send_index after payment broadcast error"
                    );
                }
                return Err(error::ReusablePayments::from(err));
            }
        };

        Ok(reusable_payments::Bip47SendResult {
            notification_txid,
            payment_txid,
            sender_index: current_index,
            version,
        })
    }

    async fn broadcast_bip47_notification(
        &self,
        recipient: &reusable_payments::PaymentCode,
        version: reusable_payments::bip47::Version,
        fee_rate: bitcoin::FeeRate,
    ) -> Result<bitcoin::Txid, error::ReusablePayments> {
        if version == reusable_payments::bip47::Version::V3 {
            return self
                .broadcast_bip47_notification_v3(recipient, fee_rate)
                .await;
        }

        let secp = bitcoin::secp256k1::Secp256k1::new();
        let network = self.inner.validator.network();
        let recipient_notif_addr =
            reusable_payments::bip47::notification_address(recipient, network, &secp)?;

        let payload_len: usize = reusable_payments::bip47::V1_PAYLOAD_LEN;
        let placeholder_payload = vec![0u8; payload_len];
        let placeholder_op_return =
            crate::messages::create_op_return_output(placeholder_payload.clone()).map_err(|e| {
                error::ReusablePayments::Rpc(format!("op_return placeholder build: {e}"))
            })?;

        let mut destinations = HashMap::new();
        destinations.insert(recipient_notif_addr, bitcoin::Amount::from_sat(546));
        let mut psbt = self
            .create_send_psbt(
                destinations,
                CreateTransactionParams {
                    fee_policy: Some(crate::types::FeePolicy::Rate(fee_rate)),
                    op_return_message: Some(placeholder_payload),
                    required_utxos: vec![],
                    drain_wallet_to: None,
                },
            )
            .await
            .map_err(error::ReusablePayments::from)?;

        let designated_idx = psbt
            .inputs
            .iter()
            .position(|i| {
                i.witness_utxo
                    .as_ref()
                    .map(|tx_out| {
                        reusable_payments::bip47::can_be_designated_input(
                            tx_out.script_pubkey.as_script(),
                        )
                    })
                    .unwrap_or(false)
            })
            .ok_or_else(|| {
                error::ReusablePayments::Rpc(
                    "notification tx has no input that exposes a pubkey".to_string(),
                )
            })?;
        let designated_outpoint = psbt
            .unsigned_tx
            .input
            .get(designated_idx)
            .ok_or_else(|| {
                error::ReusablePayments::Rpc("notification tx has no inputs".to_string())
            })?
            .previous_output;
        let designated_spk = psbt
            .inputs
            .get(designated_idx)
            .and_then(|i| i.witness_utxo.as_ref())
            .map(|tx_out| tx_out.script_pubkey.clone())
            .ok_or_else(|| {
                error::ReusablePayments::Rpc(
                    "notification tx's designated input has no witness_utxo".to_string(),
                )
            })?;

        let (blinded, _sender_code) = self
            .inner
            .compute_bip47_blinded_payload(recipient, version, designated_outpoint, designated_spk)
            .await?;
        debug_assert_eq!(blinded.len(), payload_len);

        let real_op_return = crate::messages::create_op_return_output(blinded)
            .map_err(|e| error::ReusablePayments::Rpc(format!("op_return build: {e}")))?;

        let op_return_idx = psbt
            .unsigned_tx
            .output
            .iter()
            .position(|o| o.script_pubkey == placeholder_op_return.script_pubkey)
            .ok_or_else(|| {
                error::ReusablePayments::Rpc(
                    "notification PSBT has no placeholder OP_RETURN output".to_string(),
                )
            })?;
        psbt.unsigned_tx.output[op_return_idx].script_pubkey = real_op_return.script_pubkey.clone();

        self.sign_broadcast_apply_notification(psbt).await
    }

    /// OBPP-05 v3 notification: a single `OP_1 <A> <F> <G> OP_3 OP_CHECKMULTISIG`
    /// output, where A is a fresh change pubkey used for the blinding ECDH.
    async fn broadcast_bip47_notification_v3(
        &self,
        recipient: &reusable_payments::PaymentCode,
        fee_rate: bitcoin::FeeRate,
    ) -> Result<bitcoin::Txid, error::ReusablePayments> {
        // Above Core's dust threshold (~786 sats) for a bare 1-of-3 multisig output.
        const V3_NOTIFICATION_SATS: u64 = 1_000;

        let secp = bitcoin::secp256k1::Secp256k1::new();

        let (eph_index, eph_spk_bytes) = {
            let mut wallet_write = self.inner.write_wallet().await?;
            let mut bdk_db_lock = self.inner.bdk_db.lock().await;
            wallet_write
                .with_mut(|wallet| {
                    let info = wallet.reveal_next_address(bdk_wallet::KeychainKind::Internal);
                    let index = info.index;
                    let spk = info.address.script_pubkey().to_bytes();
                    wallet
                        .persist_async(&mut bdk_db_lock)
                        .map_ok(move |_: bool| (index, spk))
                })
                .await
                .map_err(|err| {
                    error::ReusablePayments::Rpc(format!("v3 notification change reveal: {err}"))
                })?
        };

        let master = self.inner.master_xpriv_inner().await?;
        let eph_path = bitcoin::bip32::DerivationPath::from(
            [
                bitcoin::bip32::ChildNumber::from_hardened_idx(84)?,
                bitcoin::bip32::ChildNumber::from_hardened_idx(1)?,
                bitcoin::bip32::ChildNumber::from_hardened_idx(0)?,
                bitcoin::bip32::ChildNumber::from_normal_idx(1)?,
                bitcoin::bip32::ChildNumber::from_normal_idx(eph_index)?,
            ]
            .as_slice(),
        );
        let eph_priv = reusable_payments::scan::ScrubOnDrop::new(
            master.derive_priv(&secp, &eph_path)?.private_key,
        );
        let eph_pub = eph_priv.public_key(&secp);
        let network = self.inner.validator.network();
        let expected_spk =
            bitcoin::Address::p2wpkh(&bitcoin::CompressedPublicKey(eph_pub), network)
                .script_pubkey();
        if expected_spk.to_bytes() != eph_spk_bytes {
            return Err(error::ReusablePayments::Rpc(
                "v3 notification: derived ephemeral key does not match wallet descriptor"
                    .to_string(),
            ));
        }

        let script = self
            .inner
            .compute_bip47_v3_notification_script(recipient, &eph_priv)
            .await?;
        drop(eph_priv);

        let psbt = {
            let mut wallet_write = self.inner.write_wallet().await?;
            tokio::task::block_in_place(|| {
                wallet_write.with_mut(|wallet| {
                    let mut builder = wallet.build_tx();
                    builder.add_recipient(
                        script.clone(),
                        bitcoin::Amount::from_sat(V3_NOTIFICATION_SATS),
                    );
                    builder.fee_rate(fee_rate);
                    builder.finish().map_err(error::CreateSendPsbt::CreateTx)
                })
            })
            .map_err(error::ReusablePayments::from)?
        };

        self.sign_broadcast_apply_notification(psbt).await
    }

    async fn sign_broadcast_apply_notification(
        &self,
        psbt: bdk_wallet::bitcoin::psbt::Psbt,
    ) -> Result<bitcoin::Txid, error::ReusablePayments> {
        let tx = self
            .sign_transaction(psbt)
            .await
            .map_err(|err| error::ReusablePayments::Rpc(format!("sign notification: {err}")))?;
        let txid = tx.compute_txid();

        if crate::rpc_client::broadcast_transaction(&self.inner.main_client, &tx)
            .await
            .map_err(|err| error::ReusablePayments::Rpc(format!("broadcast notification: {err}")))?
            .is_none()
        {
            return Err(error::ReusablePayments::Rpc(
                "notification broadcast rejected: OP_DRIVECHAIN not supported".to_string(),
            ));
        }

        let mut bdk_db_lock = self.inner.bdk_db.lock().await;
        let last_seen = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let _ = self
            .inner
            .write_wallet()
            .await?
            .with_mut(|wallet| {
                wallet.apply_unconfirmed_txs(vec![(tx, last_seen.as_secs())]);
                wallet.persist_async(&mut bdk_db_lock)
            })
            .await
            .map_err(|err| error::ReusablePayments::Rpc(format!("persist notification: {err}")))?;

        tracing::info!(%txid, "broadcast BIP47 notification tx");
        Ok(convert::bdk_txid_to_bitcoin_txid(txid))
    }

    pub async fn send_to_silent_payment(
        &self,
        recipients: Vec<(reusable_payments::SilentPaymentAddress, bitcoin::Amount)>,
        fee_sat_per_vbyte: u64,
    ) -> Result<bitcoin::Txid, error::ReusablePayments> {
        use bitcoin::FeeRate;

        if recipients.is_empty() {
            return Err(error::ReusablePayments::Rpc(
                "send_to_silent_payment requires at least one recipient".to_string(),
            ));
        }

        self.inner.ensure_scanner_activated().await?;

        let fee_rate = FeeRate::from_sat_per_vb(fee_sat_per_vbyte)
            .ok_or_else(|| error::ReusablePayments::Rpc("invalid fee_sat_per_vbyte".to_string()))?;

        let placeholder_xonly = {
            let secp = bitcoin::secp256k1::Secp256k1::new();
            let dummy_sk = bitcoin::secp256k1::SecretKey::from_slice(&[1u8; 32])
                .expect("[1; 32] is a valid secp256k1 scalar");
            let dummy_pk = dummy_sk.public_key(&secp);
            dummy_pk.x_only_public_key().0
        };
        let placeholder_script = bitcoin::ScriptBuf::new_p2tr_tweaked(
            bitcoin::key::TweakedPublicKey::dangerous_assume_tweaked(placeholder_xonly),
        );

        let mut psbt = {
            let mut wallet_write = self.inner.write_wallet().await?;
            tokio::task::block_in_place(|| {
                wallet_write.with_mut(|wallet| {
                    let mut builder = wallet.build_tx();
                    builder.ordering(bdk_wallet::TxOrdering::Untouched);
                    for (_, amount) in &recipients {
                        builder.add_recipient(placeholder_script.clone(), *amount);
                    }
                    builder.fee_rate(fee_rate);
                    builder.finish().map_err(error::CreateSendPsbt::CreateTx)
                })
            })
            .map_err(error::ReusablePayments::from)?
        };

        let (eligible_inputs, all_outpoints): (
            Vec<(bitcoin::OutPoint, bitcoin::ScriptBuf)>,
            Vec<bitcoin::OutPoint>,
        ) = {
            let mut eligible = Vec::with_capacity(psbt.inputs.len());
            let mut all = Vec::with_capacity(psbt.inputs.len());
            for (i, psbt_input) in psbt.inputs.iter().enumerate() {
                let outpoint = psbt.unsigned_tx.input[i].previous_output;
                all.push(outpoint);
                let spk = psbt_input
                    .witness_utxo
                    .as_ref()
                    .map(|t| t.script_pubkey.clone())
                    .ok_or_else(|| {
                        error::ReusablePayments::Rpc(format!(
                            "sp send: input {i} has no witness_utxo"
                        ))
                    })?;
                if !reusable_payments::is_bip352_eligible_spk(spk.as_script()) {
                    return Err(error::ReusablePayments::SilentPaymentCrypto(
                        reusable_payments::silent_payments::CryptoError::IneligibleInput {
                            outpoint,
                        },
                    ));
                }
                eligible.push((outpoint, spk));
            }
            (eligible, all)
        };

        if eligible_inputs.is_empty() {
            return Err(error::ReusablePayments::SilentPaymentCrypto(
                reusable_payments::silent_payments::CryptoError::NoEligibleInputs,
            ));
        }

        let real_outputs = self
            .inner
            .compute_sp_outputs(&recipients, &eligible_inputs, &all_outpoints)
            .await?;

        if real_outputs.len() != recipients.len() {
            return Err(error::ReusablePayments::Rpc(format!(
                "sp send: compute_outputs returned {} outputs for {} recipients",
                real_outputs.len(),
                recipients.len()
            )));
        }

        for (idx, real) in (0..recipients.len()).zip(real_outputs) {
            let output = psbt.unsigned_tx.output.get_mut(idx).ok_or_else(|| {
                error::ReusablePayments::Rpc(format!(
                    "sp send: PSBT has fewer outputs ({idx}) than recipients",
                ))
            })?;
            if output.script_pubkey != placeholder_script {
                return Err(error::ReusablePayments::Rpc(format!(
                    "sp send: output at index {idx} is not the recipient placeholder",
                )));
            }
            if output.value != real.value {
                return Err(error::ReusablePayments::Rpc(format!(
                    "sp send: PSBT amount {} != computed amount {}",
                    output.value, real.value,
                )));
            }
            output.script_pubkey = real.script_pubkey;
        }

        let tx = self
            .sign_transaction(psbt)
            .await
            .map_err(|err| error::ReusablePayments::Rpc(format!("sp sign: {err}")))?;
        let txid = tx.compute_txid();

        if crate::rpc_client::broadcast_transaction(&self.inner.main_client, &tx)
            .await
            .map_err(|err| error::ReusablePayments::Rpc(format!("sp broadcast: {err}")))?
            .is_none()
        {
            return Err(error::ReusablePayments::Rpc(
                "sp broadcast rejected: OP_DRIVECHAIN not supported".to_string(),
            ));
        }

        let mut bdk_db_lock = self.inner.bdk_db.lock().await;
        let last_seen = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let _ = self
            .inner
            .write_wallet()
            .await?
            .with_mut(|wallet| {
                wallet.apply_unconfirmed_txs(vec![(tx, last_seen.as_secs())]);
                wallet.persist_async(&mut bdk_db_lock)
            })
            .await
            .map_err(|err| error::ReusablePayments::Rpc(format!("persist sp tx: {err}")))?;

        tracing::info!(%txid, "broadcast silent-payment tx");
        Ok(convert::bdk_txid_to_bitcoin_txid(txid))
    }

    pub async fn reusable_owned_outputs(
        &self,
    ) -> Result<Vec<reusable_payments::ReusableOwnedOutput>, error::ReusablePayments> {
        self.inner.read_reusable_owned_outputs().await
    }

    pub async fn reusable_scan_status_snapshot(
        &self,
    ) -> Result<reusable_payments::scan::ScanStatus, error::ReusablePayments> {
        use bitcoin_jsonrpsee::client::MainClient;
        let cursor_opt = self.inner.load_scan_cursor_if_present().await?;
        let tip_height: u32 = self
            .inner
            .main_client
            .getblockcount()
            .await
            .map(|h| h as u32)
            .unwrap_or(0);
        let (birthday_height, last_scanned_height, catching_up) = match cursor_opt {
            Some(c) => (
                c.birthday_height,
                c.last_scanned_height,
                c.last_scanned_height < tip_height,
            ),
            None => (0, 0, false),
        };
        Ok(reusable_payments::scan::ScanStatus {
            tip_height,
            last_scanned_height,
            birthday_height,
            catching_up,
        })
    }

    pub async fn rescan_reusable_payments(
        &self,
        from_height: u32,
    ) -> Result<(), error::ReusablePayments> {
        use bitcoin_jsonrpsee::{
            client::MainClient,
            jsonrpsee::core::{client::ClientT, params::ArrayParams},
        };

        self.inner.ensure_wallet_unlocked().await?;
        self.inner.ensure_scanner_activated().await?;

        let _scanner_guard = self
            .inner
            .scanner_lock
            .acquire()
            .await
            .expect("scanner semaphore is never closed");

        self.inner.drop_scan_events_above(from_height).await?;

        let map_rpc =
            |err: bitcoin_jsonrpsee::jsonrpsee::core::client::Error| -> error::ReusablePayments {
                error::ReusablePayments::Rpc(err.to_string())
            };
        let tip_height: u32 = self
            .inner
            .main_client
            .getblockcount()
            .await
            .map_err(map_rpc)? as u32;

        let mut ctx = match self.inner.build_scan_context_for_deep_rescan().await {
            Ok(ctx) => ctx,
            Err(error::ReusablePayments::NotFound(_))
            | Err(error::ReusablePayments::NotUnlocked(_)) => return Ok(()),
            Err(err) => return Err(err),
        };

        let mut prev_block_hash: Option<bitcoin::BlockHash> = None;
        for height in from_height..=tip_height {
            let hash = self
                .inner
                .main_client
                .getblockhash(height as usize)
                .await
                .map_err(map_rpc)?;
            let mut params = ArrayParams::new();
            params
                .insert(hash.to_string())
                .map_err(|e| error::ReusablePayments::Rpc(e.to_string()))?;
            params
                .insert(0u32)
                .map_err(|e| error::ReusablePayments::Rpc(e.to_string()))?;
            let hex_str: String = self
                .inner
                .main_client
                .request("getblock", params)
                .await
                .map_err(map_rpc)?;
            let bytes = hex::decode(&hex_str)
                .map_err(|e| error::ReusablePayments::ConsensusDecode(e.to_string()))?;
            let block: bitcoin::Block = bitcoin::consensus::deserialize(&bytes)
                .map_err(|e| error::ReusablePayments::ConsensusDecode(e.to_string()))?;

            if let Some(expected_prev) = prev_block_hash
                && block.header.prev_blockhash != expected_prev
            {
                tracing::warn!(
                    height,
                    expected = %expected_prev,
                    got = %block.header.prev_blockhash,
                    "rescan_reusable_payments: mid-rescan reorg detected, aborting"
                );
                self.inner.drop_scan_events_above(height).await?;
                return Err(error::ReusablePayments::Rpc(
                    "rescan aborted due to mid-rescan reorg, retry".to_string(),
                ));
            }

            self.inner
                .scan_block_with_ctx(&block, height, &mut ctx)
                .await?;
            prev_block_hash = Some(block.block_hash());
        }
        Ok(())
    }
}

/// Number of most-recently-produced blocks whose consumed BMM requests are
/// retained in `bmm_requests_undo` so they can be restored on a reorg. Undo rows
/// for older blocks are pruned to keep the table bounded (rows are otherwise only
/// removed when their block is disconnected, which never happens for the blocks
/// that stay on the main chain). Bitcoin reorgs deeper than a handful of blocks
/// are already extraordinary, so this is generous headroom; a reorg deeper than
/// this would merely degrade to the pre-fix behaviour (operator re-issues
/// `create_bmm_request`) for the affected blocks, never a hard error.
const BMM_REQUESTS_UNDO_RETAINED_BLOCKS: i64 = 100;

/// Snapshot the BMM requests keyed to `prev_blockhash` into `bmm_requests_undo`
/// (keyed by the mined `block_hash`) and delete them from `bmm_requests`, within
/// a single transaction. Called when a block is generated on top of
/// `prev_blockhash`; the snapshot lets `restore_bmm_requests_from_undo` put the
/// requests back if that block is later disconnected by a reorg. Undo rows for
/// blocks beyond the most recent `BMM_REQUESTS_UNDO_RETAINED_BLOCKS` producing
/// blocks are pruned so the table stays bounded.
fn snapshot_and_delete_bmm_requests(
    connection: &mut Connection,
    prev_blockhash: &bitcoin::BlockHash,
    block_hash: &bitcoin::BlockHash,
) -> Result<(), rusqlite::Error> {
    let tx = connection.transaction()?;
    tx.execute(
        "INSERT INTO bmm_requests_undo \
         (block_hash, sidechain_number, prev_block_hash, side_block_hash) \
         SELECT ?1, sidechain_number, prev_block_hash, side_block_hash \
         FROM bmm_requests WHERE prev_block_hash = ?2;",
        (block_hash.as_byte_array(), prev_blockhash.as_byte_array()),
    )?;
    tx.execute(
        "DELETE FROM bmm_requests where prev_block_hash = ?;",
        [prev_blockhash.as_byte_array()],
    )?;
    // Keep only the undo rows for the most recently produced blocks, so blocks
    // that stay on the main chain (and are therefore never disconnected) don't
    // accumulate undo rows forever.
    tx.execute(
        "DELETE FROM bmm_requests_undo \
         WHERE block_hash NOT IN ( \
             SELECT block_hash FROM bmm_requests_undo \
             GROUP BY block_hash \
             ORDER BY MAX(rowid) DESC \
             LIMIT ?1 \
         );",
        [BMM_REQUESTS_UNDO_RETAINED_BLOCKS],
    )?;
    tx.commit()
}

/// Restore the BMM requests snapshotted for `block_hash` back into
/// `bmm_requests`, removing them from `bmm_requests_undo`, within a single
/// transaction. Returns the number of rows restored. Called when `block_hash` is
/// disconnected by a reorg.
fn restore_bmm_requests_from_undo(
    connection: &mut Connection,
    block_hash: &bitcoin::BlockHash,
) -> Result<usize, rusqlite::Error> {
    let tx = connection.transaction()?;
    let restored = tx.execute(
        "INSERT OR IGNORE INTO bmm_requests \
         (sidechain_number, prev_block_hash, side_block_hash) \
         SELECT sidechain_number, prev_block_hash, side_block_hash \
         FROM bmm_requests_undo WHERE block_hash = ?;",
        [block_hash.as_byte_array()],
    )?;
    tx.execute(
        "DELETE FROM bmm_requests_undo WHERE block_hash = ?;",
        [block_hash.as_byte_array()],
    )?;
    tx.commit()?;
    Ok(restored)
}

#[cfg(test)]
mod bmm_requests_undo_tests {
    use bitcoin::hashes::Hash as _;
    use rusqlite::Connection;

    use super::{restore_bmm_requests_from_undo, snapshot_and_delete_bmm_requests};

    fn block_hash(byte: u8) -> bitcoin::BlockHash {
        bitcoin::BlockHash::from_byte_array([byte; 32])
    }

    fn open_db() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        // Verbatim `bmm_requests` schema plus the new `bmm_requests_undo` table,
        // matching the migrations in `init_db_connection`.
        connection
            .execute_batch(
                "CREATE TABLE bmm_requests
                    (sidechain_number INTEGER NOT NULL,
                     prev_block_hash BLOB NOT NULL,
                     side_block_hash BLOB NOT NULL,
                     UNIQUE(sidechain_number, prev_block_hash));
                 CREATE TABLE bmm_requests_undo
                    (block_hash BLOB NOT NULL,
                     sidechain_number INTEGER NOT NULL,
                     prev_block_hash BLOB NOT NULL,
                     side_block_hash BLOB NOT NULL);",
            )
            .unwrap();
        connection
    }

    fn insert_request(connection: &Connection, sidechain_number: u8, prev: &bitcoin::BlockHash) {
        connection
            .execute(
                "INSERT INTO bmm_requests (sidechain_number, prev_block_hash, side_block_hash) \
                 VALUES (?1, ?2, ?3);",
                (
                    sidechain_number,
                    prev.as_byte_array(),
                    block_hash(0).as_byte_array(),
                ),
            )
            .unwrap();
    }

    fn row_count(connection: &Connection, table: &str) -> i64 {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn distinct_undo_blocks(connection: &Connection) -> i64 {
        connection
            .query_row(
                "SELECT COUNT(DISTINCT block_hash) FROM bmm_requests_undo",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// A BMM request consumed by block production is snapshotted, then restored
    /// verbatim when the producing block is disconnected by a reorg.
    #[test]
    fn bmm_request_restored_when_producing_block_disconnected() {
        let mut connection = open_db();

        let prev = block_hash(1);
        let mined = block_hash(2);
        let side = block_hash(3);

        // Operator queues a BMM request against the current tip `prev`.
        connection
            .execute(
                "INSERT INTO bmm_requests (sidechain_number, prev_block_hash, side_block_hash) \
                 VALUES (?1, ?2, ?3);",
                (5, prev.as_byte_array(), side.as_byte_array()),
            )
            .unwrap();
        assert_eq!(row_count(&connection, "bmm_requests"), 1);

        // Producing `mined` on top of `prev` consumes the request into the undo log.
        snapshot_and_delete_bmm_requests(&mut connection, &prev, &mined).unwrap();
        assert_eq!(row_count(&connection, "bmm_requests"), 0);
        assert_eq!(row_count(&connection, "bmm_requests_undo"), 1);

        // Disconnecting `mined` restores the request for the reverted tip `prev`.
        let restored = restore_bmm_requests_from_undo(&mut connection, &mined).unwrap();
        assert_eq!(restored, 1);
        assert_eq!(row_count(&connection, "bmm_requests"), 1);
        assert_eq!(row_count(&connection, "bmm_requests_undo"), 0);

        let (sidechain_number, restored_side): (u64, Vec<u8>) = connection
            .query_row(
                "SELECT sidechain_number, side_block_hash FROM bmm_requests \
                 WHERE prev_block_hash = ?;",
                [prev.as_byte_array()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(sidechain_number, 5);
        assert_eq!(restored_side, side.as_byte_array().to_vec());
    }

    /// Blocks that stay on the main chain (never disconnected) must not
    /// accumulate undo rows without bound: only the most recent
    /// `BMM_REQUESTS_UNDO_RETAINED_BLOCKS` producing blocks are retained, and the
    /// oldest snapshot is dropped once that many newer blocks have been produced.
    #[test]
    fn old_undo_rows_are_pruned() {
        let mut connection = open_db();

        // Produce `RETAINED + 1` blocks, each consuming a distinct BMM request.
        let total = super::BMM_REQUESTS_UNDO_RETAINED_BLOCKS + 1;
        for i in 0..total {
            let prev = block_hash(i as u8);
            let mined = block_hash((total - i) as u8);
            insert_request(&connection, 0, &prev);
            snapshot_and_delete_bmm_requests(&mut connection, &prev, &mined).unwrap();
        }

        // The table is bounded to the retention window, not the block count.
        assert_eq!(
            distinct_undo_blocks(&connection),
            super::BMM_REQUESTS_UNDO_RETAINED_BLOCKS
        );

        // The very first block's snapshot has been pruned, so disconnecting it
        // restores nothing (degrades to pre-fix behaviour for ancient reorgs).
        let oldest = block_hash(total as u8);
        assert_eq!(
            restore_bmm_requests_from_undo(&mut connection, &oldest).unwrap(),
            0
        );

        // The most recent block's snapshot is retained and still restorable.
        let newest = block_hash(1);
        assert_eq!(
            restore_bmm_requests_from_undo(&mut connection, &newest).unwrap(),
            1
        );
    }
}
