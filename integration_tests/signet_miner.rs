//! In-process signet miner for the harness.
//!
//! Fetches a template, finishes the block with the enforcer's own
//! [`bip300301_enforcer_lib::mining`] helpers, and submits it. Signs with the
//! challenge key directly rather than through the node wallet, so blocks can be
//! mined before the node has one, as when building the cached chain.

use bip300301_enforcer_lib::{
    mining::{self, SignetTxs},
    rpc_client::BIP300301_RULE,
};
use bitcoin::{
    Amount, Block, BlockHash, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
    absolute::LockTime,
    block::Header,
    consensus::encode::{deserialize_hex, serialize_hex},
    hashes::Hash as _,
    opcodes::all::OP_PUSHNUM_1,
    script::Builder,
    secp256k1::{self, Secp256k1},
    sighash::{EcdsaSighashType, SighashCache},
    transaction,
};
use bitcoin_jsonrpsee::client::CoinbaseTxnOrValue;
use cusf_enforcer_mempool::server::RpcClient as _;
use jsonrpsee::{core::client::ClientT as _, rpc_params};
use thiserror::Error;

use crate::util::BitcoindClient;

#[derive(Debug, Error)]
pub enum MineSignetBlockError {
    #[error(transparent)]
    ConsensusDecode(#[from] bitcoin::consensus::encode::Error),
    #[error(transparent)]
    ConsensusDecodeHex(#[from] bitcoin::consensus::encode::FromHexError),
    #[error("exhausted the nonce space without meeting the target")]
    GrindExhausted,
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
    /// From the enforcer's block template server or bitcoind.
    #[error(transparent)]
    JsonRpc(#[from] jsonrpsee::core::ClientError),
    #[error("Missing coinbasetxn in the enforcer's block template")]
    MissingCoinbaseTxn,
    #[error("Missing coinbasevalue in the node's block template")]
    MissingCoinbaseValue,
    #[error(transparent)]
    NoWitnessCommitment(#[from] mining::NoWitnessCommitment),
    #[error("`getblocktemplate` answered a BIP23 proposal verdict, not a template")]
    UnexpectedProposalVerdict,
    #[error(transparent)]
    Sighash(#[from] bitcoin::sighash::P2wpkhError),
    #[error("Submitting block failed with error: `{err_msg}`")]
    SubmitBlock { err_msg: String },
}

/// Where a block's template, and so its coinbase, comes from.
pub enum TemplateSource<'a> {
    /// The enforcer's template server, whose coinbase carries the BIP300
    /// messages.
    Enforcer(&'a jsonrpsee::http_client::HttpClient),
    /// The node itself, for plain blocks paying the coinbase to `payout`.
    Node { payout: ScriptBuf },
}

#[derive(Clone)]
pub struct SignetMiner {
    secret_key: secp256k1::SecretKey,
    /// P2WPKH to `secret_key`.
    challenge: ScriptBuf,
}

impl SignetMiner {
    pub fn new(secret_key: secp256k1::SecretKey, challenge: ScriptBuf) -> Self {
        Self {
            secret_key,
            challenge,
        }
    }

    /// Mine one block on top of `bitcoind`'s tip and submit it there.
    pub async fn mine_block(
        &self,
        bitcoind: &BitcoindClient,
        source: TemplateSource<'_>,
    ) -> Result<BlockHash, MineSignetBlockError> {
        let mut request = bitcoin_jsonrpsee::client::BlockTemplateRequest {
            rules: vec!["signet".to_owned(), "segwit".to_owned()],
            ..Default::default()
        };
        let (template, coinbase) = match source {
            TemplateSource::Enforcer(gbt_client) => {
                request.capabilities.insert("coinbasetxn".to_owned());
                let template = get_block_template(gbt_client, request).await?;
                let CoinbaseTxnOrValue::Txn(coinbase) = &template.coinbase_txn_or_value else {
                    return Err(MineSignetBlockError::MissingCoinbaseTxn);
                };
                let coinbase = bitcoin::consensus::deserialize(&coinbase.data)?;
                (template, coinbase)
            }
            TemplateSource::Node { payout } => {
                // A node that enforces the drivechain rules serves no template
                // unless the client acknowledges them; others ignore the name.
                request.rules.push(BIP300301_RULE.to_owned());
                let template = get_block_template(bitcoind, request).await?;
                let CoinbaseTxnOrValue::ValueSats(value) = template.coinbase_txn_or_value else {
                    return Err(MineSignetBlockError::MissingCoinbaseValue);
                };
                let coinbase = coinbase_paying(template.height, Amount::from_sat(value), payout);
                (template, coinbase)
            }
        };
        let time = block_time(bitcoind, &template).await?;
        let txdata = std::iter::once(Ok(coinbase))
            .chain(
                template
                    .transactions
                    .iter()
                    .map(|tx| bitcoin::consensus::deserialize(&tx.data)),
            )
            .collect::<Result<Vec<Transaction>, _>>()?;
        let header = Header {
            version: template.version,
            prev_blockhash: template.prev_blockhash,
            merkle_root: bitcoin::TxMerkleNode::all_zeros(),
            time,
            bits: template.compact_target,
            nonce: 0,
        };
        let mut block = Block { header, txdata };
        mining::add_witness_commitment(&mut block);
        let () = self.sign(&mut block)?;
        let header = block.header;
        block.header = tokio::task::spawn_blocking(move || mining::grind(header))
            .await?
            .ok_or(MineSignetBlockError::GrindExhausted)?;
        let block_hash = block.block_hash();
        // `null` on success, the rejection reason otherwise.
        let submitblock_output: Option<String> = bitcoind
            .request("submitblock", rpc_params![serialize_hex(&block)])
            .await?;
        if let Some(err_msg) = submitblock_output {
            return Err(MineSignetBlockError::SubmitBlock { err_msg });
        }
        Ok(block_hash)
    }

    /// Solve the challenge: a P2WPKH spend by `secret_key`.
    fn sign(&self, block: &mut Block) -> Result<(), MineSignetBlockError> {
        let SignetTxs { to_sign, .. } = SignetTxs::new(block, &self.challenge)?;
        let sighash = SighashCache::new(&to_sign).p2wpkh_signature_hash(
            0,
            &self.challenge,
            Amount::ZERO,
            EcdsaSighashType::All,
        )?;
        let secp = Secp256k1::signing_only();
        let signature = bitcoin::ecdsa::Signature::sighash_all(secp.sign_ecdsa(
            &secp256k1::Message::from_digest(sighash.to_byte_array()),
            &self.secret_key,
        ));
        let witness = Witness::p2wpkh(&signature, &self.secret_key.public_key(&secp));
        let () = mining::add_signet_solution(block, &ScriptBuf::new(), &witness)?;
        Ok(())
    }
}

async fn get_block_template(
    client: &jsonrpsee::http_client::HttpClient,
    request: bitcoin_jsonrpsee::client::BlockTemplateRequest,
) -> Result<bitcoin_jsonrpsee::client::BlockTemplate, MineSignetBlockError> {
    client
        .get_block_template(request)
        .await?
        .into_template()
        .map(|template| *template)
        .ok_or(MineSignetBlockError::UnexpectedProposalVerdict)
}

/// A coinbase paying all of `value` to `payout`, with the BIP34 height.
fn coinbase_paying(height: u32, value: Amount, payout: ScriptBuf) -> Transaction {
    let mut script_sig = Builder::new().push_int(height.into());
    // A coinbase scriptSig must be at least 2 bytes, and heights up to 16
    // push as a single opcode.
    if height <= 16 {
        script_sig = script_sig.push_opcode(OP_PUSHNUM_1);
    }
    Transaction {
        version: transaction::Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: script_sig.into_script(),
            sequence: Sequence::ENABLE_LOCKTIME_NO_RBF,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value,
            script_pubkey: payout,
        }],
    }
}

/// One second after the tip, like the Python miner with `--block-interval 1`,
/// so a run of blocks doesn't depend on the wall clock. The first block after
/// genesis starts from the present instead, anchoring a fresh chain to now.
async fn block_time(
    bitcoind: &BitcoindClient,
    template: &bitcoin_jsonrpsee::client::BlockTemplate,
) -> Result<u32, MineSignetBlockError> {
    let earliest = if template.height == 1 {
        template.current_time
    } else {
        let tip_hex: String = bitcoind
            .request(
                "getblockheader",
                rpc_params![template.prev_blockhash, false],
            )
            .await?;
        let tip: Header = deserialize_hex(&tip_hex)?;
        u64::from(tip.time) + 1
    };
    Ok(std::cmp::max(earliest, template.mintime) as u32)
}
