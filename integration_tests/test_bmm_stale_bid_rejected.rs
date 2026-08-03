//! Verifies the mechanism that stops a losing/expired BMM bid targeting one
//! mainchain block from being mistaken for a valid BMM request if it's later
//! mined into a *different* block -- even when that later block's coinbase
//! carries a genuine M7 accept for the exact same sidechain block hash.
//!
//! There are actually two independent layers of defense, and this test
//! exercises both:
//!
//! 1. **Active**: as soon as a block supersedes the tip a pending bid was
//!    targeting, the enforcer's mempool-sync task deprioritizes that bid to
//!    an effective feerate of roughly -21,000,000 sat/vB
//!    (`cusf-enforcer-mempool`'s `RejectTx` sync action, via
//!    `prioritisetransaction`). Ordinary fee-driven `getblocktemplate`
//!    construction will never again select it, even though it's still
//!    nominally resident in the mempool.
//! 2. **Passive / consensus backstop**: `handle_m8`
//!    (`lib/validator/task/mod.rs`) checks an M8's embedded
//!    `prev_mainchain_block_hash` against the *actual* parent of whatever
//!    block it lands in, not just whether some M7 with matching `(S, H)` is
//!    present somewhere in that block's coinbase. A mismatch is
//!    `BmmRequestExpired` -- non-fatal at the message level, but it poisons
//!    the *whole* containing block: `connect_block` fails, and the enforcer
//!    `invalidateblock`s it. This is what protects against a miner who
//!    ignores (or isn't running) layer 1 and force-includes the stale bid
//!    anyway, e.g. via `generateblock`/`submitblock` with an explicit raw
//!    transaction rather than ordinary mempool selection.
//!
//! This test broadcasts a losing bid `L`, confirms bitcoind will not
//! naturally re-include it (layer 1), then hand-crafts a block that force
//! -includes it anyway alongside a legitimately-corresponding, freshly
//! targeted bid for the exact same sidechain block hash, and confirms the
//! whole block is rejected (layer 2).

use std::{str::FromStr as _, time::Duration};

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    messages::M8BmmRequest,
    proto::{
        self,
        common::{ConsensusHex, ReverseHex},
        mainchain::{BlockHeaderInfo, CreateBmmCriticalDataTransactionRequest, GetChainTipRequest},
    },
    types::BmmCommitment,
};
use bitcoin::{
    Amount, BlockHash, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid,
    consensus::encode::serialize_hex, transaction::Version,
};
use buffa::MessageField;
use serde::Deserialize;

use crate::{
    block_verdict::{Expect, assert_enforcer_verdict},
    setup::{DummySidechain, PostSetup, Sidechain},
};

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

fn sidechain_block_hash(label: &str) -> [u8; 32] {
    use bitcoin::hashes::Hash;
    bitcoin::hashes::sha256::Hash::hash(format!("stale bid {label}").as_bytes()).to_byte_array()
}

const RAW_BID_FEE: Amount = Amount::from_sat(5_000);

/// Craft, sign, and broadcast a raw M8 directly from bitcoind's own wallet --
/// entirely independent of the enforcer's BDK wallet, so it can never be
/// picked up, tracked, or RBF'd away by the enforcer's own bid bookkeeping.
async fn broadcast_raw_bid(
    post_setup: &PostSetup,
    prev_mainchain_block_hash: BlockHash,
    h_star: [u8; 32],
) -> anyhow::Result<Txid> {
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
            (amount > RAW_BID_FEE).then_some((u, amount))
        })
        .ok_or_else(|| anyhow::anyhow!("no spendable UTXO in bitcoind wallet"))?;

    let change_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?;
    let change_address = bitcoin::Address::from_str(change_address.trim())?
        .require_network(post_setup.network.into())?;

    let script_pubkey = ScriptBuf::new_op_return(&M8BmmRequest::build(
        DummySidechain::SIDECHAIN_NUMBER,
        BmmCommitment(h_star),
        prev_mainchain_block_hash,
    )?);

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
                value: input_value - RAW_BID_FEE,
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

    let txid_str = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "sendrawtransaction", [signed_hex, "0".to_owned()])
        .run_utf8()
        .await?;
    Ok(Txid::from_str(txid_str.trim())?)
}

