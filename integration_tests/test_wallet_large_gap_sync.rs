//! Regression test for the wallet crossing a gap far larger than it can
//! afford to replay one block at a time.
//!
//! Connecting missing blocks individually costs two node RPCs and a wallet
//! persist per block, and each persist gets slower as the local chain grows,
//! so a wallet that is hundreds of thousands of blocks behind never finishes.
//! Past `MAX_BLOCK_BY_BLOCK_REPLAY` in `lib/wallet/cusf_block_producer.rs`,
//! `sync_wallet_to_tip` must instead advance the local chain with a single
//! checkpoint update and recover its transactions with a full scan against
//! the chain source.
//!
//! The gap is mined while the enforcer is down, and paid to an address the
//! enforcer's own wallet owns, so the coinbases are only discoverable by
//! scanning the chain source. A checkpoint jump that skipped the scan would
//! close the gap but silently lose the money.

use std::time::Duration;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::mainchain::{CreateNewAddressRequest, GetBalanceRequest},
};
use futures::channel::mpsc;
use tokio::time::sleep;

use crate::{
    integration_test::{fund_enforcer, wait_for_wallet_sync},
    setup::{DummySidechain, Mode, Network, PostSetup, PreSetup, SetupOpts},
    util::BinPaths,
};

pub const TEST_NAME: &str = "wallet_large_gap_sync";

/// Must exceed `MAX_BLOCK_BY_BLOCK_REPLAY`, or the wallet takes the
/// block-by-block path and the assertions below fail (loudly, and correctly).
const GAP_BLOCKS: u32 = 2_100;

/// Emitted by `sync_wallet_to_tip` when it takes the checkpoint path.
const CHECKPOINT_LOG: &str = "checkpointing chain forward and running a full scan";

/// Emitted once per block by the block-by-block path.
const REPLAY_LOG: &str = "unable to connect block to bdk_chain";

/// Block until electrs has indexed up to bitcoind's tip.
///
/// The wallet full-scans exactly once, during startup sync, and the test
/// harness runs the enforcer with `--wallet-skip-periodic-sync`, so there is
/// no later retry to paper over a chain source that was still catching up.
async fn wait_for_electrs_tip(post_setup: &PostSetup) -> anyhow::Result<()> {
    const POLL_INTERVAL: Duration = Duration::from_millis(500);
    const TIMEOUT: Duration = Duration::from_secs(180);

    let target_height: u32 = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    let url = format!(
        "http://127.0.0.1:{}/blocks/tip/height",
        post_setup.reserved_ports.electrs_electrum_http.port()
    );
    tracing::debug!("waiting for electrs to index up to block {target_height}");

    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        // electrs returns 5xx while it is still opening its index, so a
        // failed request here is expected rather than fatal.
        let indexed_height: Option<u32> = match client.get(&url).send().await {
            Ok(response) => response
                .text()
                .await
                .ok()
                .and_then(|body| body.trim().parse().ok()),
            Err(_) => None,
        };
        if indexed_height.is_some_and(|height| height >= target_height) {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "electrs did not index up to block {target_height} within {TIMEOUT:?} \
             (stuck at {indexed_height:?})"
        );
        sleep(POLL_INTERVAL).await;
    }
}

/// Concatenate the enforcer's rolling log files.
///
/// The enforcer's `stdout.txt` only holds the run the harness spawned most
/// recently, whereas the rolling log in the data dir spans the restart, which
/// is the run this test needs to make assertions about.
fn read_enforcer_log(post_setup: &PostSetup) -> anyhow::Result<String> {
    let log_dir = post_setup.directories.enforcer_dir.join("logs");
    let mut combined = String::new();
    for entry in std::fs::read_dir(&log_dir)? {
        let path = entry?.path();
        if path.is_file() {
            combined.push_str(&std::fs::read_to_string(&path)?);
        }
    }
    anyhow::ensure!(
        !combined.is_empty(),
        "no enforcer log content found in {}",
        log_dir.display()
    );
    Ok(combined)
}

pub async fn test_wallet_large_gap_sync(bin_paths: BinPaths) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths.clone(), Network::Regtest)?;
    let setup_opts: SetupOpts = Default::default();
    let mut post_setup = pre_setup
        .setup(Mode::Mempool, setup_opts, res_tx.clone())
        .await?;

    fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    let balance_before = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    tracing::info!(?balance_before, "wallet balance before the gap");

    // Owned by the enforcer's wallet, so the gap's coinbases are money the
    // full scan is obliged to find.
    let gap_address = post_setup
        .wallet_service_client
        .create_new_address(CreateNewAddressRequest::default())
        .await?
        .into_owned()
        .address;

    tracing::info!("killing enforcer, then mining {GAP_BLOCKS} blocks behind its back");
    post_setup.kill_enforcer().await?;
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [GAP_BLOCKS.to_string(), gap_address],
        )
        .run_utf8()
        .await?;
    wait_for_electrs_tip(&post_setup).await?;

    tracing::info!("restarting enforcer -- it must checkpoint across the gap, not replay it");
    post_setup
        .restart_enforcer(&bin_paths, Vec::<String>::new(), res_tx.clone())
        .await?;
    wait_for_wallet_sync(&mut post_setup).await?;

    let balance_after = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    tracing::info!(?balance_after, "wallet balance after crossing the gap");
    anyhow::ensure!(
        balance_after.confirmed_sats > balance_before.confirmed_sats,
        "the full scan must recover the coinbases paid to the wallet during the gap, \
         but confirmed_sats did not grow: {balance_after:?} (was {balance_before:?})"
    );

    let enforcer_log = read_enforcer_log(&post_setup)?;
    anyhow::ensure!(
        enforcer_log.contains(CHECKPOINT_LOG),
        "expected the wallet to close a {GAP_BLOCKS}-block gap with a checkpoint and full \
         scan, but {CHECKPOINT_LOG:?} never appeared in the enforcer log. Has \
         MAX_BLOCK_BY_BLOCK_REPLAY been raised above {GAP_BLOCKS}?"
    );
    anyhow::ensure!(
        !enforcer_log.contains("no chain source is configured"),
        "the harness configures a chain source, so the wallet must not have taken the \
         block-by-block fallback"
    );
    // The point of the fix: the gap is crossed in one update, not one block at
    // a time. A handful of these can legitimately show up for the small tail
    // the replay loop still handles, but nothing on the order of the gap.
    let replayed = enforcer_log.matches(REPLAY_LOG).count();
    anyhow::ensure!(
        replayed < GAP_BLOCKS as usize / 10,
        "the wallet replayed {replayed} blocks individually while crossing a \
         {GAP_BLOCKS}-block gap; it should have checkpointed across it instead"
    );
    tracing::info!("crossed a {GAP_BLOCKS}-block gap with {replayed} individual block connects");

    Ok(())
}
