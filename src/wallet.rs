use crate::gen::sidechain::sidechain_client::SidechainClient;
use crate::gen::sidechain::{
    CollectTransactionsRequest, ConnectMainBlockRequest, GetChainTipRequest, GetUtxoSetRequest,
    GetWithdrawalBundleRequest, SubmitBlockRequest, SubmitTransactionRequest,
};
use crate::types::SidechainProposal;
use crate::validator::Validator;
use bdk::blockchain::ElectrumBlockchain;
use bdk::database::SqliteDatabase;
use bdk::keys::bip39::{Language, Mnemonic};
use bdk::keys::{DerivableKey, ExtendedKey};
use bdk::template::Bip84;
use bdk::wallet::AddressIndex;
use bdk::{KeychainKind, SignOptions, SyncOptions};
use bip300301_messages::bitcoin::opcodes::all::{OP_PUSHBYTES_1, OP_PUSHBYTES_36};
use bip300301_messages::bitcoin::opcodes::OP_TRUE;
use bip300301_messages::bitcoin::Witness;
use bip300301_messages::OP_DRIVECHAIN;
use bitcoin::opcodes::all::OP_RETURN;
use bitcoin::{Amount, Network, Txid};
use cusf_sidechain_types::{
    Hashable, OutPoint, Output, ADDRESS_LENGTH, HASH_LENGTH, MAIN_ADDRESS_LENGTH,
};
use ed25519_dalek_bip32::{ChildIndex, DerivationPath, ExtendedSigningKey};
use miette::{miette, IntoDiagnostic, Result};
use rusqlite::{Connection, Row};
use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
use std::path::Path;
use std::str::FromStr;
use tonic::transport::Channel;

pub struct Wallet {
    main_client: Client,
    validator: Validator,
    sidechain_clients: HashMap<u8, SidechainClient<Channel>>,
    bitcoin_wallet: bdk::Wallet<SqliteDatabase>,
    db_connection: Connection,
    bitcoin_blockchain: ElectrumBlockchain,
    sidechain_wallet: Connection,
    mnemonic: Mnemonic,
    // seed
    // sidechain number
    // index
    // address (20 byte hash of public key)
    // utxos
}

impl Wallet {
    pub async fn new<P: AsRef<Path>>(datadir: P, validator: Validator) -> Result<Self> {
        let network = Network::Regtest; // Or this can be Network::Bitcoin, Network::Signet or Network::Regtest
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
        let xprv = xkey
            .into_xprv(network)
            .ok_or(miette!("couldn't get xprv"))?;

        std::fs::create_dir_all(&datadir).into_diagnostic()?;

        // Create a BDK wallet structure using BIP 84 descriptor ("m/84h/1h/0h/0" and "m/84h/1h/0h/1")
        let bitcoin_wallet = bdk::Wallet::new(
            Bip84(xprv, KeychainKind::External),
            Some(Bip84(xprv, KeychainKind::Internal)),
            network,
            SqliteDatabase::new(datadir.as_ref().join("wallet.sqlite")),
        )
        .into_diagnostic()?;

        let bitcoin_wallet_client =
            bdk::electrum_client::Client::new("127.0.0.1:60401").into_diagnostic()?;
        let bitcoin_blockchain = ElectrumBlockchain::from(bitcoin_wallet_client);

        use rusqlite_migration::{Migrations, M};

        let sidechain_wallet = {
            let migrations = Migrations::new(vec![
                M::up(
                    "CREATE TABLE keys
                   (sidechain_number INTEGER NOT NULL,
                    key_index INTEGER NOT NULL,
                    address BLOB NOT NULL,
                    PRIMARY KEY(sidechain_number, key_index)
                    );",
                ),
                M::up(
                    "CREATE TABLE utxos
                (id INTEGER NOT NULL PRIMARY KEY,
                 sidechain_number INTEGER NOT NULL,
                 key_index INTEGER NOT NULL,
                 value INTEGER NOT NULL,
                 main_fee INTEGER,
                 transaction_number INTEGER,
                 transaction_output_number INTEGER,
                 block_number INTEGER,
                 coinbase_output_number INTEGER,
                 deposit_number INTEGER,
                 FOREIGN KEY (sidechain_number, key_index) REFERENCES keys(sidechain_number, key_index),
                 UNIQUE (transaction_number, transaction_output_number),
                 UNIQUE (block_number, coinbase_output_number),
                 UNIQUE (deposit_number)
                 );"),
                M::up(
                    "CREATE TABLE transaction_inputs
                   (id INTEGER NOT NULL PRIMARY KEY,
                    sidechain_number INTEGER NOT NULL,
                    utxo_id INTEGER NOT NULL UNIQUE,
                    FOREIGN KEY(utxo_id) REFERENCES utxo(id)
                    );",
                ),
                M::up(
                    "CREATE TABLE transaction_outputs
                   (id INTEGER NOT NULL PRIMARY KEY,
                    sidechain_number INTEGER NOT NULL,
                    address BLOB NOT NULL,
                    value INTEGER NOT NULL,
                    main_address BLOB,
                    main_fee INTEGER);",
                ),
            ]);

            // seed
            // sidechain number
            // index
            // address (20 byte hash of public key)
            // utxos

            let mut sidechain_wallet =
                Connection::open(datadir.as_ref().join("sidechain_wallet.sqlite"))
                    .into_diagnostic()?;

            migrations
                .to_latest(&mut sidechain_wallet)
                .into_diagnostic()?;
            sidechain_wallet
        };

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

            let mut db_connection =
                Connection::open(datadir.as_ref().join("db.sqlite")).into_diagnostic()?;

            migrations.to_latest(&mut db_connection).into_diagnostic()?;
            db_connection
        };

        let main_datadir = Path::new("../../data/bitcoin/");
        let main_client = create_client(main_datadir)?;

        let sidechain_clients = HashMap::new();

        let mut wallet = Self {
            main_client,
            validator,
            sidechain_clients,
            bitcoin_wallet,
            db_connection,
            sidechain_wallet,
            bitcoin_blockchain,
            mnemonic,
        };
        let sidechains = wallet.get_sidechains().await?;
        for sidechain in &sidechains {
            let endpoint = format!("http://[::1]:{}", 50052 + sidechain.sidechain_number as u32);
            let sidechain_client = SidechainClient::connect(endpoint).await.into_diagnostic()?;
            wallet.sidechain_clients.insert(0, sidechain_client);
        }
        Ok(wallet)
    }

