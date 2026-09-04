//! Mining blocks without a wallet, backing
//! `BlockProducerService.GenerateToAddress`. The coinbase pays out to a
//! caller-provided address. On regtest, blocks are constructed and their PoW
//! ground locally; on signet, blocks are produced by the signet miner script,
//! signed by the Bitcoin Core node's wallet.

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bitcoin::{
    Amount, Block, BlockHash, Network, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid,
    Witness,
    absolute::{Height, LockTime},
    block::Version as BlockVersion,
    consensus::Encodable as _,
    constants::{SUBSIDY_HALVING_INTERVAL, genesis_block},
    hash_types::TxMerkleNode,
    hashes::Hash as _,
    merkle_tree,
    opcodes::{OP_0, all::OP_RETURN},
    script::PushBytesBuf,
    transaction::Version as TxVersion,
};
use bitcoin_jsonrpsee::{
    MainClient as _,
    client::{BlockTemplate, BlockTemplateRequest, CoinbaseTxnOrValue},
};

use crate::{
    bins::{self, CommandExt as _},
    block_producer::{BlockProducer, error},
    errors::ErrorChain,
    messages::CoinbaseBuilder,
    types::{BmmCommitment, SidechainNumber},
};

pub(in crate::block_producer) fn bmm_auction_winners(
    bids: impl IntoIterator<Item = (SidechainNumber, BmmCommitment, Txid, Amount)>,
) -> HashMap<SidechainNumber, (BmmCommitment, Txid, Amount)> {
    let mut winners = HashMap::new();
    for (sidechain_number, commitment, txid, fee) in bids {
        let winner = winners
            .entry(sidechain_number)
            .or_insert((commitment, txid, fee));
        if fee > winner.2 {
            *winner = (commitment, txid, fee);
        }
    }
    winners
}

/// The bids that must be kept out of a block: every bid for a slot whose
/// winner the block is known to contain, apart from that winner itself.
///
/// A slot missing from `forced_winners` keeps all of its bids. Excluding the
/// losers of a slot whose winner might not be in the block risks settling that
/// slot with no bid at all.
pub(in crate::block_producer) fn losing_bmm_bids(
    seen_bmm_requests: HashMap<SidechainNumber, HashMap<BmmCommitment, HashSet<Txid>>>,
    forced_winners: &HashMap<SidechainNumber, Txid>,
) -> Vec<Txid> {
    seen_bmm_requests
        .into_iter()
        .filter_map(|(sidechain_number, requests)| {
            let winner_txid = *forced_winners.get(&sidechain_number)?;
            Some(
                requests
                    .into_values()
                    .flatten()
                    .filter(move |txid| *txid != winner_txid),
            )
        })
        .flatten()
        .collect()
}

/// BMM request cleanup happens after the block has been submitted and observed
/// by the validator. Keep the mined block as the operation's result even if the
/// best-effort cleanup fails, so callers are not invited to retry an operation
/// that has already happened.
fn finish_bmm_request_cleanup(
    block_hash: BlockHash,
    result: Result<(), rusqlite::Error>,
) -> BlockHash {
    if let Err(err) = result {
        tracing::error!(
            %block_hash,
            "failed to delete BMM requests for mined block: {:#}",
            ErrorChain::new(&err),
        );
    }
    block_hash
}

fn target_block_interval(signet_challenge: &bitcoin::Script) -> std::time::Duration {
    const L2L_SIGNET_CHALLENGE: &[u8] = b"00141551188e5153533b4fdd555449e640d9cc129456";
    const L2L_SIGNET_TARGET_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
    const DEFAULT_TARGET_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);
    if signet_challenge.as_bytes() == L2L_SIGNET_CHALLENGE {
        L2L_SIGNET_TARGET_INTERVAL
    } else {
        DEFAULT_TARGET_INTERVAL
    }
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

const WITNESS_RESERVED_VALUE: [u8; 32] = [0; 32];

