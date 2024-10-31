use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bdk::{
    bitcoin::{
        consensus::Encodable as _, hashes::Hash as _, psbt::PartiallySignedTransaction,
        script::PushBytesBuf, BlockHash, Network,
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
use bip300301::{
    client::{BlockchainInfo, GetRawTransactionClient, GetRawTransactionVerbose},
    jsonrpsee::http_client::HttpClient,
    MainClient,
};
use bitcoin::{
    absolute::{Height, LockTime},
    block::Version as BlockVersion,
    consensus::{Decodable as _, Encodable as _},
    constants::{genesis_block, SUBSIDY_HALVING_INTERVAL},
    hash_types::TxMerkleNode,
    hashes::{sha256d, Hash as _},
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
use rusqlite::{Connection, Row};

use crate::{
    cli::WalletConfig,
    convert,
    messages::{self, CoinbaseBuilder, M8_BMM_REQUEST_TAG},
    types::{Ctip, SidechainAck, SidechainNumber, SidechainProposal},
    validator::Validator,
};
use error::WalletError;

pub mod error;

pub struct Wallet {
    main_client: HttpClient,
    validator: Validator,
    bitcoin_wallet: Arc<Mutex<bdk::Wallet<SqliteDatabase>>>,
    db_connection: Arc<Mutex<rusqlite::Connection>>,
    bitcoin_blockchain: ElectrumBlockchain,
    _mnemonic: Mnemonic,
    // seed
    // sidechain number
    // index
    // address (20 byte hash of public key)
    // utxos
    last_sync: Arc<RwLock<Option<SystemTime>>>,
}

impl Wallet {
    pub async fn new(
        data_dir: &Path,
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

        let wallet_database = SqliteDatabase::new(data_dir.join("wallet.sqlite"));
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
                // TODO: delete this? Not persisting to this table anywhere.
                // Not clear how we're going to keep this in sync with the
                // actual mempool. Seems like we could accomplish the same
                // thing through `listtransactions`
                M::up(
                    "CREATE TABLE mempool
                   (txid BLOB UNIQUE NOT NULL,
                    tx_data BLOB NOT NULL);",
                ),
                // This is really a table of _pending_ deposits. Gets wiped
                // upon generating a fresh block. How will this work for
                // non-regtest?
                M::up(
                    "CREATE TABLE deposits
                   (sidechain_number INTEGER NOT NULL,
                    address BLOB NOT NULl,
                    amount INTEGER NOT NULL,
                    txid BLOB NOT NULL);",
                ),
                M::up(
                    "CREATE TABLE bmm_requests
                    (sidechain_number INTEGER NOT NULL,
                     side_block_hash BLOB NOT NULL,
                     UNIQUE(sidechain_number, side_block_hash));",
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
            bitcoin_wallet: Arc::new(Mutex::new(bitcoin_wallet)),
            db_connection: Arc::new(Mutex::new(db_connection)),
            bitcoin_blockchain,
            _mnemonic: mnemonic,

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
        let addr = self
            .bitcoin_wallet
            .lock()
            .get_address(AddressIndex::New)
            .into_diagnostic()?;

        tracing::debug!("Generate block: fetched address: {}", addr.address);

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
        if description_hash != ack.description_hash {
            tracing::error!(
                "Handle sidechain ACK: invalid actual hash vs. ACK hash: {} != {}",
                description_hash,
                ack.description_hash,
            );

            return false;
        }

        true
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

            if ack_all_proposals {
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

            let bmm_hashes = self.get_bmm_requests()?;
            for (sidechain_number, bmm_hash) in &bmm_hashes {
                tracing::info!(
                    "Generate: adding BMM accept for SC {} with hash: {}",
                    sidechain_number,
                    hex::encode(bmm_hash)
                );
                coinbase_builder = coinbase_builder.bmm_accept(*sidechain_number, bmm_hash);
            }

            let coinbase_outputs = coinbase_builder.build().into_diagnostic()?;
            let deposits = self.get_pending_deposits(None)?;
            let deposit_transactions: Vec<Transaction> = deposits
                .into_iter()
                .map(|deposit| deposit.transaction)
                .collect();

            tracing::info!(
                "Generate: mining block with {} coinbase outputs, {} deposits",
                coinbase_outputs.len(),
                deposit_transactions.len()
            );

            self.mine(&coinbase_outputs, deposit_transactions).await?;
            self.delete_pending_sidechain_proposals()?;
            self.delete_pending_deposits()?;
            self.delete_bmm_requests()?;
        }
        Ok(())
    }

    fn delete_pending_deposits(&self) -> Result<()> {
        self.db_connection
            .lock()
            .execute("DELETE FROM deposits;", ())
            .into_diagnostic()?;
        Ok(())
    }

    fn create_deposit_op_drivechain_output(
        sidechain_number: SidechainNumber,
        sidechain_ctip_amount: Amount,
        value_sats: Amount,
    ) -> bdk::bitcoin::TxOut {
        let deposit_txout =
            messages::create_m5_deposit_output(sidechain_number, sidechain_ctip_amount, value_sats);

        bdk::bitcoin::TxOut {
            script_pubkey: bdk::bitcoin::ScriptBuf::from_bytes(
                deposit_txout.script_pubkey.to_bytes(),
            ),
            value: deposit_txout.value.to_sat(),
        }
    }

    async fn fetch_ctip_transaction(&self, ctip_txid: Txid) -> Result<bdk::bitcoin::Transaction> {
        let block_hash = None;

        let rpc_res = self
            .main_client
            .get_raw_transaction(ctip_txid, GetRawTransactionVerbose::<true>, block_hash)
            .await
            .into_diagnostic()?;

        let Some(transaction_hex) = rpc_res.as_str() else {
            return Err(miette!("failed to fetch ctip transaction"));
        };

        let transaction_bytes = hex::decode(transaction_hex).unwrap();
        let transaction =
            bitcoin::consensus::encode::deserialize::<Transaction>(&transaction_bytes)
                .into_diagnostic()?;

        convert::bitcoin_tx_to_bdk_tx(transaction).into_diagnostic()
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "false positive for `bitcoin_wallet`"
    )]
    async fn create_deposit_psbt(
        &self,
        op_drivechain_output: bdk::bitcoin::TxOut,
        sidechain_address_data: PushBytesBuf,
        sidechain_ctip: Option<&Ctip>,
        fee_sats: Option<Amount>,
    ) -> Result<PartiallySignedTransaction> {
        // If the sidechain has a Ctip (i.e. treasury UTXO), the BIP300 rules mandate that we spend the previous
        // Ctip.
        let ctip_foreign_utxo = match sidechain_ctip {
            Some(sidechain_ctip) => {
                let outpoint = bdk::bitcoin::OutPoint {
                    txid: convert::bitcoin_txid_to_bdk_txid(sidechain_ctip.outpoint.txid),
                    vout: sidechain_ctip.outpoint.vout,
                };

                let ctip_transaction = self
                    .fetch_ctip_transaction(sidechain_ctip.outpoint.txid)
                    .await?;

                let psbt_input = bdk::bitcoin::psbt::Input {
                    non_witness_utxo: Some(ctip_transaction),
                    ..bdk::bitcoin::psbt::Input::default()
                };

                Some((psbt_input, outpoint))
            }
            None => None,
        };

        let (psbt, _) = {
            let wallet = self.bitcoin_wallet.lock();

            let mut builder = wallet.build_tx();

            builder
                // important: the M5 OP_DRIVECHAIN output must come directly before the OP_RETURN sidechain address output.
                .add_recipient(
                    op_drivechain_output.script_pubkey,
                    op_drivechain_output.value,
                )
                .add_data(&sidechain_address_data);

            if let Some(fee_sats) = fee_sats {
                builder.fee_absolute(fee_sats.to_sat());
            }

            if let Some((ctip_psbt_input, outpoint)) = ctip_foreign_utxo {
                // This might be wrong. Seems to work!
                let satisfaction_weight = 0;

                builder
                    .add_foreign_utxo(outpoint, ctip_psbt_input, satisfaction_weight)
                    .map_err(WalletError)?;
            }

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
        sidechain_address: String,
        value_sats: Amount,
        fee_sats: Option<Amount>,
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
            value_sats,
        );

        tracing::debug!(
            "Created OP_DRIVECHAIN output with value `{}`, spk `{}` ",
            op_drivechain_output.value,
            op_drivechain_output.script_pubkey.to_asm_string(),
        );

        let sidechain_address_bytes = sidechain_address.into_bytes();
        let sidechain_address_data =
            PushBytesBuf::try_from(sidechain_address_bytes).map_err(|err| {
                miette!("failed to convert sidechain address to PushBytesBuf: {err:#}")
            })?;

        let psbt = self
            .create_deposit_psbt(
                op_drivechain_output,
                sidechain_address_data,
                sidechain_ctip,
                fee_sats,
            )
            .await?;

        tracing::debug!("Created deposit PSBT: {psbt}",);

        let tx = self.sign_transaction(psbt)?;
        let txid = tx.txid();

        tracing::info!("Signed deposit transaction: `{txid}`",);

        tracing::debug!("Serialized deposit transaction: {}", {
            let tx_bytes = bdk::bitcoin::consensus::serialize(&tx);
            hex::encode(tx_bytes)
        });

        self.broadcast_transaction(tx).await?;

        tracing::info!("Broadcasted deposit transaction: `{txid}`",);

        Ok(convert::bdk_txid_to_bitcoin_txid(txid))
    }

    /// Fetches pending deposits. If `sidechain_number` is provided, only
    /// deposits for that sidechain are returned.
    fn get_pending_deposits(
        &self,
        sidechain_number: Option<SidechainNumber>,
    ) -> Result<Vec<Deposit>> {
        let with_connection = |connection: &Connection| -> Result<_> {
            let mut statement = match sidechain_number {
                Some(_sidechain_number) => connection
                    .prepare(
                        "SELECT sidechain_number, address, amount, tx_data
                         FROM deposits INNER JOIN mempool ON deposits.txid = mempool.txid
                         WHERE sidechain_number = ?1;",
                    )
                    .into_diagnostic()?,
                None => connection
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
                    Transaction::consensus_decode_from_finite_reader(&mut tx_data.as_slice())
                        .unwrap();
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
                    .query_map([u8::from(sidechain_number)], func)
                    .into_diagnostic()?,
                None => statement.query_map([], func).into_diagnostic()?,
            };
            let mut deposits = vec![];
            for deposit in rows {
                let deposit = deposit.into_diagnostic()?;
                deposits.push(deposit);
            }
            Ok(deposits)
        };
        with_connection(&self.db_connection.lock())
    }

    fn get_bmm_requests(&self) -> Result<Vec<(SidechainNumber, [u8; 32])>> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, _> {
            let mut statement = connection
                .prepare("SELECT sidechain_number, side_block_hash FROM bmm_requests")
                .into_diagnostic()?;

            let queried = statement
                .query_map([], |row| {
                    let sidechain_number: u8 = row.get(0)?;
                    let data_hash: [u8; 32] = row.get(1)?;
                    Ok((SidechainNumber::from(sidechain_number), data_hash))
                })
                .into_diagnostic()?
                .collect::<Result<_, _>>()
                .into_diagnostic()?;

            Ok(queried)
        };
        with_connection(&self.db_connection.lock())
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

    fn get_utxos(&self) -> Result<()> {
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
    fn delete_bmm_requests(&self) -> Result<()> {
        self.db_connection
            .lock()
            .execute("DELETE FROM bmm_requests;", ())
            .into_diagnostic()?;
        Ok(())
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
        mut psbt: PartiallySignedTransaction,
    ) -> Result<bdk::bitcoin::Transaction> {
        if !self
            .bitcoin_wallet
            .lock()
            .sign(&mut psbt, SignOptions::default())
            .map_err(WalletError::from)?
        {
            return Err(miette!("failed to sign transaction"));
        }

        tracing::debug!("Signed PSBT: {psbt}",);

        Ok(psbt.extract_tx())
    }

    // Creates a BMM request transaction. Does NOT broadcast. `height` is the block height this
    // transaction MUST be included in. `amount` is the amount to spend in the BMM bid.
    pub fn create_bmm_request(
        &self,
        sidechain_number: SidechainNumber,
        sidechain_block_hash: bdk::bitcoin::BlockHash,
        prev_mainchain_block_hash: bdk::bitcoin::BlockHash,
        amount: bdk::bitcoin::Amount,
        locktime: bdk::bitcoin::absolute::LockTime,
    ) -> Result<bdk::bitcoin::Transaction> {
        let psbt = self.build_bmm_tx(
            sidechain_number,
            sidechain_block_hash,
            prev_mainchain_block_hash,
            amount,
            locktime,
        )?;

        let tx = self.sign_transaction(psbt)?;

        tracing::info!("BMM request psbt signed successfully");

        self.insert_new_bmm_request(sidechain_number, sidechain_block_hash)?;
        tracing::info!("inserted new bmm request into db");

        Ok(tx)
    }

    fn insert_new_bmm_request(
        &self,
        sidechain_number: SidechainNumber,
        side_block_hash: BlockHash,
    ) -> Result<()> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_> {
            connection
                .prepare(
                    "INSERT INTO bmm_requests (sidechain_number, side_block_hash) VALUES (?1, ?2)",
                )
                .into_diagnostic()?
                .execute((
                    u8::from(sidechain_number),
                    side_block_hash.to_byte_array().to_vec(),
                ))
                .into_diagnostic()
        };

        with_connection(&self.db_connection.lock())?;
        Ok(())
    }

    fn bmm_request_message(
        sidechain_number: SidechainNumber,
        sidechain_block_hash: BlockHash,
        prev_mainchain_block_hash: BlockHash,
    ) -> Result<bdk::bitcoin::ScriptBuf> {
        let message = [
            M8_BMM_REQUEST_TAG.to_vec(),
            vec![sidechain_number.into()],
            BlockHash::to_byte_array(sidechain_block_hash).to_vec(),
            BlockHash::to_byte_array(prev_mainchain_block_hash).to_vec(),
        ]
        .concat();

        let bytes = bdk::bitcoin::script::PushBytesBuf::try_from(message).into_diagnostic()?;
        Ok(bdk::bitcoin::ScriptBuf::new_op_return(&bytes))
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "false positive for `bitcoin_wallet`"
    )]
    fn build_bmm_tx(
        &self,
        sidechain_number: SidechainNumber,
        sidechain_block_hash: bdk::bitcoin::BlockHash,
        prev_mainchain_block_hash: bdk::bitcoin::BlockHash,
        amount: bdk::bitcoin::Amount,
        locktime: bdk::bitcoin::absolute::LockTime,
    ) -> Result<bdk::bitcoin::psbt::PartiallySignedTransaction> {
        // https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip301.md#m8-bmm-request
        let message = Self::bmm_request_message(
            sidechain_number,
            sidechain_block_hash,
            prev_mainchain_block_hash,
        )?;

        let (psbt, _) = {
            let bitcoin_wallet = self.bitcoin_wallet.lock();
            let mut builder = bitcoin_wallet.build_tx();
            builder
                .nlocktime(locktime)
                .add_recipient(message, amount.to_sat());
            builder.finish().into_diagnostic()?
        };

        Ok(psbt)
    }

    // Broadcasts a transaction to the Bitcoin network.
    pub async fn broadcast_transaction(&self, tx: bdk::bitcoin::Transaction) -> Result<()> {
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

    pub fn get_new_address(&self) -> Result<bdk::bitcoin::Address> {
        // Satisfy clippy with a single function call per lock
        let with_wallet = |wallet: &bdk::Wallet<SqliteDatabase>| -> Result<bdk::bitcoin::Address> {
            // Using AddressIndex::LastUnused here means that we get a new address
            // when funds are received. Without this we'd need to take care not
            // to cross the wallet scan gap.
            let info = wallet
                .get_address(AddressIndex::LastUnused)
                .map_err(|e| miette!("unable to get address: {}", e))?;
            Ok(info.address)
        };

        with_wallet(&self.bitcoin_wallet.lock())
    }
}

type PendingTransactions = (
    SidechainNumber,
    Vec<(u64, cusf_sidechain_types::OutPoint, u64)>,
    Vec<cusf_sidechain_types::Output>,
);

type SidechainUTXOs = BTreeMap<u64, (cusf_sidechain_types::OutPoint, u32, u64, Option<u64>)>;

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
pub struct Deposit {
    pub sidechain_number: u8,
    pub address: Vec<u8>,
    pub amount: u64,
    pub transaction: Transaction,
}
