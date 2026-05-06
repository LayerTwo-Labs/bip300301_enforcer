//! Verifies that the enforcer rejects a block whose coinbase carries a
//! BIP-300 rule violation. Specifically: two M4 (ack-bundles) messages,
//! which `CoinbaseMessages::push` rejects with `DuplicateM4`.

use std::time::Duration;

use bip300301_enforcer_lib::{bins::CommandExt as _, messages::M4AckBundles};
use bitcoin::{
    Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Witness,
    block::Header,
    consensus::encode::{deserialize_hex, serialize_hex},
    hashes::Hash as _,
    script::{Builder as ScriptBuilder, PushBytesBuf},
    transaction::Version,
};
use futures::channel::mpsc;
use serde::Deserialize;
use tokio::time::sleep;

use crate::{
    integration_test::{activate_sidechain, fund_enforcer, propose_sidechain},
    setup::{DummySidechain, PostSetup, Sidechain},
};

pub async fn test_invalid_block(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (sidechain_res_tx, _sidechain_res_rx) = mpsc::unbounded();
    let mut _sidechain = DummySidechain::setup((), &post_setup, sidechain_res_tx).await?;

    propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    // `fund_enforcer` mines 100 blocks in a burst; their timestamps end up
    // ahead of wall clock, so a block mined now would be rejected as
    // `time-too-old`. Wait until wall clock surpasses median-time-past.
    wait_past_mtp(&post_setup).await?;

    let pre_mine_tip: BlockHash = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?
        .parse()?;
    tracing::info!(%pre_mine_tip, "pre-mine tip");

    let bad_block_hash = mine_block_with_duplicate_m4(&post_setup).await?;
    tracing::info!(%bad_block_hash, "submitted block with duplicate M4");

    poll_until_tip(&post_setup, pre_mine_tip, Duration::from_secs(10)).await?;

    let chaintips_json = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getchaintips", [])
        .run_utf8()
        .await?;
    let chaintips: Vec<serde_json::Value> = serde_json::from_str(&chaintips_json)?;
    let bad_tip_status = chaintips
        .iter()
        .find(|tip| {
            tip.get("hash")
                .and_then(|h| h.as_str())
                .and_then(|h| h.parse::<BlockHash>().ok())
                == Some(bad_block_hash)
        })
        .and_then(|tip| tip.get("status"))
        .and_then(|s| s.as_str());
    anyhow::ensure!(
        bad_tip_status == Some("invalid"),
        "expected bad block status `invalid`, got {bad_tip_status:?}"
    );

    // `tracing` writes to stdout in the integration-test enforcer config.
    let stdout_path = post_setup.directories.enforcer_dir.join("stdout.txt");
    let stdout_contents = std::fs::read_to_string(&stdout_path)?;
    anyhow::ensure!(
        stdout_contents.contains("rejecting block: M4 already included at index"),
        "enforcer stdout at `{}` did not contain the duplicate-M4 rejection log",
        stdout_path.display()
    );

    Ok(())
}

/// Block-template fields we read from bitcoind's `getblocktemplate`. The test
/// talks to bitcoind directly (not via the enforcer's GBT proxy) because the
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

/// Build a block with a hand-rolled coinbase containing two M4 OP_RETURN
/// outputs, grind its PoW, and submit it. Returns the block hash bitcoind
/// accepted at consensus (the enforcer should subsequently invalidate).
async fn mine_block_with_duplicate_m4(post_setup: &PostSetup) -> anyhow::Result<BlockHash> {
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

    // Two M4 OP_RETURN outputs with different upvote payloads — the second
    // hits `CoinbaseMessages::push`'s `DuplicateM4` check (lib/messages.rs:506)
    // regardless of contents.
    let m4_a: ScriptBuf = M4AckBundles::OneByte {
        upvotes: vec![0x00],
    }
    .try_into()?;
    let m4_b: ScriptBuf = M4AckBundles::OneByte {
        upvotes: vec![0x01],
    }
    .try_into()?;

    let coinbase = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![coinbase_input],
        output: vec![
            TxOut {
                script_pubkey: post_setup.mining_address.script_pubkey(),
                value: Amount::from_sat(template.coinbasevalue),
            },
            TxOut {
                script_pubkey: witness_commit_script,
                value: Amount::ZERO,
            },
            TxOut {
                script_pubkey: m4_a,
                value: Amount::ZERO,
            },
            TxOut {
                script_pubkey: m4_b,
                value: Amount::ZERO,
            },
        ],
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
        .bitcoin_util
        .command::<String, _, _, _, _>([], "grind", [serialize_hex(&header)])
        .run_utf8()
        .await?;
    let header: Header = deserialize_hex(header_hex.trim())?;

    let block = Block {
        header,
        txdata: vec![coinbase],
    };
    let block_hash = block.block_hash();
    let block_hex = serialize_hex(&block);
    let submit_resp = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "submitblock", [block_hex])
        .run_utf8()
        .await?;
    anyhow::ensure!(
        submit_resp.is_empty(),
        "submitblock unexpectedly rejected: `{submit_resp}`"
    );

    Ok(block_hash)
}

async fn wait_past_mtp(post_setup: &PostSetup) -> anyhow::Result<()> {
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
        sleep(wait).await;
    }
    Ok(())
}

async fn poll_until_tip(
    post_setup: &PostSetup,
    expected: BlockHash,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let tip: BlockHash = post_setup
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "getbestblockhash", [])
            .run_utf8()
            .await?
            .parse()?;
        if tip == expected {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for tip to revert to `{expected}`; current tip is `{tip}`"
            );
        }
        sleep(Duration::from_millis(200)).await;
    }
}
