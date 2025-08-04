use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bitcoin::{
    Amount, Block, BlockHash, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut,
    Txid, Witness,
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
    client::{BlockTemplateRequest, BoolWitness, GetRawMempoolClient as _},
};
use futures::{
    StreamExt as _,
    stream::{self, FusedStream},
};

use crate::{
    bins::{self, CommandExt as _},
    errors::ErrorChain,
    messages::{CoinbaseBuilder, M4AckBundles},
    types::{Ctip, SidechainAck, SidechainNumber, WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD},
    wallet::{
        Wallet,
        error::{self, BitcoinCoreRPC},
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
    /// Extend coinbase txouts for a new block
    pub(in crate::wallet) async fn extend_coinbase_txouts(
        &self,
        ack_all_proposals: bool,
        mainchain_tip: BlockHash,
        coinbase_txouts: &mut Vec<TxOut>,
    ) -> Result<(), error::GenerateCoinbaseTxouts> {
        let mut coinbase_builder = CoinbaseBuilder::new(coinbase_txouts)?;
        tracing::debug!(
            message = "Extending coinbase txouts",
            ack_all_proposals = ack_all_proposals,
            mainchain_tip = hex::encode(mainchain_tip.as_byte_array()),
        );

        // This is a list of pending sidechain proposals from /our/ wallet, fetched from
        // the DB.
        let sidechain_proposals = self
            .get_our_sidechain_proposals()
            .await
            .inspect_err(|err| {
                tracing::error!(
                    "Failed to get sidechain proposals: {:#}",
                    ErrorChain::new(err)
                );
            })?;

        // Sidechain proposals that already exist in the chain,
        // or will already be proposed in coinbase txouts
        let proposed_sidechains = self
            .validator()
            .get_sidechains()?
            .into_iter()
            .map(|(sidechain_proposal_id, _)| sidechain_proposal_id)
            .chain(coinbase_builder.messages().m1_sidechain_proposal_ids())
            .collect::<HashSet<_>>();

        for sidechain_proposal in sidechain_proposals {
            if !proposed_sidechains.contains(&sidechain_proposal.compute_id()) {
                coinbase_builder.propose_sidechain(sidechain_proposal)?;
            }
        }

        let mut sidechain_acks = self.get_sidechain_acks().await?;

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
                    )
                    .await?;
                    sidechain_acks.push(SidechainAck {
                        sidechain_number,
                        description_hash: sidechain_proposal.description.sha256d_hash(),
                    });
                }
            }
        }

        for sidechain_ack in sidechain_acks {
            if !self.validate_sidechain_ack(&sidechain_ack, &active_sidechain_proposals) {
                self.delete_sidechain_ack(&sidechain_ack).await?;
                tracing::info!(
                    "Unable to handle sidechain ack, deleted: {}",
                    sidechain_ack.sidechain_number
                );
                continue;
            }
            if coinbase_builder
                .messages()
                .m2_ack_slot_vout(&sidechain_ack.sidechain_number)
                .is_some()
            {
                continue;
            }

            tracing::debug!(
                "Adding ACK for sidechain {}",
                sidechain_ack.sidechain_number
            );

            coinbase_builder.ack_sidechain(
                sidechain_ack.sidechain_number,
                sidechain_ack.description_hash,
            )?;
        }

        let bmm_hashes = self.get_bmm_requests(&mainchain_tip).await?;
        for (sidechain_number, bmm_hash) in bmm_hashes {
            if coinbase_builder
                .messages()
                .m7_bmm_accept_slot_vout(&sidechain_number)
                .is_some()
            {
                continue;
            }
            tracing::info!(
                "Adding BMM accept for SC {} with hash: {}",
                sidechain_number,
                bmm_hash
            );
            coinbase_builder.bmm_accept(sidechain_number, bmm_hash)?;
        }
        for (sidechain_id, m6ids) in self.get_bundle_proposals().await? {
            for (m6id, _blinded_m6, m6id_info) in m6ids {
                if m6id_info.is_none() {
                    coinbase_builder.propose_bundle(sidechain_id, m6id)?;
                }
            }
        }
        // Ack bundles
        // TODO: Exclusively ack bundles that are known to the wallet
        // TODO: ack bundles when M2 messages are present
        if ack_all_proposals && coinbase_builder.messages().m2_ack_slots().is_empty() {
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
        let () = coinbase_builder.build()?;
        Ok(())
    }

    /// Generate suffix txs for a new block
    pub(in crate::wallet) async fn generate_suffix_txs(
        &self,
        ctips: &HashMap<SidechainNumber, crate::types::Ctip>,
    ) -> Result<Vec<Transaction>, error::GenerateSuffixTxs> {
        let mut res = Vec::new();
        for (sidechain_id, m6ids) in self.get_bundle_proposals().await? {
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
    async fn select_block_txs(&self) -> Result<Vec<Transaction>, error::SelectBlockTxs> {
        let ctips = self.inner.validator.get_ctips()?;
        let mut res = self.generate_suffix_txs(&ctips).await?;

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
    async fn finalize_coinbase(
        &self,
        best_block_height: u32,
        coinbase_outputs: &[TxOut],
    ) -> Result<Transaction, error::GetNewAddress> {
        let coinbase_addr = self.get_new_address().await?;
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
    async fn finalize_block(
        &self,
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
    ) -> Result<Block, error::FinalizeBlock> {
        let best_block_hash = self.validator().get_mainchain_tip()?;
        let best_block_height = self.validator().get_header_info(&best_block_hash)?.height;
        tracing::trace!(%best_block_hash, %best_block_height, "Found mainchain tip");

        let coinbase_tx = self
            .finalize_coinbase(best_block_height, coinbase_outputs)
            .await?;
        let txdata = std::iter::once(coinbase_tx).chain(transactions).collect();
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as u32;
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
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
    ) -> Result<BlockHash, error::Mine> {
        let transaction_count = transactions.len();

        let mut block = self.finalize_block(coinbase_outputs, transactions).await?;
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

        if blocks.get() > 1 {
            return Err(error::VerifyCanMine::MultipleBlocksOnSignet);
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
            return Err(error::VerifyCanMine::NoSignetChallengeFound);
        };

        let address =
            bitcoin::Address::from_script(&signet_challenge, bitcoin::params::Params::SIGNET)?;

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
            return Err(error::VerifyCanMine::SignetChallengeAddressMissing(address));
        }

        tracing::debug!("verified ability to solve signet challenge");

        let () = self.check_has_binary(&PathBuf::from("python3"))?;
        tracing::debug!("verified existence of `python3`");

        let () = self.check_has_binary(&self.inner.config.mining_opts.bitcoin_cli_path)?;
        tracing::debug!("verified existence of `bitcoin-cli`");

        let () = self.check_has_binary(&self.inner.config.mining_opts.bitcoin_util_path)?;
        tracing::debug!("verified existence of `bitcoin-util`");

        Ok(())
    }

    async fn get_signet_miner_path(&self) -> Result<PathBuf, error::GetSignetMinerPath> {
        if let Some(signet_mining_script_path) = self
            .inner
            .config
            .mining_opts
            .signet_mining_script_path
            .clone()
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
        let current_height = self
            .validator()
            .get_header_info(&self.validator().get_mainchain_tip()?)?
            .height;

        let is_about_to_difficulty_adjust = (current_height as u64 + 1)
            % self
                .validator()
                .network()
                .params()
                .difficulty_adjustment_interval()
            == 0;

        // Having some issues with our own block template generation for the 50th
        // difficulty adjustment (suspiciously round number...). Cannot get it to work!
        // Hack to get around: mine a completely normal Bitcoin Core block, if we're about
        // to adjust.
        // Crux of the issue is calculating the `nBits` value for the block header.
        let getblocktemplate_command = if is_about_to_difficulty_adjust {
            tracing::debug!("about to difficulty adjust, NOT using our own block template");
            None
        } else {
            Some(format!(
                "bitcoin-cli -rpcconnect={} -rpcport={} getblocktemplate",
                self.inner.config.serve_rpc_addr.ip(),
                self.inner.config.serve_rpc_addr.port()
            ))
        };

        let mining_script_path = self.get_signet_miner_path().await?;
        let miner = bins::SignetMiner {
            path: mining_script_path,
            bitcoin_cli: self.inner.config.bitcoin_cli(bitcoin::Network::Signet),
            bitcoin_util: self.inner.config.mining_opts.bitcoin_util_path.clone(),
            block_interval: Some(Duration::from_secs(60)),
            nbits: None,
            coinbase_recipient,
            getblocktemplate_command,
            coinbasetxn: true,
            debug: self.inner.config.mining_opts.signet_mining_script_debug,
        };

        let mut command = miner.command("generate", vec![]);
        tracing::debug!("Running signet miner: {:?}", command);

        let _stdout: String = command.run_utf8().await?;

        // The output of the signet miner is unfortunately not very useful,
        // so we have to fetch the most recent block in order to get the hash.
        let block_hash = self
            .inner
            .main_client
            .getbestblockhash()
            .await
            .map_err(|err| {
                let err = error::BitcoinCoreRPC {
                    method: "getbestblockhash".to_owned(),
                    error: err,
                };
                error::GenerateSignetBlock::FetchMostRecentBlockHash(err)
            })?;

        tracing::info!("Generated signet block: {}", block_hash);

        Ok(block_hash)
    }

    /// Build and mine a single block
    async fn generate_block(
        &self,
        ack_all_proposals: bool,
    ) -> Result<BlockHash, error::GenerateBlock> {
        let Some(mainchain_tip) = self.inner.validator.try_get_mainchain_tip()? else {
            return Err(error::GenerateBlock::ValidatorNotSynced);
        };
        if self.inner.validator.network() == Network::Signet {
            return self
                .generate_signet_block(self.inner.config.mining_opts.coinbase_recipient.clone())
                .await
                .map_err(error::GenerateBlock::GenerateSignetBlock);
        }
        let mut coinbase_outputs = Vec::new();
        let () = self
            .extend_coinbase_txouts(ack_all_proposals, mainchain_tip, &mut coinbase_outputs)
            .await?;
        let transactions = self.select_block_txs().await?;
        let mut coinbase_builder = CoinbaseBuilder::new(&mut coinbase_outputs)?;
        for tx in &transactions {
            if let Some(bmm_request) = crate::messages::parse_m8_tx(tx)
                && coinbase_builder
                    .messages()
                    .m7_bmm_accept_slot_vout(&bmm_request.sidechain_number)
                    .is_none()
            {
                coinbase_builder.bmm_accept(
                    bmm_request.sidechain_number,
                    bmm_request.sidechain_block_hash,
                )?;
            }
        }
        let () = coinbase_builder.build()?;

        tracing::info!(
            coinbase_outputs = %coinbase_outputs.len(),
            transactions = %transactions.len(),
            "Mining block",
        );

        let block_hash = self.mine(&coinbase_outputs, transactions).await?;
        self.delete_bmm_requests(&mainchain_tip)
            .await
            .map_err(error::GenerateBlock::DeleteBmmRequests)?;
        Ok(block_hash)
    }

    pub fn generate_blocks<Ref>(
        this: Ref,
        count: NonZeroU32,
        ack_all_proposals: bool,
    ) -> impl FusedStream<Item = Result<BlockHash, error::GenerateBlock>>
    where
        Ref: std::borrow::Borrow<Self>,
    {
        tracing::info!("Generate: creating {} block(s)", count);
        stream::try_unfold((this, count.get()), move |(this, remaining)| async move {
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
