use std::{path::Path, time::Duration};

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    messages::{M1ProposeSidechain, M2AckSidechain, M4AckBundles, M7BmmAccept},
    types::{BmmCommitment, SidechainDescription, WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD},
};
use bitcoin::{
    Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Witness,
    block::Header,
    consensus::encode::{deserialize_hex, serialize_hex},
    hashes::{Hash as _, sha256d},
    script::{Builder as ScriptBuilder, PushBytesBuf},
    transaction::Version,
};
use serde::Deserialize;
use tokio::time::sleep;

use crate::{
    integration_test::{deposit, setup_active_sidechain},
    mine::mine_check_block_events,
    setup::{DummySidechain, PostSetup},
};

struct BadBlockCase {
    name: &'static str,
    extra_coinbase_outputs: fn() -> anyhow::Result<Vec<TxOut>>,
    expected_log_contains: &'static str,
}

const CASES: &[BadBlockCase] = &[
    BadBlockCase {
        name: "duplicate_m1",
        extra_coinbase_outputs: duplicate_m1_outputs,
        expected_log_contains: "rejecting block: M1 sidechain proposal for slot",
    },
    BadBlockCase {
        name: "duplicate_m2",
        extra_coinbase_outputs: duplicate_m2_outputs,
        expected_log_contains: "rejecting block: M2 that acks proposal for slot",
    },
    BadBlockCase {
        name: "duplicate_m4",
        extra_coinbase_outputs: duplicate_m4_outputs,
        expected_log_contains: "rejecting block: M4 already included at index",
    },
    BadBlockCase {
        name: "duplicate_m7",
        extra_coinbase_outputs: duplicate_m7_outputs,
        expected_log_contains: "rejecting block: M7 for slot",
    },
];

fn duplicate_m1_outputs() -> anyhow::Result<Vec<TxOut>> {
    let proposal = M1ProposeSidechain {
        sidechain_number: DummySidechain::SIDECHAIN_NUMBER,
        description: SidechainDescription(b"duplicate-m1 test".to_vec()),
    };
    let m1_a: ScriptBuf = ScriptBuf::try_from(M1ProposeSidechain {
        sidechain_number: proposal.sidechain_number,
        description: proposal.description.clone(),
    })?;
    let m1_b: ScriptBuf = proposal.try_into()?;
    Ok(vec![zero_value(m1_a), zero_value(m1_b)])
}

fn duplicate_m2_outputs() -> anyhow::Result<Vec<TxOut>> {
    let m2_a: ScriptBuf = M2AckSidechain {
        sidechain_number: DummySidechain::SIDECHAIN_NUMBER,
        description_hash: sha256d::Hash::from_byte_array([0xAA; 32]),
    }
    .try_into()?;
    let m2_b: ScriptBuf = M2AckSidechain {
        sidechain_number: DummySidechain::SIDECHAIN_NUMBER,
        description_hash: sha256d::Hash::from_byte_array([0xBB; 32]),
    }
    .try_into()?;
    Ok(vec![zero_value(m2_a), zero_value(m2_b)])
}

fn duplicate_m4_outputs() -> anyhow::Result<Vec<TxOut>> {
    let m4_a: ScriptBuf = M4AckBundles::OneByte {
        upvotes: vec![0x00],
    }
    .try_into()?;
    let m4_b: ScriptBuf = M4AckBundles::OneByte {
        upvotes: vec![0x01],
    }
    .try_into()?;
    Ok(vec![zero_value(m4_a), zero_value(m4_b)])
}

fn duplicate_m7_outputs() -> anyhow::Result<Vec<TxOut>> {
    let slot = DummySidechain::SIDECHAIN_NUMBER;
    let m7_a: ScriptBuf = M7BmmAccept {
        sidechain_number: slot,
        sidechain_block_hash: BmmCommitment([0xAA; 32]),
    }
    .try_into()?;
    let m7_b: ScriptBuf = M7BmmAccept {
        sidechain_number: slot,
        sidechain_block_hash: BmmCommitment([0xBB; 32]),
    }
    .try_into()?;
    Ok(vec![zero_value(m7_a), zero_value(m7_b)])
}

pub async fn test_invalid_block(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let _sidechain = DummySidechain::setup(&post_setup).await?;

    setup_active_sidechain(&mut post_setup).await?;
    wait_past_mtp(&post_setup).await?;

    let stdout_path = post_setup.directories.enforcer_dir.join("stdout.txt");

    let mut failures = Vec::new();
    for case in CASES {
        match run_case(&post_setup, case, &stdout_path).await {
            Ok(()) => tracing::info!(case = case.name, "case passed"),
            Err(err) => {
                tracing::error!(case = case.name, "case failed: {err:#}");
                failures.push(format!("{}: {err:#}", case.name));
            }
        }
    }

    if !failures.is_empty() {
        anyhow::bail!(
            "invalid_block cases failed:\n  - {}",
            failures.join("\n  - ")
        );
    }
    Ok(())
}