impl BlockProducer {
    async fn fetch_block_template(
        &self,
        rules: Vec<String>,
    ) -> Result<BlockTemplate, error::GetBlockTemplate> {
        self.gbt_client()
            .get_block_template(BlockTemplateRequest {
                mode: None,
                data: None,
                rules,
                capabilities: HashSet::from(["coinbasetxn".to_string()]),
                long_poll_id: None,
            })
            .await
            .map_err(|err| error::GetBlockTemplate {
                source: err,
                template_error: self.last_gbt_error(),
            })
    }

    async fn select_block_txs(
        &self,
        mainchain_tip: BlockHash,
    ) -> Result<Vec<(Transaction, Amount)>, error::SelectBlockTxs> {
        let template = self
            .fetch_block_template(vec![
                "segwit".to_string(),
                crate::rpc_client::BIP300301_RULE.to_string(),
            ])
            .await?;

        // The template is built on its server's tip. We build on the
        // validator's. If those disagree one of them is still catching up, and
        // mixing the template's tx set onto a different parent yields a block
        // Core rejects as `inconclusive`. Say so plainly instead.
        if template.prev_blockhash != mainchain_tip {
            return Err(error::SelectBlockTxs::TemplateTipMismatch {
                template_tip: template.prev_blockhash,
                validator_tip: mainchain_tip,
            });
        }

        // A `coinbasetxn` template comes from the enforcer's own template
        // server, which builds it through the block producer hooks: the tx set
        // respects the enforcer's mempool rules and already ends with the
        // withdrawal-payout suffix txs. A `coinbasevalue` template comes from
        // Bitcoin Core, which knows nothing of drivechain rules, so the suffix
        // txs must be generated locally.
        let mut res = match template.coinbase_txn_or_value {
            CoinbaseTxnOrValue::Txn(_) => Vec::new(),
            CoinbaseTxnOrValue::ValueSats(_) => {
                let ctips = self.validator().get_ctips()?;
                self.generate_suffix_txs(&ctips)
                    .await?
                    .into_iter()
                    .map(|tx| (tx, Amount::ZERO))
                    .collect()
            }
        };

        for template_tx in template.transactions {
            let txid = template_tx.txid;
            let transaction: Transaction =
                bitcoin::consensus::deserialize(&template_tx.data).map_err(|err| {
                    error::SelectBlockTxs::DecodeTemplateTransaction { txid, source: err }
                })?;
            let fee = template_tx.fee.to_unsigned().map_err(|_| {
                error::SelectBlockTxs::NegativeTemplateTransactionFee {
                    txid,
                    fee: template_tx.fee,
                }
            })?;
            res.push((transaction, fee));
        }

        Ok(res)
    }

    /// Construct a coinbase tx paying out to `coinbase_spk`.
    fn finalize_coinbase(
        &self,
        best_block_height: u32,
        coinbase_spk: ScriptBuf,
        coinbase_outputs: &[TxOut],
        fees: Amount,
    ) -> Transaction {
        let script_sig = bitcoin::blockdata::script::Builder::new()
            .push_int((best_block_height + 1) as i64)
            .push_opcode(OP_0)
            .into_script();
        let value = get_block_value(best_block_height + 1, fees, Network::Regtest);
        let output = if value > Amount::ZERO {
            vec![TxOut {
                script_pubkey: coinbase_spk,
                value,
            }]
        } else {
            vec![TxOut {
                script_pubkey: ScriptBuf::builder().push_opcode(OP_RETURN).into_script(),
                value: Amount::ZERO,
            }]
        };
        Transaction {
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
        }
    }

