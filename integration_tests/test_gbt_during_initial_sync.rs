//! Regression test for the `getblocktemplate` endpoint being absent for the
//! whole of the enforcer's initial sync, which with `--enable-wallet` covers
//! the wallet's catch-up over every block it missed. Miners could neither tell
//! a slow start from a dead enforcer, nor submit blocks they had found.
//!
//! `submitblock` only needs the node, so it must work throughout.
//! `getblocktemplate` needs a synced mempool, so it must say so as a JSON-RPC
//! error rather than as a refused connection.
//!
//! The sync window is held open by killing the enforcer, mining a gap behind
//! its back, and restarting. Landing inside the window is asserted rather than
//! assumed, so a window that closed too early fails the test.

use std::time::Duration;

use bip300301_enforcer_lib::bins::CommandExt as _;
use cusf_enforcer_mempool::server::RpcClient as _;
use futures::channel::mpsc;

use crate::{
    integration_test::{wait_for_validator_tip, wait_for_wallet_sync},
    setup::{
        Mode, Network, PostSetup, PreSetup, SetupOpts, read_enforcer_log, wait_for_block_templates,
        wait_for_port, wait_for_port_free,
    },
    util::BinPaths,
};

pub const TEST_NAME: &str = "gbt_during_initial_sync";

/// Bitcoin Core's code for "cannot build a template yet", which the enforcer
/// reuses.
const RPC_CLIENT_IN_INITIAL_DOWNLOAD: i32 = -10;

/// Far above [`GAP_BLOCKS`], so the wallet crosses the gap the slow way rather
/// than checkpointing across it, which is what holds the sync open.
const MAX_BLOCK_BY_BLOCK_REPLAY: u32 = 1_000_000;

/// Mined while the enforcer is down. The window the probes need is the whole
/// initial sync, not just this gap, so it only has to be big enough to make
/// that window comfortable.
const GAP_BLOCKS: u32 = 200;

/// Emitted once per block by the wallet's block-by-block replay.
const REPLAY_LOG: &str = "unable to connect block to bdk_chain";

/// Emitted by `spawn_gbt_server` when the JSON-RPC endpoint binds.
const LISTENING_LOG: &str = "Listening for JSON-RPC on";

/// Emitted by `sync_mempool` once the initial sync is done.
const SYNCED_LOG: &str = "Initial mempool sync complete";

/// Initial syncs this data dir has seen finish, over every run.
fn initial_syncs_completed(post_setup: &PostSetup) -> anyhow::Result<usize> {
    let log = read_enforcer_log(&post_setup.directories.enforcer_dir)?;
    Ok(log.matches(SYNCED_LOG).count())
}

fn enforcer_args() -> Vec<String> {
    vec![format!(
        "--wallet-max-block-by-block-replay={MAX_BLOCK_BY_BLOCK_REPLAY}"
    )]
}

async fn request_block_template(
    post_setup: &PostSetup,
) -> Result<cusf_enforcer_mempool::server::BlockTemplateResponse, jsonrpsee::core::client::Error> {
    let mut request = bitcoin_jsonrpsee::client::BlockTemplateRequest::default();
    request.capabilities.insert("coinbasetxn".to_owned());
    post_setup.gbt_client.get_block_template(request).await
}

/// Raw hex of the block bitcoind currently has at its tip.
async fn tip_block_hex(post_setup: &PostSetup) -> anyhow::Result<String> {
    let block_hash = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?
        .trim()
        .to_owned();
    let block_hex = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [block_hash, "0".to_owned()])
        .run_utf8()
        .await?
        .trim()
        .to_owned();
    Ok(block_hex)
}

