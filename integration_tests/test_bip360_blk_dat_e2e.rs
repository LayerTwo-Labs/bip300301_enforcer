//! E2E: Bob mines a P2MR spend block; Alice (stock + enforcer) accepts the tip;
//! Alice's on-disk `blk*.dat` entry must match the block Bob submitted.
//!
//! Flow:
//! 1. Tier A dual peers: Alice = stock Core 31 + bip360 enforcer; Bob = `BITCOIND_P2MR`.
//! 2. Fund a Schnorr P2MR UTXO (wallet → P2MR; confirmed on both nodes).
//! 3. Build an enforcer-format Schnorr spend on the pile.
//! 4. Prefer Bob mempool (`sendraw`); always assemble + `submitblock` on **Bob**,
//!    capturing the exact `Block` Bob produced.
//! 5. Wait until Alice's tip hash matches and Alice's enforcer accepts (no invalidate).
//! 6. Parse Alice's `regtest/blocks/blk*.dat` (XOR-aware) and require the stored
//!    block to consensus-serialize identically to Bob's mined block.
//!
//! Needs `BITCOIND_P2MR` + stock Core (`just setup-core` / `just setup-p2mr`).
//! Opt-in recipe: `just bip360-blk-dat-e2e` (not in green `it-all` matrix).
//! Drivechain counterpart: `drivechain_blk_dat_e2e`.

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    validator::pqc::signer_dev::{
        SignAlgorithm, build_p2mr_spend_from_prevout, p2mr_output_for_hybrid_ec_slh,
    },
};
use bitcoin::{Network, sighash::TapSighashType};
use futures::{StreamExt as _, channel::mpsc};
use tracing::Instrument as _;

use crate::{
    bip360_block::{
        HYBRID_EC_ENTROPY, SCHNORR_ENTROPY, SLH_ENTROPY, build_block_from_template, submit_block,
        wait_for_bitcoind_height, wait_for_enforcer_synced,
    },
    bip360_dual_node::{
        DualNodePeers, ROUND_SPEND_OUTPUT, round0_wallet_to_p2mr, send_raw_to_node,
        setup_tier_a_dual_node_peers,
    },
    bip360_tx_report,
    blk_dat::{assert_blocks_equal, blocks_dir, ensure_tip, rpc_get_block, wait_for_block_on_disk},
    block_verdict::{Expect, assert_enforcer_verdict},
    util::{BinPaths, TestFileRegistry},
};

/// Trial name for libtest-mimic / `just it`.
pub const TEST_NAME: &str = "bip360_blk_dat_e2e";

