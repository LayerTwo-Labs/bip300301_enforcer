//! Dual-node P2P mempool E2E: five rounds on one sat pile (rounds 0–4).

#![cfg(feature = "bip360")]

use futures::{StreamExt as _, channel::mpsc};
use tracing::Instrument as _;

use crate::{
    bip360_dual_node::{
        TEST_NAME, round0_wallet_to_p2mr, round1_bob_schnorr_spend, round2_alice_hybrid_spend,
        round3_bob_mldsa_spend, round4_alice_kitchen_sink_spend, setup_dual_node_peers,
    },
    bip360_tx_report::log_tx_report,
    util::{BinPaths, TestFileRegistry},
};

async fn bip360_p2p_mempool_e2e_task(
    peers: crate::bip360_dual_node::DualNodePeers,
) -> anyhow::Result<()> {
    let mut peers = peers;
    tracing::info!("dual-node peers ready");

    let pile = round0_wallet_to_p2mr(&mut peers).await?;
    tracing::info!(?pile.outpoint, "round 0 complete: wallet → Schnorr P2MR");

    let pile = round1_bob_schnorr_spend(&mut peers, pile).await?;
    tracing::info!(?pile.outpoint, "round 1 complete: Bob Schnorr spend");

    let pile = round2_alice_hybrid_spend(&mut peers, pile).await?;
    tracing::info!(?pile.outpoint, "round 2 complete: Alice hybrid EC+SLH spend");

    let pile = round3_bob_mldsa_spend(&mut peers, pile).await?;
    tracing::info!(?pile.outpoint, "round 3 complete: Bob ML-DSA spend");

    let final_report = round4_alice_kitchen_sink_spend(&mut peers, pile).await?;
    log_tx_report(4, "alice-mempool", &final_report);
    tracing::info!(
        pqc_algos = ?final_report.pqc_algos,
        "round 4 complete: Alice kitchen-sink triple-algo spend"
    );

    tracing::info!(
        "Removing {}, {}",
        peers.alice.directories.base_dir.path().display(),
        peers.bob.directories.base_dir.path().display()
    );
    drop(peers.alice.tasks);
    drop(peers.bob.tasks);
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    peers.alice.directories.base_dir.cleanup()?;
    peers.bob.directories.base_dir.cleanup()?;
    Ok(())
}

/// Five-round dual-node P2P mempool E2E on a single sat pile.
pub async fn test_bip360_p2p_mempool_e2e(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let peers = setup_dual_node_peers(bin_paths, &file_registry).await?;
    let _test_task: crate::util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = bip360_p2p_mempool_e2e_task(peers).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .instrument(tracing::info_span!("test", name = TEST_NAME))
    })
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("unexpected end of P2P E2E task result stream"))?
}