pub async fn test_gbt_during_initial_sync(bin_paths: BinPaths) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths.clone(), Network::Regtest)?;
    let setup_opts: SetupOpts = SetupOpts {
        enforcer_args: enforcer_args(),
        ..Default::default()
    };
    let mut post_setup = pre_setup
        .setup(Mode::GetBlockTemplate, setup_opts, res_tx.clone())
        .await?;
    let serve_rpc_port = post_setup.reserved_ports.enforcer_serve_rpc.port();

    // Baseline: a synced enforcer serves templates, so the probes below read a
    // real difference.
    let template = crate::util::expect_block_template(request_block_template(&post_setup).await?)?;
    tracing::info!(
        height = template.height,
        "synced enforcer serves block templates"
    );

    // Submitted during the sync below. bitcoind already has this block, so
    // only bitcoind can answer `duplicate`, which is the proof of passthrough.
    let known_block_hex = tip_block_hex(&post_setup).await?;

    // The restarted enforcer must bind the endpoint before it logs one of
    // these.
    let syncs_before_restart = initial_syncs_completed(&post_setup)?;

    tracing::info!("killing enforcer, then mining {GAP_BLOCKS} blocks behind its back");
    post_setup.kill_enforcer().await?;
    // The probes must reach the restarted enforcer, not the old one.
    wait_for_port_free("127.0.0.1", serve_rpc_port, Duration::from_secs(10)).await?;
    let mining_address = post_setup.mining_address.to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [GAP_BLOCKS.to_string(), mining_address],
        )
        .run_utf8()
        .await?;

    tracing::info!("restarting enforcer -- it must serve JSON-RPC while it crosses the gap");
    post_setup
        .restart_enforcer(&bin_paths, enforcer_args(), res_tx.clone())
        .await?;

    // `restart_enforcer` only waits for the gRPC port. This wait used to sit
    // behind the whole initial sync.
    wait_for_port("127.0.0.1", serve_rpc_port, Duration::from_secs(30))
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "the `getblocktemplate` endpoint must bind before the initial sync \
                 finishes, but nothing was listening on port {serve_rpc_port}: {err:#}"
            )
        })?;

    // The ordering this test exists for, and what makes the probes below
    // probes of a *syncing* enforcer.
    let syncs_now = initial_syncs_completed(&post_setup)?;
    anyhow::ensure!(
        syncs_now == syncs_before_restart,
        "the JSON-RPC endpoint only came up once the restarted enforcer had finished \
         its initial sync ({SYNCED_LOG:?} had already been logged {syncs_now} times, up \
         from {syncs_before_restart}, by the time port {serve_rpc_port} opened). Miners \
         are refused for the whole of the sync, templates and `submitblock` alike"
    );

    // Probe 1: templates.
    let err = match request_block_template(&post_setup).await {
        Err(err) => err,
        Ok(response) => {
            let template = crate::util::expect_block_template(response)?;
            anyhow::bail!(
                "the enforcer served a block template at height {} while it was still \
             crossing the {GAP_BLOCKS}-block gap, so it built one from a mempool it had \
             not finished syncing -- or the gap was crossed in the moments between the \
             port opening and this request, in which case raise GAP_BLOCKS",
                template.height
            )
        }
    };
    let jsonrpsee::core::client::Error::Call(err) = err else {
        anyhow::bail!(
            "`getblocktemplate` during the initial sync must fail as a JSON-RPC error, \
             not at the transport: {err:#}"
        )
    };
    anyhow::ensure!(
        err.code() == RPC_CLIENT_IN_INITIAL_DOWNLOAD,
        "`getblocktemplate` during the initial sync must report \
         {RPC_CLIENT_IN_INITIAL_DOWNLOAD} (`RPC_CLIENT_IN_INITIAL_DOWNLOAD`), the code \
         Bitcoin Core uses when it cannot build a template yet, but it reported {} ({})",
        err.code(),
        err.message()
    );
    tracing::info!(
        "syncing enforcer refused a template with `{}: {}`",
        err.code(),
        err.message()
    );

    // Probe 2: block submission, which is what the outage actually cost.
    let submit_result = post_setup
        .gbt_client
        .submit_block(known_block_hex)
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "`submitblock` must reach the node while the enforcer syncs, but the \
                 call failed: {err:#}"
            )
        })?;
    anyhow::ensure!(
        submit_result.as_deref() == Some("duplicate"),
        "bitcoind already has the submitted block, so it must answer `duplicate`; \
         got {submit_result:?}"
    );
    tracing::info!("syncing enforcer passed `submitblock` through to the node");

    // The handover, on the same endpoint, without a rebind.
    wait_for_validator_tip(&post_setup).await?;
    wait_for_wallet_sync(&mut post_setup).await?;
    wait_for_block_templates(&post_setup.gbt_client).await?;
    let template = crate::util::expect_block_template(request_block_template(&post_setup).await?)?;
    tracing::info!(
        height = template.height,
        "enforcer serves block templates again once synced"
    );

    let enforcer_log = read_enforcer_log(&post_setup.directories.enforcer_dir)?;
    anyhow::ensure!(
        enforcer_log.contains(REPLAY_LOG),
        "expected the restarted enforcer to cross the {GAP_BLOCKS}-block gap one block at \
         a time, but {REPLAY_LOG:?} never appeared in its log. Did it come up without \
         --wallet-max-block-by-block-replay={MAX_BLOCK_BY_BLOCK_REPLAY}, and checkpoint \
         across the gap instead?"
    );
    // Belt and braces for the probes above, whatever the timing was.
    let line_of = |needle: &str| {
        enforcer_log
            .lines()
            .position(|line| line.contains(needle))
            .ok_or_else(|| anyhow::anyhow!("{needle:?} never appeared in the enforcer log"))
    };
    let listening = line_of(LISTENING_LOG)?;
    let synced = line_of(SYNCED_LOG)?;
    anyhow::ensure!(
        listening < synced,
        "the JSON-RPC endpoint must bind before the initial mempool sync completes, but \
         {LISTENING_LOG:?} was logged at line {listening}, after {SYNCED_LOG:?} at line \
         {synced}"
    );

    Ok(())
}