async fn bip360_blk_dat_e2e_task(mut peers: DualNodePeers) -> anyhow::Result<()> {
    anyhow::ensure!(
        peers.tier_a,
        "expected Tier A dual peers (Alice=stock, Bob=BITCOIND_P2MR)"
    );

    let pile = round0_wallet_to_p2mr(&mut peers).await?;
    tracing::info!(?pile.outpoint, "funded Schnorr P2MR pile");

    let (next_spk, _) = p2mr_output_for_hybrid_ec_slh(&HYBRID_EC_ENTROPY, &SLH_ENTROPY)
        .map_err(anyhow::Error::msg)?;
    let spend_tx = build_p2mr_spend_from_prevout(
        SignAlgorithm::Schnorr,
        &SCHNORR_ENTROPY,
        TapSighashType::All,
        pile.outpoint,
        pile.txout.clone(),
        ROUND_SPEND_OUTPUT,
        next_spk,
    )
    .map_err(anyhow::Error::msg)?;
    let spend_txid = spend_tx.compute_txid();
    bip360_tx_report::log_tx_report(
        0,
        "blk-dat-e2e-spend",
        &bip360_tx_report::tx_report(&spend_tx, None),
    );

    match send_raw_to_node(&peers.bob, &spend_tx).await {
        Ok(()) => {
            if bip360_tx_report::wait_for_mempool_entry(
                &peers.bob,
                spend_txid,
                Duration::from_secs(15),
            )
            .await
            .is_ok()
            {
                tracing::info!(%spend_txid, "Bob mempool holds spend before mine");
            } else {
                tracing::warn!(%spend_txid, "sendraw ok but getmempoolentry timed out; mining via submitblock");
            }
        }
        Err(err) => {
            tracing::warn!(
                %spend_txid,
                error = %format!("{err:#}"),
                "Bob mempool rejected spend; mining via submitblock on Bob"
            );
        }
    }

    let bob_block = build_block_from_template(&mut peers.bob, vec![spend_tx.clone()]).await?;
    let block_hash = submit_block(&mut peers.bob, &bob_block).await?;
    anyhow::ensure!(
        bob_block.block_hash() == block_hash,
        "Bob block_hash() {} != submit_block returned {block_hash}",
        bob_block.block_hash()
    );
    anyhow::ensure!(
        bob_block
            .txdata
            .iter()
            .any(|tx| tx.compute_txid() == spend_txid),
        "Bob mined block missing spend txid {spend_txid}"
    );
    let bob_height = {
        let h: u32 = peers
            .bob
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "getblockcount", [])
            .run_utf8()
            .await?
            .trim()
            .parse()?;
        h
    };
    ensure_tip(&peers.bob, block_hash, "Bob").await?;
    tracing::info!(%block_hash, height = bob_height, %spend_txid, "Bob mined spend block");

    wait_for_bitcoind_height(&peers.alice, bob_height).await?;
    ensure_tip(&peers.alice, block_hash, "Alice").await?;
    wait_for_enforcer_synced(&mut peers.alice).await?;
    assert_enforcer_verdict(
        &mut peers.alice,
        block_hash,
        Expect::Accepted,
        Duration::from_secs(30),
    )
    .await?;
    ensure_tip(&peers.alice, block_hash, "Alice-after-enforcer").await?;

    let alice_blocks_dir = blocks_dir(&peers.alice);
    let alice_disk_block =
        wait_for_block_on_disk(&alice_blocks_dir, block_hash, Network::Regtest).await?;
    assert_blocks_equal("bob_mined", &bob_block, "alice_disk", &alice_disk_block)?;

    // Optional: Bob RPC and Bob disk should also match the mined object.
    let bob_rpc = rpc_get_block(&peers.bob, block_hash).await?;
    assert_blocks_equal("bob_mined", &bob_block, "bob_rpc", &bob_rpc)?;
    let bob_disk =
        wait_for_block_on_disk(&blocks_dir(&peers.bob), block_hash, Network::Regtest).await?;
    assert_blocks_equal("bob_mined", &bob_block, "bob_disk", &bob_disk)?;

    anyhow::ensure!(
        alice_disk_block
            .txdata
            .iter()
            .any(|tx| tx.compute_txid() == spend_txid),
        "Alice disk block missing spend {spend_txid}"
    );

    tracing::info!(
        %block_hash,
        %spend_txid,
        alice_blocks_dir = %alice_blocks_dir.display(),
        "PASS: Bob mined block == Alice blk*.dat (enforcer kept tip)"
    );

    drop(peers.alice.tasks);
    drop(peers.bob.tasks);
    tokio::time::sleep(Duration::from_secs(1)).await;
    peers.alice.directories.base_dir.cleanup()?;
    peers.bob.directories.base_dir.cleanup()?;
    Ok(())
}

/// E2E: Bob-mined P2MR block appears unaltered in Alice's `blk*.dat`.
pub async fn test_bip360_blk_dat_e2e(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let peers = setup_tier_a_dual_node_peers(bin_paths, &file_registry).await?;
    let _test_task: crate::util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = bip360_blk_dat_e2e_task(peers).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .instrument(tracing::info_span!("test", name = TEST_NAME))
    })
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("unexpected end of blk.dat e2e task result stream"))?
}
