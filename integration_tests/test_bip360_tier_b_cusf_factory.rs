//! Tier B (TB-factory) — dual-process CUSF mining: second stock bitcoind as block factory.
//!
//! **Alice** = user-facing tip authority + bip360 enforcer (stock Core).
//! **Miner** = second stock bitcoind that only assembles/submits blocks.
//!
//! Same spend shapes as TB-mine (Schnorr then kitchen-sink via enforcer-format witnesses).
//! Blocks are produced by Miner (`submitblock`); Alice receives tip via P2P and keeps it
//! (enforcer `Expect::Accepted`, no invalidate). Both nodes are Unpatched (no `BITCOIND_P2MR`).
//!
//! Dual-process demo — **not** in `just it-all` (slower than single-node TB-mine).
//! Single-process twin: `bip360_tier_b_cusf_miner` / `just bip360-tier-b-cusf`.
//! Docs: `docs/TIER_B_CUSF_MINER.md` § dual-process factory.

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::{bins::CommandExt as _, validator::pqc::signer_dev::SignAlgorithm};
use futures::{StreamExt as _, channel::mpsc};
use tracing::Instrument as _;

use crate::{
    bip360_block::{
        SCHNORR_ENTROPY, build_block_with_coinbase, build_valid_kitchen_sink_spend_txs,
        build_valid_p2mr_spend_txs, funding_prevout_at_height, prepare_coinbase, submit_block,
        wait_for_bitcoind_height, wait_for_enforcer_synced,
    },
    bip360_dual_node::{TIER_B_FACTORY_TEST_NAME, setup_cusf_factory_peers},
    bip360_tx_report,
    block_verdict::{Expect, assert_enforcer_verdict},
    setup::PostSetup,
    util::{BinPaths, TestFileRegistry},
};

/// Alice stock tip must equal `block_hash` (not reorged / invalidated away).
async fn ensure_alice_tip(
    alice: &PostSetup,
    block_hash: bitcoin::BlockHash,
    label: &str,
) -> anyhow::Result<()> {
    let best = alice
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?;
    anyhow::ensure!(
        best.trim().eq_ignore_ascii_case(&block_hash.to_string()),
        "Alice tip {} != submitted block {block_hash} ({label})",
        best.trim()
    );
    Ok(())
}

/// Miner builds + `submitblock`; Alice receives tip via P2P; enforcer keeps tip.
async fn miner_submit_valid_spend_block(
    alice: &mut PostSetup,
    miner: &mut PostSetup,
    label: &str,
    funding_and_spend: (bitcoin::Transaction, bitcoin::Transaction),
) -> anyhow::Result<bitcoin::BlockHash> {
    let (funding_tx, spend_tx) = funding_and_spend;
    bip360_tx_report::log_tx_report(
        0,
        &format!("cusf-factory-{label}"),
        &bip360_tx_report::tx_report(&spend_tx, None),
    );

    // Block factory: Miner assembles and submits (not Alice alone).
    let (template, coinbase) = prepare_coinbase(miner).await?;
    let block =
        build_block_with_coinbase(miner, &template, coinbase, vec![funding_tx, spend_tx]).await?;
    let block_hash = submit_block(miner, &block).await?;
    tracing::info!(
        %block_hash,
        label,
        path = "cusf_mine_factory",
        "Miner submitted enforcer-format spend via stock Core submitblock (dual-process factory)"
    );

    let miner_height = miner
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse::<u32>()?;
    wait_for_bitcoind_height(alice, miner_height).await?;

    // Immediate Alice tip check (pre-enforcer settlement).
    ensure_alice_tip(alice, block_hash, label).await?;

    assert_enforcer_verdict(alice, block_hash, Expect::Accepted, Duration::from_secs(30)).await?;

    // Re-check tip after enforcer — proves Alice tip retained post-verdict.
    ensure_alice_tip(alice, block_hash, &format!("{label}-post-enforcer")).await?;
    Ok(block_hash)
}

async fn bip360_tier_b_cusf_factory_body(
    alice: &mut PostSetup,
    miner: &mut PostSetup,
) -> anyhow::Result<()> {
    tracing::info!(
        path = "cusf_mine_factory",
        "TB-factory dual-process ready: Alice=tip+enforcer, Miner=block factory (both stock)"
    );

    wait_for_enforcer_synced(alice).await?;

    // 1) Schnorr-only script-path spend (minimal enforcer protocol leaf).
    // Distinct coinbases at heights 1 and 2 so the two funds never double-spend each other.
    // Funding txs signed with Alice's wallet (owns matured coinbases); Miner mines.
    let prevout = funding_prevout_at_height(alice, 1).await?;
    let schnorr_txs =
        build_valid_p2mr_spend_txs(alice, prevout, SignAlgorithm::Schnorr, &SCHNORR_ENTROPY)
            .await?;
    let _ = miner_submit_valid_spend_block(alice, miner, "schnorr", schnorr_txs).await?;

    // 2) Kitchen-sink triple-algo spend, funded from the height-2 coinbase.
    let prevout = funding_prevout_at_height(alice, 2).await?;
    let kitchen_txs = build_valid_kitchen_sink_spend_txs(alice, prevout).await?;
    bip360_tx_report::assert_kitchen_sink_spend_shape(&kitchen_txs.1)?;
    let _ = miner_submit_valid_spend_block(alice, miner, "kitchen-sink", kitchen_txs).await?;

    tracing::info!(
        path = "cusf_mine_factory",
        "Tier B CUSF dual-process factory PASS: Miner produced Schnorr + kitchen-sink; Alice tip retained"
    );
    Ok(())
}

async fn bip360_tier_b_cusf_factory_task(
    peers: crate::bip360_dual_node::DualNodePeers,
) -> anyhow::Result<()> {
    let mut alice = peers.alice;
    let mut miner = peers.bob;

    // Run body first; always drop child tasks so bitcoind/enforcer die on error paths too.
    let result = bip360_tier_b_cusf_factory_body(&mut alice, &mut miner).await;

    tracing::info!(
        "Removing {}, {}",
        alice.directories.base_dir.path().display(),
        miner.directories.base_dir.path().display()
    );
    drop(alice.tasks);
    drop(miner.tasks);
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Best-effort dir cleanup on both success and failure paths.
    drop(alice.directories.base_dir.cleanup());
    drop(miner.directories.base_dir.cleanup());
    result
}

/// Dual stock nodes: Miner `submitblock` of enforcer Schnorr + kitchen-sink; Alice tip kept.
pub async fn test_bip360_tier_b_cusf_factory(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let peers = setup_cusf_factory_peers(bin_paths, &file_registry).await?;
    let _test_task: crate::util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = bip360_tier_b_cusf_factory_task(peers).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .instrument(tracing::info_span!("test", name = TIER_B_FACTORY_TEST_NAME))
    })
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("unexpected end of TB-factory task result stream"))?
}