async fn run_case(
    post_setup: &PostSetup,
    case: &BadBlockCase,
    stdout_path: &Path,
) -> anyhow::Result<()> {
    let bad_block_hash = submit_invalid_block(post_setup, case).await?;
    tracing::info!(case = case.name, %bad_block_hash, "submitted bad block");

    // Poll `getchaintips` for the bad block to show `status="invalid"`.
    // Tip-based polling is unreliable here because the test environment
    // background-mines valid blocks every ~10s, so the tip may advance past
    // (rather than revert to) the pre-submit hash even when our block is
    // correctly invalidated.
    poll_until_chaintip_invalid(post_setup, bad_block_hash, Duration::from_secs(10)).await?;

    // Each case's expected substring uniquely identifies a single message
    // type (M1/M2/M4/M7), so cross-case collisions are impossible — no need
    // to slice per case.
    let stdout_contents = std::fs::read_to_string(stdout_path)?;
    anyhow::ensure!(
        stdout_contents.contains(case.expected_log_contains),
        "enforcer log did not contain `{}`",
        case.expected_log_contains
    );
    Ok(())
}

async fn submit_invalid_block(
    post_setup: &PostSetup,
    case: &BadBlockCase,
) -> anyhow::Result<BlockHash> {
    submit_block_with_extra_coinbase_outputs(post_setup, (case.extra_coinbase_outputs)()?).await
}

/// Submit a block with `extra_outputs` appended to the coinbase, after the
/// standard reward + witness-commitment outputs. Bypasses the enforcer's
/// mining proxy by talking to bitcoind directly via `submitblock`.
async fn submit_block_with_extra_coinbase_outputs(
    post_setup: &PostSetup,
    extra_outputs: Vec<TxOut>,
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
    outputs.extend(extra_outputs);
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

fn zero_value(script_pubkey: ScriptBuf) -> TxOut {
    TxOut {
        script_pubkey,
        value: Amount::ZERO,
    }
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

async fn poll_until_chaintip_invalid(
    post_setup: &PostSetup,
    block_hash: BlockHash,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let target = block_hash.to_string();
    loop {
        let chaintips_json = post_setup
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "getchaintips", [])
            .run_utf8()
            .await?;
        let chaintips: Vec<serde_json::Value> = serde_json::from_str(&chaintips_json)?;
        let status = chaintips
            .iter()
            .find(|tip| tip.get("hash").and_then(|h| h.as_str()) == Some(&target))
            .and_then(|tip| tip.get("status"))
            .and_then(|s| s.as_str());
        match status {
            Some("invalid") => return Ok(()),
            _ if tokio::time::Instant::now() >= deadline => anyhow::bail!(
                "timed out waiting for `{block_hash}` to be marked invalid in chaintips; \
                 last status: {status:?}"
            ),
            _ => sleep(Duration::from_millis(200)).await,
        }
    }
}

/// BIP300 M4: once an `M6ID`'s vote count exceeds
/// `WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD`, the block that pushes it over
/// MUST include the corresponding M6 — otherwise the block is invalid.
///
/// This test drives the enforcer to vote_count == THRESHOLD (= 5 in this
/// codebase), then hand-submits a block via `submitblock` that contains
/// the next M4 OneByte upvote (which would push vote_count to 6) but
/// omits the M6 from txdata. The enforcer must reject the block.
pub async fn test_block_missing_required_m6(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let mut sidechain = DummySidechain::setup(&post_setup).await?;

    setup_active_sidechain(&mut post_setup).await?;

    // Treasury needs funds before a withdrawal bundle can be proposed.
    let deposit_address = sidechain.get_deposit_address();
    deposit(
        &mut post_setup,
        &mut sidechain,
        &deposit_address,
        Amount::from_btc(1.0)?,
        Amount::from_sat(1_000),
    )
    .await?;

    // Submit a withdrawal via the enforcer; M3 will be injected into the
    // next coinbase mined through the enforcer's proxy.
    let receive_address = post_setup.receive_address.clone();
    let _m6id = sidechain
        .create_withdrawal(
            &mut post_setup,
            &receive_address,
            Amount::from_btc(0.5)?,
            Amount::from_sat(1_000),
        )
        .await?;

    // Block N: enforcer mines M3 (vote_count = 0 after this block).
    mine_check_block_events(&mut post_setup, 1, None, |_, _| Ok(())).await?;
    // Blocks N+1..N+5: enforcer mines 5 M4 OneByte upvotes for the pending
    // bundle, bringing vote_count to THRESHOLD (5). The *next* upvote would
    // push it past THRESHOLD and is the one that must be paired with M6.
    mine_check_block_events(
        &mut post_setup,
        u32::from(WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD),
        Some(true),
        |_, _| Ok(()),
    )
    .await?;

    wait_past_mtp(&post_setup).await?;

    // Hand-craft the offending block: a single M4 OneByte upvote at index 0
    // (chronologically-first pending M6ID for this sidechain), with no M6
    // in txdata. Must be rejected.
    let extra_outputs = vec![zero_value(
        M4AckBundles::OneByte {
            upvotes: vec![0x00],
        }
        .try_into()?,
    )];
    let bad_block_hash =
        submit_block_with_extra_coinbase_outputs(&post_setup, extra_outputs).await?;
    tracing::info!(%bad_block_hash, "submitted missing-M6 block");

    poll_until_chaintip_invalid(&post_setup, bad_block_hash, Duration::from_secs(10)).await?;

    let stdout_path = post_setup.directories.enforcer_dir.join("stdout.txt");
    let stdout = std::fs::read_to_string(&stdout_path)?;
    anyhow::ensure!(
        stdout.contains("MissingRequiredM6") || stdout.contains("missing required M6"),
        "enforcer log did not contain expected must-include-M6 rejection message"
    );
    Ok(())
}
