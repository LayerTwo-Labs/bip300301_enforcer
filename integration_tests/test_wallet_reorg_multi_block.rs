//! Regression test for a multi-block-deep reorg corrupting the wallet's local
//! chain view.
//!
//! `connect_missing_block` (`lib/wallet/mod.rs`) walks a stack of ancestor
//! heights back from BDK's reported `try_include_height` until it can connect
//! a block. Each retry re-fetches a block via `getblockhash`, but used to
//! pass the *original* `try_include_height` argument instead of the current
//! stack entry's height on every nested retry -- so on any catch-up spanning
//! more than one missing block, it fetched the same (wrong) block repeatedly
//! while applying it at progressively lower heights, corrupting BDK's
//! checkpoint history.
//!
//! This only manifests when the wallet has to catch up across the reorg in
//! one jump (BDK's ancestor-walk needs an actual gap to retry over) -- an
//! enforcer that's live and streaming ZMQ block-connect/disconnect events
//! one at a time never takes this path. So this test kills the enforcer,
//! drives the reorg entirely at the bitcoind level while it's down, then
//! restarts it -- exactly the condition under which this was first found
//! during live multi-node testing.

use bip300301_enforcer_lib::{bins::CommandExt as _, proto::mainchain::GetBalanceRequest};
use futures::channel::mpsc;

use crate::{
    integration_test::{fund_enforcer, wait_for_wallet_sync},
    setup::{DummySidechain, Mode, Network, PreSetup, SetupOpts},
    util::BinPaths,
};

pub const TEST_NAME: &str = "wallet_reorg_multi_block";

const FORK_DEPTH: u32 = 15;

pub async fn test_wallet_reorg_multi_block(bin_paths: BinPaths) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths.clone(), Network::Regtest)?;
    let setup_opts: SetupOpts = Default::default();
    let mut post_setup = pre_setup
        .setup(Mode::Mempool, setup_opts, res_tx.clone())
        .await?;

    // `fund_enforcer` mines 100 blocks to a single address, so its coinbases
    // mature (100 confirmations) at a staggered rate as height advances --
    // the *last* funded block isn't mature until 100 more blocks are mined
    // on top. Mine well past that point before taking any balance snapshot,
    // so the small height change from the reorg below (net +2) can't itself
    // mature additional coinbases and mask (or mimic) the actual corruption
    // bug under test.
    fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    let throwaway_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            ["110".to_string(), throwaway_address.clone()],
        )
        .run_utf8()
        .await?;
    wait_for_wallet_sync(&mut post_setup).await?;

    let balance_before = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    anyhow::ensure!(
        balance_before.confirmed_sats > 0,
        "expected a confirmed balance after funding, got {balance_before:?}"
    );
    tracing::info!(?balance_before, "wallet funded");

    // Kill the enforcer *before* touching the chain, so the reorg below
    // happens entirely while it's down -- it can only learn about the new
    // tip by catching up across the gap on restart, not by observing it live.
    post_setup.kill_enforcer().await?;

    // Roll back FORK_DEPTH blocks, then mine a longer (FORK_DEPTH + 2)
    // replacement chain from the fork point, so the reorg definitely sticks.
    // None of this touches the funding tx's block -- it's on the shared
    // common ancestor -- so the wallet's confirmed balance must be unchanged
    // afterwards.
    let tip_height: u32 = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    let fork_height = tip_height - FORK_DEPTH;
    let first_invalid_hash = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblockhash", [(fork_height + 1).to_string()])
        .run_utf8()
        .await?
        .trim()
        .to_string();
    tracing::info!(
        tip_height,
        fork_height,
        %first_invalid_hash,
        "invalidating down to the fork point (enforcer is down)"
    );
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "invalidateblock", [first_invalid_hash])
        .run_utf8()
        .await?;
    // A *different* address from the one used to build the original chain --
    // regtest block generation is otherwise deterministic enough (same
    // parent, same miner, same empty mempool, same coinbase value/height)
    // that the replacement block at the fork height can come out
    // byte-identical to the one just invalidated, which bitcoind then
    // rejects outright as `duplicate-invalid` rather than accepting a
    // harmless re-mine.
    let replacement_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [(FORK_DEPTH + 2).to_string(), replacement_address],
        )
        .run_utf8()
        .await?;

    let new_tip_height: u32 = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    anyhow::ensure!(
        new_tip_height == tip_height + 2,
        "expected the replacement chain to win by 2 blocks, got height {new_tip_height} \
         (was {tip_height})"
    );

    // electrs (v3.2.0, pinned by this repo's test harness) is known to panic
    // mid-index on some reorgs -- an unrelated, pre-existing limitation, not
    // the enforcer -- so it may need restarting from a clean index before
    // the enforcer (which depends on it for wallet sync) can come back up.
    tracing::info!("restarting electrs");
    post_setup
        .restart_electrs(&bin_paths, res_tx.clone())
        .await?;

    tracing::info!("restarting enforcer -- it must now catch up across the reorg in one jump");
    post_setup
        .restart_enforcer(&bin_paths, Vec::<String>::new(), res_tx.clone())
        .await?;

    wait_for_wallet_sync(&mut post_setup).await?;

    let balance_after = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    tracing::info!(
        ?balance_after,
        "wallet balance after multi-block reorg + restart"
    );
    anyhow::ensure!(
        balance_after.confirmed_sats == balance_before.confirmed_sats,
        "wallet balance should be unaffected by a {FORK_DEPTH}-block-deep reorg that doesn't \
         touch the funding tx's block, but confirmed_sats changed: {balance_after:?} \
         (was {balance_before:?})"
    );
    anyhow::ensure!(
        balance_after.pending_sats == balance_before.pending_sats,
        "pending_sats should also be unaffected: {balance_after:?} (was {balance_before:?})"
    );

    // Not just a number: prove the wallet is actually still usable -- a
    // corrupted checkpoint history could report a plausible-looking balance
    // yet still fail real coin selection/signing.
    tracing::info!("confirming the wallet can still build and sign a real transaction");
    use bip300301_enforcer_lib::proto::mainchain::SendTransactionRequest;
    let destination = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_string();
    let _send_resp = post_setup
        .wallet_service_client
        .send_transaction(SendTransactionRequest {
            destinations: std::collections::HashMap::from([(destination, 100_000_u64)])
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .await?;

    Ok(())
}