async fn get_tip_info(post_setup: &mut PostSetup) -> anyhow::Result<(ReverseHex, u32, BlockHash)> {
    let BlockHeaderInfo {
        block_hash: tip_block_hash,
        height: tip_height,
        ..
    } = post_setup
        .validator_service_client
        .get_chain_tip(GetChainTipRequest::default())
        .await?
        .into_owned()
        .block_header_info
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("Expected `block_header_info` in GetChainTipResponse"))?;
    let tip_block_hash_field = tip_block_hash
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("Expected `block_hash` in BlockHeaderInfo"))?;
    let decoded: BlockHash = tip_block_hash_field
        .clone()
        .decode::<BlockHeaderInfo, BlockHash>("block_hash")?;
    Ok((tip_block_hash_field, tip_height, decoded))
}

async fn get_raw_tx_hex(post_setup: &PostSetup, txid: Txid) -> anyhow::Result<String> {
    let hex = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getrawtransaction", [txid.to_string()])
        .run_utf8()
        .await?;
    Ok(hex.trim().to_owned())
}

/// Hand-craft and submit a block whose coinbase legitimately accepts `h_star`
/// (a real M7, corresponding to `fresh_tx_hex`'s correctly-targeted M8), but
/// whose txdata *also* includes `stale_tx_hex` -- a raw M8 for that same
/// `h_star`, previously broadcast against a now-superseded tip. Bypasses
/// ordinary mempool/fee-based selection entirely (unlike real mining, which
/// would never pick a deprioritized transaction like this on its own) to
/// isolate and test the consensus-level backstop.
async fn submit_block_with_stale_bid(
    post_setup: &PostSetup,
    h_star: [u8; 32],
    fresh_tx_hex: &str,
    stale_tx_hex: &str,
) -> anyhow::Result<BlockHash> {
    crate::bmm_block::submit_block_with_bmm_accepts(
        post_setup,
        &[(DummySidechain::SIDECHAIN_NUMBER, h_star)],
        &[fresh_tx_hex, stale_tx_hex],
    )
    .await
}

