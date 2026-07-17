//! Tier A kitchen-sink demo: stock Core 31 Alice (CUSF) + P2MR Bob (`cryptoquick:p2mr`).
//!
//! Same five-round sat pile as trial #34, but Bob is jbride/bitcoin#2 head so P2MR
//! spends can try the mempool path. Overload kitchen-sink remains the CUSF product
//! format; OP_SUBSTR protocols stay valid elsewhere (see docs/TIER_A_P2MR_ALIGNMENT.md).

#![cfg(feature = "bip360")]

use futures::{StreamExt as _, channel::mpsc};
use tracing::Instrument as _;

use crate::{
    bip360_dual_node::{
        TIER_A_TEST_NAME, round0_wallet_to_p2mr, round1_bob_schnorr_spend,
        round2_alice_hybrid_spend, round3_bob_mldsa_spend, round4_alice_kitchen_sink_spend,
        setup_tier_a_dual_node_peers,
    },
    bip360_tx_report::log_tx_report,
    util::{BinPaths, TestFileRegistry},
};

async fn bip360_kitchen_sink_tier_a_task(
    peers: crate::bip360_dual_node::DualNodePeers,
) -> anyhow::Result<()> {
    let mut peers = peers;
    anyhow::ensure!(
        peers.tier_a,
        "expected Tier A dual peers (Bob = BITCOIND_P2MR / cryptoquick:p2mr)"
    );
    tracing::info!(
        "Tier A dual-node ready: Alice=stock Core+enforcer, Bob=P2MR Core (jbride#2 head)"
    );

    let pile = round0_wallet_to_p2mr(&mut peers).await?;
    tracing::info!(?pile.outpoint, "round 0 complete: wallet → Schnorr P2MR");

    let pile = round1_bob_schnorr_spend(&mut peers, pile).await?;
    tracing::info!(?pile.outpoint, "round 1 complete: Bob Schnorr spend");

    let pile = round2_alice_hybrid_spend(&mut peers, pile).await?;
    tracing::info!(?pile.outpoint, "round 2 complete: Alice hybrid EC+SLH spend");

    let pile = round3_bob_mldsa_spend(&mut peers, pile).await?;
    tracing::info!(?pile.outpoint, "round 3 complete: Bob ML-DSA spend");

    let (final_report, path) = round4_alice_kitchen_sink_spend(&mut peers, pile).await?;
    log_tx_report(4, "tier-a-kitchen-sink", &final_report);
    tracing::info!(
        pqc_algos = ?final_report.pqc_algos,
        weight = final_report.weight,
        ?path,
        "round 4 complete: kitchen-sink triple-algo climax (Tier A)"
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

/// Five-round kitchen-sink Tier A demo on stock Alice + cryptoquick P2MR Bob.
pub async fn test_bip360_kitchen_sink_tier_a(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let peers = setup_tier_a_dual_node_peers(bin_paths, &file_registry).await?;
    let _test_task: crate::util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = bip360_kitchen_sink_tier_a_task(peers).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .instrument(tracing::info_span!("test", name = TIER_A_TEST_NAME))
    })
    .into();
    res_rx.next().await.ok_or_else(|| {
        anyhow::anyhow!("unexpected end of Tier A kitchen-sink task result stream")
    })?
}
