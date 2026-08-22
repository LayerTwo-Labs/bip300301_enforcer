//! Hand-crafting and submitting mainchain blocks that carry BMM messages.
//!
//! `generateblock` can't mine a BMM request -- an M8 with no matching M7 in
//! the coinbase is rejected ("Cannot include BMM request; not accepted by
//! miners") -- and the enforcer's own block production settles the bids it
//! includes, which defeats simulating a block someone else found.
//!
//! So this module assembles such blocks itself, keeping the consensus-critical
//! bits (witness commitment, merkle roots, MTP) in one place.

use std::time::Duration;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    messages::M7BmmAccept,
    types::{BmmCommitment, SidechainNumber},
};
use bitcoin::{
    Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction,
    TxMerkleNode, TxOut, Witness,
    block::Header,
    consensus::encode::{deserialize_hex, serialize_hex},
    hashes::Hash as _,
    script::PushBytesBuf,
    transaction::Version,
};
use serde::Deserialize;

use crate::setup::PostSetup;

#[derive(Deserialize)]
struct TemplateTx {
    fee: u64,
}

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
    transactions: Vec<TemplateTx>,
}

impl BlockTemplate {
    /// The block subsidy alone, with the template's fee income backed out.
    ///
    /// A hand-assembled block holds a different tx set than the template, so
    /// claiming `coinbasevalue` overpays and is rejected as `bad-cb-amount`.
    /// Forgoing the fees is always valid, and survives regtest's halvings.
    fn subsidy_sats(&self) -> u64 {
        let template_fees: u64 = self.transactions.iter().map(|tx| tx.fee).sum();
        self.coinbasevalue.saturating_sub(template_fees)
    }
}

fn deserialize_bits<'de, D: serde::Deserializer<'de>>(de: D) -> Result<CompactTarget, D::Error> {
    let hex = String::deserialize(de)?;
    u32::from_str_radix(&hex, 16)
        .map(CompactTarget::from_consensus)
        .map_err(serde::de::Error::custom)
}

/// Block timestamps must exceed the median time past of the previous 11
/// blocks. On regtest, where blocks are mined back-to-back, MTP can sit ahead
/// of the wall clock; sleep until it doesn't.
pub async fn wait_past_mtp(post_setup: &PostSetup) -> anyhow::Result<()> {
    let tip_hash = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?;
    let tip_json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [tip_hash])
        .run_utf8()
        .await?;
    let mediantime = serde_json::from_str::<serde_json::Value>(&tip_json)?["mediantime"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("getblock response missing mediantime"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    if mediantime >= now {
        let wait = Duration::from_secs(mediantime - now + 1);
        tracing::info!(?wait, mediantime, now, "waiting for wall clock to pass MTP");
        tokio::time::sleep(wait).await;
    }
    Ok(())
}

/// Hand-craft and submit a block whose coinbase carries an M7 accept for each
/// `(sidechain, h*)` in `accepts`, and whose txdata is exactly `tx_hexes` (in
/// order, after the coinbase), so one block can settle several sidechains'
/// bids while leaving another's in the mempool.
///
/// Bypasses fee-based selection, so callers can place transactions ordinary
/// mining would never pick.
pub async fn submit_block_with_bmm_accepts(
    post_setup: &PostSetup,
    accepts: &[(SidechainNumber, [u8; 32])],
    tx_hexes: &[&str],
) -> anyhow::Result<BlockHash> {
    wait_past_mtp(post_setup).await?;

    let template_json = post_setup
        .bitcoin_cli
        // We craft the BIP300/BIP301 coinbase commitments below, so we ack the
        // rule the way the enforcer does. Stock nodes ignore the extra rule.
        .command::<String, _, _, _, _>(
            [],
            "getblocktemplate",
            [r#"{"rules":["segwit","bip300301"]}"#],
        )
        .run_utf8()
        .await?;
    let template: BlockTemplate = serde_json::from_str(&template_json)?;

    const WITNESS_RESERVED_VALUE: [u8; 32] = [0; 32];
    // The accepted commitments go into the coinbase scriptSig as well as the
    // M7 outputs, purely to keep crafted blocks distinct. Two calls at the
    // same height otherwise produce byte-identical blocks -- same template,
    // same coinbase, same `time`, and grinding starts from nonce 0 -- and
    // bitcoind refuses the second as `duplicate-invalid` if the first was
    // rejected and invalidated.
    let coinbase_input = bitcoin::TxIn {
        previous_output: OutPoint::null(),
        script_sig: accepts
            .iter()
            .fold(
                bitcoin::script::Builder::new().push_int(template.height as i64),
                |builder, (sidechain_number, h_star)| {
                    builder
                        .push_int(i64::from(u8::from(*sidechain_number)))
                        .push_slice(h_star)
                },
            )
            .into_script(),
        sequence: Sequence::MAX,
        witness: Witness::from_slice(&[WITNESS_RESERVED_VALUE]),
    };

    let txs: Vec<Transaction> = tx_hexes
        .iter()
        .map(|hex| deserialize_hex(hex).map_err(anyhow::Error::from))
        .collect::<anyhow::Result<_>>()?;

    // Real (not all-zero) witness merkle root: the coinbase's own wtxid is
    // taken as all-zero per BIP141, but the other transactions carry real
    // witness data.
    let witness_root = bitcoin::merkle_tree::calculate_root(
        std::iter::once(bitcoin::Wtxid::all_zeros().to_raw_hash())
            .chain(txs.iter().map(|tx| tx.compute_wtxid().to_raw_hash())),
    )
    .map(bitcoin::WitnessMerkleNode::from)
    .ok_or_else(|| anyhow::anyhow!("failed to compute witness merkle root"))?;
    let witness_commitment =
        Block::compute_witness_commitment(&witness_root, &WITNESS_RESERVED_VALUE);
    let witness_commit_script = {
        const WITNESS_COMMITMENT_HEADER: [u8; 4] = [0xaa, 0x21, 0xa9, 0xed];
        let mut payload = PushBytesBuf::from(WITNESS_COMMITMENT_HEADER);
        payload.extend_from_slice(witness_commitment.as_byte_array())?;
        ScriptBuf::new_op_return(payload)
    };

    let mut coinbase_outputs = vec![
        TxOut {
            script_pubkey: post_setup.mining_address.script_pubkey(),
            value: Amount::from_sat(template.subsidy_sats()),
        },
        TxOut {
            script_pubkey: witness_commit_script,
            value: Amount::ZERO,
        },
    ];
    for (sidechain_number, h_star) in accepts {
        let m7_script: ScriptBuf = M7BmmAccept {
            sidechain_number: *sidechain_number,
            sidechain_block_hash: BmmCommitment(*h_star),
        }
        .try_into()?;
        coinbase_outputs.push(TxOut {
            script_pubkey: m7_script,
            value: Amount::ZERO,
        });
    }

    let coinbase = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![coinbase_input],
        output: coinbase_outputs,
    };

    let merkle_root = bitcoin::merkle_tree::calculate_root(
        std::iter::once(coinbase.compute_txid())
            .chain(txs.iter().map(|tx| tx.compute_txid()))
            .map(|txid| txid.to_raw_hash()),
    )
    .map(TxMerkleNode::from)
    .ok_or_else(|| anyhow::anyhow!("failed to compute merkle root"))?;

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
        txdata: std::iter::once(coinbase).chain(txs).collect(),
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