    pub fn get_new_sidechain_address(
        &mut self,
        sidechain_number: u8,
    ) -> Result<(u32, [u8; ADDRESS_LENGTH])> {
        let tx = self.sidechain_wallet.transaction().into_diagnostic()?;
        let mut key_index = tx
            .query_row("SELECT MAX(key_index) FROM keys;", [], |row| {
                Ok(row.get(0).unwrap_or(0))
            })
            .into_diagnostic()?;
        key_index += 1;
        let seed = self.mnemonic.to_seed("");
        let xpriv = ExtendedSigningKey::from_seed(&seed).into_diagnostic()?;
        let derivation_path = DerivationPath::new([
            ChildIndex::Hardened(1),
            ChildIndex::Hardened(0),
            ChildIndex::Hardened(0),
            ChildIndex::Hardened(sidechain_number as u32 + 1),
            ChildIndex::Hardened(key_index),
        ]);
        let child = xpriv.derive(&derivation_path).into_diagnostic()?;
        let verifying_key = child.verifying_key();
        let verifying_key_bytes = verifying_key.to_bytes();
        let mut hasher = blake3::Hasher::new();
        hasher.update(&verifying_key_bytes);
        let mut address_reader = hasher.finalize_xof();
        let mut address = [0; ADDRESS_LENGTH];
        address_reader.fill(&mut address);
        tx.execute(
            "INSERT INTO keys (sidechain_number, key_index, address) VALUES (?1, ?2, ?3)",
            (&sidechain_number, &key_index, &address),
        )
        .into_diagnostic()?;
        tx.commit().into_diagnostic()?;
        Ok((key_index, address))
    }

    pub fn get_block_height(&self) -> Result<u32> {
        let block_height: u32 = self
            .main_client
            .send_request("getblockcount", &[])
            .into_diagnostic()?
            .ok_or(miette!("failed to get block count"))?;
        Ok(block_height)
    }