/// Runs as a phase of `bmm_bid_lifecycle`, which it shares setup with
/// exactly, rather than paying for its own node and enforcer.
pub async fn stale_bid_rejected_body(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    // Mature a spendable UTXO in bitcoind's *own* wallet -- separate from the
    // enforcer's BDK wallet -- so the stale bid below is a raw, hand-crafted
    // transaction the enforcer's own bid-tracking never sees.
    //
    // Mined with `generateblock` and an explicit empty tx list, not
    // `generatetoaddress`. The latter builds from the mempool, and a BMM bid
    // sitting there gets mined into a block whose coinbase carries no matching
    // M7 -- which the enforcer rightly rejects, since an unaccepted M8 is
    // invalid. Harmless on a fresh chain, fatal when anything ran before us
    // and left a bid in flight.
    const MATURITY_BLOCKS: usize = 101;
    let mining_address = post_setup.mining_address.to_string();
    for _ in 0..MATURITY_BLOCKS {
        let () = post_setup
            .bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "generateblock",
                [mining_address.clone(), "[]".to_owned()],
            )
            .run_utf8()
            .await
            .map(drop)?;
    }

    let h_star = sidechain_block_hash("target");

    // ---- Bid L: a real, standalone, fee-paying M8 for the *current* tip ----
    let (_, _, tip_0) = get_tip_info(post_setup).await?;
    let stale_txid = broadcast_raw_bid(&*post_setup, tip_0, h_star).await?;
    let stale_tx_hex = get_raw_tx_hex(&*post_setup, stale_txid).await?;
    tracing::info!(%stale_txid, %tip_0, "Broadcast stale bid L");

    // ---- L loses: an empty block advances the tip without it ----
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generateblock", [mining_address, "[]".to_owned()])
        .run_utf8()
        .await?;
    let result: GenerateBlockResult = serde_json::from_str(&json)?;
    let empty_block_hash: BlockHash = result.hash.parse()?;
    let () = assert_enforcer_verdict(
        post_setup,
        empty_block_hash,
        Expect::Accepted,
        Duration::from_secs(15),
    )
    .await?;
    tracing::info!(%empty_block_hash, "Bid L lost: a block was mined without it, tip advanced");

    // ---- Layer 1 (active): the enforcer's own mempool-sync task
    // deprioritizes L the moment its targeted tip is superseded. L is still
    // nominally resident in the mempool -- "losing" is not a double-spend --
    // but from here on real fee-based mining will never pick it again.
    let entry_json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getmempoolentry", [stale_txid.to_string()])
        .run_utf8()
        .await?;
    let entry: serde_json::Value = serde_json::from_str(&entry_json)?;
    let deprioritized_fee = entry
        .get("fees")
        .and_then(|fees| fees.get("modified"))
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| anyhow::anyhow!("getmempoolentry: missing fees.modified"))?;
    anyhow::ensure!(
        deprioritized_fee < 0.0,
        "expected the enforcer to have deprioritized the losing bid to a negative effective \
         fee (found {deprioritized_fee}); real fee-based mining relies on this to never \
         re-select it"
    );
    tracing::info!(
        deprioritized_fee,
        "Confirmed layer 1: the enforcer deprioritized L to a negative effective fee -- \
         ordinary fee-driven mining will never select it again"
    );

    // ---- A fresh, *correctly targeted* bid for the exact same h* is placed
    // via the enforcer's own wallet, from entirely different UTXOs, so it can
    // never RBF or otherwise interact with L. This is what makes the next
    // block's M7 legitimately "correspond" (matching S, H) to L too.
    let (prev_bytes_1, height_1, tip_1) = get_tip_info(post_setup).await?;
    let fresh_txid = post_setup
        .wallet_service_client
        .create_bmm_critical_data_transaction(CreateBmmCriticalDataTransactionRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            value_sats: proto::wrap_u64(5_000),
            height: proto::wrap_u32(height_1),
            critical_hash: MessageField::some(ConsensusHex::encode(&h_star)),
            prev_bytes: MessageField::some(prev_bytes_1),
        })
        .await?
        .into_owned()
        .txid
        .into_option()
        .and_then(|txid| proto::unwrap_string(txid.hex))
        .ok_or_else(|| anyhow::anyhow!("response must include a txid on success"))?;
    let fresh_tx_hex = get_raw_tx_hex(&*post_setup, Txid::from_str(&fresh_txid)?).await?;
    tracing::info!(%fresh_txid, %tip_1, "Placed a fresh, correctly-targeted bid for the same h*");

    // ---- Layer 2 (consensus backstop): hand-craft a block that force
    // -includes L anyway (as e.g. a miner ignoring layer 1's fee signal, or
    // one not running this enforcer's mempool-sync at all, might), alongside
    // the legitimately-corresponding fresh bid and its matching M7.
    let poisoned_block_hash =
        submit_block_with_stale_bid(&*post_setup, h_star, &fresh_tx_hex, &stale_tx_hex).await?;
    tracing::info!(%poisoned_block_hash, "Submitted a hand-crafted block containing both bids");

    let () = assert_enforcer_verdict(
        post_setup,
        poisoned_block_hash,
        Expect::Rejected {
            log_contains: "BMM request expired",
        },
        Duration::from_secs(15),
    )
    .await?;
    tracing::info!(
        "Confirmed layer 2: the enforcer rejected the whole block because it contained a \
         stale M8 whose prev_mainchain_block_hash no longer matched -- despite the block also \
         containing a legitimately-corresponding, freshly-targeted M7/M8 pair for the exact \
         same sidechain block hash"
    );

    Ok(())
}
