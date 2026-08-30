use std::{str::FromStr as _, time::Duration};

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    messages::{M1ProposeSidechain, M2AckSidechain, M4AckBundles, M7BmmAccept, M8BmmRequest},
    types::{BmmCommitment, SidechainDescription, SidechainNumber, op_drivechain_script},
};
use bitcoin::{
    Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Txid, Witness,
    block::Header,
    consensus::encode::{deserialize_hex, serialize_hex},
    hashes::Hash as _,
    script::{Builder as ScriptBuilder, PushBytesBuf},
    transaction::Version,
};
use futures::channel::mpsc;
use serde::Deserialize;

use crate::{
    block_verdict::{Expect, assert_enforcer_verdict, wait_for_enforcer_height},
    bmm_block::{submit_block_with_bmm_accepts, wait_past_mtp},
    integration_test::{activate_sidechain, fund_enforcer, propose_sidechain},
    setup::{DummySidechain, PostSetup, Sidechain},
};

pub(crate) struct BadBlockCase {
    name: &'static str,
    extra_coinbase_outputs: fn() -> anyhow::Result<Vec<TxOut>>,
    expected_log_contains: &'static str,
}

/// Shared with the sync-path test (`test_invalid_block_during_sync`), which
/// mines this block while the enforcer is down.
pub(crate) const DUPLICATE_M1: BadBlockCase = BadBlockCase {
    name: "duplicate_m1",
    extra_coinbase_outputs: duplicate_m1_outputs,
    expected_log_contains: "rejecting block: M1 sidechain proposal for slot",
};

