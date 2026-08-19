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
//!
//! Part of the gap is paid to an address at a derivation index the wallet has
//! never revealed, separated from the revealed ones by a run of unused
//! indices. Addresses are not used in strictly increasing order in general --
//! a recovered seed may come from a wallet that hands out addresses without
//! requiring them to be used -- so a scan that searches for the first unused
//! index and stops there silently drops everything funded after the gap. Only
//! a scan that keeps going for a stop gap past the last used index finds it.
//!
//! The gap is then crossed a second time with the sync backend dark. The
//! checkpoint-and-scan path needs the chain source, but a missing backend
//! must degrade the wallet, not park the enforcer: the wallet falls back to
//! block-by-block replay, exactly as if no sync source were configured,
//! while the enforcer keeps serving gRPC and enforcing new blocks, and the
//! backend is still picked up in the background once it returns. The replay
//! only recovers addresses the wallet has revealed -- money on unrevealed
//! indices stays invisible until a scan -- which is the same trade-off
//! `--wallet-sync-source disabled` already accepts.
//!
//! (The chain source client used to be initialized inline during wallet
//! construction, inside a retry loop that only returned once the backend was
//! reachable. Nothing else -- the validator sync, the gRPC server, the
//! mempool task -- is spawned until the wallet exists, so a dark electrs held
//! the whole process hostage: no port was ever bound, and the enforcer looked
//! simply hung. The dark restarts here pin the fix: the gRPC port must open
//! inside `restart_enforcer`'s wait.)
//!
//! Finally, a *small* gap is crossed with the backend still dark: within
//! `MAX_BLOCK_BY_BLOCK_REPLAY` the wallet never wants the chain source in
//! the first place -- it catches up by connecting blocks against Bitcoin
//! Core alone, taking the replay path by choice rather than as a fallback,
//! with no checkpoint and no fallback warn.
//!
//! Last, the same recovery is driven by an operator rather than by a gap.
//! Everything above reaches `full_scan` through `sync_wallet_to_tip`, which
//! only gets there by falling far enough behind; the `FullScan` RPC asks for
//! that scan on a wallet that is perfectly in sync. That is what a restored
//! seed needs, and it is what the `--wallet-full-scan` startup flag used to
//! cover -- as an RPC, without a restart. The obligation is the same one the
//! gap crossings carry: money at an index the wallet never revealed.

use std::{str::FromStr as _, time::Duration};

use bdk_wallet::miniscript::{Descriptor, DescriptorPublicKey};
use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        mainchain::{
            CreateNewAddressRequest, FullScanRequest, FullScanResponse, GetBalanceRequest,
            GetInfoRequest, ListUnspentOutputsRequest, get_info_response,
        },
        unwrap_string,
    },
};
use bitcoin::{BlockHash, secp256k1::Secp256k1};
use futures::channel::mpsc;
use tokio::time::sleep;

use crate::{
    integration_test::{fund_enforcer, wait_for_validator_tip, wait_for_wallet_sync},
    setup::{
        DummySidechain, Mode, Network, PostSetup, PreSetup, SetupOpts, read_enforcer_log,
        wait_for_enforcer_log,
    },
    util::BinPaths,
};

pub const TEST_NAME: &str = "wallet_large_gap_sync";

/// Must exceed `MAX_BLOCK_BY_BLOCK_REPLAY`, or the wallet takes the
/// block-by-block path and the assertions below fail (loudly, and correctly).
const GAP_BLOCKS: u32 = 2_100;

/// External keychain index of the address funded behind a derivation gap.
///
/// Must sit past BDK's default lookahead of 25: within the lookahead the
/// indexer already holds the SPK, so block replay would match the coinbase
/// without the scan having had to discover the index, and the test would pass
/// for the wrong reason. Must also sit comfortably inside the full scan's stop
/// gap, which is the reach this test pins down.
const DERIVATION_GAP_INDEX: u32 = 50;

/// How much of the gap is paid to [`DERIVATION_GAP_INDEX`]. Mined first, so
/// these coinbases are far past maturity once the gap is closed.
const GAP_INDEX_BLOCKS: u32 = 10;

