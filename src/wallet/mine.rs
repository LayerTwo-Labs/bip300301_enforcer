use std::{
    collections::{HashMap, HashSet},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bip300301::{
    client::{BlockTemplateRequest, BoolWitness, GetRawMempoolClient as _},
    MainClient as _,
};
use bitcoin::{
    absolute::{Height, LockTime},
    block::Version as BlockVersion,
    consensus::Encodable as _,
    constants::{genesis_block, SUBSIDY_HALVING_INTERVAL},
    hash_types::TxMerkleNode,
    hashes::Hash as _,
    merkle_tree,
    opcodes::{all::OP_RETURN, OP_0},
    script::PushBytesBuf,
    transaction::Version as TxVersion,
    Amount, Block, BlockHash, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut,
    Txid, Witness,
};
use futures::{
    stream::{self, FusedStream},
    StreamExt as _,
};
use miette::{miette, IntoDiagnostic as _, Result};

use crate::{
    messages::{CoinbaseBuilder, M4AckBundles},
    types::{Ctip, SidechainAck, SidechainNumber, WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD},
    wallet::{
        error::{self, BitcoinCoreRPC, TonicStatusExt},
        Wallet,
    },
};

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

impl Wallet {
    /// Generate coinbase txouts for a new block
    pub(in crate::wallet) fn generate_coinbase_txouts(
        &self,
        ack_all_proposals: bool,
        mainchain_tip: BlockHash,
    ) -> Result<Vec<TxOut>, error::GenerateCoinbaseTxouts> {
        tracing::debug!(
            message = "Generating coinbase txouts",
            ack_all_proposals = ack_all_proposals,
            mainchain_tip = hex::encode(mainchain_tip.as_byte_array()),
        );

        // This is a list of pending sidechain proposals from /our/ wallet, fetched from
        // the DB.
        let sidechain_proposals = self.get_our_sidechain_proposals().inspect_err(|err| {
            tracing::error!("Failed to get sidechain proposals: {err:?}");
        })?;

        let mut coinbase_builder = CoinbaseBuilder::new();
        for sidechain_proposal in sidechain_proposals {
            coinbase_builder.propose_sidechain(sidechain_proposal)?;
        }

        let mut sidechain_acks = self.get_sidechain_acks()?;

        // This is a map of pending sidechain proposals from the /validator/, i.e.
        // proposals broadcasted by (potentially) someone else, and already active.
        let active_sidechain_proposals = self.get_active_sidechain_proposals()?;

        if ack_all_proposals && !active_sidechain_proposals.is_empty() {
            tracing::info!(
                "Handle sidechain ACK: acking all sidechains regardless of what DB says"
            );

            for (sidechain_number, sidechain_proposal) in &active_sidechain_proposals {
                let sidechain_number = *sidechain_number;

                if !sidechain_acks
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

            coinbase_builder.ack_sidechain(
                sidechain_ack.sidechain_number,
                sidechain_ack.description_hash,
            )?;
        }

        let bmm_hashes = self.get_bmm_requests(&mainchain_tip)?;
        for (sidechain_number, bmm_hash) in &bmm_hashes {
            tracing::info!(
                "Generate: adding BMM accept for SC {} with hash: {}",
                sidechain_number,
                hex::encode(bmm_hash)
            );
            coinbase_builder.bmm_accept(*sidechain_number, bmm_hash)?;
        }
        for (sidechain_id, m6ids) in self.get_bundle_proposals()? {
            for (m6id, _blinded_m6, m6id_info) in m6ids {
                if m6id_info.is_none() {
                    coinbase_builder.propose_bundle(sidechain_id, m6id)?;
                }
            }
        }
        // Ack bundles
        // TODO: Exclusively ack bundles that are known to the wallet
        // TODO: ack bundles when M2 messages are present
        if ack_all_proposals && coinbase_builder.messages().m2_acks().is_empty() {
            let active_sidechains = self.inner.validator.get_active_sidechains()?;
            let upvotes = active_sidechains
                .into_iter()
                .map(|sidechain| {
                    if self
                        .inner
                        .validator
                        .get_pending_withdrawals(&sidechain.proposal.sidechain_number)?
                        .is_empty()
                    {
                        Ok(M4AckBundles::ABSTAIN_ONE_BYTE)
                    } else {
                        Ok(0)
                    }
                })
                .collect::<Result<_, crate::validator::GetPendingWithdrawalsError>>()?;
            coinbase_builder.ack_bundles(M4AckBundles::OneByte { upvotes })?;
        }
        let res = coinbase_builder.build()?;
        Ok(res)
    }

    /// Generate suffix txs for a new block
    pub(in crate::wallet) fn generate_suffix_txs(
        &self,
        ctips: &HashMap<SidechainNumber, crate::types::Ctip>,
    ) -> Result<Vec<Transaction>, error::GenerateSuffixTxs> {
        let mut res = Vec::new();
        for (sidechain_id, m6ids) in self.get_bundle_proposals()? {
            let mut ctip = None;
            for (_m6id, blinded_m6, m6id_info) in m6ids {
                let Some(m6id_info) = m6id_info else { continue };
                if m6id_info.vote_count > WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD {
                    let Ctip { outpoint, value } = if let Some(ctip) = ctip {
                        ctip
                    } else {
                        *ctips
                            .get(&sidechain_id)
                            .ok_or_else(|| error::GenerateSuffixTxs::MissingCtip { sidechain_id })?
                    };
                    let new_value = (value - *blinded_m6.fee()) - *blinded_m6.payout();
                    let m6 = blinded_m6.into_m6(sidechain_id, outpoint, value)?;
                    ctip = Some(Ctip {
                        outpoint: OutPoint {
                            txid: m6.compute_txid(),
                            vout: (m6.output.len() - 1) as u32,
                        },
                        value: new_value,
                    });
                    res.push(m6);
                }
            }
        }
        Ok(res)
    }

    /// select non-coinbase txs for a new block
    async fn select_block_txs(&self) -> miette::Result<Vec<Transaction>> {
        let ctips = self.inner.validator.get_ctips()?;
        let mut res = self.generate_suffix_txs(&ctips)?;

        // We want to include all transactions from the mempool into our newly generated block.
        // This approach is perhaps a bit naive, and could fail if there are conflicting TXs
        // pending. On signet the block is constructed using `getblocktemplate`, so this will not
        // be an issue there.
        //
        // Including all the mempool transactions here ensure that pending sidechain deposit
        // transactions get included into a block.
        let raw_mempool = self
            .inner
            .main_client
            .get_raw_mempool(BoolWitness::<false>, BoolWitness::<false>)
            .await
            .map_err(|err| error::BitcoinCoreRPC {
                method: "getrawmempool".to_string(),
                error: err,
            })?;

        for txid in raw_mempool {
            let transaction = self.fetch_transaction(txid).await?;
            res.push(transaction);
        }

        Ok(res)
    }

    /// Construct a coinbase tx from txouts
    fn finalize_coinbase(
        &self,
        best_block_height: u32,
        coinbase_outputs: &[TxOut],
    ) -> miette::Result<Transaction> {
        let coinbase_addr = self.get_new_address()?;
        tracing::trace!(%coinbase_addr, "Fetched address");
        let coinbase_spk = coinbase_addr.script_pubkey();

        let script_sig = bitcoin::blockdata::script::Builder::new()
            .push_int((best_block_height + 1) as i64)
            .push_opcode(OP_0)
            .into_script();
        let value = get_block_value(best_block_height + 1, Amount::ZERO, Network::Regtest);
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
        Ok(Transaction {
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
        })
    }

    /// Finalize a new block by constructing the coinbase tx
    fn finalize_block(
        &self,
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
    ) -> miette::Result<Block> {
        let best_block_hash = self.validator().get_mainchain_tip()?;
        let best_block_height = self.validator().get_header_info(&best_block_hash)?.height;
        tracing::trace!(%best_block_hash, %best_block_height, "Found mainchain tip");

        let coinbase_tx = self.finalize_coinbase(best_block_height, coinbase_outputs)?;
        let txdata = std::iter::once(coinbase_tx).chain(transactions).collect();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .into_diagnostic()?
            .as_secs() as u32;
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
            let () = push_bytes
                .extend_from_slice(witness_commitment.as_byte_array())
                .into_diagnostic()?;
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
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
    ) -> miette::Result<BlockHash> {
        let transaction_count = transactions.len();

        let mut block = self.finalize_block(coinbase_outputs, transactions)?;
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
            .inner
            .main_client
            .submit_block(hex::encode(block_bytes))
            .await
            .map_err(|err| error::BitcoinCoreRPC {
                method: "submitblock".to_string(),
                error: err,
            })?;
        let block_hash = block.header.block_hash();
        tracing::info!(%block_hash, %transaction_count, "Submitted block");
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(block_hash)
    }

    fn check_has_binary(&self, binary: &str) -> Result<(), tonic::Status> {
        // Python is needed for executing the signet miner script.
        let check = std::process::Command::new("which")
            .arg(binary)
            .output()
            .map_err(|err| {
                tonic::Status::internal(format!("{binary} is required for mining on signet: {err}"))
            })?;

        if !check.status.success() {
            Err(tonic::Status::failed_precondition(format!(
                "{} is required for mining on signet",
                binary
            )))
        } else {
            Ok(())
        }
    }

    pub async fn verify_can_mine(&self, blocks: u32) -> Result<()> {
        if blocks == 0 {
            return tonic::Status::invalid_argument("must provide a positive number of blocks")
                .into_diagnostic();
        }

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
            _ => {
                return tonic::Status::failed_precondition(format!(
                    "cannot generate blocks on {}",
                    self.validator().network(),
                ))
                .into_diagnostic();
            }
        }

        if blocks > 1 {
            return tonic::Status::invalid_argument(
                "cannot generate more than one block on signet",
            )
            .into_diagnostic();
        }

        let template = self
            .inner
            .main_client
            .get_block_template(BlockTemplateRequest {
                rules: vec!["signet".to_string(), "segwit".to_string()],
                capabilities: HashSet::new(),
            })
            .await
            .map_err(|err| BitcoinCoreRPC {
                method: "getblocktemplate".to_string(),
                error: err,
            })?;

        let Some(signet_challenge) = template.signet_challenge else {
            return Err(miette!("no signet challenge found"));
        };

        let address =
            bitcoin::Address::from_script(&signet_challenge, bitcoin::params::Params::SIGNET)
                .map_err(|err| miette!("unable to parse signet challenge: {err}"))?;

        let address_info = self
            .inner
            .main_client
            .get_address_info(address.as_unchecked())
            .await
            .map_err(|err| error::BitcoinCoreRPC {
                method: "getaddressinfo".to_string(),
                error: err,
            })?;

        if !address_info.is_mine {
            return tonic::Status::failed_precondition(format!(
                "signet challenge address {} is not in mainchain wallet",
                address,
            ))
            .into_diagnostic();
        }

        tracing::debug!("verified ability to solve signet challenge");

        if let Err(status) = self.check_has_binary("python3") {
            return status.into_diagnostic();
        }
        tracing::debug!("verified existence of python3");

        if let Err(status) = self.check_has_binary("bitcoin-util") {
            return status.into_diagnostic();
        }
        tracing::debug!("verified existence of bitcoin-util");

        Ok(())
    }

    // Generate a single signet block, through shelling out to the signet miner script
    // from Bitcoin Core. We assume that validation of this request has
    // happened elsewhere (i.e. that we're on signet, and have signing
    // capabilities).
    async fn generate_signet_block(&self, address: &str) -> miette::Result<BlockHash> {
        use std::process::Command;
        // Store the signet miner in a temporary directory that's consistent across
        // invocations. This means we'll only need to download it once for every time
        // we start the process.
        let dir = std::env::temp_dir().join(format!("signet-miner-{}", std::process::id()));

        // Check if signet miner directory exists
        if !std::path::Path::new(&dir).exists() {
            tracing::info!("Signet miner not found, downloading into {}", dir.display());

            Command::new("mkdir")
                .args(["-p", &dir.to_string_lossy()])
                .output()
                .into_diagnostic()
                .map_err(|e| miette!("Failed to create signet miner directory: {}", e))?;

            // Execute the download script
            let mut command = Command::new("bash");

            // https://github.com/LayerTwo-Labs/bitcoin-patched/commit/01010b132a616f151c461a4318ab86c274395912
            const BITCOIN_PATCHED_COMMIT: &str = "01010b132a616f151c461a4318ab86c274395912";
            command.current_dir(&dir)
                .arg("-c")
                .arg(format!(r#"
                    git clone -n --depth=1 --filter=tree:0 \
                    https://github.com/LayerTwo-Labs/bitcoin-patched.git signet-miner && \
                    cd signet-miner && \
                    git sparse-checkout set --no-cone contrib/signet/miner test/functional/test_framework && \
                    git checkout {BITCOIN_PATCHED_COMMIT}
                "#));

            let output = command
                .output()
                .into_diagnostic()
                .map_err(|e| miette!("Failed to download signet miner: {}", e))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(miette!("Failed to download signet miner: {}", stderr));
            }

            tracing::info!("Successfully downloaded signet miner");
        } else {
            tracing::info!("Signet miner already exists");
        }

        let mut command = Command::new("python3");
        command
            .current_dir(&dir)
            .arg("signet-miner/contrib/signet/miner")
            .args(["--cli", "bitcoin-cli"])
            .arg("generate")
            .args(["--address", address])
            .args(["--grind-cmd", "bitcoin-util grind"])
            .args(["--block-interval", "60"])
            .arg("--coinbasetxn") // enable coinbasetxn capability for getblocktemplate
            .arg(format!(
                "--getblocktemplate-command=bitcoin-cli -rpcconnect={} -rpcport={} getblocktemplate",
                self.serve_rpc_addr.ip(),
                self.serve_rpc_addr.port()
            ))
            .arg("--min-nbits");

        tracing::info!("Running signet miner: {:?}", command);

        let output = command
            .output()
            .into_diagnostic()
            .map_err(|e| miette!("Failed to execute signet miner: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(miette!("Signet miner failed: {}", stderr));
        }

        tracing::debug!(
            "Signet miner output: {}",
            String::from_utf8_lossy(&output.stdout)
        );

        // The output of the signet miner is unfortunately not very useful,
        // so we have to fetch the most recent block in order to get the hash.
        let block_hash = self
            .inner
            .main_client
            .getbestblockhash()
            .await
            .map_err(|err| miette!("Failed to fetch most recent block hash: {err}"))?;

        tracing::info!("Generated signet block: {}", block_hash);

        Ok(block_hash)
    }

    /// Build and mine a single block
    async fn generate_block(&self, ack_all_proposals: bool) -> miette::Result<BlockHash> {
        let Some(mainchain_tip) = self.inner.validator.try_get_mainchain_tip()? else {
            return Err(miette!("Validator is not synced"));
        };
        if self.inner.validator.network() == Network::Signet {
            let address = self.get_new_address()?;
            return self
                .generate_signet_block(address.to_string().as_str())
                .await;
        }
        let coinbase_outputs = self.generate_coinbase_txouts(ack_all_proposals, mainchain_tip)?;
        let transactions = self.select_block_txs().await?;

        tracing::info!(
            coinbase_outputs = %coinbase_outputs.len(),
            transactions = %transactions.len(),
            "Mining block",
        );

        let block_hash = self.mine(&coinbase_outputs, transactions).await?;
        self.delete_bmm_requests(&mainchain_tip)?;
        Ok(block_hash)
    }

    pub fn generate_blocks<Ref>(
        this: Ref,
        count: u32,
        ack_all_proposals: bool,
    ) -> impl FusedStream<Item = miette::Result<BlockHash>>
    where
        Ref: std::borrow::Borrow<Self>,
    {
        tracing::info!("Generate: creating {} block(s)", count);
        stream::try_unfold((this, count), move |(this, remaining)| async move {
            if remaining == 0 {
                Ok(None)
            } else {
                let block_hash = this.borrow().generate_block(ack_all_proposals).await?;
                Ok(Some((block_hash, (this, remaining - 1))))
            }
        })
        .fuse()
    }
}
