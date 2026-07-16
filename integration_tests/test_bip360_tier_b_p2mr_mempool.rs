//! Tier B — **known open problem** (expected FAIL until fixed).
//!
//! # What this asserts
//!
//! After funding a P2MR UTXO on the dual stack (stock Alice + cryptoquick P2MR Bob),
//! an **enforcer-format** spend must be accepted into **Bob’s mempool** via
//! `sendrawtransaction` (no `submitblock` escape hatch).
//!
//! Shapes checked (stop at first failure):
//! 1. Schnorr-only script-path spend  
//! 2. Kitchen-sink (Schnorr + ML-DSA + SLH) spend  
//!
//! # Why it fails today
//!
//! Bob (`BITCOIND_P2MR` / jbride#2 head) rejects enforcer overload spends, typically:
//! `mempool-script-verify-flag-failed (Witness program hash mismatch)` or RPC `-26`.
//! Tier A still demos CUSF tip retention via submitblock; Tier B is pure mempool interop.
//!
//! # How to run (opt-in — not in default suite)
//!
//! ```bash
//! just setup-p2mr   # and just setup for electrs
//! BIP360_TIER_B=1 just bip360-tier-b-mempool
//! ```
//!
//! See [`docs/TIER_B_P2MR_MEMPOOL.md`](../docs/TIER_B_P2MR_MEMPOOL.md) for bounty notes.

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::validator::pqc::signer_dev::{
    SignAlgorithm, build_kitchen_sink_spend_from_prevout, build_p2mr_spend_from_prevout,
    p2mr_output_for_algorithm,
};
use bitcoin::sighash::TapSighashType;
use futures::{StreamExt as _, channel::mpsc};
use tracing::Instrument as _;

use crate::{
    bip360_block::{HYBRID_EC_ENTROPY, MLDSA_ENTROPY, SCHNORR_ENTROPY, SLH_ENTROPY},
    bip360_dual_node::{
        ROUND_SPEND_OUTPUT, TIER_B_TEST_NAME, assert_bob_p2mr_mempool_accepts_spend,
        round0_wallet_to_p2mr, setup_tier_a_dual_node_peers,
    },
    util::{BinPaths, TestFileRegistry},
};

fn tier_b_opt_in() -> bool {
    matches!(
        std::env::var("BIP360_TIER_B").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

async fn bip360_tier_b_p2mr_mempool_task(
    peers: crate::bip360_dual_node::DualNodePeers,
) -> anyhow::Result<()> {
    let mut peers = peers;
    anyhow::ensure!(
        peers.tier_a,
        "Tier B reuses Tier A topology (Alice=stock, Bob=BITCOIND_P2MR)"
    );

    tracing::info!(
        "Tier B open problem: require Bob P2MR mempool for enforcer-format spends \
         (docs/TIER_B_P2MR_MEMPOOL.md)"
    );

    // Fund Schnorr P2MR pile (funding already works on both mempools).
    let pile = round0_wallet_to_p2mr(&mut peers).await?;
    tracing::info!(?pile.outpoint, "funding complete — attacking Bob mempool with spends");

    // --- Shape 1: Schnorr-only (minimal overload leaf) ---
    let (next_spk, _) = p2mr_output_for_algorithm(SignAlgorithm::Schnorr, &SCHNORR_ENTROPY)
        .map_err(anyhow::Error::msg)?;
    let schnorr_spend = build_p2mr_spend_from_prevout(
        SignAlgorithm::Schnorr,
        &SCHNORR_ENTROPY,
        TapSighashType::All,
        pile.outpoint,
        pile.txout.clone(),
        ROUND_SPEND_OUTPUT,
        next_spk,
    )
    .map_err(anyhow::Error::msg)?;

    assert_bob_p2mr_mempool_accepts_spend(&peers, &schnorr_spend, "schnorr")
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "{err}\n\
                 \n\
                 Observed previously: Witness program hash mismatch on control block / leaf \
                 (enforcer overload vs cryptoquick P2MR Core dialect)."
            )
        })?;

    // If Schnorr already passes, still require kitchen-sink (product climax).
    // Spend a fresh funding path: re-use pile only if schnorr didn't consume — it did
    // enter mempool but is unconfirmed; build kitchen-sink from same pile would double-spend
    // in mempool. Fund a second pile for kitchen-sink.
    let pile2 = round0_wallet_to_p2mr(&mut peers).await?;
    let dest = peers.alice.mining_address.script_pubkey();
    let kitchen_spend = build_kitchen_sink_spend_from_prevout(
        &HYBRID_EC_ENTROPY,
        &MLDSA_ENTROPY,
        &SLH_ENTROPY,
        TapSighashType::All,
        pile2.outpoint,
        pile2.txout,
        ROUND_SPEND_OUTPUT,
        dest,
    )
    .map_err(anyhow::Error::msg)?;

    assert_eq!(
        kitchen_spend.input[0].witness.len(),
        5,
        "kitchen-sink witness depth"
    );
    assert_bob_p2mr_mempool_accepts_spend(&peers, &kitchen_spend, "kitchen-sink").await?;

    tracing::info!(
        "Tier B PASS: Bob P2MR mempool accepted enforcer-format Schnorr + kitchen-sink spends"
    );

    drop(peers.alice.tasks);
    drop(peers.bob.tasks);
    tokio::time::sleep(Duration::from_secs(1)).await;
    peers.alice.directories.base_dir.cleanup()?;
    peers.bob.directories.base_dir.cleanup()?;
    Ok(())
}

/// Opt-in Tier B mempool interop trial (**expected FAIL** until fixed).
pub async fn test_bip360_tier_b_p2mr_mempool(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    if !tier_b_opt_in() {
        tracing::info!(
            "skipping {TIER_B_TEST_NAME} (opt-in known-fail / bounty target). \
             Run: BIP360_TIER_B=1 just bip360-tier-b-mempool"
        );
        return Ok(());
    }

    let (res_tx, mut res_rx) = mpsc::unbounded();
    let peers = setup_tier_a_dual_node_peers(bin_paths, &file_registry).await?;
    let _test_task: crate::util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = bip360_tier_b_p2mr_mempool_task(peers).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .instrument(tracing::info_span!("test", name = TIER_B_TEST_NAME))
    })
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("unexpected end of Tier B mempool task result stream"))?
}
