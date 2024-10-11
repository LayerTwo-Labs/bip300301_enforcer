use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use bdk::{
    bitcoin::Network,
    blockchain::ElectrumBlockchain,
    database::SqliteDatabase,
    electrum_client::ConfigBuilder,
    keys::{
        bip39::{Language, Mnemonic},
        DerivableKey, ExtendedKey,
    },
    template::Bip84,
    wallet::AddressIndex,
    KeychainKind, SyncOptions,
};
use bip300301::{client::BlockchainInfo, jsonrpsee::http_client::HttpClient, MainClient};
use bitcoin::{
    absolute::{Height, LockTime},
    block::Version as BlockVersion,
    consensus::Encodable as _,
    constants::{genesis_block, SUBSIDY_HALVING_INTERVAL},
    hash_types::TxMerkleNode,
    hashes::Hash as _,
    merkle_tree,
    opcodes::{
        all::{OP_PUSHBYTES_36, OP_RETURN},
        OP_0,
    },
    transaction::Version as TxVersion,
    Amount, Block, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness,
};
use miette::{miette, IntoDiagnostic, Result};
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;

use crate::{
    cli::WalletConfig,
    types::{SidechainNumber, SidechainProposal},
    validator::Validator,
};

pub struct Wallet {
    main_client: HttpClient,
    validator: Validator,
    bitcoin_wallet: Arc<Mutex<bdk::Wallet<SqliteDatabase>>>,
    db_connection: Arc<Mutex<rusqlite::Connection>>,
    bitcoin_blockchain: ElectrumBlockchain,
    mnemonic: Mnemonic,
    // seed
    // sidechain number
    // index
    // address (20 byte hash of public key)
    // utxos
    last_sync: Arc<RwLock<Option<SystemTime>>>,
}

impl Wallet {
    pub async fn new<P: AsRef<Path>>(
        data_dir: P,
        config: &WalletConfig,
        main_client: HttpClient,
        validator: Validator,
    ) -> Result<Self> {
        // Generate fresh mnemonic

        /*
        let mnemonic: GeneratedKey<_, miniscript::Segwitv0> =
                                        Mnemonic::generate((WordCount::Words12, Language::English)).unwrap();
        // Convert mnemonic to string
        let mnemonic_words = mnemonic.to_string();
        // Parse a mnemonic
        let mnemonic = Mnemonic::parse(&mnemonic_words).unwrap();
        */

        let mnemonic = Mnemonic::parse_in_normalized(
            Language::English,
            "betray annual dog current tomorrow media ghost dynamic mule length sure salad",
        )
        .into_diagnostic()?;
        // Generate the extended key
        let xkey: ExtendedKey = mnemonic.clone().into_extended_key().into_diagnostic()?;
        // Get xprv from the extended key
        let network = {
            let magic_bytes = validator.network().magic().to_bytes();
            bdk::bitcoin::Network::from_magic(bdk::bitcoin::network::Magic::from_bytes(magic_bytes))
                .unwrap()
        };
        let xprv = xkey
            .into_xprv(network)
            .ok_or(miette!("couldn't get xprv"))?;

        // Ensure that the data directory exists
        match std::fs::create_dir_all(&data_dir) {
            Ok(_) => (),
            Err(e) => {
                return Err(miette!(
                    "failed to create data dir {}: {e:#}",
                    data_dir.as_ref().display()
                ));
            }
        }
        tracing::trace!(
            "Ensured data directory exists: {}",
            data_dir.as_ref().display()
        );

        let wallet_database = SqliteDatabase::new(data_dir.as_ref().join("wallet.sqlite"));
        // Create a BDK wallet structure using BIP 84 descriptor ("m/84h/1h/0h/0" and "m/84h/1h/0h/1")
        let bitcoin_wallet = bdk::Wallet::new(
            Bip84(xprv, KeychainKind::External),
            Some(Bip84(xprv, KeychainKind::Internal)),
            network,
            wallet_database,
        )
        .into_diagnostic()?;

        let bitcoin_blockchain = {
            let electrum_url = format!("{}:{}", config.electrum_host, config.electrum_port);

            tracing::debug!("creating electrum client: {electrum_url}");

            // Apply a reasonably short timeout to prevent the wallet from hanging
            let timeout = 5;
            let config = ConfigBuilder::new().timeout(Some(timeout)).build();

            let electrum_client = bdk::electrum_client::Client::from_config(&electrum_url, config)
                .map_err(|err| miette!("failed to create electrum client: {err:#}"))?;

            ElectrumBlockchain::from(electrum_client)
        };

        use rusqlite_migration::{Migrations, M};

        let db_connection = {
            // 1️⃣ Define migrations
            let migrations = Migrations::new(vec![
                M::up(
                    "CREATE TABLE sidechain_proposals
                   (number INTEGER NOT NULL,
                    data BLOB NOT NULL,
                    UNIQUE(number, data));",
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
                    UNIQUE(sidechain_number, bundle_hash));",
                ),
                M::up(
                    "CREATE TABLE bundle_acks
                   (sidechain_number INTEGER NOT NULL,
                    bundle_hash BLOB NOT NULL,
                    UNIQUE(sidechain_number, bundle_hash));",
                ),
                M::up(
                    "CREATE TABLE mempool
                   (txid BLOB UNIQUE NOT NULL,
                    tx_data BLOB NOT NULL);",
                ),
            ]);

            let db_name = "db.sqlite";
            let mut db_connection =
                Connection::open(data_dir.as_ref().join(db_name)).into_diagnostic()?;

            tracing::debug!("Created database connection to {db_name}");

            migrations.to_latest(&mut db_connection).into_diagnostic()?;

            tracing::debug!("Ran migrations on {db_name}");
            db_connection
        };