    /// Finalize a new block by constructing the coinbase tx
    fn finalize_block(
        &self,
        coinbase_spk: ScriptBuf,
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
        fees: Amount,
    ) -> Result<Block, error::FinalizeBlock> {
        let best_block_hash = self.validator().get_mainchain_tip()?;
        let tip_header = self.validator().get_header_info(&best_block_hash)?;
        let best_block_height = tip_header.height;
        tracing::trace!(%best_block_hash, %best_block_height, "Found mainchain tip");

        let coinbase_tx =
            self.finalize_coinbase(best_block_height, coinbase_spk, coinbase_outputs, fees);
        let txdata = std::iter::once(coinbase_tx).chain(transactions).collect();
        // Keep block times strictly increasing so blocks mined faster than once
        // per second are not rejected as `time-too-old` (timestamp must exceed
        // the median-time-past). This mirrors `getblocktemplate`'s `mintime`.
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as u32;
        let timestamp = now.max(tip_header.timestamp.saturating_add(1));
        let genesis_block = genesis_block(bitcoin::Network::Regtest);
        let bits = genesis_block.header.bits;
        let header = bitcoin::block::Header {
            version: BlockVersion::NO_SOFT_FORK_SIGNALLING,
            prev_blockhash: best_block_hash,
            // merkle root is computed after the witness commitment is added to coinbase
            merkle_root: TxMerkleNode::all_zeros(),
            time: timestamp,
            bits,
            nonce: 0,
        };
        let mut block = Block { header, txdata };
        let witness_root = block.witness_root().unwrap();
        let witness_commitment =
            Block::compute_witness_commitment(&witness_root, &WITNESS_RESERVED_VALUE);

        // https://github.com/bitcoin/bips/blob/master/bip-0141.mediawiki#commitment-structure
        const WITNESS_COMMITMENT_HEADER: [u8; 4] = [0xaa, 0x21, 0xa9, 0xed];
        let witness_commitment_spk = {
            let mut push_bytes = PushBytesBuf::from(WITNESS_COMMITMENT_HEADER);
            let () = push_bytes.extend_from_slice(witness_commitment.as_byte_array())?;
            ScriptBuf::new_op_return(push_bytes)
        };
        block.txdata[0].output.push(TxOut {
            script_pubkey: witness_commitment_spk,
            value: bitcoin::Amount::ZERO,
        });
        let mut tx_hashes: Vec<_> = block.txdata.iter().map(Transaction::compute_txid).collect();
        block.header.merkle_root = merkle_tree::calculate_root_inline(&mut tx_hashes)
            .unwrap()
            .to_raw_hash()
            .into();
        Ok(block)
    }

    /// Mine a block
    async fn mine(
        &self,
        coinbase_spk: ScriptBuf,
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
        fees: Amount,
    ) -> Result<BlockHash, error::Mine> {
        let transaction_count = transactions.len();

        let mut block = self.finalize_block(coinbase_spk, coinbase_outputs, transactions, fees)?;
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
        if let Some(reason) = self
            .main_client()
            .submit_block(hex::encode(block_bytes))
            .await
            .map_err(|err| error::BitcoinCoreRPC {
                method: "submitblock".to_string(),
                error: err,
            })?
        {
            return Err(error::Mine::BlockRejected { reason });
        }
        let block_hash = block.header.block_hash();
        tracing::info!(%block_hash, %transaction_count, "Submitted block");
        let () = self.await_block_connection(block_hash).await?;
        Ok(block_hash)
    }