/// The rest of the gap, paid to the wallet's next unused (revealed) address.
const SEQUENTIAL_GAP_BLOCKS: u32 = GAP_BLOCKS - GAP_INDEX_BLOCKS;

/// Mined after the enforcer restarts against a dark backend. Small: these are
/// processed one `connect_block` at a time, and what they prove is that the
/// block pipeline is running at all while the gap remains uncrossed.
const LIVENESS_BLOCKS: u32 = 5;

/// Mined behind the enforcer's back for the small-gap phase. Enough that the
/// balance assertion sees movement, small enough to stay far inside
/// `MAX_BLOCK_BY_BLOCK_REPLAY` so the wallet catches up by connecting blocks
/// without ever wanting the chain source.
const SMALL_GAP_BLOCKS: u32 = 10;

/// External keychain index the RPC-driven scan has to reach.
///
/// Past everything the gap crossings revealed: those leave the external
/// keychain revealed up to [`DERIVATION_GAP_INDEX`] plus BDK's lookahead, so a
/// smaller index would be matched by block connection alone and the scan would
/// be proving nothing. Still comfortably inside the scan's stop gap measured
/// from the last *used* index, which is the reach being exercised.
const RPC_SCAN_INDEX: u32 = 150;

/// How many blocks are paid to [`RPC_SCAN_INDEX`]. Only has to be enough to
/// tell "found them all" from "found some".
const RPC_SCAN_BLOCKS: u32 = 3;

/// Emitted by `sync_wallet_to_tip` when it takes the checkpoint path.
const CHECKPOINT_LOG: &str = "checkpointing chain forward and running a full scan";

/// Emitted once per block by the block-by-block path.
const REPLAY_LOG: &str = "unable to connect block to bdk_chain";

/// Emitted (warn) by `sync_wallet_to_tip` when it is too far behind to replay
/// block-by-block but has no chain source client to checkpoint and scan with,
/// and falls back to the replay anyway.
const FALLBACK_LOG: &str = "no chain source is available";

/// Emitted (warn) at startup when the first connection attempt to the sync
/// backend fails with a transient error and the retrying moves to the
/// background task.
const BACKEND_UNREACHABLE_LOG: &str = "wallet sync backend not reachable at startup";

/// Emitted (info) by the background init task once the backend is reached.
const BACKEND_AVAILABLE_LOG: &str = "wallet sync backend became available";

/// Block until electrs has indexed up to bitcoind's tip.
///
/// The test harness runs the enforcer with `--wallet-skip-periodic-sync`, so
/// each full scan runs exactly once, at the point the test drives it, with no
/// later retry to paper over a chain source that was still catching up. Every
/// scan must therefore be sequenced after this wait.
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

/// Number of confirmed wallet UTXOs paying `address`.
async fn confirmed_utxos_at(post_setup: &mut PostSetup, address: &str) -> anyhow::Result<usize> {
    let utxos = post_setup
        .wallet_service_client
        .list_unspent_outputs(ListUnspentOutputsRequest::default())
        .await?
        .into_owned();
    Ok(utxos
        .outputs
        .into_iter()
        .filter(|output| output.is_confirmed)
        .filter_map(|output| unwrap_string(output.address))
        .filter(|found| found == address)
        .count())
}

/// The wallet's own chain tip, as it reports it.
async fn wallet_tip(post_setup: &mut PostSetup) -> anyhow::Result<BlockHash> {
    post_setup
        .wallet_service_client
        .get_info(GetInfoRequest::default())
        .await?
        .into_owned()
        .tip
        .into_option()
        .and_then(|tip| tip.hash.into_option())
        .ok_or_else(|| anyhow::anyhow!("expected `tip.hash` in GetInfoResponse"))?
        .decode::<get_info_response::Tip, BlockHash>("hash")
        .map_err(anyhow::Error::from)
}