    pub async fn generate_block(
        &self,
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
    ) -> Result<Block> {
        let addr = self
            .bitcoin_wallet
            .get_address(AddressIndex::New)
            .into_diagnostic()?;
        let script_pubkey = addr.script_pubkey();
        let block_height = self.get_block_height()?;
        println!("Block height: {block_height}");
        let block_hash: String = self
            .main_client
            .send_request("getblockhash", &[json!(block_height)])
            .into_diagnostic()?
            .ok_or(miette!("failed to get block hash"))?;
        let prev_blockhash = BlockHash::from_str(&block_hash).into_diagnostic()?;

        let start = SystemTime::now();
        let time = start
            .duration_since(UNIX_EPOCH)
            .into_diagnostic()?
            .as_secs() as u32;

        let script_sig = bitcoin::blockdata::script::Builder::new()
            .push_int((block_height + 1) as i64)
            .push_opcode(OP_0)
            .into_script();
        let value = get_block_value(block_height + 1, 0, Network::Regtest);

        let output = if value > 0 {
            vec![TxOut {
                script_pubkey,
                value,
            }]
        } else {
            vec![TxOut {
                script_pubkey: ScriptBuf::builder().push_opcode(OP_RETURN).into_script(),
                value: 0,
            }]
        };

        const WITNESS_RESERVED_VALUE: [u8; 32] = [0; 32];

        let txdata = [
            vec![Transaction {
                version: 2,
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
            version: Version::NO_SOFT_FORK_SIGNALLING,
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
            value: 0,
        });
        let mut tx_hashes: Vec<_> = block.txdata.iter().map(Transaction::txid).collect();
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

        let _: Option<()> = self
            .main_client
            .send_request("submitblock", &[json!(block_hex)])
            .into_diagnostic()?;

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
    }

    pub fn get_balance(&self) -> Result<()> {
        self.bitcoin_wallet
            .sync(&self.bitcoin_blockchain, SyncOptions::default())
            .into_diagnostic()?;
        let balance = self.bitcoin_wallet.get_balance().into_diagnostic()?;
        let immature = Amount::from_sat(balance.immature);
        let untrusted_pending = Amount::from_sat(balance.untrusted_pending);
        let trusted_pending = Amount::from_sat(balance.trusted_pending);
        let confirmed = Amount::from_sat(balance.confirmed);
        println!("Confirmed: {confirmed}");
        println!("Immature: {immature}");
        println!("Untrusted pending: {untrusted_pending}");
        println!("Trusted pending: {trusted_pending}");
        Ok(())
    }

    pub fn get_utxos(&self) -> Result<()> {
        self.bitcoin_wallet
            .sync(&self.bitcoin_blockchain, SyncOptions::default())
            .into_diagnostic()?;
        let utxos = self.bitcoin_wallet.list_unspent().into_diagnostic()?;
        for utxo in &utxos {
            println!(
                "address: {}, value: {}",
                utxo.txout.script_pubkey, utxo.txout.value
            );
        }
        Ok(())
    }

    pub fn propose_sidechain(&self, sidechain_number: u8, data: &[u8]) -> Result<()> {
        self.db_connection
            .execute(
                "INSERT INTO sidechain_proposals (number, data) VALUES (?1, ?2)",
                (sidechain_number, data),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub fn ack_sidechain(&self, sidechain_number: u8, data_hash: &[u8; 32]) -> Result<()> {
        self.db_connection
            .execute(
                "INSERT INTO sidechain_acks (number, data_hash) VALUES (?1, ?2)",
                (sidechain_number, data_hash),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub fn nack_sidechain(&self, sidechain_number: u8, data_hash: &[u8; 32]) -> Result<()> {
        self.db_connection
            .execute(
                "DELETE FROM sidechain_acks WHERE number = ?1 AND data_hash = ?2",
                (sidechain_number, data_hash),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub fn get_sidechain_acks(&self) -> Result<Vec<SidechainAck>> {
        let mut statement = self
            .db_connection
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
            .execute(
                "DELETE FROM sidechain_acks WHERE number = ?1 AND data_hash = ?2",
                (ack.sidechain_number, ack.data_hash),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub async fn get_pending_sidechain_proposals(
        &mut self,
    ) -> Result<HashMap<u8, SidechainProposal>> {
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
        let mut statement = self
            .db_connection
            .prepare("SELECT number, data FROM sidechain_proposals")
            .into_diagnostic()?;
        let rows = statement
            .query_map([], |row| {
                let data: Vec<u8> = row.get(1)?;
                Ok(Sidechain {
                    sidechain_number: row.get(0)?,
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
        sidechain_number: u8,
    ) -> Result<Option<(bitcoin::OutPoint, u64, u64)>> {
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
            .execute("DELETE FROM sidechain_proposals;", ())
            .into_diagnostic()?;
        Ok(())
    }

    pub async fn is_sidechain_active(&mut self, sidechain_number: u8) -> Result<bool> {
        let sidechains = self.get_sidechains().await?;
        for sidechain in sidechains {
            if sidechain.sidechain_number == sidechain_number {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn send(
        &mut self,
        sidechain_number: u8,
        address: &str,
        value: u64,
        fee: u64,
    ) -> Result<()> {
        let address = base58::decode(address).into_diagnostic()?;

        let (_, change_address) = self.get_new_sidechain_address(sidechain_number)?;
        let tx = self.sidechain_wallet.transaction().into_diagnostic()?;
        let transaction = {
            let mut statement = tx.prepare("SELECT value, deposit_number FROM utxos WHERE sidechain_number = ?1 ORDER BY value ASC").into_diagnostic()?;
            let deposits: Vec<_> = statement
                .query_map([sidechain_number], |row| {
                    let value: u64 = row.get(0)?;
                    let deposit_number: u64 = row.get(1)?;
                    Ok((value, deposit_number))
                })
                .into_diagnostic()?
                .collect();
            let mut inputs = vec![];
            let mut value_in = 0;
            for deposit in deposits {
                let (deposit_value, deposit_number) = deposit.unwrap();
                value_in += deposit_value;
                let input = cusf_sidechain_types::OutPoint::Deposit {
                    sequence_number: deposit_number,
                };
                inputs.push(input);
                if value_in >= value {
                    break;
                }
            }
            let mut outputs = vec![];
            let output = cusf_sidechain_types::Output::Regular {
                address: address.try_into().unwrap(),
                value,
            };
            outputs.push(output);
            let change_value = value_in - value - fee;
            if change_value > 0 {
                let change = cusf_sidechain_types::Output::Regular {
                    address: change_address,
                    value: change_value,
                };
                outputs.push(change);
            }

            cusf_sidechain_types::Transaction { inputs, outputs }
        };
        tx.commit().into_diagnostic()?;
        let transaction_bytes = bincode::serialize(&transaction).into_diagnostic()?;
        let sidechain_client = self.sidechain_clients.get_mut(&0).unwrap();
        let request = SubmitTransactionRequest {
            transaction: transaction_bytes,
        };
        sidechain_client
            .submit_transaction(request)
            .await
            .into_diagnostic()?;
        Ok(())
    }

    pub async fn deposit(
        &mut self,
        sidechain_number: u8,
        amount: u64,
        address: &Option<String>,
    ) -> Result<()> {
        if !self.is_sidechain_active(sidechain_number).await? {
            return Err(miette!("sidechain slot {sidechain_number} is not active"));
        }
        let message = [
            OP_DRIVECHAIN.to_u8(),
            OP_PUSHBYTES_1.to_u8(),
            sidechain_number,
            OP_TRUE.to_u8(),
        ];
        let op_drivechain = ScriptBuf::from_bytes(message.into());

        let sequence_number = self.validator.get_ctip_sequence_number(sidechain_number)?;

        let deposit_number = match sequence_number {
            Some(sequence_number) => sequence_number + 1,
            None => 0,
        };
        let (key_index, address) = match address {
            Some(address) => (None, base58::decode(address).into_diagnostic()?),
            None => {
                let (key_index, address) = self.get_new_sidechain_address(sidechain_number)?;
                (Some(key_index), address.to_vec())
            }
        };
        if address.len() != 20 {
            return Err(miette!(
                "invalid address length, is is {} bytes, when it must be 20 bytes",
                address.len()
            ));
        }
        let message = [vec![OP_RETURN.to_u8()], address.clone()].concat();
        let address_op_return = ScriptBuf::from_bytes(message);

        let ctip = self.get_ctip(sidechain_number).await?;

        // FIXME: Make this easier to read.
        let ctip_amount = ctip.map(|ctip| ctip.1).unwrap_or(0);

        let mut builder = self.bitcoin_wallet.build_tx();
        builder
            .ordering(bdk::wallet::tx_builder::TxOrdering::Untouched)
            .add_recipient(op_drivechain.clone(), ctip_amount + amount)
            .add_recipient(address_op_return, 0);

        if let Some((ctip_outpoint, _, _)) = ctip {
            let transaction_hex: String = self
                .main_client
                .send_request("getrawtransaction", &[json!(ctip_outpoint.txid)])
                .into_diagnostic()?
                .unwrap();
            let transaction_bytes = hex::decode(transaction_hex).unwrap();
            let mut cursor = Cursor::new(transaction_bytes);
            let transaction = Transaction::consensus_decode(&mut cursor).into_diagnostic()?;
            builder
                .add_foreign_utxo(
                    ctip_outpoint,
                    bitcoin::psbt::Input {
                        non_witness_utxo: Some(transaction),
                        ..bitcoin::psbt::Input::default()
                    },
                    0,
                )
                .into_diagnostic()?;
        }

        let (mut psbt, _details) = builder.finish().into_diagnostic()?;
        self.bitcoin_wallet
            .sign(&mut psbt, SignOptions::default())
            .into_diagnostic()?;
        let transaction = psbt.extract_tx();

        let mut tx_data = vec![];
        let mut cursor = Cursor::new(&mut tx_data);
        transaction
            .consensus_encode(&mut cursor)
            .into_diagnostic()?;
        self.db_connection
            .execute(
                "INSERT INTO deposits (sidechain_number, address, amount, txid) VALUES (?1, ?2, ?3, ?4)",
                (sidechain_number, &address, amount, transaction.txid().as_byte_array()),
            )
            .into_diagnostic()?;
        self.db_connection
            .execute(
                "INSERT INTO mempool (txid, tx_data) VALUES (?1, ?2)",
                (transaction.txid().as_byte_array(), &tx_data),
            )
            .into_diagnostic()?;
        if let Some(key_index) = key_index {
            self.sidechain_wallet
            .execute(
                "INSERT INTO utxos (sidechain_number, key_index, value, deposit_number) VALUES (?1, ?2, ?3, ?4)",
                (&sidechain_number, &key_index, &amount, &deposit_number),
            )
            .into_diagnostic()?;
        }
        Ok(())
    }

    pub fn delete_deposits(&self) -> Result<()> {
        self.db_connection
            .execute("DELETE FROM deposits;", ())
            .into_diagnostic()?;
        Ok(())
    }

    pub async fn get_deposits(&mut self, sidechain_number: u8) -> Result<()> {
        let deposits = self
            .validator
            .get_deposits(sidechain_number)
            .map_err(|e| miette::miette!(e.to_string()))?;
        dbg!(deposits);
        Ok(())
    }

    pub fn get_pending_deposits(&self, sidechain_number: Option<u8>) -> Result<Vec<Deposit>> {
        let mut statement = match sidechain_number {
            Some(_sidechain_number) => self
                .db_connection
                .prepare(
                    "SELECT sidechain_number, address, amount, tx_data
                         FROM deposits INNER JOIN mempool ON deposits.txid = mempool.txid
                         WHERE sidechain_number = ?1;",
                )
                .into_diagnostic()?,
            None => self
                .db_connection
                .prepare(
                    "SELECT sidechain_number, address, amount, tx_data
                     FROM deposits INNER JOIN mempool ON deposits.txid = mempool.txid;",
                )
                .into_diagnostic()?,
        };
        // FIXME: Make this code more sane.
        let func = |row: &Row| {
            let sidechain_number: u8 = row.get(0)?;
            let address: Vec<u8> = row.get(1)?;
            let amount: u64 = row.get(2)?;
            let tx_data: Vec<u8> = row.get(3)?;
            let transaction =
                Transaction::consensus_decode_from_finite_reader(&mut tx_data.as_slice()).unwrap();
            let deposit = Deposit {
                sidechain_number,
                address,
                amount,
                transaction,
            };
            Ok(deposit)
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

    pub async fn get_bmm_requests(&mut self) -> Result<Vec<(u8, [u8; HASH_LENGTH])>> {
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
        sidechain_number: u8,
    ) -> Result<(
        cusf_sidechain_types::Header,
        Vec<cusf_sidechain_types::Output>,
        Vec<cusf_sidechain_types::Transaction>,
    )> {
        let sidechain_client = match self.sidechain_clients.get_mut(&sidechain_number) {
            Some(sidechain_client) => sidechain_client,
            None => return Err(miette!("sidechain is not active")),
        };
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
        println!("header: {}", hex::encode(header.hash()));
        println!(
            "prev_side_block_hash: {}",
            hex::encode(header.prev_side_block_hash)
        );
        println!("merkle_root: {}", hex::encode(header.merkle_root));
        let block = (header, coinbase, transactions);
        Ok(block)
    }

    pub async fn mine_side_block(&mut self, sidechain_number: u8) -> Result<()> {
        let block = self.get_next_block(sidechain_number).await?;
        let sidechain_client = match self.sidechain_clients.get_mut(&sidechain_number) {
            Some(sidechain_client) => sidechain_client,
            None => return Err(miette!("sidechain is not active")),
        };
        let block = bincode::serialize(&block).into_diagnostic()?;
        let request = SubmitBlockRequest { block };
        sidechain_client
            .submit_block(request)
            .await
            .into_diagnostic()?;
        Ok(())
    }

    pub fn add_output(&mut self, value: u64, main_fee: Option<u64>) -> Result<()> {
        let (sidechain_number, ids_outpoints_values, outputs) = self.get_pending_transaction()?;
        let (_key_index, address) = self.get_new_sidechain_address(sidechain_number)?;
        let value_in: u64 = ids_outpoints_values
            .iter()
            .map(|(_id, _outpoint, value)| value)
            .sum();
        let mut value_out: u64 = outputs.iter().map(|output| output.total_value()).sum();

        let main_address = {
            if main_fee.is_none() {
                None
            } else {
                /*
                let main_address = self
                    .bitcoin_wallet
                    .get_address(AddressIndex::New)
                    .into_diagnostic()?;
                let main_address = match main_address.address.payload {
                    bitcoin::address::Payload::PubkeyHash(pubkey_hash) => {
                        pubkey_hash.to_byte_array()
                    }
                    bitcoin::address::Payload::ScriptHash(script_hash) => {
                        script_hash.to_byte_array()
                    }
                    bitcoin::address::Payload::WitnessProgram(witness_hash) => {
                        witness_hash.to_byte_array()
                    }
                    _ => todo!(),
                };
                */
                let main_address = [0; MAIN_ADDRESS_LENGTH];
                Some(main_address)
            }
        };
        let output = match (main_address, main_fee) {
            (Some(main_address), Some(main_fee)) => cusf_sidechain_types::Output::Withdrawal {
                address,
                main_address,
                value,
                fee: main_fee,
            },
            (None, None) => cusf_sidechain_types::Output::Regular { address, value },
            _ => {
                return Err(miette!("invalid arguments to add_output"));
            }
        };
        value_out += output.total_value();
        if value_in < value_out {
            return Err(miette!("not enough value in"));
        }
        self.sidechain_wallet
            .execute(
                "INSERT INTO transaction_outputs
            (sidechain_number, address, value, main_address, main_fee)
            VALUES (?1, ?2, ?3, ?4, ?5)",
                (sidechain_number, address, value, main_address, main_fee),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub fn spend(&self, utxo_id: u64) -> Result<()> {
        let utxo_sidechain_number = self
            .sidechain_wallet
            .query_row(
                "SELECT sidechain_number FROM utxos WHERE id = ?1",
                [utxo_id],
                |row| {
                    let sidechain_number: u8 = row.get(0)?;
                    Ok(sidechain_number)
                },
            )
            .into_diagnostic()?;
        let mut statement = self
            .sidechain_wallet
            .prepare("SELECT sidechain_number FROM transaction_inputs")
            .into_diagnostic()?;
        let sidechain_numbers: Vec<_> = statement
            .query_map([], |row| {
                let sidechain_number: u8 = row.get(0)?;
                Ok(sidechain_number)
            })
            .into_diagnostic()?
            .collect();
        for sidechain_number in sidechain_numbers {
            let sidechain_number = sidechain_number.into_diagnostic()?;
            if utxo_sidechain_number != sidechain_number {
                return Err(miette!("trying to add a utxo from sidechain {utxo_sidechain_number} to a transaction for sidechainn {sidechain_number}"));
            }
        }
        self.sidechain_wallet
            .execute(
                "INSERT INTO transaction_inputs (sidechain_number, utxo_id) VALUES (?1, ?2)",
                (utxo_sidechain_number, utxo_id),
            )
            .into_diagnostic()?;
        Ok(())
    }

    pub fn clear_pending_transaction(&mut self) -> Result<()> {
        let tx = self.sidechain_wallet.transaction().into_diagnostic()?;
        tx.execute("DELETE FROM transaction_inputs", ())
            .into_diagnostic()?;
        tx.execute("DELETE FROM transaction_outputs", ())
            .into_diagnostic()?;
        tx.commit().into_diagnostic()?;
        Ok(())
    }

    pub async fn submit_pending_transaction(&mut self) -> Result<()> {
        let (sidechain_number, ids_outpoints_values, outputs) = self.get_pending_transaction()?;
        let inputs = ids_outpoints_values
            .iter()
            .map(|(_id, outpoint, _value)| outpoint)
            .cloned()
            .collect();
        let transaction = cusf_sidechain_types::Transaction { inputs, outputs };
        let transaction_bytes = bincode::serialize(&transaction).into_diagnostic()?;

        let sidechain_client = self.sidechain_clients.get_mut(&sidechain_number).unwrap();
        let request = SubmitTransactionRequest {
            transaction: transaction_bytes,
        };

        sidechain_client
            .submit_transaction(request)
            .await
            .into_diagnostic()?;
        {
            // FIXME: This is a crutch.
            // TODO: Implement actual syncing of utxos from sidechain.
            let tx = self.sidechain_wallet.transaction().into_diagnostic()?;
            for (id, _outpoint, _value) in &ids_outpoints_values {
                tx.execute("DELETE FROM utxos WHERE id = ?1", (id,))
                    .into_diagnostic()?;
            }
            tx.commit().into_diagnostic()?;
        }
        self.clear_pending_transaction()?;
        Ok(())
    }

    pub fn get_pending_transaction(&self) -> Result<PendingTransactions> {
        let mut statement = self
            .sidechain_wallet
            .prepare(
                "SELECT id, value,
                transaction_number, transaction_output_number,
                block_number, coinbase_output_number,
                deposit_number
                FROM utxos WHERE id IN (SELECT utxo_id FROM transaction_inputs)",
            )
            .into_diagnostic()?;
        let utxos: Vec<_> = statement
            .query_map([], |row| {
                let id: u64 = row.get(0)?;
                let value: u64 = row.get(1)?;
                let transaction_number: Option<u64> = row.get(2)?;
                let transaction_output_number: Option<u8> = row.get(3)?;
                let block_number: Option<u32> = row.get(4)?;
                let coinbases_output_number: Option<u8> = row.get(5)?;
                let deposit_number: Option<u64> = row.get(6)?;
                Ok((
                    id,
                    value,
                    transaction_number,
                    transaction_output_number,
                    block_number,
                    coinbases_output_number,
                    deposit_number,
                ))
            })
            .into_diagnostic()?
            .collect();
        if utxos.is_empty() {
            return Err(miette!("no pending transaction"));
        }
        let sidechain_number = {
            let (utxo_id, _, _, _, _, _, _) = utxos[0].as_ref().unwrap();
            self.sidechain_wallet
                .query_row(
                    "SELECT sidechain_number FROM utxos WHERE id = ?1",
                    [utxo_id],
                    |row| {
                        let sidechain_number: u8 = row.get(0)?;
                        Ok(sidechain_number)
                    },
                )
                .into_diagnostic()?
        };

        let mut ids_outpoints_values = vec![];
        for utxo in utxos {
            let (
                id,
                value,
                transaction_number,
                transaction_output_number,
                block_number,
                coinbase_output_number,
                deposit_number,
            ) = utxo.unwrap();
            let outpoint = match (
                transaction_number,
                transaction_output_number,
                block_number,
                coinbase_output_number,
                deposit_number,
            ) {
                (Some(transaction_number), Some(output_number), None, None, None) => {
                    cusf_sidechain_types::OutPoint::Regular {
                        transaction_number,
                        output_number,
                    }
                }
                (None, None, Some(block_number), Some(output_number), None) => {
                    cusf_sidechain_types::OutPoint::Coinbase {
                        block_number,
                        output_number,
                    }
                }
                (None, None, None, None, Some(sequence_number)) => {
                    cusf_sidechain_types::OutPoint::Deposit { sequence_number }
                }
                _ => {
                    todo!();
                }
            };
            ids_outpoints_values.push((id, outpoint, value));
        }

        let mut statement = self
            .sidechain_wallet
            .prepare("SELECT id, address, value, main_address, main_fee FROM transaction_outputs")
            .into_diagnostic()?;

        let raw_outputs: Vec<_> = statement
            .query_map([], |row| {
                let id: u64 = row.get(0)?;
                let address: Vec<u8> = row.get(1)?;
                let address: [u8; ADDRESS_LENGTH] = address.try_into().unwrap();
                let value: u64 = row.get(2)?;
                let main_address: Option<Vec<u8>> = row.get(3)?;
                let main_address: Option<[u8; MAIN_ADDRESS_LENGTH]> =
                    main_address.map(|main_address| main_address.try_into().unwrap());
                let main_fee: Option<u64> = row.get(4)?;
                Ok((id, address, value, main_address, main_fee))
            })
            .into_diagnostic()?
            .collect();

        let mut outputs = vec![];

        for raw_output in &raw_outputs {
            let (_id, address, value, main_address, main_fee) = raw_output.as_ref().unwrap();
            let output = match (main_address, main_fee) {
                (Some(main_address), Some(main_fee)) => cusf_sidechain_types::Output::Withdrawal {
                    address: *address,
                    main_address: *main_address,
                    value: *value,
                    fee: *main_fee,
                },
                (None, None) => cusf_sidechain_types::Output::Regular {
                    address: *address,
                    value: *value,
                },
                _ => return Err(miette!("invalid output in database")),
            };
            outputs.push(output);
        }

        Ok((sidechain_number, ids_outpoints_values, outputs))
    }

    pub async fn sync_side_utxos(&mut self, sidechain_number: u8) -> Result<()> {
        let sidechain_client = self.sidechain_clients.get_mut(&sidechain_number).unwrap();
        let utxos = sidechain_client
            .get_utxo_set(GetUtxoSetRequest {})
            .await
            .into_diagnostic()?
            .into_inner()
            .utxos;
        let utxos: HashMap<OutPoint, Output> = bincode::deserialize(&utxos).into_diagnostic()?;
        let wallet_utxos = self.get_side_utxos(sidechain_number)?;
        let tx = self.sidechain_wallet.transaction().into_diagnostic()?;
        for (id, (outpoint, _key_index, _value, _main_fee)) in &wallet_utxos {
            if !utxos.contains_key(outpoint) {
                tx.execute("DELETE FROM utxos WHERE id = ?1", (id,))
                    .into_diagnostic()?;
            }
        }
        let wallet_addresses = {
            let mut statement = tx
                .prepare("SELECT address, key_index FROM keys WHERE sidechain_number = ?1")
                .into_diagnostic()?;
            let addresses: HashMap<[u8; ADDRESS_LENGTH], u32> = statement
                .query_map([sidechain_number], |row| {
                    let address: Vec<u8> = row.get(0)?;
                    let address: [u8; ADDRESS_LENGTH] = address.try_into().unwrap();
                    let key_index: u32 = row.get(1)?;
                    Ok((address, key_index))
                })
                .into_diagnostic()?
                .map(|address| address.unwrap())
                .collect();
            addresses
        };
        for (outpoint, output) in &utxos {
            let address = output.address();
            if let Some(key_index) = wallet_addresses.get(&address) {
                let (value, main_fee) = match output {
                    cusf_sidechain_types::Output::Withdrawal { value, fee, .. } => {
                        (value, Some(fee))
                    }
                    cusf_sidechain_types::Output::Regular { value, .. } => (value, None),
                };
                match outpoint {
                    cusf_sidechain_types::OutPoint::Regular {
                        transaction_number,
                        output_number,
                    } => {
                        tx.execute(
                            "INSERT OR IGNORE INTO utxos
                            (sidechain_number,
                             key_index,
                             value,
                             main_fee,
                             transaction_number,
                             transaction_output_number)
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6);",
                            (
                                sidechain_number,
                                key_index,
                                value,
                                main_fee,
                                transaction_number,
                                output_number,
                            ),
                        )
                        .into_diagnostic()?;
                    }
                    cusf_sidechain_types::OutPoint::Deposit { sequence_number } => {
                        tx.execute(
                            "INSERT OR IGNORE INTO utxos
                            (sidechain_number,
                             key_index,
                             value,
                             main_fee,
                             deposit_number)
                            VALUES (?1, ?2, ?3, ?4, ?5)",
                            (
                                sidechain_number,
                                key_index,
                                value,
                                main_fee,
                                sequence_number,
                            ),
                        )
                        .into_diagnostic()?;
                    }
                    cusf_sidechain_types::OutPoint::Coinbase {
                        block_number,
                        output_number,
                    } => {
                        tx.execute(
                            "INSERT OR IGNORE INTO utxos
                            (sidechain_number,
                             key_index,
                             value,
                             main_fee,
                             block_number,
                             coinbase_output_number)
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            (
                                sidechain_number,
                                key_index,
                                value,
                                main_fee,
                                block_number,
                                output_number,
                            ),
                        )
                        .into_diagnostic()?;
                    }
                }
            }
            let total_value = Amount::from_sat(output.total_value());
            let output_type = match output {
                cusf_sidechain_types::Output::Regular { .. } => "regular",
                cusf_sidechain_types::Output::Withdrawal { .. } => "withdrawal",
            };
            println!("{outpoint} : {output_type} : {total_value}",);
        }
        tx.commit().into_diagnostic()?;
        Ok(())
    }

    pub fn get_side_utxos(&self, sidechain_number: u8) -> Result<SidechainUTXOs> {
        let mut statement = self
            .sidechain_wallet
            .prepare(
                "SELECT id, key_index, value, main_fee,
                transaction_number, transaction_output_number,
                block_number, coinbase_output_number,
                deposit_number
                FROM utxos WHERE sidechain_number = ?1",
            )
            .into_diagnostic()?;
        let utxos: Vec<_> = statement
            .query_map([sidechain_number], |row| {
                let id: u64 = row.get(0)?;
                let key_index: u32 = row.get(1)?;
                let value: u64 = row.get(2)?;
                let main_fee: Option<u64> = row.get(3)?;
                let transaction_number: Option<u64> = row.get(4)?;
                let transaction_output_number: Option<u8> = row.get(5)?;
                let block_number: Option<u32> = row.get(6)?;
                let coinbases_output_number: Option<u8> = row.get(7)?;
                let deposit_number: Option<u64> = row.get(8)?;
                Ok((
                    id,
                    key_index,
                    value,
                    main_fee,
                    transaction_number,
                    transaction_output_number,
                    block_number,
                    coinbases_output_number,
                    deposit_number,
                ))
            })
            .into_diagnostic()?
            .collect();
        let mut id_to_outpoint_key_index_value_main_fee = BTreeMap::new();
        for utxo in utxos {
            let (
                id,
                key_index,
                value,
                main_fee,
                transaction_number,
                transaction_output_number,
                block_number,
                coinbase_output_number,
                deposit_number,
            ) = utxo.unwrap();
            let outpoint = match (
                transaction_number,
                transaction_output_number,
                block_number,
                coinbase_output_number,
                deposit_number,
            ) {
                (Some(transaction_number), Some(output_number), None, None, None) => {
                    cusf_sidechain_types::OutPoint::Regular {
                        transaction_number,
                        output_number,
                    }
                }
                (None, None, Some(block_number), Some(output_number), None) => {
                    cusf_sidechain_types::OutPoint::Coinbase {
                        block_number,
                        output_number,
                    }
                }
                (None, None, None, None, Some(sequence_number)) => {
                    cusf_sidechain_types::OutPoint::Deposit { sequence_number }
                }
                _ => {
                    todo!();
                }
            };
            id_to_outpoint_key_index_value_main_fee
                .insert(id, (outpoint, key_index, value, main_fee));
        }
        Ok(id_to_outpoint_key_index_value_main_fee)
    }

    pub async fn get_withdrawal_bundle(
        &mut self,
        sidechain_number: u8,
    ) -> Result<bitcoin::Transaction> {
        let sidechain_client = self.sidechain_clients.get_mut(&sidechain_number).unwrap();
        let bundle = sidechain_client
            .get_withdrawal_bundle(GetWithdrawalBundleRequest {})
            .await
            .into_diagnostic()?
            .into_inner()
            .bundle;
        let mut cursor = Cursor::new(bundle);
        let bundle = bitcoin::Transaction::consensus_decode(&mut cursor).into_diagnostic()?;
        Ok(bundle)
    }
}

type PendingTransactions = (
    u8,
    Vec<(u64, cusf_sidechain_types::OutPoint, u64)>,
    Vec<cusf_sidechain_types::Output>,
);

type SidechainUTXOs = BTreeMap<u64, (cusf_sidechain_types::OutPoint, u32, u64, Option<u64>)>;

#[derive(Debug)]
pub struct Deposit {
    pub sidechain_number: u8,
    pub address: Vec<u8>,
    pub amount: u64,
    pub transaction: Transaction,
}

#[derive(Debug)]
pub struct Sidechain {
    pub sidechain_number: u8,
    pub data: Vec<u8>,
}

use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ureq_jsonrpc::{json, Client};

pub fn create_client(main_datadir: &Path) -> Result<Client> {
    let auth = std::fs::read_to_string(main_datadir.join("regtest/.cookie")).into_diagnostic()?;
    let mut auth = auth.split(':');
    let user = auth
        .next()
        .ok_or(miette!("failed to get rpcuser"))?
        .to_string();
    let password = auth
        .next()
        .ok_or(miette!("failed to get rpcpassword"))?
        .to_string();
    Ok(Client {
        host: "localhost".into(),
        port: 18443,
        user,
        password,
        id: "mainchain".into(),
    })
}

use bdk::bitcoin::constants::SUBSIDY_HALVING_INTERVAL;
use bdk::bitcoin::{self, base58};
use bitcoin::absolute::{Height, LockTime};
use bitcoin::block::Version;
use bitcoin::consensus::Encodable;
use bitcoin::constants::genesis_block;
use bitcoin::hash_types::TxMerkleNode;
use bitcoin::hashes::Hash;
use bitcoin::opcodes::OP_0;
use bitcoin::{consensus::Decodable, Block};
use bitcoin::{merkle_tree, BlockHash, ScriptBuf, Sequence, Transaction, TxIn, TxOut};

fn get_block_value(height: u32, fees: u64, network: Network) -> u64 {
    let mut subsidy = 50 * Amount::ONE_BTC.to_sat();
    let subsidy_halving_interval = match network {
        Network::Regtest => 150,
        _ => SUBSIDY_HALVING_INTERVAL,
    };
    let halvings = height / subsidy_halving_interval;
    if halvings >= 64 {
        fees
    } else {
        subsidy >>= halvings;
        subsidy + fees
    }
}

#[derive(Debug)]
pub struct SidechainAck {
    pub sidechain_number: u8,
    pub data_hash: [u8; 32],
}