    /// Wait until the validator has processed `block_hash`. A successful
    /// `submitblock` only means Bitcoin Core accepted the block. The
    /// enforcer syncs it asynchronously, so returning before the validator
    /// has caught up would let the next block template build on a stale
    /// tip, which again would lead to Bitcoin Core rejecting blocks as duplicate.
    async fn await_block_connection(
        &self,
        block_hash: BlockHash,
    ) -> Result<(), error::AwaitBlockConnection> {
        const TIMEOUT: Duration = Duration::from_secs(10);
        const POLL_INTERVAL: Duration = Duration::from_millis(25);
        let poll = async {
            loop {
                // Block info is written in the same DB transaction that
                // advances the validator tip, so once it is present the
                // validator's view includes `block_hash`, even if the tip
                // has since moved past it.
                if self
                    .validator()
                    .try_get_block_infos(&block_hash, 0)?
                    .is_some()
                {
                    return Ok(());
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        };
        tokio::time::timeout(TIMEOUT, poll)
            .await
            .map_err(|_elapsed| error::AwaitBlockConnection::Timeout {
                block_hash,
                timeout: TIMEOUT,
            })?
    }

    fn check_has_binary(&self, binary: &Path) -> Result<(), error::MissingBinary> {
        let binary = binary.to_string_lossy().to_string();

        // Python is needed for executing the signet miner script.
        let check = std::process::Command::new("which")
            .arg(&binary)
            .output()
            .map_err(|err| error::MissingBinary {
                name: binary.clone(),
                source: Some(err),
            })?;

        if !check.status.success() {
            Err(error::MissingBinary {
                name: binary,
                source: None,
            })
        } else {
            Ok(())
        }
    }

    pub async fn verify_can_mine(&self, blocks: NonZeroU32) -> Result<(), error::VerifyCanMine> {
        match self.validator().network() {
            // Mining on regtest always works.
            bitcoin::Network::Regtest => return Ok(()),

            // Verify that's we're able to mine on signet. This involves solving the
            // signet challenge. This challenge can be complex - but the typical signet
            // challenge is just a script pubkey that belongs to the signet creators
            // wallet.
            //
            // We make a qualified guess here that the signet challenge is just a script pubkey,
            // and verify that the corresponding address is in the mainchain wallet.
            bitcoin::Network::Signet => (),
            network => {
                return Err(error::VerifyCanMine::Network(network));
            }
        }

        // Signet blocks come out of the signet miner, which pulls its template
        // from our own `getblocktemplate` server (see
        // `generate_signet_block`). That is also the only place the drivechain
        // coinbase messages get added on signet, so there is no serving this
        // request without the template server running. Checked before the block
        // count, as it is the more fundamental misconfiguration of the two.
        if !self.config().enable_block_template_server {
            return Err(error::VerifyCanMine::NoBlockTemplateServerOnSignet);
        }

        if blocks.get() > 1 {
            return Err(error::VerifyCanMine::MultipleBlocksOnSignet);
        }

        let template = self
            .fetch_block_template(vec![
                "signet".to_string(),
                "segwit".to_string(),
                crate::rpc_client::BIP300301_RULE.to_string(),
            ])
            .await?;

        let Some(signet_challenge) = template.signet_challenge else {
            return Err(error::VerifyCanMine::NoSignetChallengeFound);
        };

        let address =
            bitcoin::Address::from_script(&signet_challenge, bitcoin::params::Params::SIGNET)?;

        let address_info = self
            .main_client()
            .get_address_info(address.as_unchecked())
            .await
            .map_err(|err| error::BitcoinCoreRPC {
                method: "getaddressinfo".to_string(),
                error: err,
            })?;

        if !address_info.is_mine {
            return Err(error::VerifyCanMine::SignetChallengeAddressMissing(address));
        }

        tracing::debug!("verified ability to solve signet challenge");

        let () = self.check_has_binary(&PathBuf::from("python3"))?;
        tracing::debug!("verified existence of `python3`");

        let () = self.check_has_binary(&self.config().mining_opts.bitcoin_cli_path)?;
        tracing::debug!("verified existence of `bitcoin-cli`");

        let () = self.check_has_binary(&self.config().mining_opts.bitcoin_util_path)?;
        tracing::debug!("verified existence of `bitcoin-util`");

        Ok(())
    }

    async fn get_signet_miner_path(&self) -> Result<PathBuf, error::GetSignetMinerPath> {
        if let Some(signet_mining_script_path) =
            self.config().mining_opts.signet_mining_script_path.clone()
        {
            tracing::debug!(
                "Using custom signet miner script path: {}",
                signet_mining_script_path.display()
            );
            Ok(signet_mining_script_path)
        } else {
            tracing::debug!("Using default signet miner script path");

            // Store the signet miner in a temporary directory that's consistent across
            // invocations. This means we'll only need to download it once for every time
            // we start the process.
            let dir = std::env::temp_dir().join(format!("signet-miner-{}", std::process::id()));

            // Check if signet miner directory exists
            if !std::path::Path::new(&dir).exists() {
                use tokio::process::Command;
                tracing::info!("Signet miner not found, downloading into {}", dir.display());

                let mut command = Command::new("mkdir");
                command.args(["-p", &dir.to_string_lossy()]);
                command.kill_on_drop(true); // important: avoid lingering mining processes that may loop forever with new requests
                command
                    .run_utf8()
                    .await
                    .map_err(error::GetSignetMinerPath::CreateSignetMinerDir)?;

                // Execute the download script
                let mut command = Command::new("bash");

                // https://github.com/LayerTwo-Labs/bitcoin-patched/blob/db46e768a88a5c5cf5ec1b1a6bc56023cc201884/contrib/signet/miner
                const BITCOIN_PATCHED_COMMIT: &str = "db46e768a88a5c5cf5ec1b1a6bc56023cc201884";
                command.current_dir(&dir)
                .arg("-c")
                .arg(format!(r#"
                    git clone -n --depth=1 --filter=tree:0 \
                    https://github.com/LayerTwo-Labs/bitcoin-patched.git signet-miner && \
                    cd signet-miner && \
                    git sparse-checkout set --no-cone contrib/signet/miner test/functional/test_framework && \
                    git checkout {BITCOIN_PATCHED_COMMIT}
                "#));

                let _output = command
                    .run_utf8()
                    .await
                    .map_err(error::GetSignetMinerPath::DownloadSignetMiner)?;
                tracing::info!("Successfully downloaded signet miner");
            } else {
                tracing::info!("Signet miner already exists");
            }

            Ok(dir.join("signet-miner/contrib/signet/miner"))
        }
    }

    // Generate a single signet block, through shelling out to the signet miner script
    // from Bitcoin Core. We assume that validation of this request has
    // happened elsewhere (i.e. that we're on signet, and have signing
    // capabilities).
    async fn generate_signet_block(
        &self,
        coinbase_recipient: Option<bitcoin::Address>,
    ) -> Result<BlockHash, error::GenerateSignetBlock> {
        let tip_header = self
            .validator()
            .get_header_info(&self.validator().get_mainchain_tip()?)?;

        let getblocktemplate_command = Some(format!(
            "bitcoin-cli -rpcconnect={} -rpcport={} getblocktemplate",
            self.config().serve_rpc_addr.ip(),
            self.config().serve_rpc_addr.port()
        ));
        let target_block_interval = self.signet_challenge().map(target_block_interval);

        let mining_script_path = self.get_signet_miner_path().await?;
        let miner = bins::SignetMiner {
            path: mining_script_path,
            bitcoin_cli: self.config().bitcoin_cli(bitcoin::Network::Signet),
            bitcoin_util: self.config().mining_opts.bitcoin_util_path.clone(),
            block_interval: target_block_interval,
            nbits: None,
            coinbase_recipient,
            getblocktemplate_command,
            coinbasetxn: true,
            debug: self.config().mining_opts.signet_mining_script_debug,
        };

        let mut command_args = Vec::new();
        if let Some(target_block_interval) = target_block_interval
            && let tip_header_time =
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(tip_header.timestamp.into())
            && let now = std::time::SystemTime::now()
            && let Ok(tip_age) = now.duration_since(tip_header_time)
            && tip_age > target_block_interval
        {
            let next_block_time = tip_header.timestamp
                + tip_age.as_secs().midpoint(target_block_interval.as_secs()) as u32;
            command_args.push(format!("--set-block-time={next_block_time}"));
        };

        let mut command = miner.command("generate", command_args);
        // Important: avoid lingering mining processes that may loop forever with new requests
        command.kill_on_drop(true);

        // We want to stream stdout/stderr as they come in, for investigating hanging processes.
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        tracing::debug!("Running signet miner: {:?}", command);

        let start = std::time::Instant::now();
        let mut child = command.spawn().map_err(bins::CommandError::from)?;

        let stdout_pipe = child.stdout.take().expect("stdout was piped");
        let stderr_pipe = child.stderr.take().expect("stderr was piped");

        use tokio::io::{AsyncBufReadExt as _, BufReader};

        let stdout_task = tokio::spawn(async move {
            let reader = BufReader::new(stdout_pipe);
            let mut lines = reader.lines();
            let mut collected = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "signet_miner::stdout", "{line}");
                if !collected.is_empty() {
                    collected.push('\n');
                }
                collected.push_str(&line);
            }
            collected
        });

        let stderr_task = tokio::spawn(async move {
            let reader = BufReader::new(stderr_pipe);
            let mut lines = reader.lines();
            let mut collected = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "signet_miner::stderr", "{line}");
                if !collected.is_empty() {
                    collected.push('\n');
                }
                collected.push_str(&line);
            }
            collected
        });

        const SIGNET_MINER_TIMEOUT: Duration = Duration::from_secs(10);
        let status = tokio::time::timeout(SIGNET_MINER_TIMEOUT, child.wait())
            .await
            .map_err(|_elapsed| {
                tracing::error!(
                    "signet miner subprocess timed out after {}s",
                    SIGNET_MINER_TIMEOUT.as_secs()
                );
                error::GenerateSignetBlock::Timeout {
                    duration: SIGNET_MINER_TIMEOUT,
                }
            })?
            .map_err(bins::CommandError::from)?;

        // Stdout+stderr is already streamed above, so don't need to log it again
        let _stdout = stdout_task.await.unwrap_or_default();
        let stderr = stderr_task.await.unwrap_or_default();

        let duration = start.elapsed();

        tracing::debug!("Signet miner finished in {duration:?}: '{status}'");

        if stderr.contains("WARNING submitblock returned bad-diffbits") {
            let err_msg = "block rejected: bad-diffbits";
            return Err(bins::CommandError::Stderr(err_msg.to_string().into_bytes()).into());
        }

        if !status.success() {
            // The miner's stderr only carries the JSON-RPC code and message
            // from `bitcoin-cli getblocktemplate`. The underlying template
            // error is recorded by the block producer hooks.
            let mut stderr = stderr;
            if let Some(template_err) = self.last_gbt_error().as_deref() {
                stderr.push_str("\nblock template error: ");
                stderr.push_str(template_err);
            }
            return Err(bins::CommandError::Stderr(stderr.into_bytes()).into());
        }

        // The output of the signet miner is unfortunately not very useful,
        // so we have to fetch the most recent block in order to get the hash.
        let block_hash = self.main_client().getbestblockhash().await.map_err(|err| {
            let err = error::BitcoinCoreRPC {
                method: "getbestblockhash".to_owned(),
                error: err,
            };
            error::GenerateSignetBlock::FetchMostRecentBlockHash(err)
        })?;

        tracing::info!("Generated signet block: {}", block_hash);

        let () = self.await_block_connection(block_hash).await?;

        Ok(block_hash)
    }

