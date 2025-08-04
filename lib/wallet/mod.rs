use std::{
    collections::HashMap,
    future::Future,
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
use futures::{FutureExt, TryFutureExt};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
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
        sync::NoSyncClient,
        util::{RwLockReadGuardSome, RwLockUpgradableReadGuardSome, RwLockWriteGuardSome},
    },
};

mod cusf_block_producer;
pub mod error;
mod mine;
pub mod mnemonic;
mod sync;
mod thread_safe_connection;
mod util;

type BundleProposals = Vec<(M6id, BlindedM6<'static>, Option<PendingM6idInfo>)>;

pub(crate) type Persistence = thread_safe_connection::ThreadSafeConnection;
type BdkWallet = bdk_wallet::PersistedWallet<Persistence>;

type ElectrumClient = BdkElectrumClient<bdk_electrum::electrum_client::Client>;
type EsploraClient = bdk_esplora::esplora_client::AsyncClient;
type ChainSource = Either<ElectrumClient, Either<EsploraClient, NoSyncClient>>;

struct WalletInner {
    main_client: HttpClient,
    validator: Validator,
    magic: bitcoin::p2p::Magic,
    // Unlocked, ready-to-go wallet: Some
    // Locked wallet: None
    bitcoin_wallet: async_lock::RwLock<Option<BdkWallet>>,
    /// Persistence for the BDK wallet
    bdk_db: tokio::sync::Mutex<Persistence>,
    // Persistence for things /we/ care about. Wallet seed, M* messages, ++.
    self_db: tokio::sync::Mutex<rusqlite::Connection>,
    chain_source: ChainSource,
    last_sync: async_lock::RwLock<Option<SystemTime>>,
    config: Config,
}

impl WalletInner {
    async fn init_esplora_client(
        config: &WalletConfig,
        network: Network,
    ) -> Result<EsploraClient, error::InitEsploraClient> {
        let default_url = match network {
            Network::Signet => "https://explorer.drivechain.info/api",
            Network::Regtest => "http://localhost:3003",
            network => return Err(error::UnsupportedNetwork(network).into()),
        };
        let default_url = url::Url::parse(default_url)?;

        let esplora_url = config.esplora_url.clone().unwrap_or(default_url);

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
        let (default_host, default_port) = match network {
            Network::Signet => ("explorer.drivechain.info", 50001),
            Network::Regtest => ("127.0.0.1", 60401), // Default for mempool/electrs
            network => return Err(error::UnsupportedNetwork(network).into()),
        };
        let electrum_host = config
            .electrum_host
            .clone()
            .unwrap_or(default_host.to_string());
        let electrum_port = config.electrum_port.unwrap_or(default_port);
        let electrum_url = format!("{electrum_host}:{electrum_port}");
        tracing::debug!(%electrum_url, "creating electrum client");
        // Apply a reasonably short timeout to prevent the wallet from hanging
        let timeout = 5;
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
        ]);

        let db_name = "db.sqlite";
        let path = data_dir.join(db_name);
        let mut db_connection = Connection::open(path.clone())?;
        tracing::info!("Created database connection to {}", path.display());
        migrations.to_latest(&mut db_connection)?;
        tracing::debug!("Ran migrations on {}", path.display());
        Ok(db_connection)
    }

    async fn initialize_wallet_from_mnemonic(
        mnemonic: &Mnemonic,
        network: bdk_wallet::bitcoin::Network,
        wallet_database: &mut Persistence,
    ) -> Result<BdkWallet, error::InitWalletFromMnemonic> {
        let extended_key: ExtendedKey = mnemonic.clone().into_extended_key()?;

        let xpriv = extended_key
            .into_xprv(network)
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
        validator: Validator,
        magic: bitcoin::p2p::Magic,
    ) -> Result<Self, error::InitWallet> {
        let network = {
            let validator_network = validator.network();
            bdk_wallet::bitcoin::Network::from_str(validator_network.to_string().as_str())?
        };

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

        let chain_source = match config.wallet_opts.sync_source {
            WalletSyncSource::Electrum => {
                let electrum_client = Self::init_electrum_client(&config.wallet_opts, network)?;
                Either::Left(electrum_client)
            }
            WalletSyncSource::Esplora => {
                let esplora_client =
                    Self::init_esplora_client(&config.wallet_opts, network).await?;
                Either::Right(Either::Left(esplora_client))
            }
            WalletSyncSource::Disabled => Either::Right(Either::Right(NoSyncClient {})),
        };
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
            bitcoin_wallet: async_lock::RwLock::new(bitcoin_wallet),
            bdk_db: tokio::sync::Mutex::new(wallet_database),
            self_db: tokio::sync::Mutex::new(db_connection),
            chain_source,
            last_sync: async_lock::RwLock::new(None),
        })
    }

    /// Warn if lock takes this long to acquire
    const LOCK_WARN_DURATION: Duration = Duration::from_secs(1);

    #[allow(clippy::significant_drop_in_scrutinee, reason = "false positive")]
    async fn read_wallet(&self) -> Result<RwLockReadGuardSome<BdkWallet>, error::NotUnlocked> {
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
    #[allow(clippy::significant_drop_in_scrutinee, reason = "false positive")]
    async fn read_wallet_upgradable(
        &self,
    ) -> Result<RwLockUpgradableReadGuardSome<BdkWallet>, error::NotUnlocked> {
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

    #[allow(clippy::significant_drop_in_scrutinee, reason = "false positive")]
    async fn write_wallet(&self) -> Result<RwLockWriteGuardSome<BdkWallet>, error::NotUnlocked> {
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
        let with_connection = |connection: &Connection| -> Result<_, _> {
            for (sidechain_number, m6id) in iter {
                let _ = connection.execute(
                    "DELETE FROM bundle_proposals where sidechain_number = ?1 AND bundle_hash = ?2;",
                    (sidechain_number.0, m6id.0.as_byte_array())
                )?;
            }
            Ok(())
        };
        let connection = self.self_db.lock().await;
        with_connection(&connection)
    }

    // Gets wiped upon generating a new block.
    async fn delete_pending_sidechain_proposals<I>(
        &self,
        proposals: I,
    ) -> Result<(), rusqlite::Error>
    where
        I: IntoIterator<Item = SidechainProposalId>,
    {
        let with_connection = |connection: &Connection| -> Result<_, rusqlite::Error> {
            for proposal_id in proposals {
                let _ = connection.execute(
                    "DELETE FROM sidechain_proposals where sidechain_number = ?1 AND data_hash = ?2;",
                    (proposal_id.sidechain_number.0, proposal_id.description_hash.as_byte_array())
                )?;
            }
            Ok(())
        };
        let connection = self.self_db.lock().await;
        with_connection(&connection)
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
    ) -> Result<Self, error::InitWallet> {
        let inner =
            Arc::new(WalletInner::new(data_dir, config, main_client, validator, magic).await?);
        Ok(Self { inner })
    }

    pub async fn sync_task<F: Future<Output = ()>>(
        &self,
        shutdown_signal: F,
    ) -> Result<(), miette::Report> {
        const SYNC_INTERVAL: Duration = Duration::from_secs(15);
        tracing::debug!(
            interval = %jiff::SignedDuration::try_from(SYNC_INTERVAL).unwrap(),
            "wallet sync task: starting"
        );

        // Needed so we can use `tokio::select!`
        futures::pin_mut!(shutdown_signal);

        let mut sleep = tokio::time::sleep(SYNC_INTERVAL).boxed();
        loop {
            tokio::select! {
                biased;  // Prioritize shutdown

                res = &mut shutdown_signal => {
                    tracing::info!("shutting down sync task");
                    return Ok(res);
                }
                _ = &mut sleep => {
                    let tick = Uuid::new_v4().simple();
                    let span = tracing::span!(tracing::Level::DEBUG,
                        "wallet_sync",
                        %tick,
                    );
                    let guard = span.enter();
                    if let Err(err) = self.inner.sync().await {
                        tracing::error!("wallet sync error: {:#}", ErrorChain::new(&err));
                    }
                    drop(guard);
                    sleep = tokio::time::sleep(SYNC_INTERVAL).boxed();
                }
            }
        }
    }

    #[allow(clippy::result_large_err)]
    pub(crate) fn parse_checked_address(
        &self,
        address: &str,
    ) -> Result<bitcoin::Address, tonic::Status> {
        let network = self.validator().network();
        let address = bdk_wallet::bitcoin::Address::from_str(address).map_err(|err| {
            tonic::Status::invalid_argument(format!("invalid bitcoin address: {err:#}"))
        })?;

        let address = address.require_network(network).map_err(|_| {
            tonic::Status::invalid_argument(format!(
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
                    let bundle_proposal_tx = bitcoin::consensus::deserialize(&bundle_tx_bytes)?;
                    let bundle_proposal_tx =
                        BlindedM6::try_from(std::borrow::Cow::Owned(bundle_proposal_tx))?;
                    bundle_proposals
                        .entry(sidechain_number)
                        .or_default()
                        .push((m6id, bundle_proposal_tx));
                    Ok(())
                })?;
            Ok(bundle_proposals)
        };
        let connection = self.inner.self_db.lock().await;
        let bundle_proposals = with_connection(&connection)?;
        drop(connection);
        // Filter out proposals that have already been created
        let res = bundle_proposals
            .into_iter()
            .map(Ok::<_, error::GetBundleProposals>)
            .transpose_into_fallible()
            .filter_map(|(sidechain_id, m6ids)| {
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

    // Gets wiped upon generating a new block.
    async fn delete_bmm_requests(
        &self,
        prev_blockhash: &bitcoin::BlockHash,
    ) -> Result<(), rusqlite::Error> {
        self.inner.self_db.lock().await.execute(
            "DELETE FROM bmm_requests where prev_block_hash = ?;",
            [prev_blockhash.as_byte_array()],
        )?;
        Ok(())
    }

    fn create_deposit_op_drivechain_output(
        sidechain_number: SidechainNumber,
        sidechain_ctip_amount: Amount,
        value: Amount,
    ) -> bdk_wallet::bitcoin::TxOut {
        let deposit_txout =
            messages::create_m5_deposit_output(sidechain_number, sidechain_ctip_amount, value);

        bdk_wallet::bitcoin::TxOut {
            script_pubkey: bdk_wallet::bitcoin::ScriptBuf::from_bytes(
                deposit_txout.script_pubkey.to_bytes(),
            ),
            value: deposit_txout.value,
        }
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
                use rand::RngCore;
                let mut bytes = vec![0u8; <sha256::Hash as Hash>::Engine::BLOCK_SIZE];
                rand::thread_rng().fill_bytes(&mut bytes);
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
            {
                if let Some(sidechain_id) = sidechain_addrs.get(&address) {
                    return TxOutKind::OpReturnAddress(*sidechain_id);
                }
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

    #[allow(
        clippy::significant_drop_tightening,
        reason = "false positive for `bitcoin_wallet`"
    )]
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
        );
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
        tracing::debug!(%txid, "Broadcasting deposit transaction...");
        let mut broadcast_successfully: bool =
            crate::rpc_client::broadcast_transaction(&self.inner.main_client, &tx)
                .await
                .map_err(error::CreateDeposit::BroadcastTx)?
                .is_some();
        if self.inner.validator.network() == Network::Signet
            && self.inner.magic.as_ref() == crate::p2p::SIGNET_MAGIC_BYTES
        {
            broadcast_successfully |= crate::p2p::broadcast_nonstandard_tx(
                crate::p2p::SIGNET_MINER_P2P_ADDR.into(),
                block_height as i32,
                self.inner.magic,
                tx,
            )
            .await
            .map_err(error::CreateDeposit::BroadcastNonstandardTx)?;
        }
        if broadcast_successfully {
            tracing::info!(%txid, "Broadcast deposit transaction successfully");
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

    #[allow(
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
                },
            ) => anchor.confirmation_time.cmp(&last_seen),
            (
                ChainPosition::Unconfirmed {
                    last_seen: Some(last_seen),
                },
                ChainPosition::Confirmed { anchor, .. },
            ) => last_seen.cmp(&anchor.confirmation_time),
            (
                ChainPosition::Unconfirmed {
                    last_seen: Some(a_last_seen),
                },
                ChainPosition::Unconfirmed {
                    last_seen: Some(b_last_seen),
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
                    Some((_, _, seq)) => {
                        let spent_treasury_utxo = self
                            .validator()
                            .get_treasury_utxo(sidechain_number, seq - 1)?;
                        Some(crate::types::Ctip {
                            outpoint: spent_treasury_utxo.outpoint,
                            value: spent_treasury_utxo.total_value,
                        })
                    }
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
                if let Some(spent_ctip) = spent_ctip {
                    if spent_ctip.value > treasury_output.value {
                        return Ok(None);
                    }
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

    #[allow(
        clippy::significant_drop_tightening,
        reason = "false positive for `bitcoin_wallet`"
    )]
    async fn create_send_psbt(
        &self,
        destinations: HashMap<bitcoin::Address, Amount>,
        params: CreateTransactionParams,
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

        // sanity check that we did things correctly
        if !applied_changes {
            panic!("PROGRAMMER ERROR: no changes in wallet after applying unconfirmed transaction");
        }

        tracing::debug!(%txid, "Applied unconfirmed transaction to wallet");

        Ok(convert::bdk_txid_to_bitcoin_txid(txid))
    }

    #[allow(
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

    #[allow(dead_code)]
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
            .sign(&mut psbt, bdk_wallet::signer::SignOptions::default())
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
        let message = [
            &M8BmmRequest::TAG[..],
            &[sidechain_number.into()],
            sidechain_block_hash.0.as_slice(),
            &prev_mainchain_block_hash.to_byte_array(),
        ]
        .concat();
        let bytes = bdk_wallet::bitcoin::script::PushBytesBuf::try_from(message)?;
        Ok(bdk_wallet::bitcoin::ScriptBuf::new_op_return(&bytes))
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "false positive for `bitcoin_wallet`"
    )]
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

    /// Creates a BMM request transaction. Does NOT broadcast.
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
        if self
            .insert_new_bmm_request(
                sidechain_number,
                prev_mainchain_block_hash,
                sidechain_block_hash,
            )
            .await?
        {
            tracing::info!("BMM request: inserted new bmm request into db");
            Ok(Some(tx))
        } else {
            tracing::warn!(
                "BMM request: Ignored, request exists with same sidechain slot and previous block hash"
            );
            Ok(None)
        }
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

    #[allow(clippy::significant_drop_tightening)]
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
        let tx_bytes = bitcoin::consensus::serialize(blinded_m6.as_ref());
        self.inner.self_db
            .lock()
            .await
            .execute(
                "INSERT OR IGNORE INTO bundle_proposals (sidechain_number, bundle_hash, bundle_tx) VALUES (?1, ?2, ?3)",
                (sidechain_number.0, m6id.0.as_byte_array(), tx_bytes),
            )?;
        Ok(m6id)
    }

    /// Connect a missing block to the BDK chain. This is a recursive function that will
    /// retry if we get a 'nested' alert from BDK, about further missing ancestors.
    async fn connect_missing_block(
        &mut self,
        try_include_height: u32,
    ) -> std::result::Result<(), error::ConnectBlock> {
        use bitcoin_jsonrpsee::{
            MainClient as _,
            client::{GetBlockClient as _, U8Witness},
        };

        let try_include_hash = self
            .inner
            .main_client
            .getblockhash(try_include_height as usize)
            .await
            .map_err(|err| {
                error::ConnectBlock::GetBlockHash(error::BitcoinCoreRPC {
                    method: "getblockhash".to_string(),
                    error: err,
                })
            })?;

        let try_include_block = self
            .inner
            .main_client
            .get_block(try_include_hash, U8Witness::<0>)
            .await
            .map_err(|err| {
                error::ConnectBlock::GetBlock(error::BitcoinCoreRPC {
                    method: "getblock".to_string(),
                    error: err,
                })
            })?;

        let infos = self
            .inner
            .validator
            .get_block_infos(&try_include_block.0.block_hash(), 0)?;

        assert_eq!(infos.len(), 1);

        let (header_info, block_info) = infos.head;

        tracing::debug!(
            "connecting missing block {} at height {}",
            try_include_block.0.block_hash(),
            header_info.height
        );

        match self
            .inner
            .handle_connect_block(&try_include_block.0, header_info.height, &block_info)
            .await
        {
            Ok(_) => Ok(()),
            // We can receive 'nested' alerts from BDK, about further missing ancestors. We therefore
            // recurse, but make sure to only do so if the recommended try_include_height is /below/
            // what we just tried. Otherwise we'll just loop forever.
            Err(error::ConnectBlock::BdkConnect(bdk_chain::local_chain::CannotConnectError {
                try_include_height,
            })) if try_include_height < header_info.height => {
                tracing::info!(
                    "recursing to connect missing block at height {}",
                    try_include_height
                );

                // Need to box the recursive call to satisfy the compiler
                Box::pin(self.connect_missing_block(try_include_height)).await
            }
            err => err,
        }?;

        tracing::debug!(
            "connected missing block {} at height {}",
            try_include_block.0.block_hash(),
            header_info.height
        );

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
}
