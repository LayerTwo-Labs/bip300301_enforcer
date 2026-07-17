//! Tier B (TB-mine) — CUSF mining path (**PASS**; also in `just it-all`).
//!
//! Soft-fork inventory is included via block assembly + `submitblock` on **stock**
//! Core. The enforcer keeps the tip. No P2MR Core `sendrawtransaction` is required.
//!
//! This is the CUSF-facing Tier B (Sztorc: separate activator, Core untouched).
//! For Bob mempool interop (TB-sendraw, shapes 1+2+3 PASS; kitchen-sink green post-Core 3B) see
//! `bip360_tier_b_p2mr_mempool` (`just bip360-tier-b-mempool`) — not a CUSF tip failure.

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::{bins::CommandExt as _, validator::pqc::signer_dev::SignAlgorithm};
use futures::channel::mpsc;

use crate::{
    bip360_block::{
        SCHNORR_ENTROPY, bip360_setup_opts, build_block_with_coinbase,
        build_valid_kitchen_sink_spend_txs, build_valid_p2mr_spend_txs, funding_prevout_at_height,
        prepare_coinbase, submit_block, wait_for_enforcer_synced,
    },
    bip360_tx_report,
    block_verdict::{Expect, assert_enforcer_verdict},
    setup::{Mode, PreSetup},
};

/// Stock Core tip must equal `block_hash` (not reorged / invalidated away).
async fn ensure_stock_tip(
    post_setup: &crate::setup::PostSetup,
    block_hash: bitcoin::BlockHash,
    label: &str,
) -> anyhow::Result<()> {
    let best = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?;
    anyhow::ensure!(
        best.trim().eq_ignore_ascii_case(&block_hash.to_string()),
        "stock Core tip {} != submitted block {block_hash} ({label})",
        best.trim()
    );
    Ok(())
}

async fn submit_valid_spend_block(
    post_setup: &mut crate::setup::PostSetup,
    label: &str,
    funding_and_spend: (bitcoin::Transaction, bitcoin::Transaction),
) -> anyhow::Result<bitcoin::BlockHash> {
    let (funding_tx, spend_tx) = funding_and_spend;
    bip360_tx_report::log_tx_report(
        0,
        &format!("cusf-mine-{label}"),
        &bip360_tx_report::tx_report(&spend_tx, None),
    );

    let (template, coinbase) = prepare_coinbase(post_setup).await?;
    let block =
        build_block_with_coinbase(post_setup, &template, coinbase, vec![funding_tx, spend_tx])
            .await?;
    let block_hash = submit_block(post_setup, &block).await?;
    tracing::info!(
        %block_hash,
        label,
        path = "cusf_mine",
        "submitted enforcer-format spend via stock Core submitblock (CUSF mining path)"
    );

    // Immediate stock tip check (pre-enforcer).
    ensure_stock_tip(post_setup, block_hash, label).await?;

    // Enforcer keeps tip: Accepted means still on active chain and enforcer height
    // past the block (no invalidateblock).
    assert_enforcer_verdict(
        post_setup,
        block_hash,
        Expect::Accepted,
        Duration::from_secs(15),
    )
    .await?;

    // Re-check tip after enforcer processing — proves stock tip retained post-verdict
    // (not invalidated after the initial getbestblockhash).
    ensure_stock_tip(post_setup, block_hash, &format!("{label}-post-enforcer")).await?;
    Ok(block_hash)
}

/// Stock Core + enforcer: Schnorr then kitchen-sink spends via `submitblock` only.
pub async fn test_bip360_tier_b_cusf_miner(pre_setup: PreSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut post_setup = pre_setup
        .setup(Mode::NoMempool, bip360_setup_opts(), res_tx)
        .await?;

    wait_for_enforcer_synced(&mut post_setup).await?;

    // 1) Schnorr-only script-path spend (minimal enforcer protocol leaf).
    // Distinct coinbases at heights 1 and 2 so the two funds never double-spend each other.
    // After this submitblock the tip advances; kitchen-sink then funds from height 2 on the
    // longer chain (not a claim that height 2 is immature until Schnorr alone).
    let prevout = funding_prevout_at_height(&post_setup, 1).await?;
    let schnorr_txs = build_valid_p2mr_spend_txs(
        &post_setup,
        prevout,
        SignAlgorithm::Schnorr,
        &SCHNORR_ENTROPY,
    )
    .await?;
    let _ = submit_valid_spend_block(&mut post_setup, "schnorr", schnorr_txs).await?;

    // 2) Kitchen-sink triple-algo spend (product climax), funded from the height-2 coinbase.
    let prevout = funding_prevout_at_height(&post_setup, 2).await?;
    let kitchen_txs = build_valid_kitchen_sink_spend_txs(&post_setup, prevout).await?;
    bip360_tx_report::assert_kitchen_sink_spend_shape(&kitchen_txs.1)?;
    let _ = submit_valid_spend_block(&mut post_setup, "kitchen-sink", kitchen_txs).await?;

    tracing::info!(
        path = "cusf_mine",
        "Tier B CUSF mining path PASS: stock Core mined enforcer Schnorr + kitchen-sink; tip retained"
    );
    Ok(())
}
