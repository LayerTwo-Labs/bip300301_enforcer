use std::{
    borrow::BorrowMut,
    collections::{BTreeMap, HashMap},
    path::Path,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bdk_electrum::{
    electrum_client::{self, ElectrumApi},
    BdkElectrumClient,
};
use bdk_wallet::{
    self, file_store,
    keys::{
        bip39::{Language, Mnemonic},
        DerivableKey as _, ExtendedKey,
    },
    ChangeSet, KeychainKind,
};
use bip300301::{
    client::{
        BlockchainInfo, BoolWitness, GetRawMempoolClient, GetRawTransactionClient,
        GetRawTransactionVerbose,
    },
    jsonrpsee::http_client::HttpClient,
    MainClient,
};
use bitcoin::{
    absolute::{Height, LockTime},
    block::Version as BlockVersion,
    consensus::Encodable as _,
    constants::{genesis_block, SUBSIDY_HALVING_INTERVAL},
    hash_types::TxMerkleNode,
    hashes::{sha256, sha256d, Hash as _, HashEngine},
    merkle_tree,
    opcodes::{
        all::{OP_PUSHBYTES_36, OP_RETURN},
        OP_0,
    },
    script::PushBytesBuf,
    transaction::Version as TxVersion,
    Amount, Block, Network, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness,
};
use miette::{miette, IntoDiagnostic, Result};
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;

use crate::{
    cli::WalletConfig,
    convert,
    messages::{self, CoinbaseBuilder, M8_BMM_REQUEST_TAG},
    types::{BDKWalletTransaction, Ctip, M6id, SidechainAck, SidechainNumber, SidechainProposal},
    validator::Validator,
};

pub mod error;

#[derive(Debug)]
pub struct Deposit {
    pub sidechain_number: u8,
    pub address: Vec<u8>,
    pub amount: u64,
    pub transaction: Transaction,
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

pub struct Wallet {
    main_client: HttpClient,
    validator: Validator,
    bitcoin_wallet: Mutex<bdk_wallet::PersistedWallet<file_store::Store<ChangeSet>>>,
    bitcoin_db: Mutex<file_store::Store<ChangeSet>>,
    db_connection: Arc<Mutex<rusqlite::Connection>>,
    bitcoin_blockchain: BdkElectrumClient<bdk_electrum::electrum_client::Client>,
    last_sync: Arc<RwLock<Option<SystemTime>>>,
}

impl Wallet {
    pub async fn new(
        data_dir: &Path,
        config: &WalletConfig,
        main_client: HttpClient,
        validator: Validator,
    ) -> Result<Self> {
        let mnemonic = Mnemonic::parse_in_normalized(
            Language::English,
            "betray annual dog current tomorrow media ghost dynamic mule length sure salad",
        )
        .into_diagnostic()?;
        // Generate the extended key
        let xkey: ExtendedKey = mnemonic.clone().into_extended_key().into_diagnostic()?;
        // Get xprv from the extended key
        let network = {
            let validator_network = validator.network();
            bdk_wallet::bitcoin::Network::from_str(validator_network.to_string().as_str())
                .into_diagnostic()?
        };

        tracing::info!(
            "Instantiating {} wallet with data dir: {}",
            network,
            data_dir.display()
        );

        let xprv = xkey
            .into_xprv(network)
            .ok_or(miette!("couldn't get xprv"))?;

        let mut wallet_database = file_store::Store::open_or_create_new(
            b"bip300301_enforcer",
            data_dir.join("wallet.db"),
        )
        .into_diagnostic()?;

        // Create a BDK wallet structure using BIP 84 descriptor ("m/84h/1h/0h/0" and "m/84h/1h/0h/1")

        let external_desc = format!("wpkh({xprv}/84'/1'/0'/0/*)");
        let internal_desc = format!("wpkh({xprv}/84'/1'/0'/1/*)");

        tracing::debug!("Attempting load of existing BDK wallet");
        let bitcoin_wallet = bdk_wallet::Wallet::load()
            .descriptor(KeychainKind::External, Some(external_desc.clone()))
            .descriptor(KeychainKind::Internal, Some(internal_desc.clone()))
            .extract_keys()
            .check_network(network)
            .load_wallet(&mut wallet_database)
            .map_err(|err| miette!("failed to load wallet: {err:#}"))?;

        let bitcoin_wallet = match bitcoin_wallet {
            Some(wallet) => {
                tracing::info!("Loaded existing BDK wallet");
                wallet
            }

            None => {
                tracing::info!("Creating new BDK wallet");

                bdk_wallet::Wallet::create(external_desc, internal_desc)
                    .network(network)
                    .create_wallet(&mut wallet_database)
                    .map_err(|err| miette!("failed to create wallet: {err:#}"))?
            }
        };

        let bitcoin_blockchain = {
            let (default_host, default_port) = match network {
                Network::Signet => ("drivechain.live", 50001),
                Network::Regtest => ("127.0.0.1", 60401), // Default for romanz/electrs
                default => return Err(miette!("unsupported network: {default}")),
            };

            let electrum_host = config
                .electrum_host
                .clone()
                .unwrap_or(default_host.to_string());
            let electrum_port = config.electrum_port.unwrap_or(default_port);

            let electrum_url = format!("{}:{}", electrum_host, electrum_port);

            tracing::debug!("creating electrum client: {electrum_url}");

            // Apply a reasonably short timeout to prevent the wallet from hanging
            let timeout = 5;
            let config = electrum_client::ConfigBuilder::new()
                .timeout(Some(timeout))
                .build();

            let electrum_client = electrum_client::Client::from_config(&electrum_url, config)
                .map_err(|err| miette!("failed to create electrum client: {err:#}"))?;

            // let features = electrum_client.server_features().into_diagnostic()?;
            let header = electrum_client.block_header(0).into_diagnostic()?;

            // Verify the Electrum server is on the same chain as we are.
            if header.block_hash().as_byte_array() != network.chain_hash().as_bytes() {
                return Err(miette!(
                    "Electrum server ({}) is not on the same chain as the wallet ({})",
                    header.block_hash(),
                    network.chain_hash(),
                ));
            }

            BdkElectrumClient::new(electrum_client)
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
            ]);

            let db_name = "db.sqlite";
            let path = data_dir.join(db_name);
            let mut db_connection = Connection::open(path.clone()).into_diagnostic()?;

            tracing::info!("Created database connection to {}", path.display());

            migrations.to_latest(&mut db_connection).into_diagnostic()?;

            tracing::debug!("Ran migrations on {}", path.display());
            db_connection
        };

        let wallet = Self {
            main_client,
            validator,
            // bitcoin_wallet: Arc::new(Mutex::new(bitcoin_wallet)),
            bitcoin_wallet: Mutex::new(bitcoin_wallet),
            bitcoin_db: Mutex::new(wallet_database),
            db_connection: Arc::new(Mutex::new(db_connection)),
            bitcoin_blockchain,

            last_sync: Arc::new(RwLock::new(None)),
        };
        Ok(wallet)
    }

    pub fn validator(&self) -> &Validator {
        &self.validator
    }

    pub async fn generate_block(
        &self,
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
    ) -> Result<Block> {
        let addr = self.get_new_address()?;

        tracing::debug!("Generate block: fetched address: {}", addr);

        let script_pubkey = addr.script_pubkey();

        let BlockchainInfo {
            blocks: block_height,
            best_blockhash,
            ..
        } = self
            .main_client
            .get_blockchain_info()
            .await
            .map_err(|err| error::BitcoinCoreRPC {
                method: "getblockchaininfo".to_string(),
                error: err,
            })?;

        tracing::debug!("Generate block: found best block: `{best_blockhash}` @ {block_height}",);

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
            prev_blockhash: best_blockhash,
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
            value: bitcoin::Amount::ZERO,
        });
        let mut tx_hashes: Vec<_> = block.txdata.iter().map(Transaction::compute_txid).collect();
        block.header.merkle_root = merkle_tree::calculate_root_inline(&mut tx_hashes)
            .unwrap()
            .to_raw_hash()
            .into();
        Ok(block)
    }

    /// Returns pending sidechain proposals from the wallet. These are not yet
    /// active on the chain, and not possible to vote on.
    fn get_our_sidechain_proposals(&self) -> Result<Vec<SidechainProposal>, rusqlite::Error> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, rusqlite::Error> {
            let mut statement =
                connection.prepare("SELECT number, data FROM sidechain_proposals")?;

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
        with_connection(&self.db_connection.lock())
    }

    fn get_sidechain_acks(&self) -> Result<Vec<SidechainAck>> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_> {
            let mut statement = connection
                .prepare("SELECT number, data_hash FROM sidechain_acks")
                .into_diagnostic()?;
            let rows = statement
                .query_map([], |row| {
                    let description_hash: [u8; 32] = row.get(1)?;
                    Ok(SidechainAck {
                        sidechain_number: SidechainNumber(row.get(0)?),
                        description_hash: sha256d::Hash::from_byte_array(description_hash),
                    })
                })
                .into_diagnostic()?
                .collect::<Result<_, _>>()
                .into_diagnostic()?;
            Ok(rows)
        };
        with_connection(&self.db_connection.lock())
    }

    /// Fetches sidechain proposals from the validator. Returns proposals that
    /// are already included into a block, and possible to vote on.
    async fn get_active_sidechain_proposals(
        &self,
    ) -> Result<HashMap<SidechainNumber, SidechainProposal>> {
        let pending_proposals = self
            .validator
            .get_sidechains()?
            .into_iter()
            .map(|(_, sidechain)| (sidechain.proposal.sidechain_number, sidechain.proposal))
            .collect();
        Ok(pending_proposals)
    }

    pub fn ack_sidechain(
        &self,
        sidechain_number: SidechainNumber,
        data_hash: sha256d::Hash,
    ) -> Result<()> {
        let sidechain_number: u8 = sidechain_number.into();
        let data_hash: &[u8; 32] = data_hash.as_byte_array();
        self.db_connection
            .lock()
            .execute(
                "INSERT INTO sidechain_acks (number, data_hash) VALUES (?1, ?2)",
                (sidechain_number, data_hash),
            )
            .into_diagnostic()?;
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

    fn delete_sidechain_ack(&self, ack: &SidechainAck) -> Result<()> {
        self.db_connection
            .lock()
            .execute(
                "DELETE FROM sidechain_acks WHERE number = ?1 AND data_hash = ?2",
                (ack.sidechain_number.0, ack.description_hash.as_byte_array()),
            )
            .into_diagnostic()?;
        Ok(())
    }

    /// Get BMM requests with the specified previous blockhash.
    /// Returns pairs of sidechain numbers and side blockhash.
    fn get_bmm_requests(
        &self,
        prev_blockhash: &bitcoin::BlockHash,
    ) -> Result<Vec<(SidechainNumber, [u8; 32])>> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, _> {
            let mut statement = connection
                .prepare(
                    "SELECT sidechain_number, side_block_hash FROM bmm_requests WHERE prev_block_hash = ?"
                )
                .into_diagnostic()?;

            let queried = statement
                .query_map([prev_blockhash.as_byte_array()], |row| {
                    let sidechain_number: u8 = row.get(0)?;
                    let side_blockhash: [u8; 32] = row.get(1)?;
                    Ok((SidechainNumber::from(sidechain_number), side_blockhash))
                })
                .into_diagnostic()?
                .collect::<Result<_, _>>()
                .into_diagnostic()?;

            Ok(queried)
        };
        with_connection(&self.db_connection.lock())
    }

    // Gets wiped upon generating a new block.
    // TODO: how will this work for non-regtest?
    fn delete_pending_sidechain_proposals(&self) -> Result<()> {
        self.db_connection
            .lock()
            .execute("DELETE FROM sidechain_proposals;", ())
            .into_diagnostic()?;
        Ok(())
    }

    // Gets wiped upon generating a new block.
    // TODO: how will this work for non-regtest?
    fn delete_bmm_requests(&self, prev_blockhash: &bitcoin::BlockHash) -> Result<()> {
        self.db_connection
            .lock()
            .execute(
                "DELETE FROM bmm_requests where prev_block_hash = ?;",
                [prev_blockhash.as_byte_array()],
            )
            .into_diagnostic()?;
        Ok(())
    }

    async fn mine(&self, coinbase_outputs: &[TxOut], transactions: Vec<Transaction>) -> Result<()> {
        let transaction_count = transactions.len();

        let mut block = self.generate_block(coinbase_outputs, transactions).await?;
        loop {
            block.header.nonce += 1;
            if block.header.validate_pow(block.header.target()).is_ok() {
                break;
            }
        }
        let mut block_bytes = vec![];
        block
            .consensus_encode(&mut block_bytes)
            .map_err(error::EncodeBlock)?;

        let () = self
            .main_client
            .submit_block(hex::encode(block_bytes))
            .await
            .map_err(|err| error::BitcoinCoreRPC {
                method: "submitblock".to_string(),
                error: err,
            })?;

        tracing::info!(
            "Generate: submitted block with {} transactions: `{}`",
            transaction_count,
            block.header.block_hash()
        );

        std::thread::sleep(Duration::from_millis(500));
        Ok(())
    }

    pub async fn generate(&self, count: u32, ack_all_proposals: bool) -> Result<()> {
        tracing::info!("Generate: creating {} blocks", count);

        for _ in 0..count {
            // This is a list of pending sidechain proposals from /our/ wallet, fetched from
            // the DB.
            let sidechain_proposals = self.get_our_sidechain_proposals().into_diagnostic()?;
            let mut coinbase_builder = CoinbaseBuilder::new();
            for sidechain_proposal in sidechain_proposals {
                coinbase_builder = coinbase_builder.propose_sidechain(sidechain_proposal);
            }

            let mut sidechain_acks = self.get_sidechain_acks()?;

            // This is a map of pending sidechain proposals from the /validator/, i.e.
            // proposals broadcasted by (potentially) someone else, and already active.
            let active_sidechain_proposals = self.get_active_sidechain_proposals().await?;

            if ack_all_proposals && !active_sidechain_proposals.is_empty() {
                tracing::info!(
                    "Handle sidechain ACK: acking all sidechains irregardless of what DB says"
                );

                let acks = sidechain_acks.clone();
                for (sidechain_number, sidechain_proposal) in &active_sidechain_proposals {
                    let sidechain_number = *sidechain_number;

                    if !acks
                        .iter()
                        .any(|ack| ack.sidechain_number == sidechain_number)
                    {
                        tracing::debug!(
                            "Handle sidechain ACK: adding 'fake' ACK for {}",
                            sidechain_number
                        );

                        self.ack_sidechain(
                            sidechain_number,
                            sidechain_proposal.description.sha256d_hash(),
                        )?;

                        sidechain_acks.push(SidechainAck {
                            sidechain_number,
                            description_hash: sidechain_proposal.description.sha256d_hash(),
                        });
                    }
                }
            }

            for sidechain_ack in sidechain_acks {
                if !self.validate_sidechain_ack(&sidechain_ack, &active_sidechain_proposals) {
                    self.delete_sidechain_ack(&sidechain_ack)?;
                    tracing::info!(
                        "Unable to handle sidechain ack, deleted: {}",
                        sidechain_ack.sidechain_number
                    );
                    continue;
                }

                tracing::debug!(
                    "Generate: adding ACK for sidechain {}",
                    sidechain_ack.sidechain_number
                );

                coinbase_builder = coinbase_builder.ack_sidechain(
                    sidechain_ack.sidechain_number,
                    sidechain_ack.description_hash,
                );
            }

            let mainchain_tip = self.validator.get_mainchain_tip()?;
            let bmm_hashes = self.get_bmm_requests(&mainchain_tip)?;
            for (sidechain_number, bmm_hash) in &bmm_hashes {
                tracing::info!(
                    "Generate: adding BMM accept for SC {} with hash: {}",
                    sidechain_number,
                    hex::encode(bmm_hash)
                );
                coinbase_builder = coinbase_builder.bmm_accept(*sidechain_number, bmm_hash);
            }

            let coinbase_outputs = coinbase_builder.build().into_diagnostic()?;

            // We want to include all transactions from the mempool into our newly generated block.
            // This approach is perhaps a bit naive, and could fail if there are conflicting TXs
            // pending. On signet the block is constructed using `getblocktemplate`, so this will not
            // be an issue there.
            //
            // Including all the mempool transactions here ensure that pending sidechain deposit
            // transactions get included into a block.
            let raw_mempool = self
                .main_client
                .get_raw_mempool(BoolWitness::<false>, BoolWitness::<false>)
                .await
                .map_err(|err| error::BitcoinCoreRPC {
                    method: "getrawmempool".to_string(),
                    error: err,
                })?;

            let mut mempool_transactions = vec![];

            for txid in raw_mempool {
                let transaction = self.fetch_transaction(txid).await?;
                mempool_transactions.push(transaction);
            }

            tracing::info!(
                "Generate: mining block with {} coinbase outputs, {} transactions",
                coinbase_outputs.len(),
                mempool_transactions.len()
            );

            self.mine(&coinbase_outputs, mempool_transactions).await?;
            self.delete_pending_sidechain_proposals()?;
            self.delete_bmm_requests(&mainchain_tip)?;
        }
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

    async fn fetch_transaction(&self, txid: Txid) -> Result<bdk_wallet::bitcoin::Transaction> {
        let block_hash = None;

        let transaction_hex = self
            .main_client
            .get_raw_transaction(txid, GetRawTransactionVerbose::<false>, block_hash)
            .await
            .map_err(|err| error::BitcoinCoreRPC {
                method: "getrawtransaction".to_string(),
                error: err,
            })?;

        let transaction =
            bitcoin::consensus::encode::deserialize_hex::<Transaction>(&transaction_hex)
                .into_diagnostic()?;

        convert::bitcoin_tx_to_bdk_tx(transaction).into_diagnostic()
    }

    /// [`bdk_wallet::TxOrdering`] for deposit txs
    fn deposit_txordering(
        sidechain_addrs: HashMap<Vec<u8>, SidechainNumber>,
    ) -> bdk_wallet::TxOrdering {
        use bitcoin::hashes::{Hash, Hmac, HmacEngine};
        use std::cmp::Ordering;
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
    ) -> Result<bdk_wallet::bitcoin::psbt::Psbt> {
        let sidechain_number = match crate::messages::parse_op_drivechain(
            op_drivechain_output.script_pubkey.as_bytes(),
        ) {
            Ok((_, sidechain_number)) => sidechain_number,
            Err(_) => return Err(miette::miette!("Failed to parse sidechain number")),
        };
        // If the sidechain has a Ctip (i.e. treasury UTXO), the BIP300 rules mandate that we spend the previous
        // Ctip.
        let ctip_foreign_utxo = match sidechain_ctip {
            Some(sidechain_ctip) => {
                let outpoint = bdk_wallet::bitcoin::OutPoint {
                    txid: convert::bitcoin_txid_to_bdk_txid(sidechain_ctip.outpoint.txid),
                    vout: sidechain_ctip.outpoint.vout,
                };

                let ctip_transaction = self.fetch_transaction(sidechain_ctip.outpoint.txid).await?;

                let psbt_input = bdk_wallet::bitcoin::psbt::Input {
                    non_witness_utxo: Some(ctip_transaction),
                    ..bdk_wallet::bitcoin::psbt::Input::default()
                };

                Some((psbt_input, outpoint))
            }
            None => None,
        };

        let psbt = {
            let mut wallet = self.bitcoin_wallet.lock();
            let mut builder = wallet.borrow_mut().build_tx();

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

                builder
                    .add_foreign_utxo(outpoint, ctip_psbt_input, satisfaction_weight)
                    .into_diagnostic()?;
            }

            builder.ordering(Self::deposit_txordering(
                [(
                    sidechain_address_data.as_bytes().to_owned(),
                    sidechain_number,
                )]
                .into_iter()
                .collect(),
            ));

            builder.finish().into_diagnostic()?
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
        sidechain_address: Vec<u8>,
        value: Amount,
        fee: Option<Amount>,
    ) -> Result<bitcoin::Txid> {
        // If this is None, there's been no deposit to this sidechain yet. We're the first one!
        let sidechain_ctip = self.validator.try_get_ctip(sidechain_number)?;
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
            "Created OP_DRIVECHAIN output with value `{}`, spk `{}` ",
            op_drivechain_output.value,
            op_drivechain_output.script_pubkey.to_asm_string(),
        );

        let sidechain_address_data = bdk_wallet::bitcoin::script::PushBytesBuf::try_from(
            sidechain_address,
        )
        .map_err(|err| miette!("failed to convert sidechain address to PushBytesBuf: {err:#}"))?;

        let psbt = self
            .create_deposit_psbt(
                op_drivechain_output,
                sidechain_address_data,
                sidechain_ctip,
                fee,
            )
            .await?;

        tracing::debug!("Created deposit PSBT: {psbt}",);

        let tx = self.sign_transaction(psbt)?;
        let txid = tx.compute_txid();

        tracing::info!("Signed deposit transaction: `{txid}`",);

        tracing::debug!("Serialized deposit transaction: {}", {
            let tx_bytes = bdk_wallet::bitcoin::consensus::serialize(&tx);
            hex::encode(tx_bytes)
        });

        self.broadcast_transaction(tx).await?;

        tracing::info!("Broadcasted deposit transaction: `{txid}`",);

        Ok(convert::bdk_txid_to_bitcoin_txid(txid))
    }

    pub async fn get_wallet_balance(&self) -> Result<bdk_wallet::Balance> {
        if self.last_sync.read().is_none() {
            return Err(miette!("get balance: wallet not synced"));
        }

        let balance = self.bitcoin_wallet.lock().balance();

        Ok(balance)
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "false positive for `bitcoin_wallet`"
    )]
    pub async fn list_wallet_transactions(&self) -> Result<Vec<BDKWalletTransaction>> {
        // Massage the wallet data into a format that we can use to calculate fees, etc.
        let wallet_data = {
            let mut guard = self.bitcoin_wallet.lock();
            let wallet = guard.borrow_mut();
            let transactions = wallet.transactions();

            transactions
                .into_iter()
                .map(|tx| {
                    let txid = tx.tx_node.txid;
                    let chain_position = tx.chain_position.cloned();
                    let tx = tx.tx_node.tx.clone();

                    let output_ownership: Vec<_> = tx
                        .output
                        .iter()
                        .map(|output| (output.value, wallet.is_mine(output.script_pubkey.clone())))
                        .collect();

                    // Just collect the inputs - we'll get their values using getrawtransaction later
                    let inputs = tx.input.clone();

                    (txid, chain_position, output_ownership, inputs)
                })
                .collect::<Vec<_>>()
        };

        // Calculate fees, received, and sent amounts
        let mut txs = Vec::new();
        for (txid, chain_position, output_ownership, inputs) in wallet_data {
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
                let transaction_hex = self
                    .main_client
                    .get_raw_transaction(
                        input.previous_output.txid,
                        GetRawTransactionVerbose::<false>,
                        None,
                    )
                    .await
                    .map_err(|err| error::BitcoinCoreRPC {
                        method: "getrawtransaction".to_string(),
                        error: err,
                    })?;

                let prev_output =
                    bitcoin::consensus::encode::deserialize_hex::<Transaction>(&transaction_hex)
                        .into_diagnostic()?;

                let value = prev_output.output[input.previous_output.vout as usize].value;
                if self.bitcoin_wallet.lock().is_mine(
                    prev_output.output[input.previous_output.vout as usize]
                        .script_pubkey
                        .clone(),
                ) {
                    sent += value;
                }
                input_value += value;
            }

            let fee = input_value - output_value;
            // Calculate net wallet change (excluding fee)
            // We need to handle received and sent separately since Amount can't be negative
            let (final_received, final_sent) = if received >= sent {
                (received - sent, Amount::from_sat(0)) // Net gain to wallet
            } else {
                (Amount::from_sat(0), sent - received - fee) // Net loss from wallet
            };

            txs.push(BDKWalletTransaction {
                txid,
                chain_position,
                fee,
                received: final_received,
                sent: final_sent,
            });
        }
        Ok(txs)
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "false positive for `bitcoin_wallet`"
    )]
    async fn create_send_psbt(
        &self,
        destinations: HashMap<bitcoin::Address, u64>,
        fee_policy: Option<crate::types::FeePolicy>,
        op_return_output: Option<bdk_wallet::bitcoin::TxOut>,
    ) -> Result<bdk_wallet::bitcoin::psbt::Psbt> {
        let psbt = {
            let mut wallet = self.bitcoin_wallet.lock();
            let mut builder = wallet.borrow_mut().build_tx();

            if let Some(op_return_output) = op_return_output {
                builder.add_recipient(op_return_output.script_pubkey, op_return_output.value);
            }

            // Add outputs for each destination address
            for (address, value) in destinations {
                builder.add_recipient(address.script_pubkey(), Amount::from_sat(value));
            }

            match fee_policy {
                Some(crate::types::FeePolicy::Absolute(fee)) => {
                    builder.fee_absolute(fee);
                }
                Some(crate::types::FeePolicy::Rate(rate)) => {
                    builder.fee_rate(rate);
                }
                None => (),
            }

            builder.finish().into_diagnostic()?
        };

        Ok(psbt)
    }

    /// Creates a transaction, sends it, and returns the TXID.
    pub async fn send_wallet_transaction(
        &self,
        destinations: HashMap<bdk_wallet::bitcoin::Address, u64>,
        fee_policy: Option<crate::types::FeePolicy>,
        op_return_message: Option<Vec<u8>>,
    ) -> Result<bitcoin::Txid> {
        let op_return_output = op_return_message
            .map(Self::create_op_return_output)
            .transpose()
            .into_diagnostic()?;

        let psbt = self
            .create_send_psbt(destinations, fee_policy, op_return_output)
            .await?;

        tracing::debug!("Created send PSBT: {psbt}",);

        let tx = self.sign_transaction(psbt)?;
        let txid = tx.compute_txid();

        tracing::info!("Signed send transaction: `{txid}`",);

        tracing::debug!("Serialized send transaction: {}", {
            let tx_bytes = bdk_wallet::bitcoin::consensus::serialize(&tx);
            hex::encode(tx_bytes)
        });

        self.broadcast_transaction(tx).await?;

        tracing::info!("Broadcasted send transaction: `{txid}`",);

        Ok(convert::bdk_txid_to_bitcoin_txid(txid))
    }

    pub fn sync(&self) -> Result<()> {
        let start = SystemTime::now();
        tracing::trace!("starting wallet sync");

        let mut wallet_lock = self.bitcoin_wallet.lock();
        let mut last_sync_write = self.last_sync.write();
        let request = wallet_lock.start_sync_with_revealed_spks();

        const BATCH_SIZE: usize = 5;
        const FETCH_PREV_TXOUTS: bool = false;

        let update = self
            .bitcoin_blockchain
            .sync(request, BATCH_SIZE, FETCH_PREV_TXOUTS)
            .into_diagnostic()?;

        wallet_lock.apply_update(update).into_diagnostic()?;

        let mut database = self.bitcoin_db.lock();
        wallet_lock.persist(&mut database).into_diagnostic()?;

        tracing::debug!(
            "wallet sync complete in {:?}",
            start.elapsed().unwrap_or_default(),
        );

        *last_sync_write = Some(SystemTime::now());
        drop(last_sync_write);
        drop(wallet_lock);
        Ok(())
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "false positive for `bitcoin_wallet`"
    )]
    fn get_utxos(&self) -> Result<()> {
        if self.last_sync.read().is_none() {
            return Err(miette!("get utxos: wallet not synced"));
        }

        let wallet_lock = self.bitcoin_wallet.lock();
        let utxos = wallet_lock.list_unspent();
        for utxo in utxos {
            tracing::trace!(
                "address: {}, value: {}",
                utxo.txout.script_pubkey,
                utxo.txout.value
            );
        }
        Ok(())
    }

    /// Persists a sidechain proposal into our database.
    /// On regtest: picked up by the next block generation.
    /// On signet: TBD, but needs some way of getting communicated to the miner.
    pub fn propose_sidechain(&self, proposal: &SidechainProposal) -> Result<(), rusqlite::Error> {
        let sidechain_number: u8 = proposal.sidechain_number.into();
        self.db_connection.lock().execute(
            "INSERT INTO sidechain_proposals (number, data) VALUES (?1, ?2)",
            (sidechain_number, &proposal.description.0),
        )?;
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

    async fn get_sidechain_ctip(
        &self,
        sidechain_number: SidechainNumber,
    ) -> Result<Option<(bitcoin::OutPoint, Amount, u64)>> {
        let ctip = self.validator.try_get_ctip(sidechain_number)?;

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

    pub fn is_sidechain_active(&self, sidechain_number: SidechainNumber) -> Result<bool> {
        let sidechains = self.validator.get_active_sidechains()?;
        let active = sidechains
            .iter()
            .any(|sc| sc.proposal.sidechain_number == sidechain_number);

        Ok(active)
    }

    fn sign_transaction(
        &self,
        mut psbt: bdk_wallet::bitcoin::psbt::Psbt,
    ) -> Result<bdk_wallet::bitcoin::Transaction> {
        if !self
            .bitcoin_wallet
            .lock()
            .sign(&mut psbt, bdk_wallet::signer::SignOptions::default())
            .into_diagnostic()?
        {
            return Err(miette!("failed to sign transaction"));
        }

        tracing::debug!("Signed PSBT: {psbt}",);

        psbt.extract_tx().into_diagnostic()
    }

    fn bmm_request_message(
        sidechain_number: SidechainNumber,
        prev_mainchain_block_hash: bdk_wallet::bitcoin::BlockHash,
        sidechain_block_hash: [u8; 32],
    ) -> Result<bdk_wallet::bitcoin::ScriptBuf> {
        let message = [
            &M8_BMM_REQUEST_TAG[..],
            &[sidechain_number.into()],
            &sidechain_block_hash,
            &prev_mainchain_block_hash.to_byte_array(),
        ]
        .concat();
        let bytes =
            bdk_wallet::bitcoin::script::PushBytesBuf::try_from(message).into_diagnostic()?;
        Ok(bdk_wallet::bitcoin::ScriptBuf::new_op_return(&bytes))
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "false positive for `bitcoin_wallet`"
    )]
    fn build_bmm_tx(
        &self,
        sidechain_number: SidechainNumber,
        prev_mainchain_block_hash: bdk_wallet::bitcoin::BlockHash,
        sidechain_block_hash: [u8; 32],
        bid_amount: bdk_wallet::bitcoin::Amount,
        locktime: bdk_wallet::bitcoin::absolute::LockTime,
    ) -> Result<bdk_wallet::bitcoin::psbt::Psbt> {
        // https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip301.md#m8-bmm-request
        let message = Self::bmm_request_message(
            sidechain_number,
            prev_mainchain_block_hash,
            sidechain_block_hash,
        )?;

        let psbt = {
            let mut bitcoin_wallet = self.bitcoin_wallet.lock();
            let mut builder = bitcoin_wallet.build_tx();
            builder
                .nlocktime(locktime)
                .add_recipient(message, bid_amount);
            builder.finish().into_diagnostic()?
        };

        Ok(psbt)
    }

    /// Returns `true` if a BMM request was inserted, `false` if a BMM request
    /// already exists for that sidechain and previous blockhash
    fn insert_new_bmm_request(
        &self,
        sidechain_number: SidechainNumber,
        prev_blockhash: bdk_wallet::bitcoin::BlockHash,
        side_block_hash: [u8; 32],
    ) -> Result<bool> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<bool, rusqlite::Error> {
            connection
                .prepare(
                    "INSERT OR ABORT INTO bmm_requests (sidechain_number, prev_block_hash, side_block_hash) VALUES (?1, ?2, ?3)",
                )?
                .execute((
                    u8::from(sidechain_number),
                    prev_blockhash.to_byte_array(),
                    side_block_hash,
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
        with_connection(&self.db_connection.lock()).into_diagnostic()
    }

    /// Creates a BMM request transaction. Does NOT broadcast.
    /// Returns `Some(tx)` if the BMM request was stored, `None` if the BMM
    /// request was not stored due to pre-existing request with the same
    /// `sidechain_number` and `prev_mainchain_block_hash`.
    pub fn create_bmm_request(
        &self,
        sidechain_number: SidechainNumber,
        prev_mainchain_block_hash: bdk_wallet::bitcoin::BlockHash,
        sidechain_block_hash: [u8; 32],
        bid_amount: bdk_wallet::bitcoin::Amount,
        locktime: bdk_wallet::bitcoin::absolute::LockTime,
    ) -> Result<Option<bdk_wallet::bitcoin::Transaction>> {
        let psbt = self.build_bmm_tx(
            sidechain_number,
            prev_mainchain_block_hash,
            sidechain_block_hash,
            bid_amount,
            locktime,
        )?;
        let tx = self.sign_transaction(psbt)?;
        tracing::info!("BMM request psbt signed successfully");
        if self.insert_new_bmm_request(
            sidechain_number,
            prev_mainchain_block_hash,
            sidechain_block_hash,
        )? {
            tracing::info!("inserted new bmm request into db");
            Ok(Some(tx))
        } else {
            tracing::warn!("Ignored BMM request; request exists with same sidechain slot and previous block hash");
            Ok(None)
        }
    }

    // Broadcasts a transaction to the Bitcoin network.
    pub async fn broadcast_transaction(&self, tx: bdk_wallet::bitcoin::Transaction) -> Result<()> {
        // Note: there's a `broadcast` method on `bitcoin_blockchain`. We're NOT using that,
        // because we're broadcasting transactions that "burn" bitcoin (from a BIP-300/1 unaware
        // perspective). To get around this we have to pass a `maxburnamount` parameter, and
        // that's not possible if going through the ElectrumBlockchain interface.
        //
        // For the interested reader, the flow of ElectrumBlockchain::broadcast is this:
        // 1. Send the raw TX from our Electrum client
        // 2. Electrum server implements this by sending it into Bitcoin Core
        // 3. Bitcoin Core responds with an error, because we're burning money.

        let mut tx_bytes = vec![];
        tx.consensus_encode(&mut tx_bytes).into_diagnostic()?;

        let encoded_tx = hex::encode(tx_bytes);

        const MAX_BURN_AMOUNT: f64 = 21_000_000.0;
        let broadcast_result = self
            .main_client
            .send_raw_transaction(encoded_tx, None, Some(MAX_BURN_AMOUNT))
            .await
            .inspect_err(|e| tracing::error!("failed to broadcast tx: {e:#}"))
            .into_diagnostic()?;

        tracing::debug!("broadcasted TXID: {:?}", broadcast_result);

        Ok(())
    }

    #[allow(clippy::significant_drop_tightening)]
    pub fn get_new_address(&self) -> Result<bdk_wallet::bitcoin::Address> {
        // Using next_unused_address here means that we get a new address
        // when funds are received. Without this we'd need to take care not
        // to cross the wallet scan gap.
        let mut wallet = self.bitcoin_wallet.lock();
        let info = wallet
            .borrow_mut()
            .next_unused_address(bdk_wallet::KeychainKind::External);

        let mut bitcoin_db = self.bitcoin_db.lock();
        let bitcoin_db = bitcoin_db.borrow_mut();
        wallet.persist(bitcoin_db).into_diagnostic()?;
        Ok(info.address)
    }

    pub fn put_withdrawal_bundle(&self, tx: Transaction) -> Result<(M6id, SidechainNumber)> {
        let (prev_treasury_utxo_sidechain_number, prev_treasury_utxo_total) =
            if let Some(input) = tx.input.first() {
                let (sidechain_number, amount) =
                    self.validator().get_ctip_value(&input.previous_output)?;
                (Some(sidechain_number), amount)
            } else {
                (None, Amount::ZERO)
            };
        let tx_bytes = bitcoin::consensus::serialize(&tx);
        let (m6id, sidechain_number) =
            crate::messages::compute_m6id(tx, prev_treasury_utxo_total).into_diagnostic()?;
        if prev_treasury_utxo_sidechain_number.is_some_and(|prev_treasury_utxo_sidechain_number| {
            prev_treasury_utxo_sidechain_number != sidechain_number
        }) {
            return Err(miette::miette!(
                "M6 sidechain number does not match previous treasury UTXO"
            ));
        }
        self.db_connection
            .lock()
            .execute(
                "INSERT INTO bundle_proposals (sidechain_number, bundle_hash, bundle_tx) VALUES (?1, ?2, ?3)",
                (sidechain_number.0, m6id.0.as_byte_array(), tx_bytes),
            )
            .into_diagnostic()?;
        Ok((m6id, sidechain_number))
    }
}

type PendingTransactions = (
    SidechainNumber,
    Vec<(u64, cusf_sidechain_types::OutPoint, u64)>,
    Vec<cusf_sidechain_types::Output>,
);

type SidechainUTXOs = BTreeMap<u64, (cusf_sidechain_types::OutPoint, u32, u64, Option<u64>)>;
