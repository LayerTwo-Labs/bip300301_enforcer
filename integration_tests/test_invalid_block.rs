use std::{str::FromStr as _, time::Duration};

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    messages::{M1ProposeSidechain, M2AckSidechain, M4AckBundles, M7BmmAccept},
    types::{BmmCommitment, SidechainDescription, op_drivechain_script},
};
use bitcoin::{
    Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Txid, Witness,
    block::Header,
    consensus::encode::{deserialize_hex, serialize_hex},
    hashes::{Hash as _, sha256d},
    script::{Builder as ScriptBuilder, PushBytesBuf},
    transaction::Version,
};
use futures::channel::mpsc;
use serde::Deserialize;
use tokio::time::sleep;

use crate::{
    block_verdict::{Expect, assert_enforcer_verdict},
    integration_test::{activate_sidechain, fund_enforcer, propose_sidechain},
    setup::{DummySidechain, PostSetup, Sidechain},
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
    let (sidechain_res_tx, _sidechain_res_rx) = mpsc::unbounded();
    let mut _sidechain = DummySidechain::setup((), &post_setup, sidechain_res_tx).await?;

    propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    wait_past_mtp(&post_setup).await?;

    let mut failures = Vec::new();
    for case in CASES {
        match run_case(&mut post_setup, case).await {
            Ok(()) => tracing::info!(case = case.name, "case passed"),
            Err(err) => {
                tracing::error!(case = case.name, "case failed: {err:#}");
                failures.push(format!("{}: {err:#}", case.name));
            }
        }
    }

    // An M5 deposit is a regular (non-coinbase) transaction, so unlike the
    // coinbase-message cases above it can't be expressed as an extra coinbase
    // output. It's mined as a raw tx via `generateblock` instead.
    const M5_CASE: &str = "m5_missing_address";
    match run_m5_missing_address_case(&mut post_setup).await {
        Ok(()) => tracing::info!(case = M5_CASE, "case passed"),
        Err(err) => {
            tracing::error!(case = M5_CASE, "case failed: {err:#}");
            failures.push(format!("{M5_CASE}: {err:#}"));
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

async fn run_case(post_setup: &mut PostSetup, case: &BadBlockCase) -> anyhow::Result<()> {
    let bad_block_hash = submit_invalid_block(post_setup, case).await?;
    tracing::info!(case = case.name, %bad_block_hash, "submitted bad block");

    // The enforcer must reject the block with the expected reason. Each case's
    // expected substring uniquely identifies a single message type
    // (M1/M2/M4/M7)
    assert_enforcer_verdict(
        post_setup,
        bad_block_hash,
        Expect::Rejected {
            log_contains: case.expected_log_contains,
        },
        Duration::from_secs(10),
    )
    .await
}

/// Blocks mined to give bitcoind's wallet a mature, spendable coinbase UTXO to
/// fund the M5 deposit transaction (coinbase outputs need 100 confirmations).
const M5_FUNDING_BLOCKS: u32 = 101;

const M5_TX_FEE: Amount = Amount::from_sat(1_000);

#[derive(Deserialize)]
struct Utxo {
    txid: String,
    vout: u32,
    amount: f64,
}

#[derive(Deserialize)]
struct SignResult {
    hex: String,
    complete: bool,
}

#[derive(Deserialize)]
struct GenerateBlockResult {
    hash: String,
}

async fn run_m5_missing_address_case(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    let bad_block_hash = submit_m5_missing_address_block(post_setup).await?;
    tracing::info!(%bad_block_hash, "submitted bad M5 deposit block");

    // Without the fix the enforcer accepts this deposit with an empty address;
    // with the fix it rejects the block because the address OP_RETURN output
    // required by BIP 300 M5 is missing.
    assert_enforcer_verdict(
        post_setup,
        bad_block_hash,
        Expect::Rejected {
            log_contains: "has no address OP_RETURN output",
        },
        Duration::from_secs(10),
    )
    .await
}

/// Build, sign, and mine (via `generateblock`) an M5 deposit for the active
/// DummySidechain slot whose transaction creates the treasury UTXO but omits
/// the address OP_RETURN output that must immediately follow it.
async fn submit_m5_missing_address_block(post_setup: &PostSetup) -> anyhow::Result<BlockHash> {
    let mining_address = post_setup.mining_address.to_string();

    // Ensure bitcoind's wallet has a mature UTXO to spend into the deposit.
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [M5_FUNDING_BLOCKS.to_string(), mining_address.clone()],
        )
        .run_utf8()
        .await?;

    let utxos: Vec<Utxo> = {
        let json = post_setup
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "listunspent", [])
            .run_utf8()
            .await?;
        serde_json::from_str(&json)?
    };
    let (utxo, input_value) = utxos
        .into_iter()
        .find_map(|u| {
            let amount = Amount::from_btc(u.amount).ok()?;
            (amount > M5_TX_FEE).then_some((u, amount))
        })
        .ok_or_else(|| anyhow::anyhow!("no spendable UTXO in bitcoind wallet"))?;

    // The deposit's sole output is a positive-value OP_DRIVECHAIN output for the
    // active slot: it raises the treasury value (so it's an M5, not an M6), but
    // there is nothing at vout+1 to carry the deposit address.
    let unsigned_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_str(&utxo.txid)?,
                vout: utxo.vout,
            },
            ..TxIn::default()
        }],
        output: vec![TxOut {
            script_pubkey: op_drivechain_script(DummySidechain::SIDECHAIN_NUMBER),
            value: input_value - M5_TX_FEE,
        }],
    };

    let signed_hex = {
        let json = post_setup
            .bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "signrawtransactionwithwallet",
                [serialize_hex(&unsigned_tx)],
            )
            .run_utf8()
            .await?;
        let signed: SignResult = serde_json::from_str(&json)?;
        anyhow::ensure!(signed.complete, "signrawtransactionwithwallet incomplete");
        signed.hex
    };

    // `generateblock` mines a block containing the raw tx, bypassing mempool
    // standardness (which rejects OP_DRIVECHAIN as non-standard) while still
    // enforcing consensus rules.
    let txs_arg = serde_json::to_string(&[signed_hex])?;
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generateblock", [mining_address, txs_arg])
        .run_utf8()
        .await?;
    let result: GenerateBlockResult = serde_json::from_str(&json)?;
    Ok(BlockHash::from_str(&result.hash)?)
}

async fn submit_invalid_block(
    post_setup: &PostSetup,
    case: &BadBlockCase,
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
    outputs.extend((case.extra_coinbase_outputs)()?);
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
