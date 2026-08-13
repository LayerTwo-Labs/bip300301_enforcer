//! Mining a block whose coinbase carries hand-built BIP300 messages.
//!
//! The block producer only ever emits coinbases it considers valid, so any test
//! that needs a specific — possibly invalid — arrangement of coinbase messages
//! has to assemble the block itself and hand it to `submitblock`.

use bip300301_enforcer_lib::bins::CommandExt as _;
use bitcoin::{
    Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Witness,
    block::Header,
    consensus::encode::{deserialize_hex, serialize_hex},
    hashes::Hash as _,
    script::{Builder as ScriptBuilder, PushBytesBuf},
    transaction::Version,
};
use serde::Deserialize;

use crate::setup::PostSetup;

/// A zero-value coinbase output carrying `script_pubkey`, the shape every
/// BIP300 coinbase message takes.
pub fn zero_value(script_pubkey: ScriptBuf) -> TxOut {
    TxOut {
        script_pubkey,
        value: Amount::ZERO,
    }
}

/// Block-template fields we read from bitcoind's `getblocktemplate`. Callers
/// talk to bitcoind directly (not via the enforcer's GBT proxy) because the
/// proxy is disabled in `Mode::NoMempool`.
#[derive(Deserialize)]
struct BlockTemplate {
    previousblockhash: BlockHash,
    version: i32,
    #[serde(deserialize_with = "deserialize_bits")]
    bits: CompactTarget,
    height: u32,
    coinbasevalue: u64,
    mintime: u32,
    curtime: u32,
    #[serde(default)]
    transactions: Vec<serde_json::Value>,
}

fn deserialize_bits<'de, D: serde::Deserializer<'de>>(de: D) -> Result<CompactTarget, D::Error> {
    let hex = String::deserialize(de)?;
    u32::from_str_radix(&hex, 16)
        .map(CompactTarget::from_consensus)
        .map_err(serde::de::Error::custom)
}

/// Mine a block on top of the current tip whose coinbase is the mining payout,
/// the witness commitment, and then `extra_coinbase_outputs` in the order
/// given. The coinbase carries no producer-generated messages, so the caller's
/// outputs are the only BIP300 messages in the block, at known indices.
///
/// Returns the submitted block's hash; use
/// [`crate::block_verdict::assert_enforcer_verdict`] to assert on what the
/// enforcer made of it. `submitblock` accepting the block says nothing about
/// the enforcer's verdict — bitcoind does not know the BIP300 rules.
pub async fn submit_block_with_coinbase_outputs(
    post_setup: &PostSetup,
    extra_coinbase_outputs: Vec<TxOut>,
) -> anyhow::Result<BlockHash> {
    let template_json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblocktemplate", [r#"{"rules":["segwit"]}"#])
        .run_utf8()
        .await?;
    let template: BlockTemplate = serde_json::from_str(&template_json)?;
    anyhow::ensure!(
        template.transactions.is_empty(),
        "test assumes empty mempool for witness-commitment shortcut; \
         got {} extra tx(s) in template",
        template.transactions.len()
    );

    const WITNESS_RESERVED_VALUE: [u8; 32] = [0; 32];
    let coinbase_input = TxIn {
        previous_output: OutPoint::null(),
        script_sig: ScriptBuilder::new()
            .push_int(template.height as i64)
            .into_script(),
        sequence: Sequence::MAX,
        witness: Witness::from_slice(&[WITNESS_RESERVED_VALUE]),
    };

    // All-zero witness merkle root because the coinbase wtxid is 0x00..00 (BIP141).
    let witness_commitment = Block::compute_witness_commitment(
        &bitcoin::WitnessMerkleNode::all_zeros(),
        &WITNESS_RESERVED_VALUE,
    );
    let witness_commit_script = {
        const WITNESS_COMMITMENT_HEADER: [u8; 4] = [0xaa, 0x21, 0xa9, 0xed];
        let mut payload = PushBytesBuf::from(WITNESS_COMMITMENT_HEADER);
        payload.extend_from_slice(witness_commitment.as_byte_array())?;
        ScriptBuf::new_op_return(payload)
    };

    let mut outputs = vec![
        TxOut {
            script_pubkey: post_setup.mining_address.script_pubkey(),
            value: Amount::from_sat(template.coinbasevalue),
        },
        TxOut {
            script_pubkey: witness_commit_script,
            value: Amount::ZERO,
        },
    ];
    outputs.extend(extra_coinbase_outputs);
    let coinbase = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![coinbase_input],
        output: outputs,
    };

    let merkle_root = TxMerkleNode::from(coinbase.compute_txid().to_raw_hash());
    let header = Header {
        version: bitcoin::block::Version::from_consensus(template.version),
        prev_blockhash: template.previousblockhash,
        merkle_root,
        time: std::cmp::max(template.curtime, template.mintime),
        bits: template.bits,
        nonce: 0,
    };
    let header_hex = post_setup
        .bitcoin_util()?
        .command::<String, _, _, _, _>([], "grind", [serialize_hex(&header)])
        .run_utf8()
        .await?;
    let header: Header = deserialize_hex(header_hex.trim())?;

    let block = Block {
        header,
        txdata: vec![coinbase],
    };
    let block_hash = block.block_hash();
    let submit_resp = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "submitblock", [serialize_hex(&block)])
        .run_utf8()
        .await?;
    anyhow::ensure!(
        submit_resp.is_empty(),
        "submitblock unexpectedly rejected: `{submit_resp}`"
    );

    Ok(block_hash)
}