const CASES: &[BadBlockCase] = &[
    DUPLICATE_M1,
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

    // A duplicate M2 needs a live proposal from an *earlier* block to ack, so
    // it spans two blocks rather than the single self-contained one every
    // `CASES` entry is built from.
    const M2_CASE: &str = "duplicate_m2";
    match run_duplicate_m2_case(&mut post_setup).await {
        Ok(()) => tracing::info!(case = M2_CASE, "case passed"),
        Err(err) => {
            tracing::error!(case = M2_CASE, "case failed: {err:#}");
            failures.push(format!("{M2_CASE}: {err:#}"));
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

    // An M8 is a regular transaction too, but needs a coinbase carrying the
    // matching M7, so it is neither an extra coinbase output nor something
    // `generateblock` can mine. Runs last: it leaves two requests resident in
    // the mempool and a rejected block behind it, which the cases above are
    // not built to tolerate.
    const M8_CASE: &str = "duplicate_m8";
    match run_duplicate_m8_case(&mut post_setup).await {
        Ok(()) => tracing::info!(case = M8_CASE, "case passed"),
        Err(err) => {
            tracing::error!(case = M8_CASE, "case failed: {err:#}");
            failures.push(format!("{M8_CASE}: {err:#}"));
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

/// An unused slot, so the proposal below cannot disturb the active
/// [`DummySidechain`] in slot 0.
const DUPLICATE_M2_SLOT: SidechainNumber = SidechainNumber(7);

/// BIP 300: "only one M2 per sidechain slot per block". The rule is about acks
/// that are actually counted -- an M2 whose description hash matches no live
/// proposal is an ordinary script, and an M2 in the same block as its M1 is
/// ignored too -- so the duplicate has to ack a proposal made in an earlier
/// block. Two blocks: one proposing, one acking it twice.
async fn run_duplicate_m2_case(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    let description = SidechainDescription(b"duplicate-m2 test".to_vec());
    let m1: ScriptBuf = M1ProposeSidechain {
        sidechain_number: DUPLICATE_M2_SLOT,
        description: description.clone(),
    }
    .try_into()?;
    let proposal_block_hash =
        submit_block_with_coinbase_outputs(post_setup, vec![zero_value(m1)]).await?;
    tracing::info!(%proposal_block_hash, "submitted proposal for the duplicate M2 to ack");
    let () = assert_enforcer_verdict(
        post_setup,
        proposal_block_hash,
        Expect::Accepted,
        Duration::from_secs(10),
    )
    .await?;

    let m2: ScriptBuf = M2AckSidechain {
        sidechain_number: DUPLICATE_M2_SLOT,
        description_hash: description.sha256d_hash(),
    }
    .try_into()?;
    let m2_output = zero_value(m2);
    let bad_block_hash =
        submit_block_with_coinbase_outputs(post_setup, vec![m2_output.clone(), m2_output]).await?;
    tracing::info!(%bad_block_hash, "submitted bad block");

    assert_enforcer_verdict(
        post_setup,
        bad_block_hash,
        Expect::Rejected {
            log_contains: "rejecting block: M2 that acks proposal for slot",
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

const M8_FUNDING_BLOCKS: u32 = 101;

const M8_TX_FEE: Amount = Amount::from_sat(5_000);

/// BIP301: "Only one `M8` can be accepted per mainchain block per sidechain
/// slot", so that a miner cannot collect the fees of several BMM requests
/// while connecting only one sidechain block.
///
/// The coinbase half of the same rule is the `duplicate_m7` case above. This
/// is the transaction half, and it cannot be caught the same way: both
/// requests here are individually valid, name the current tip, and carry the
/// same h*, so each one *corresponds* to the single M7 in the coinbase.
async fn run_duplicate_m8_case(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    let bad_block_hash = submit_duplicate_m8_block(post_setup).await?;
    tracing::info!(%bad_block_hash, "submitted block with two BMM requests for one slot");

    assert_enforcer_verdict(
        post_setup,
        bad_block_hash,
        Expect::Rejected {
            log_contains: "Multiple BMM requests accepted in sidechain slot",
        },
        Duration::from_secs(10),
    )
    .await
}

/// Build two distinct transactions carrying the same M8 payload, and submit a
/// block containing both alongside a coinbase that legitimately accepts their
/// commitment.
async fn submit_duplicate_m8_block(post_setup: &mut PostSetup) -> anyhow::Result<BlockHash> {
    let mining_address = post_setup.mining_address.to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [M8_FUNDING_BLOCKS.to_string(), mining_address],
        )
        .run_utf8()
        .await?;

    let tip: BlockHash = {
        let hex = post_setup
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "getbestblockhash", [])
            .run_utf8()
            .await?;
        BlockHash::from_str(hex.trim())?
    };
    let h_star = [0xD8; 32];
    let script_pubkey =
        M8BmmRequest::script_pubkey(DummySidechain::SIDECHAIN_NUMBER, BmmCommitment(h_star), tip)?;

    let mut tx_hexes = Vec::new();
    for request in 1..=2 {
        // Broadcast rather than merely signing: it takes the spent output out
        // of `listunspent`, so the second request is funded from a different
        // one and the two differ as transactions while their M8 payloads stay
        // byte-identical.
        let hex = build_and_broadcast_m8(post_setup, script_pubkey.clone()).await?;
        tracing::info!(request, "built a BMM request for the same slot and h*");
        tx_hexes.push(hex);
    }
    anyhow::ensure!(
        tx_hexes[0] != tx_hexes[1],
        "the two BMM requests must be distinct transactions to test anything",
    );
    let tx_hex_refs: Vec<&str> = tx_hexes.iter().map(String::as_str).collect();
    submit_block_with_bmm_accepts(
        post_setup,
        &[(DummySidechain::SIDECHAIN_NUMBER, h_star)],
        &tx_hex_refs,
    )
    .await
}

/// Fund, sign and broadcast a transaction whose first output is `script_pubkey`.
async fn build_and_broadcast_m8(
    post_setup: &PostSetup,
    script_pubkey: ScriptBuf,
) -> anyhow::Result<String> {
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
            (amount > M8_TX_FEE).then_some((u, amount))
        })
        .ok_or_else(|| anyhow::anyhow!("no spendable UTXO in bitcoind wallet"))?;
    let change_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?;
    let change_address = bitcoin::Address::from_str(change_address.trim())?
        .require_network(post_setup.network.into())?;

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
        output: vec![
            TxOut {
                script_pubkey,
                value: Amount::ZERO,
            },
            TxOut {
                script_pubkey: change_address.script_pubkey(),
                value: input_value - M8_TX_FEE,
            },
        ],
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
    let _txid: String = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "sendrawtransaction",
            [signed_hex.clone(), "0".to_owned()],
        )
        .run_utf8()
        .await?;
    Ok(signed_hex)
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

    // Let the enforcer catch up on those blocks before the bad one goes in.
    // Mining that many at once leaves it far enough behind that the verdict
    // timeout below would otherwise expire while it is still connecting the
    // funding blocks, never having seen the block under test.
    let funded_height: u32 = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    let () = wait_for_enforcer_height(post_setup, funded_height).await?;

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

pub(crate) async fn submit_invalid_block(
    post_setup: &PostSetup,
    case: &BadBlockCase,
) -> anyhow::Result<BlockHash> {
    submit_block_with_coinbase_outputs(post_setup, (case.extra_coinbase_outputs)()?).await
}

/// Mine and submit a block whose coinbase carries `extra_coinbase_outputs` on
/// top of the payout and witness commitment.
async fn submit_block_with_coinbase_outputs(
    post_setup: &PostSetup,
    extra_coinbase_outputs: Vec<TxOut>,
) -> anyhow::Result<BlockHash> {
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