/// Drive a full scan over gRPC, returning the tip it reports.
async fn request_full_scan(post_setup: &mut PostSetup) -> anyhow::Result<BlockHash> {
    post_setup
        .wallet_service_client
        .full_scan(FullScanRequest::default())
        .await?
        .into_owned()
        .tip_hash
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("expected `tip_hash` in FullScanResponse"))?
        .decode::<FullScanResponse, BlockHash>("tip_hash")
        .map_err(anyhow::Error::from)
}

/// Derive an address from the wallet's own external descriptor without
/// revealing it.
///
/// `CreateNewAddress` is `next_unused_address`, so it can only ever hand out a
/// contiguous run of indices -- it returns the same address until that address
/// is used. To fund an address sitting behind a derivation gap, the test has to
/// derive it itself from the public descriptor the wallet reports.
async fn peek_external_address(post_setup: &mut PostSetup, index: u32) -> anyhow::Result<String> {
    let descriptors = post_setup
        .wallet_service_client
        .get_info(GetInfoRequest::default())
        .await?
        .into_owned()
        .descriptors;
    let external = descriptors.get("external").ok_or_else(|| {
        anyhow::anyhow!("wallet reported no external descriptor, only: {descriptors:?}")
    })?;
    let address = Descriptor::<DescriptorPublicKey>::from_str(external)?
        .derived_descriptor(&Secp256k1::verification_only(), index)?
        .address(post_setup.network.into())?;
    Ok(address.to_string())
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

    // Also owned by the wallet, but never revealed by it, and separated from
    // `gap_address` by a run of unused indices. Nothing short of a scan that
    // keeps looking past the first unused index will ever ask about it.
    let deep_gap_address = peek_external_address(&mut post_setup, DERIVATION_GAP_INDEX).await?;
    anyhow::ensure!(
        deep_gap_address != gap_address,
        "index {DERIVATION_GAP_INDEX} derived the same address the wallet just revealed \
         ({gap_address}), so there is no derivation gap to test"
    );

    tracing::info!("killing enforcer, then mining {GAP_BLOCKS} blocks behind its back");
    post_setup.kill_enforcer().await?;
    // Mined at the start of the gap, so these are well past coinbase maturity
    // by the time the wallet comes back and counts them.
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [GAP_INDEX_BLOCKS.to_string(), deep_gap_address.clone()],
        )
        .run_utf8()
        .await?;
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [SEQUENTIAL_GAP_BLOCKS.to_string(), gap_address],
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

    let enforcer_log = read_enforcer_log(&post_setup.directories.enforcer_dir)?;
    anyhow::ensure!(
        enforcer_log.contains(CHECKPOINT_LOG),
        "expected the wallet to close a {GAP_BLOCKS}-block gap with a checkpoint and full \
         scan, but {CHECKPOINT_LOG:?} never appeared in the enforcer log. Has \
         MAX_BLOCK_BY_BLOCK_REPLAY been raised above {GAP_BLOCKS}?"
    );
    anyhow::ensure!(
        !enforcer_log.contains(FALLBACK_LOG),
        "the harness configures a reachable chain source, so the wallet must not have \
         taken the block-by-block fallback"
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

    // Checked last, so that a wallet which never ran a scan at all is
    // diagnosed by the assertions above rather than by this one. The balance
    // check cannot tell a correct scan from one that stops at the first unused
    // index -- `gap_address` is the wallet's own next unused address, so its
    // coinbases are found either way. These are the ones that are only
    // reachable by continuing past the gap.
    let deep_gap_utxos = confirmed_utxos_at(&mut post_setup, &deep_gap_address).await?;
    anyhow::ensure!(
        deep_gap_utxos == GAP_INDEX_BLOCKS as usize,
        "the full scan must recover the {GAP_INDEX_BLOCKS} coinbases paid to external index \
         {DERIVATION_GAP_INDEX} ({deep_gap_address}), which sits behind a run of unused \
         indices, but the wallet holds {deep_gap_utxos} confirmed UTXOs there. A scan that \
         stops at the first unused index never looks that far, and the money is silently lost"
    );

    // --- The same gap size again, now with the sync backend dark. ---
    //
    // Crossing a large gap needs the chain source, but needing it must not
    // mean hanging on it. With no client available the wallet must fall back
    // to block-by-block replay, exactly as if no sync source were configured:
    // slow and loud, but the enforcer keeps enforcing throughout, and the
    // wallet still recovers everything paid to its revealed addresses without
    // the backend's help.
    let balance_before_dark = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    // Owned by the wallet, revealed, and currently unfunded, so the recovery
    // below shows up as balance growth. Revealed is the operative word: the
    // block-by-block fallback only sees addresses the wallet already knows
    // about. (`create_new_address` is `next_unused_address`, which after the
    // scan above may equally well be an already-revealed index the gap
    // skipped over -- either serves.)
    let dark_gap_address = post_setup
        .wallet_service_client
        .create_new_address(CreateNewAddressRequest::default())
        .await?
        .into_owned()
        .address;

    tracing::info!("killing enforcer and electrs, then mining {GAP_BLOCKS} blocks");
    post_setup.kill_enforcer().await?;
    post_setup.kill_electrs().await?;
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [GAP_BLOCKS.to_string(), dark_gap_address.clone()],
        )
        .run_utf8()
        .await?;

    // The gRPC port must open inside `restart_enforcer`'s wait even though
    // the checkpoint-and-scan path is unavailable.
    tracing::info!("restarting enforcer with a large gap and its sync backend unreachable");
    post_setup
        .restart_enforcer(&bin_paths, Vec::<String>::new(), res_tx.clone())
        .await?;
    let _log = wait_for_enforcer_log(
        &post_setup.directories.enforcer_dir,
        &format!("enforcer to log {BACKEND_UNREACHABLE_LOG:?}"),
        |log| log.contains(BACKEND_UNREACHABLE_LOG),
    )
    .await?;

    // The validator must cross the gap without the wallet's backend.
    wait_for_validator_tip(&post_setup).await?;

    // The regression this phase pins: with the gap too large to want a
    // replay but no client to scan with, the wallet must take the fallback
    // rather than block on the backend. Blocking here parks the initial
    // sync: the fallback warn never appears, the wallet tip never moves, and
    // no block mined after the restart is ever enforced.
    let _log = wait_for_enforcer_log(
        &post_setup.directories.enforcer_dir,
        &format!("enforcer to log {FALLBACK_LOG:?}"),
        |log| log.contains(FALLBACK_LOG),
    )
    .await?;
    // The replay grinds the wallet to the tip with the backend still dark...
    wait_for_wallet_sync(&mut post_setup).await?;
    // ...and recovers the coinbases paid to the revealed address on the way,
    // without the backend's help.
    let balance_after_dark = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    tracing::info!(
        ?balance_after_dark,
        "wallet balance after the dark-backend gap"
    );
    anyhow::ensure!(
        balance_after_dark.confirmed_sats > balance_before_dark.confirmed_sats,
        "the block-by-block fallback must recover the coinbases paid to the wallet's \
         revealed address while the backend was dark, but confirmed_sats did not grow: \
         {balance_after_dark:?} (was {balance_before_dark:?})"
    );

    // Liveness: the initial sync returned and the block pipeline is running,
    // so fresh blocks keep being enforced and followed by the wallet.
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [LIVENESS_BLOCKS.to_string(), dark_gap_address],
        )
        .run_utf8()
        .await?;
    wait_for_validator_tip(&post_setup).await?;
    wait_for_wallet_sync(&mut post_setup).await?;

    // The backend returns: the background init must still pick it up for
    // later syncs, even though the wallet already caught up without it.
    tracing::info!("restarting electrs; the enforcer must pick it up in the background");
    post_setup
        .restart_electrs(&bin_paths, res_tx.clone())
        .await?;
    let _log = wait_for_enforcer_log(
        &post_setup.directories.enforcer_dir,
        &format!("enforcer to log {BACKEND_AVAILABLE_LOG:?} after electrs came back"),
        |log| log.contains(BACKEND_AVAILABLE_LOG),
    )
    .await?;

    // The rolling log spans both restarts: the first crossing checkpointed
    // and scanned, and the dark crossing must not have -- there was no
    // client to scan with, only the fallback.
    let enforcer_log = read_enforcer_log(&post_setup.directories.enforcer_dir)?;
    let checkpoints = enforcer_log.matches(CHECKPOINT_LOG).count();
    anyhow::ensure!(
        checkpoints == 1,
        "expected exactly one {CHECKPOINT_LOG:?} entry (the first, backend-up crossing); \
         found {checkpoints}. The dark crossing has no client to scan with and must take \
         the block-by-block fallback instead"
    );

    // --- A small gap, with the backend dark again. ---
    //
    // The dark crossing above exercised the too-far-behind branch of
    // `sync_wallet_to_tip`; this exercises the other one. Within
    // `MAX_BLOCK_BY_BLOCK_REPLAY` the wallet takes the block-by-block path
    // by choice -- it needs only Bitcoin Core, so a dark backend costs it
    // nothing: no checkpoint, and no fallback warn either.
    let small_gap_address = post_setup
        .wallet_service_client
        .create_new_address(CreateNewAddressRequest::default())
        .await?
        .into_owned()
        .address;
    let balance_before_small = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();

    tracing::info!("killing enforcer and electrs, then mining {SMALL_GAP_BLOCKS} blocks");
    post_setup.kill_enforcer().await?;
    post_setup.kill_electrs().await?;
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [SMALL_GAP_BLOCKS.to_string(), small_gap_address],
        )
        .run_utf8()
        .await?;

    tracing::info!("restarting enforcer with a small gap and its sync backend unreachable");
    post_setup
        .restart_enforcer(&bin_paths, Vec::<String>::new(), res_tx.clone())
        .await?;
    // Prove the degraded path ran again, rather than the backend having been
    // reachable all along.
    let _log = wait_for_enforcer_log(
        &post_setup.directories.enforcer_dir,
        &format!("enforcer to log {BACKEND_UNREACHABLE_LOG:?} a second time"),
        |log| log.matches(BACKEND_UNREACHABLE_LOG).count() >= 2,
    )
    .await?;

    wait_for_validator_tip(&post_setup).await?;
    wait_for_wallet_sync(&mut post_setup).await?;

    let balance_after_small = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    tracing::info!(
        ?balance_after_small,
        "wallet balance after the small dark gap"
    );
    anyhow::ensure!(
        balance_after_small.confirmed_sats > balance_before_small.confirmed_sats,
        "mining {SMALL_GAP_BLOCKS} blocks must mature more of the wallet's coinbases even \
         with the sync backend down, but confirmed_sats did not grow: \
         {balance_after_small:?} (was {balance_before_small:?})"
    );

    // Still exactly one checkpoint and one fallback warn in the rolling log:
    // the small gap took the replay path by choice, not as a fallback, and
    // nothing connected to a backend while electrs was down.
    let enforcer_log = read_enforcer_log(&post_setup.directories.enforcer_dir)?;
    let checkpoints = enforcer_log.matches(CHECKPOINT_LOG).count();
    anyhow::ensure!(
        checkpoints == 1,
        "a {SMALL_GAP_BLOCKS}-block gap sits far inside MAX_BLOCK_BY_BLOCK_REPLAY and must \
         be replayed block-by-block, but the {CHECKPOINT_LOG:?} count moved to {checkpoints}"
    );
    let fallbacks = enforcer_log.matches(FALLBACK_LOG).count();
    anyhow::ensure!(
        fallbacks == 1,
        "expected only the dark large-gap crossing's {FALLBACK_LOG:?} entry, but found \
         {fallbacks}; a small gap must not be treated as a fallback from the missing \
         chain source"
    );
    let connections = enforcer_log.matches(BACKEND_AVAILABLE_LOG).count();
    anyhow::ensure!(
        connections == 1,
        "{BACKEND_AVAILABLE_LOG:?} appeared {connections} times, but electrs was down for \
         the small gap, so only the first dark crossing's entry should exist; the \
         background init connected to something it shouldn't have"
    );

    // The backend returns one more time; the background task must pick it up
    // without a restart. Its retry delay is capped at 10s, so well inside the
    // poll budget.
    tracing::info!("restarting electrs; the enforcer must pick it up in the background");
    post_setup
        .restart_electrs(&bin_paths, res_tx.clone())
        .await?;
    let _log = wait_for_enforcer_log(
        &post_setup.directories.enforcer_dir,
        &format!("enforcer to log {BACKEND_AVAILABLE_LOG:?} after electrs came back"),
        |log| log.matches(BACKEND_AVAILABLE_LOG).count() >= 2,
    )
    .await?;

    // --- The same recovery, asked for over RPC instead of forced by a gap. ---
    //
    // The wallet is in sync and the backend is back, so nothing here will fall
    // behind far enough to reach `full_scan` on its own. `FullScan` is the
    // operator's way in.
    let rpc_scan_address = peek_external_address(&mut post_setup, RPC_SCAN_INDEX).await?;
    tracing::info!(
        "mining {RPC_SCAN_BLOCKS} blocks to unrevealed external index {RPC_SCAN_INDEX} \
         ({rpc_scan_address}), with the enforcer running"
    );
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [RPC_SCAN_BLOCKS.to_string(), rpc_scan_address.clone()],
        )
        .run_utf8()
        .await?;

    // The enforcer is up and following the tip, so these blocks arrive through
    // the ordinary connect_block path -- which cannot match an index the
    // indexer has never held. This is what gives the scan below its teeth: the
    // coinbases must still be missing once the blocks carrying them are
    // connected, or something other than the scan recovered them.
    wait_for_wallet_sync(&mut post_setup).await?;
    let before_rpc_scan = confirmed_utxos_at(&mut post_setup, &rpc_scan_address).await?;
    anyhow::ensure!(
        before_rpc_scan == 0,
        "external index {RPC_SCAN_INDEX} sits past every index the gap crossings revealed, \
         so connecting blocks must not have matched its {RPC_SCAN_BLOCKS} coinbases, but \
         the wallet already holds {before_rpc_scan} UTXO(s) there. Has the lookahead grown, \
         or did an earlier scan reveal that far? The scan below proves nothing as it stands"
    );

    wait_for_electrs_tip(&post_setup).await?;
    tracing::info!("requesting a full scan over gRPC");
    let scanned_tip = request_full_scan(&mut post_setup).await?;
    let after_rpc_scan = confirmed_utxos_at(&mut post_setup, &rpc_scan_address).await?;
    anyhow::ensure!(
        after_rpc_scan == RPC_SCAN_BLOCKS as usize,
        "the full scan must recover the {RPC_SCAN_BLOCKS} coinbases paid to external index \
         {RPC_SCAN_INDEX} ({rpc_scan_address}), but the wallet holds {after_rpc_scan} \
         UTXO(s) there"
    );

    // The response reports the tip the scan left the wallet at, so a caller
    // knows how much chain the result covers without a second round trip.
    let tip = wallet_tip(&mut post_setup).await?;
    anyhow::ensure!(
        scanned_tip == tip,
        "FullScan reported tip {scanned_tip}, but the wallet is at {tip}"
    );

    // A rescan over the same chain is a no-op that must still succeed and must
    // not disturb what the first one found: this is an RPC an operator can
    // reasonably hit twice.
    let rescanned_tip = request_full_scan(&mut post_setup).await?;
    anyhow::ensure!(
        rescanned_tip == tip,
        "a repeated FullScan reported tip {rescanned_tip}, but the wallet is at {tip}"
    );
    let after_rescan = confirmed_utxos_at(&mut post_setup, &rpc_scan_address).await?;
    anyhow::ensure!(
        after_rescan == after_rpc_scan,
        "a repeated full scan changed the wallet's UTXO set at external index \
         {RPC_SCAN_INDEX}: {after_rescan} UTXO(s), was {after_rpc_scan}"
    );

    Ok(())
}
