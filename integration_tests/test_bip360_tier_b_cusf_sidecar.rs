//! Tier B (TB-sidecar) — CUSF miner sidecar library path (**PASS**).
//!
//! Same kitchen-sink bar as TB-mine, but spends are held in the sidecar inventory
//! and mined via `cusf_miner_sidecar::MinerSidecar` (GBT + assemble + `submitblock`
//! to stock Core). Enforcer keeps the tip.
//!
//! **Not** in `just it-all` (optional demo twin of TB-mine; green matrix stays 34).
//! **Not** greening TB-sendraw / stock mempool for v2 spends.
//!
//! Docs: `docs/TIER_B_CUSF_SIDECAR.md`. Just: `just bip360-tier-b-cusf-sidecar`.

#![cfg(feature = "bip360")]

use std::{net::SocketAddr, time::Duration};

use bip300301_enforcer_lib::{bins::CommandExt as _, validator::pqc::signer_dev::SignAlgorithm};
use bitcoin::consensus::encode::serialize_hex;
use bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient;
use cusf_miner_sidecar::{MinerSidecar, RpcConfig, connect_bitcoind};
use futures::channel::mpsc;

use crate::{
    bip360_block::{
        SCHNORR_ENTROPY, bip360_setup_opts, build_valid_kitchen_sink_spend_txs,
        build_valid_p2mr_spend_txs, funding_prevout_at_height, wait_for_enforcer_synced,
    },
    bip360_tx_report,
    block_verdict::{Expect, assert_enforcer_verdict},
    setup::{Mode, PostSetup, PreSetup},
};

/// Build a bitcoind JSON-RPC client from harness credentials.
fn bitcoind_client(post_setup: &PostSetup) -> anyhow::Result<HttpClient> {
    let cli = &post_setup.bitcoin_cli;
    let host = cli.rpc_host.as_str();
    let port = cli.rpc_port;
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| anyhow::anyhow!("bitcoind RPC addr {host}:{port}: {e}"))?;
    let user = cli
        .rpc_user
        .clone()
        .ok_or_else(|| anyhow::anyhow!("missing bitcoind rpc_user"))?;
    let pass = cli
        .rpc_pass
        .clone()
        .ok_or_else(|| anyhow::anyhow!("missing bitcoind rpc_pass"))?;
    Ok(connect_bitcoind(&RpcConfig::new(addr, user, pass))?)
}

fn new_sidecar(post_setup: &PostSetup) -> anyhow::Result<MinerSidecar> {
    let client = bitcoind_client(post_setup)?;
    Ok(MinerSidecar::new(
        client,
        post_setup.mining_address.script_pubkey(),
    ))
}

/// Stock Core tip must equal `block_hash` (not reorged / invalidated away).
async fn ensure_stock_tip(
    post_setup: &PostSetup,
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

/// Push funding+spend into sidecar inventory and mine via library (in-process).
async fn sidecar_submit_valid_spend_block(
    post_setup: &mut PostSetup,
    sidecar: &MinerSidecar,
    label: &str,
    funding_and_spend: (bitcoin::Transaction, bitcoin::Transaction),
) -> anyhow::Result<bitcoin::BlockHash> {
    let (funding_tx, spend_tx) = funding_and_spend;
    bip360_tx_report::log_tx_report(
        0,
        &format!("cusf-sidecar-{label}"),
        &bip360_tx_report::tx_report(&spend_tx, None),
    );

    sidecar.clear();
    let fund_id = sidecar.add_tx_hex(&serialize_hex(&funding_tx))?;
    let spend_id = sidecar.add_tx_hex(&serialize_hex(&spend_tx))?;
    anyhow::ensure!(
        sidecar.len() == 2,
        "sidecar inventory expected 2 txs (got {})",
        sidecar.len()
    );
    tracing::info!(
        %fund_id,
        %spend_id,
        label,
        path = "cusf_sidecar",
        "loaded enforcer-format funding+spend into sidecar inventory"
    );

    let result = sidecar.mine().await?;
    tracing::info!(
        block_hash = %result.block_hash,
        height = result.height,
        tx_count = result.tx_count,
        label,
        path = "cusf_sidecar",
        "sidecar mined enforcer-format spend via stock Core submitblock"
    );

    anyhow::ensure!(
        result.tx_count == 2,
        "sidecar mine tx_count must be 2 (funding+spend; got {})",
        result.tx_count
    );
    anyhow::ensure!(
        sidecar.is_empty(),
        "sidecar inventory should be empty after successful mine (taken set not restored)"
    );

    // Immediate stock tip check (pre-enforcer).
    ensure_stock_tip(post_setup, result.block_hash, label).await?;

    assert_enforcer_verdict(
        post_setup,
        result.block_hash,
        Expect::Accepted,
        Duration::from_secs(15),
    )
    .await?;

    // Re-check tip after enforcer processing.
    ensure_stock_tip(
        post_setup,
        result.block_hash,
        &format!("{label}-post-enforcer"),
    )
    .await?;
    Ok(result.block_hash)
}

/// Stock Core + enforcer: Schnorr then kitchen-sink spends via sidecar inventory mine.
pub async fn test_bip360_tier_b_cusf_sidecar(pre_setup: PreSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut post_setup = pre_setup
        .setup(Mode::NoMempool, bip360_setup_opts(), res_tx)
        .await?;

    wait_for_enforcer_synced(&mut post_setup).await?;
    let sidecar = new_sidecar(&post_setup)?;

    // 1) Schnorr-only script-path spend.
    let prevout = funding_prevout_at_height(&post_setup, 1).await?;
    let schnorr_txs = build_valid_p2mr_spend_txs(
        &post_setup,
        prevout,
        SignAlgorithm::Schnorr,
        &SCHNORR_ENTROPY,
    )
    .await?;
    let _ =
        sidecar_submit_valid_spend_block(&mut post_setup, &sidecar, "schnorr", schnorr_txs).await?;

    // 2) Kitchen-sink triple-algo spend from height-2 coinbase.
    let prevout = funding_prevout_at_height(&post_setup, 2).await?;
    let kitchen_txs = build_valid_kitchen_sink_spend_txs(&post_setup, prevout).await?;
    bip360_tx_report::assert_kitchen_sink_spend_shape(&kitchen_txs.1)?;
    let _ =
        sidecar_submit_valid_spend_block(&mut post_setup, &sidecar, "kitchen-sink", kitchen_txs)
            .await?;

    tracing::info!(
        path = "cusf_sidecar",
        "Tier B CUSF sidecar PASS: stock Core mined enforcer Schnorr + kitchen-sink via inventory; tip retained"
    );
    Ok(())
}
