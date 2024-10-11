use bip300301::{
    client::{BlockchainInfo, GetRawTransactionClient, GetRawTransactionVerbose},
    jsonrpsee::http_client::HttpClient,
    MainClient,
};
use std::{
    collections::{BTreeMap, HashMap},
    io::Cursor,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use bdk::{
    bitcoin::{
        consensus::{Decodable as _, Encodable as _},
        hashes::Hash,
        Network,
    },
    blockchain::ElectrumBlockchain,
    database::SqliteDatabase,
    electrum_client::ConfigBuilder,
    keys::{
        bip39::{Language, Mnemonic},
        DerivableKey, ExtendedKey,
    },
    template::Bip84,
    wallet::AddressIndex,
    KeychainKind, SignOptions, SyncOptions,
};
use bip300301_messages::OP_DRIVECHAIN;
use bitcoin::{
    absolute::{Height, LockTime},
    base58,
    block::Version as BlockVersion,
    consensus::Encodable as _,
    constants::{genesis_block, SUBSIDY_HALVING_INTERVAL},
    hash_types::TxMerkleNode,
    hashes::Hash as _,
    merkle_tree,
    opcodes::{
        all::{OP_PUSHBYTES_1, OP_PUSHBYTES_36, OP_RETURN},
        OP_0, OP_TRUE,
    },
    transaction::Version as TxVersion,
    Amount, Block, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness,
};
use cusf_sidechain_types::{Hashable, HASH_LENGTH};
use miette::{miette, IntoDiagnostic, Result};
use rusqlite::{Connection, Row};
use tonic::transport::Channel;

use crate::{
    cli::WalletConfig,
    proto::sidechain::Client as SidechainClient,
    types::{Deposit, SidechainNumber, SidechainProposal},
    validator::Validator,
};

pub struct Wallet {
    main_client: HttpClient,
    validator: Validator,
    sidechain_clients: HashMap<SidechainNumber, SidechainClient<Channel>>,
    bitcoin_wallet: ThreadSafe<bdk::Wallet<SqliteDatabase>>,
    db_connection: ThreadSafe<rusqlite::Connection>,
    bitcoin_blockchain: ElectrumBlockchain,
    mnemonic: Mnemonic,
    // seed
    // sidechain number
    // index
    // address (20 byte hash of public key)
    // utxos
    last_sync: Arc<Mutex<Option<SystemTime>>>,
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
        .map(ThreadSafe::new);

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
                    "CREATE TABLE deposits
                   (sidechain_number INTEGER NOT NULL,
                    address BLOB NOT NULl,
                    amount INTEGER NOT NULL,
                    txid BLOB NOT NULL);",
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

        let sidechain_clients = HashMap::new();

        let wallet = Self {
            main_client,
            validator,
            sidechain_clients,
            bitcoin_wallet: bitcoin_wallet.into_diagnostic()?,
            db_connection: ThreadSafe::new(db_connection),
            bitcoin_blockchain,
            mnemonic,

            last_sync: Arc::new(Mutex::new(None)),
        };
        /*
        let sidechains = wallet.get_sidechains().await?;
        for sidechain in &sidechains {
            let endpoint = format!(
                "http://[::1]:{}",
                50052 + sidechain.sidechain_number.0 as u32
            );
            let sidechain_client = SidechainClient::connect(endpoint).await.into_diagnostic()?;
            wallet
                .sidechain_clients
                .insert(sidechain.sidechain_number, sidechain_client);
        }
        */
        Ok(wallet)
    }

    pub async fn generate_block(
        &self,
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
    ) -> Result<Block> {
        let addr = self
            .bitcoin_wallet
            .lock()?
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


        for (_sidechain_number, sidechain_client) in self.sidechain_clients.iter_mut() {
            let request = ConnectMainBlockRequest {
                block_height,
                block_hash: block_hash.clone(),
                bmm_hashes: bmm_hashes.clone(),
                deposits: vec![],
                withdrawal_bundle_event: None,
            };
            sidechain_client
                .connect_main_block(request)
                .await
                .into_diagnostic()?;
        }
        std::thread::sleep(Duration::from_millis(500));
        Ok(())
        */
    }

    pub fn get_balance(&self) -> Result<()> {
        if self
            .last_sync
            .lock()
            .map(|sync| sync.is_none())
            .unwrap_or_default()
        {
            return Err(miette!("get balance: wallet not synced"));
        }

        let balance = self
            .bitcoin_wallet
            .lock()?
            .get_balance()
            .into_diagnostic()?;
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
            .lock()?
            .sync(&self.bitcoin_blockchain, SyncOptions::default())
            .into_diagnostic()?;

        tracing::debug!(
            "wallet sync complete in {:?}",
            start.elapsed().unwrap_or_default(),
        );

        let mut last_sync = self
            .last_sync
            .lock()
            .map_err(|_| miette!("Failed to lock last_sync"))?;

        *last_sync = Some(SystemTime::now());

        Ok(())
    }

    pub fn get_utxos(&self) -> Result<()> {
        if self
            .last_sync
            .lock()
            .map(|sync| sync.is_none())
            .unwrap_or_default()
        {
            return Err(miette!("get utxos: wallet not synced"));
        }

        let utxos = self
            .bitcoin_wallet
            .lock()?
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
            .lock()?
            .execute(
                "INSERT INTO sidechain_proposals (number, data) VALUES (?1, ?2)",
                (sidechain_number, data),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub fn ack_sidechain(&self, sidechain_number: u8, data_hash: &[u8; 32]) -> Result<()> {
        self.db_connection
            .lock()?
            .execute(
                "INSERT INTO sidechain_acks (number, data_hash) VALUES (?1, ?2)",
                (sidechain_number, data_hash),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub fn nack_sidechain(&self, sidechain_number: u8, data_hash: &[u8; 32]) -> Result<()> {
        self.db_connection
            .lock()?
            .execute(
                "DELETE FROM sidechain_acks WHERE number = ?1 AND data_hash = ?2",
                (sidechain_number, data_hash),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub fn get_sidechain_acks(&self) -> Result<Vec<SidechainAck>> {
        let connection = self.db_connection.lock()?;
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
            .into_diagnostic()?;
        let mut acks = vec![];
        for ack in rows {
            let ack = ack.into_diagnostic()?;
            acks.push(ack);
        }
        Ok(acks)
    }

    pub fn delete_sidechain_ack(&self, ack: &SidechainAck) -> Result<()> {
        self.db_connection
            .lock()?
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
        let connection = self.db_connection.lock()?;
        let mut statement = connection
            .prepare("SELECT number, data FROM sidechain_proposals")
            .into_diagnostic()?;

        let rows = statement
            .query_map([], |row| {
                let data: Vec<u8> = row.get(1)?;
                let sidechain_number: u8 = row.get::<_, u8>(0)?;
                Ok(Sidechain {
                    sidechain_number: sidechain_number.into(),
                    data,
                })
            })
            .into_diagnostic()?;
        let mut proposals = vec![];
        for proposal in rows {
            let proposal = proposal.into_diagnostic()?;
            proposals.push(proposal);
        }

        Ok(proposals)
    }

    pub async fn get_sidechains(&mut self) -> Result<Vec<Sidechain>> {
        let sidechains = self
            .validator
            .get_sidechains()
            .map_err(|e| miette::miette!(e.to_string()))?
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
        let ctip = self
            .validator
            .get_ctip(sidechain_number)
            .map_err(|e| miette::miette!(e.to_string()));

        let sequence_number = self
            .validator
            .get_ctip_sequence_number(sidechain_number)?
            .unwrap();

        if let Ok(Some(ctip)) = ctip {
            let value = ctip.value;
            Ok(Some((ctip.outpoint, value, sequence_number)))
        } else {
            Ok(None)
        }
    }

    pub fn delete_sidechain_proposals(&self) -> Result<()> {
        self.db_connection
            .lock()?
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

    pub async fn deposit(
        &mut self,
        sidechain_number: SidechainNumber,
        amount: Amount,
        address: &str,
    ) -> Result<()> {
        if !self.is_sidechain_active(sidechain_number).await? {
            return Err(miette!(
                "sidechain slot {} is not active",
                sidechain_number.0
            ));
        }
        let message = [
            OP_DRIVECHAIN.to_u8(),
            OP_PUSHBYTES_1.to_u8(),
            sidechain_number.into(),
            OP_TRUE.to_u8(),
        ];
        let op_drivechain = bdk::bitcoin::ScriptBuf::from_bytes(message.into());

        let address = base58::decode(address).into_diagnostic()?;
        if address.len() != 20 {
            return Err(miette!(
                "invalid address length, is is {} bytes, when it must be 20 bytes",
                address.len()
            ));
        }
        let message = [vec![OP_RETURN.to_u8()], address.clone()].concat();
        let address_op_return = bdk::bitcoin::ScriptBuf::from_bytes(message);

        let ctip = self.get_ctip(sidechain_number).await?;

        // FIXME: Make this easier to read.
        let ctip_amount = ctip
            .map(|(_outpoint, amount, _sequence)| amount)
            .unwrap_or(Amount::ZERO);

        let ctip_outpoint_with_tx = if let Some((ctip_outpoint, _, _)) = ctip {
            let transaction_hex = self
                .main_client
                .get_raw_transaction(ctip_outpoint.txid, GetRawTransactionVerbose::<false>, None)
                .await
                .into_diagnostic()?;

            let ctip_outpoint = bdk::bitcoin::OutPoint {
                txid: bdk::bitcoin::Txid::from_byte_array(ctip_outpoint.txid.to_byte_array()),
                vout: ctip_outpoint.vout,
            };

            let transaction_bytes = hex::decode(transaction_hex).into_diagnostic()?;

            let transaction =
                bdk::bitcoin::Transaction::consensus_decode(&mut Cursor::new(transaction_bytes))
                    .into_diagnostic()?;

            Some((ctip_outpoint, transaction))
        } else {
            None
        };

        let wallet = self.bitcoin_wallet.lock()?;
        let mut builder = wallet.build_tx();
        builder
            .ordering(bdk::wallet::tx_builder::TxOrdering::Untouched)
            .add_recipient(op_drivechain.clone(), (ctip_amount + amount).to_sat())
            .add_recipient(address_op_return, 0);

        if let Some((ctip_outpoint, transaction)) = ctip_outpoint_with_tx {
            builder
                .add_foreign_utxo(
                    ctip_outpoint,
                    bdk::bitcoin::psbt::Input {
                        non_witness_utxo: Some(transaction),
                        ..bdk::bitcoin::psbt::Input::default()
                    },
                    0,
                )
                .into_diagnostic()?;
        }

        let (mut psbt, _details) = builder.finish().into_diagnostic()?;
        self.bitcoin_wallet
            .lock()?
            .sign(&mut psbt, SignOptions::default())
            .into_diagnostic()?;
        let transaction = psbt.extract_tx();

        let mut tx_data = vec![];
        let mut cursor = Cursor::new(&mut tx_data);
        transaction
            .consensus_encode(&mut cursor)
            .into_diagnostic()?;
        self.db_connection
            .lock()?
            .execute(
                "INSERT INTO deposits (sidechain_number, address, amount, txid) VALUES (?1, ?2, ?3, ?4)",
                (sidechain_number.0, &address, amount.to_sat(), transaction.txid().as_byte_array()),
            )
            .into_diagnostic()?;
        self.db_connection
            .lock()?
            .execute(
                "INSERT INTO mempool (txid, tx_data) VALUES (?1, ?2)",
                (transaction.txid().as_byte_array(), &tx_data),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub fn delete_deposits(&self) -> Result<()> {
        self.db_connection
            .lock()?
            .execute("DELETE FROM deposits;", ())
            .into_diagnostic()?;
        Ok(())
    }

    pub async fn get_deposits(&mut self, _sidechain_number: SidechainNumber) -> Result<()> {
        // FIXME: implement
        todo!()
        /*
        let deposits = self
            .validator
            .get_deposits(sidechain_number)
            .map_err(|e| miette::miette!(e.to_string()))?;
        dbg!(deposits);
        Ok(())
        */
    }

    pub fn get_pending_deposits(&self, sidechain_number: Option<u8>) -> Result<Vec<Deposit>> {
        let query = match sidechain_number {
            Some(_sidechain_number) => {
                "SELECT sidechain_number, address, amount, tx_data
                         FROM deposits INNER JOIN mempool ON deposits.txid = mempool.txid
                         WHERE sidechain_number = ?1;"
            }
            None => {
                "SELECT sidechain_number, address, amount, tx_data
                     FROM deposits INNER JOIN mempool ON deposits.txid = mempool.txid;"
            }
        };

        let connection = self.db_connection.lock()?;

        let mut statement = connection.prepare(query).into_diagnostic()?;

        // FIXME: Make this code more sane.
        let func = |_row: &Row| {
            // FIXME: implement
            todo!()
            /*
            let sidechain_number: u8 = row.get(0)?;
            let address: Vec<u8> = row.get(1)?;
            let amount: u64 = row.get(2)?;
            let tx_data: Vec<u8> = row.get(3)?;
            let transaction =
                Transaction::consensus_decode_from_finite_reader(&mut tx_data.as_slice()).unwrap();
            let deposit = Deposit {
                sidechain_id: sidechain_number.into(),
                address,
                amount,
                transaction,
            };
            Ok(deposit)
            */
        };
        let rows = match sidechain_number {
            Some(sidechain_number) => statement
                .query_map([sidechain_number], func)
                .into_diagnostic()?,
            None => statement.query_map([], func).into_diagnostic()?,
        };
        let mut deposits = vec![];
        for deposit in rows {
            let deposit = deposit.into_diagnostic()?;
            deposits.push(deposit);
        }
        Ok(deposits)
    }

    pub async fn get_bmm_requests(&mut self) -> Result<Vec<(SidechainNumber, [u8; HASH_LENGTH])>> {
        let mut bmm_requests = vec![];
        let active_sidechains = self.get_sidechains().await?;
        for sidechain in &active_sidechains {
            let (header, _coinbase, _transactions) =
                self.get_next_block(sidechain.sidechain_number).await?;
            let bmm_hash = header.hash();
            bmm_requests.push((sidechain.sidechain_number, bmm_hash));
        }
        Ok(bmm_requests)
    }

    pub async fn get_next_block(
        &mut self,
        sidechain_number: SidechainNumber,
    ) -> Result<(
        cusf_sidechain_types::Header,
        Vec<cusf_sidechain_types::Output>,
        Vec<cusf_sidechain_types::Transaction>,
    )> {
        let _sidechain_client = match self.sidechain_clients.get_mut(&sidechain_number) {
            Some(sidechain_client) => sidechain_client,
            None => return Err(miette!("sidechain is not active")),
        };
        // FIXME: implement
        todo!()
        /*
        let prev_side_block_hash = sidechain_client
            .get_chain_tip(GetChainTipRequest {})
            .await
            .into_diagnostic()?
            .into_inner()
            .block_hash;
        let prev_side_block_hash: [u8; HASH_LENGTH] = prev_side_block_hash.try_into().unwrap();
        let transactions_bytes = sidechain_client
            .collect_transactions(CollectTransactionsRequest {})
            .await
            .into_diagnostic()?
            .into_inner()
            .transactions;
        let transactions: Vec<cusf_sidechain_types::Transaction> =
            bincode::deserialize(&transactions_bytes).into_diagnostic()?;
        let coinbase = vec![];
        let merkle_root =
            cusf_sidechain_types::Header::compute_merkle_root(&coinbase, &transactions);
        let header = cusf_sidechain_types::Header {
            prev_side_block_hash,
            merkle_root,
        };
        tracing::debug!("header: {}", hex::encode(header.hash()));
        tracing::debug!(
            "prev_side_block_hash: {}",
            hex::encode(header.prev_side_block_hash)
        );
        tracing::debug!("merkle_root: {}", hex::encode(header.merkle_root));
        let block = (header, coinbase, transactions);
        Ok(block)
        */
    }

    pub async fn mine_side_block(&mut self, sidechain_number: SidechainNumber) -> Result<()> {
        let block = self.get_next_block(sidechain_number).await?;
        let _sidechain_client = match self.sidechain_clients.get_mut(&sidechain_number) {
            Some(sidechain_client) => sidechain_client,
            None => return Err(miette!("sidechain is not active")),
        };
        let _block = bincode::serialize(&block).into_diagnostic()?;
        // FIXME: implement
        todo!()
        /*
        let request = SubmitBlockRequest { block };
        let () = sidechain_client
            .submit_block(request)
            .await
            .into_diagnostic()?;
        Ok(())
        */
    }

    pub async fn get_withdrawal_bundle(
        &mut self,
        sidechain_number: SidechainNumber,
    ) -> Result<bitcoin::Transaction> {
        let _sidechain_client = self.sidechain_clients.get_mut(&sidechain_number).unwrap();
        // FIXME: implement
        todo!()
        /*
        let bundle = sidechain_client
            .get_withdrawal_bundle(GetWithdrawalBundleRequest {})
            .await
            .into_diagnostic()?
            .into_inner()
            .bundle;
        let bundle = bitcoin::consensus::deserialize(&bundle).into_diagnostic()?;
        Ok(bundle)
        */
    }
}

struct ThreadSafe<D> {
    underlying: Arc<Mutex<D>>,
}

impl<D> ThreadSafe<D> {
    pub fn new(underlying: D) -> Self {
        Self {
            underlying: Arc::new(Mutex::new(underlying)),
        }
    }

    pub fn lock(&self) -> Result<MutexGuard<'_, D>, miette::Error> {
        self.underlying
            .lock()
            .map_err(|e| miette::miette!("Mutex poison error: {}", e))
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