    /// Build and mine a single block, paying the block reward to
    /// `coinbase_addr`. The caller is responsible for verifying that mining is
    /// possible (see [`Self::verify_can_mine`]).
    pub async fn generate_block(
        &self,
        coinbase_addr: bitcoin::Address,
        ack_all_proposals: bool,
    ) -> Result<BlockHash, error::GenerateBlock> {
        if self.validator().network() == Network::Signet {
            return self
                .generate_signet_block(Some(coinbase_addr))
                .await
                .map_err(error::GenerateBlock::GenerateSignetBlock);
        }
        let coinbase_spk = coinbase_addr.script_pubkey();
        let Some(mainchain_tip) = self.validator().try_get_mainchain_tip()? else {
            return Err(error::GenerateBlock::ValidatorNotSynced);
        };
        let mut coinbase_outputs = Vec::new();
        let () = self
            .extend_coinbase_txouts(ack_all_proposals, mainchain_tip, &mut coinbase_outputs)
            .await?;
        let selected = self.select_block_txs(mainchain_tip).await?;
        let winners = bmm_auction_winners(selected.iter().filter_map(|(tx, fee)| {
            let request = crate::messages::parse_m8_tx(tx)?;
            (request.prev_mainchain_block_hash == mainchain_tip).then(|| {
                (
                    request.sidechain_number,
                    request.sidechain_block_hash,
                    tx.compute_txid(),
                    *fee,
                )
            })
        }));
        let mut coinbase_builder = CoinbaseBuilder::new(&mut coinbase_outputs)?;
        let mut fees = Amount::ZERO;
        let mut transactions = Vec::with_capacity(selected.len());
        for (tx, fee) in selected {
            if let Some(request) = crate::messages::parse_m8_tx(&tx) {
                let txid = tx.compute_txid();
                if winners
                    .get(&request.sidechain_number)
                    .is_none_or(|(_, winner_txid, _)| *winner_txid != txid)
                {
                    continue;
                }
                coinbase_builder
                    .bmm_accept(request.sidechain_number, request.sidechain_block_hash)?;
            }
            fees += fee;
            transactions.push(tx);
        }
        let () = coinbase_builder.build()?;

        tracing::info!(
            coinbase_outputs = %coinbase_outputs.len(),
            transactions = %transactions.len(),
            %fees,
            "Mining block",
        );

        let block_hash = self
            .mine(coinbase_spk, &coinbase_outputs, transactions, fees)
            .await?;
        let cleanup_result = self
            .db()
            .delete_bmm_requests(&mainchain_tip, &block_hash)
            .await;
        Ok(finish_bmm_request_cleanup(block_hash, cleanup_result))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use bitcoin::{Amount, BlockHash, Txid, hashes::Hash as _};

    use super::{bmm_auction_winners, finish_bmm_request_cleanup, losing_bmm_bids};
    use crate::types::{BmmCommitment, SidechainNumber};

    #[test]
    fn highest_bmm_fee_wins_each_sidechain_slot() {
        let slot = SidechainNumber(7);
        let low_txid = Txid::from_byte_array([1; 32]);
        let high_txid = Txid::from_byte_array([2; 32]);
        let other_txid = Txid::from_byte_array([3; 32]);
        let winners = bmm_auction_winners([
            (
                slot,
                BmmCommitment([1; 32]),
                low_txid,
                Amount::from_sat(1_000),
            ),
            (
                slot,
                BmmCommitment([2; 32]),
                high_txid,
                Amount::from_sat(2_000),
            ),
            (
                SidechainNumber(8),
                BmmCommitment([3; 32]),
                other_txid,
                Amount::from_sat(500),
            ),
        ]);

        assert_eq!(winners[&slot].1, high_txid);
        assert_eq!(winners[&SidechainNumber(8)].1, other_txid);
    }

    /// A slot's losing bids are only excluded once the block is known to
    /// contain that slot's winner. A slot whose winner might be squeezed out
    /// by tx selection keeps every one of its bids, so the slot is not left
    /// with none.
    #[test]
    fn only_settled_slots_exclude_their_losing_bids() {
        let settled_slot = SidechainNumber(7);
        let unsettled_slot = SidechainNumber(8);
        let winner_txid = Txid::from_byte_array([1; 32]);
        let loser_txid = Txid::from_byte_array([2; 32]);
        let unsettled_txid = Txid::from_byte_array([3; 32]);
        let seen_bmm_requests = HashMap::from([
            (
                settled_slot,
                HashMap::from([
                    (BmmCommitment([1; 32]), HashSet::from([winner_txid])),
                    (BmmCommitment([2; 32]), HashSet::from([loser_txid])),
                ]),
            ),
            (
                unsettled_slot,
                HashMap::from([(BmmCommitment([3; 32]), HashSet::from([unsettled_txid]))]),
            ),
        ]);
        let forced_winners = HashMap::from([(settled_slot, winner_txid)]);

        assert_eq!(
            losing_bmm_bids(seen_bmm_requests, &forced_winners),
            vec![loser_txid]
        );
    }

    #[test]
    fn cleanup_failure_preserves_mined_block_result() {
        let block_hash = BlockHash::from_byte_array([0x42; 32]);
        assert_eq!(
            finish_bmm_request_cleanup(block_hash, Err(rusqlite::Error::InvalidQuery)),
            block_hash
        );
    }
}