        let wallet = Self {
            main_client,
            validator,
            bitcoin_wallet: Arc::new(Mutex::new(bitcoin_wallet)),
            db_connection: Arc::new(Mutex::new(db_connection)),
            bitcoin_blockchain,
            mnemonic,

            last_sync: Arc::new(RwLock::new(None)),
        };
        Ok(wallet)
    }

    pub async fn generate_block(
        &self,
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
    ) -> Result<Block> {
        let addr = self
            .bitcoin_wallet
            .lock()
            .get_address(AddressIndex::New)
            .into_diagnostic()?;
        let script_pubkey = addr.script_pubkey();

        let BlockchainInfo {
            blocks: block_height,
            best_blockhash,
            ..
        } = self
            .main_client
            .get_blockchain_info()
            .await
            .into_diagnostic()?;
        tracing::debug!("Block height: {block_height}");
        tracing::debug!("Best block: {best_blockhash}");
        let prev_blockhash = best_blockhash;

        let start = SystemTime::now();
        let time = start
            .duration_since(UNIX_EPOCH)
            .into_diagnostic()?
            .as_secs() as u32;

        let script_sig = bitcoin::blockdata::script::Builder::new()
            .push_int((block_height + 1) as i64)
            .push_opcode(OP_0)
            .into_script();
        let value = get_block_value(block_height + 1, Amount::ZERO, Network::Regtest);

        let output = if value > Amount::ZERO {
            vec![TxOut {
                script_pubkey: ScriptBuf::from_bytes(script_pubkey.to_bytes()),
                value,
            }]
        } else {
            vec![TxOut {
                script_pubkey: ScriptBuf::builder().push_opcode(OP_RETURN).into_script(),
                value: Amount::ZERO,
            }]
        };

        const WITNESS_RESERVED_VALUE: [u8; 32] = [0; 32];

        let txdata = [
            vec![Transaction {
                version: TxVersion::TWO,
                lock_time: LockTime::Blocks(Height::ZERO),
                input: vec![TxIn {
                    previous_output: bitcoin::OutPoint {
                        txid: Txid::all_zeros(),
                        vout: 0xFFFF_FFFF,
                    },
                    sequence: Sequence::MAX,
                    witness: Witness::from_slice(&[WITNESS_RESERVED_VALUE]),
                    script_sig,
                }],
                output: [&output, coinbase_outputs].concat(),
            }],
            transactions,
        ]
        .concat();

        let genesis_block = genesis_block(bitcoin::Network::Regtest);
        let bits = genesis_block.header.bits;
        let header = bitcoin::block::Header {
            version: BlockVersion::NO_SOFT_FORK_SIGNALLING,
            prev_blockhash,
            // merkle root is computed after the witness commitment is added to coinbase
            merkle_root: TxMerkleNode::all_zeros(),
            time,
            bits,
            nonce: 0,
        };
        let mut block = Block { header, txdata };
        let witness_root = block.witness_root().unwrap();
        let witness_commitment =
            Block::compute_witness_commitment(&witness_root, &WITNESS_RESERVED_VALUE);

        let script_pubkey_bytes = [
            vec![OP_RETURN.to_u8(), OP_PUSHBYTES_36.to_u8()],
            vec![0xaa, 0x21, 0xa9, 0xed],
            witness_commitment.as_byte_array().into(),
        ]
        .concat();
        let script_pubkey = ScriptBuf::from_bytes(script_pubkey_bytes);
        block.txdata[0].output.push(TxOut {
            script_pubkey,
            value: Amount::ZERO,
        });
        let mut tx_hashes: Vec<_> = block.txdata.iter().map(Transaction::compute_txid).collect();
        block.header.merkle_root = merkle_tree::calculate_root_inline(&mut tx_hashes)
            .unwrap()
            .to_raw_hash()
            .into();
        Ok(block)
    }

    pub async fn mine(
        &mut self,
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
    ) -> Result<()> {
        let mut block = self.generate_block(coinbase_outputs, transactions).await?;
        loop {
            block.header.nonce += 1;
            if block.header.validate_pow(block.header.target()).is_ok() {
                break;
            }
        }
        let mut block_bytes = vec![];
        block.consensus_encode(&mut block_bytes).into_diagnostic()?;
        let block_hex = hex::encode(block_bytes);

        let () = self
            .main_client
            .submit_block(block_hex)
            .await
            .into_diagnostic()?;

        // FIXME: implement
        todo!();
        /*
        let block_hash = block.header.block_hash().as_byte_array().to_vec();
        let block_height: u32 = self
            .main_client
            .send_request("getblockcount", &[])
            .into_diagnostic()?
            .ok_or(miette!("failed to get block count"))?;
        let bmm_hashes: Vec<Vec<u8>> = self
            .get_bmm_requests()
            .await?
            .into_iter()
            .map(|(_sidechain_number, bmm_hash)| bmm_hash.to_vec())
            .collect();


        std::thread::sleep(Duration::from_millis(500));
        Ok(())
        */
    }

    pub fn get_balance(&self) -> Result<()> {
        if self.last_sync.read().is_none() {
            return Err(miette!("get balance: wallet not synced"));
        }

        let balance = self.bitcoin_wallet.lock().get_balance().into_diagnostic()?;
        let immature = Amount::from_sat(balance.immature);
        let untrusted_pending = Amount::from_sat(balance.untrusted_pending);
        let trusted_pending = Amount::from_sat(balance.trusted_pending);
        let confirmed = Amount::from_sat(balance.confirmed);

        tracing::trace!("Confirmed: {confirmed}");
        tracing::trace!("Immature: {immature}");
        tracing::trace!("Untrusted pending: {untrusted_pending}");
        tracing::trace!("Trusted pending: {trusted_pending}");
        Ok(())
    }

    pub fn sync(&self) -> Result<()> {
        let start = SystemTime::now();
        tracing::trace!("starting wallet sync");
        self.bitcoin_wallet
            .lock()
            .sync(&self.bitcoin_blockchain, SyncOptions::default())
            .into_diagnostic()?;
        tracing::debug!(
            "wallet sync complete in {:?}",
            start.elapsed().unwrap_or_default(),
        );
        *self.last_sync.write() = Some(SystemTime::now());
        Ok(())
    }

    pub fn get_utxos(&self) -> Result<()> {
        if self.last_sync.read().is_none() {
            return Err(miette!("get utxos: wallet not synced"));
        }

        let utxos = self
            .bitcoin_wallet
            .lock()
            .list_unspent()
            .into_diagnostic()?;
        for utxo in &utxos {
            tracing::trace!(
                "address: {}, value: {}",
                utxo.txout.script_pubkey,
                utxo.txout.value
            );
        }
        Ok(())
    }

    pub fn propose_sidechain(&self, sidechain_number: u8, data: &[u8]) -> Result<()> {
        self.db_connection
            .lock()
            .execute(
                "INSERT INTO sidechain_proposals (number, data) VALUES (?1, ?2)",
                (sidechain_number, data),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub fn ack_sidechain(&self, sidechain_number: u8, data_hash: &[u8; 32]) -> Result<()> {
        self.db_connection
            .lock()
            .execute(
                "INSERT INTO sidechain_acks (number, data_hash) VALUES (?1, ?2)",
                (sidechain_number, data_hash),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub fn nack_sidechain(&self, sidechain_number: u8, data_hash: &[u8; 32]) -> Result<()> {
        self.db_connection
            .lock()
            .execute(
                "DELETE FROM sidechain_acks WHERE number = ?1 AND data_hash = ?2",
                (sidechain_number, data_hash),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub fn get_sidechain_acks(&self) -> Result<Vec<SidechainAck>> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_> {
            let mut statement = connection
                .prepare("SELECT number, data_hash FROM sidechain_acks")
                .into_diagnostic()?;
            let rows = statement
                .query_map([], |row| {
                    let data_hash: [u8; 32] = row.get(1)?;
                    Ok(SidechainAck {
                        sidechain_number: row.get(0)?,
                        data_hash,
                    })
                })
                .into_diagnostic()?
                .collect::<Result<_, _>>()
                .into_diagnostic()?;
            Ok(rows)
        };
        with_connection(&self.db_connection.lock())
    }

    pub fn delete_sidechain_ack(&self, ack: &SidechainAck) -> Result<()> {
        self.db_connection
            .lock()
            .execute(
                "DELETE FROM sidechain_acks WHERE number = ?1 AND data_hash = ?2",
                (ack.sidechain_number, ack.data_hash),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub async fn get_pending_sidechain_proposals(
        &mut self,
    ) -> Result<HashMap<SidechainNumber, SidechainProposal>> {
        let pending_proposals = self
            .validator
            .get_sidechain_proposals()
            .map_err(|e| miette::miette!(e.to_string()))?
            .into_iter()
            .map(|(_, sidechain_proposal)| {
                (sidechain_proposal.sidechain_number, sidechain_proposal)
            })
            .collect();
        Ok(pending_proposals)
    }

    pub fn get_sidechain_proposals(&mut self) -> Result<Vec<Sidechain>> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_> {
            let mut statement = connection
                .prepare("SELECT number, data FROM sidechain_proposals")
                .into_diagnostic()?;
            let proposals = statement
                .query_map([], |row| {
                    let data: Vec<u8> = row.get(1)?;
                    let sidechain_number: u8 = row.get::<_, u8>(0)?;
                    Ok(Sidechain {
                        sidechain_number: sidechain_number.into(),
                        data,
                    })
                })
                .into_diagnostic()?
                .collect::<Result<_, _>>()
                .into_diagnostic()?;
            Ok(proposals)
        };
        with_connection(&self.db_connection.lock())
    }

    pub async fn get_sidechains(&mut self) -> Result<Vec<Sidechain>> {
        let sidechains = self
            .validator
            .get_sidechains()?
            .into_iter()
            .map(|sidechain| Sidechain {
                sidechain_number: sidechain.sidechain_number,
                data: sidechain.data,
            })
            .collect();
        Ok(sidechains)
    }

    pub async fn get_ctip(
        &mut self,
        sidechain_number: SidechainNumber,
    ) -> Result<Option<(bitcoin::OutPoint, Amount, u64)>> {
        let ctip = self.validator.get_ctip(sidechain_number)?;

        let sequence_number = self
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

    pub fn delete_sidechain_proposals(&self) -> Result<()> {
        self.db_connection
            .lock()
            .execute("DELETE FROM sidechain_proposals;", ())
            .into_diagnostic()?;
        Ok(())
    }

    pub async fn is_sidechain_active(&mut self, sidechain_number: SidechainNumber) -> Result<bool> {
        let sidechains = self.get_sidechains().await?;
        for sidechain in sidechains {
            if sidechain.sidechain_number == sidechain_number {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

type PendingTransactions = (
    SidechainNumber,
    Vec<(u64, cusf_sidechain_types::OutPoint, u64)>,
    Vec<cusf_sidechain_types::Output>,
);

type SidechainUTXOs = BTreeMap<u64, (cusf_sidechain_types::OutPoint, u32, u64, Option<u64>)>;

#[derive(Debug)]
pub struct Sidechain {
    pub sidechain_number: SidechainNumber,
    pub data: Vec<u8>,
}

fn get_block_value(height: u32, fees: Amount, network: Network) -> Amount {
    let subsidy_sats = 50 * Amount::ONE_BTC.to_sat();
    let subsidy_halving_interval = match network {
        Network::Regtest => 150,
        _ => SUBSIDY_HALVING_INTERVAL,
    };
    let halvings = height / subsidy_halving_interval;
    if halvings >= 64 {
        fees
    } else {
        fees + Amount::from_sat(subsidy_sats >> halvings)
    }
}

#[derive(Debug)]
pub struct SidechainAck {
    pub sidechain_number: u8,
    pub data_hash: [u8; 32],
}
