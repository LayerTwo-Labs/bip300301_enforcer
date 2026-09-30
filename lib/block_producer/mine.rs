//! Mining blocks without a wallet, backing
//! `BlockProducerService.GenerateToAddress`. The coinbase pays out to a
//! caller-provided address. Blocks are constructed and their PoW ground
//! locally; on signet, the Bitcoin Core node's wallet also signs the block.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bitcoin::{
    Amount, Block, BlockHash, CompactTarget, Network, Script, ScriptBuf, Sequence, Transaction,
    TxIn, TxOut, Txid, Witness,
    absolute::{Height, LockTime},
    block::Version as BlockVersion,
    consensus::{
        Encodable as _,
        encode::{deserialize_hex, serialize_hex},
    },
    constants::SUBSIDY_HALVING_INTERVAL,
    hash_types::TxMerkleNode,
    hashes::Hash as _,
    opcodes::{OP_0, all::OP_RETURN},
    transaction::Version as TxVersion,
};
use bitcoin_jsonrpsee::{
    MainClient as _,
    client::{BlockTemplate, BlockTemplateRequest, CoinbaseTxnOrValue},
    jsonrpsee::{core::client::ClientT as _, http_client::HttpClient, rpc_params},
};

use crate::{
    block_producer::{BlockProducer, error},
    messages::CoinbaseBuilder,
    mining::{self, SignetTxs},
    types::{
        AckAllProposalsPolicy, BmmCommitment, HeaderInfo, SidechainNumber, WithdrawalBundlePolicy,
    },
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

/// A signet block's time. Behind schedule, catch up halfway to now, like
/// Bitcoin Core's `contrib/signet/miner` with `--set-block-time`. Otherwise
/// now: stamping `interval` after the tip instead would run the chain ahead
/// of the clock with every block mined faster than that.
fn signet_block_time(tip_time: u64, now: u64, interval: Duration) -> u64 {
    let interval = interval.as_secs();
    let tip_age = now.saturating_sub(tip_time);
    if tip_age > interval {
        tip_time + tip_age.midpoint(interval)
    } else {
        now.max(tip_time + 1)
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

/// What a block's header takes from its template.
struct TemplateHeader {
    bits: CompactTarget,
    mintime: u64,
}

impl BlockProducer {
    async fn fetch_block_template(
        &self,
        gbt_client: &HttpClient,
        rules: Vec<String>,
    ) -> Result<BlockTemplate, error::GetBlockTemplate> {
        gbt_client
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
    ) -> Result<(Vec<(Transaction, Amount)>, TemplateHeader), error::SelectBlockTxs> {
        let mut rules = vec![
            "segwit".to_string(),
            crate::rpc_client::BIP300301_RULE.to_string(),
        ];
        if self.validator().network() == Network::Signet {
            rules.push("signet".to_string());
        }
        let gbt_client = self
            .gbt_client()
            .ok_or(error::SelectBlockTxs::NoBlockTemplateServer)?;
        let template = self.fetch_block_template(gbt_client, rules).await?;
        let template_header = TemplateHeader {
            bits: template.compact_target,
            mintime: template.mintime,
        };

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

        // The enforcer's own template server builds the template through the
        // block producer hooks: the tx set respects the enforcer's mempool
        // rules and already ends with the withdrawal-payout suffix txs. It
        // only builds `coinbasetxn` templates, so a `coinbasevalue` one was
        // not built that way, and mining it would leave the payouts out.
        if let CoinbaseTxnOrValue::ValueSats(_) = template.coinbase_txn_or_value {
            return Err(error::SelectBlockTxs::NoCoinbaseTxn);
        }
        let mut res = Vec::with_capacity(template.transactions.len());
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

        Ok((res, template_header))
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
        let value = get_block_value(best_block_height + 1, fees, self.validator().network());
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
                witness: Witness::new(),
                script_sig,
            }],
            output: [&output, coinbase_outputs].concat(),
        }
    }

    /// Now, or on signet catching up with a stale tip. Either way after the
    /// tip, so blocks mined faster than once per second are not rejected as
    /// `time-too-old`.
    fn block_time(
        &self,
        tip_header: &HeaderInfo,
        template: &TemplateHeader,
    ) -> Result<u32, std::time::SystemTimeError> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let tip_time = u64::from(tip_header.timestamp);
        let time = match self.signet_challenge() {
            Some(challenge) => signet_block_time(tip_time, now, target_block_interval(challenge)),
            None => now.max(tip_time + 1),
        };
        Ok(time.max(template.mintime) as u32)
    }

    /// Finalize a new block by constructing the coinbase tx
    fn finalize_block(
        &self,
        coinbase_spk: ScriptBuf,
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
        fees: Amount,
        template: &TemplateHeader,
    ) -> Result<Block, error::FinalizeBlock> {
        let best_block_hash = self.validator().get_mainchain_tip()?;
        let tip_header = self.validator().get_header_info(&best_block_hash)?;
        let best_block_height = tip_header.height;
        tracing::trace!(%best_block_hash, %best_block_height, "Found mainchain tip");

        let coinbase_tx =
            self.finalize_coinbase(best_block_height, coinbase_spk, coinbase_outputs, fees);
        let txdata = std::iter::once(coinbase_tx).chain(transactions).collect();
        let header = bitcoin::block::Header {
            version: BlockVersion::NO_SOFT_FORK_SIGNALLING,
            prev_blockhash: best_block_hash,
            // computed after the witness commitment is added to the coinbase
            merkle_root: TxMerkleNode::all_zeros(),
            time: self.block_time(&tip_header, template)?,
            bits: template.bits,
            nonce: 0,
        };
        let mut block = Block { header, txdata };
        mining::add_witness_commitment(&mut block);
        Ok(block)
    }

    /// Solve the signet challenge with the Bitcoin Core node's wallet.
    async fn sign_signet_block(
        &self,
        block: &mut Block,
        challenge: &Script,
    ) -> Result<(), error::SignSignetBlock> {
        #[derive(serde::Deserialize)]
        struct Signed {
            hex: String,
            complete: bool,
        }
        let SignetTxs { to_spend, to_sign } = SignetTxs::new(block, challenge)?;
        let prevtxs = serde_json::json!([{
            "txid": to_spend.compute_txid(),
            "vout": 0,
            "scriptPubKey": challenge.to_hex_string(),
            "amount": 0,
        }]);
        let signed: Signed = self
            .main_client()
            .request(
                "signrawtransactionwithwallet",
                rpc_params![serialize_hex(&to_sign), prevtxs],
            )
            .await
            .map_err(|err| error::BitcoinCoreRPC {
                method: "signrawtransactionwithwallet".to_string(),
                error: err,
            })?;
        if !signed.complete {
            return Err(error::SignSignetBlock::Unsolved);
        }
        let signed: Transaction = deserialize_hex(&signed.hex)?;
        let () = mining::add_signet_solution(
            block,
            &signed.input[0].script_sig,
            &signed.input[0].witness,
        )?;
        Ok(())
    }

    /// Mine a block
    async fn mine(
        &self,
        coinbase_spk: ScriptBuf,
        coinbase_outputs: &[TxOut],
        transactions: Vec<Transaction>,
        fees: Amount,
        template: &TemplateHeader,
    ) -> Result<BlockHash, error::Mine> {
        let transaction_count = transactions.len();

        let mut block =
            self.finalize_block(coinbase_spk, coinbase_outputs, transactions, fees, template)?;
        if let Some(challenge) = self.signet_challenge() {
            let () = self.sign_signet_block(&mut block, challenge).await?;
        }
        let header = block.header;
        block.header = tokio::task::spawn_blocking(move || mining::grind(header))
            .await?
            .ok_or(error::Mine::NonceSpaceExhausted)?;
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

    pub async fn verify_can_mine(&self) -> Result<(), error::VerifyCanMine> {
        let challenge = match self.validator().network() {
            // Mining on regtest always works.
            bitcoin::Network::Regtest => return Ok(()),
            // On signet, the node's wallet has to solve the signet challenge.
            // Challenges can be complex, but the typical one is just a script
            // pubkey belonging to the signet creator's wallet: check that the
            // node's wallet owns the corresponding address.
            bitcoin::Network::Signet => self
                .signet_challenge()
                .ok_or(error::VerifyCanMine::NoSignetChallengeFound)?,
            network => {
                return Err(error::VerifyCanMine::Network(network));
            }
        };

        let address = bitcoin::Address::from_script(challenge, bitcoin::params::Params::SIGNET)?;

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
        Ok(())
    }

    /// Build and mine a single block, paying the block reward to
    /// `coinbase_addr`. The caller is responsible for verifying that mining is
    /// possible (see [`Self::verify_can_mine`]).
    pub async fn generate_block(
        &self,
        coinbase_addr: bitcoin::Address,
        ack_policy: AckAllProposalsPolicy,
        bundle_policy: WithdrawalBundlePolicy,
    ) -> Result<BlockHash, error::GenerateBlock> {
        let coinbase_spk = coinbase_addr.script_pubkey();
        let Some(mainchain_tip) = self.validator().try_get_mainchain_tip()? else {
            return Err(error::GenerateBlock::ValidatorNotSynced);
        };
        let mut coinbase_outputs = Vec::new();
        let () = self
            .extend_coinbase_txouts(
                ack_policy,
                bundle_policy,
                mainchain_tip,
                &mut coinbase_outputs,
            )
            .await?;
        let (selected, template) = self.select_block_txs(mainchain_tip).await?;
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
            .mine(
                coinbase_spk,
                &coinbase_outputs,
                transactions,
                fees,
                &template,
            )
            .await?;
        Ok(block_hash)
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{Amount, Txid, hashes::Hash as _};

    use super::bmm_auction_winners;
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
}
