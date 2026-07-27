//! How the enforcer behaves against Bitcoin Core nodes that cannot serve the
//! full block data the enforcer needs:
//!
//! * A pruned node (`-prune`) is refused at startup with a clear error; the
//!   enforcer requires full block data from its sync start onwards, which a
//!   pruned node cannot reliably provide.
//! * A node that loaded an assumeutxo snapshot (`loadtxoutset`) has its
//!   active tip at the snapshot height while the blocks below it are still
//!   being background-downloaded; `getblock` for those heights fails with
//!   `Block not available (not fully downloaded)`. The enforcer follows the
//!   node's background sync (via `getchainstates`), processing blocks as
//!   they become available instead of dying on the `getblock` error.

use std::time::Duration;

use bip300301_enforcer_lib::{
    bins::{self, CommandExt as _},
    proto::{mainchain::GetChainTipRequest, mainchain_service::ValidatorServiceClient},
};
use connectrpc::{
    client::{ClientConfig, HttpClient},
    error::ErrorCode,
};
use futures::{StreamExt as _, channel::mpsc};
use tokio::time::sleep;

use crate::{
    setup::{BitcoindKind, PreSetup, new_bitcoind, wait_for_bitcoind_ready, wait_for_port},
    util::{AbortOnDrop, Bitcoind, Enforcer},
};

/// Any valid regtest address; the coins are never spent.
const UNSPENDABLE_ADDRESS: &str = "bcrt1qrmxr2qc8eedpqw8wsdtg4spzkcmcs2adkrc9rh";

/// One entry of `testdata/assumeutxo/fixtures.json`, written by
/// `scripts/generate_assumeutxo_fixture.py`. The regtest assumeutxo
/// chainparams entry (and so the fixture chain) differs between Bitcoin
/// Core major versions.
#[derive(Debug, serde::Deserialize)]
struct SnapshotFixture {
    /// Core major versions whose chainparams accept this fixture.
    majors: Vec<u32>,
    /// The blocks file, relative to the fixture directory.
    blocks: String,
    base_height: u32,
    base_blockhash: String,
    /// Expected `dumptxoutset` hash. Also keys the snapshot cache, so a
    /// fixture regeneration invalidates cached snapshots.
    txoutset_hash: String,
}

/// Load the snapshot fixture matching the node's Core major version.
fn load_snapshot_fixture(major: u32) -> anyhow::Result<SnapshotFixture> {
    let manifest_path = assumeutxo_fixture_path("fixtures.json");
    let fixtures: Vec<SnapshotFixture> =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    fixtures
        .into_iter()
        .find(|fixture| fixture.majors.contains(&major))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no assumeutxo fixture for Bitcoin Core major {major}, generate one \
                 with scripts/generate_assumeutxo_fixture.py"
            )
        })
}

/// The Core major version the node reports via `getnetworkinfo`.
async fn node_major_version(bitcoin_cli: &bins::BitcoinCli) -> anyhow::Result<u32> {
    let network_info: serde_json::Value = serde_json::from_str(
        &bitcoin_cli
            .command::<String, _, String, _, _>([], "getnetworkinfo", [])
            .run_utf8()
            .await?,
    )?;
    let version = network_info["version"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing `version` in getnetworkinfo: {network_info}"))?;
    Ok(bip300301_enforcer_lib::version::major_from_version_int(
        version,
    ))
}

/// How long we give the enforcer to notice an unusable node and exit.
const ENFORCER_EXIT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long we give the enforcer to catch up with block data the node has
/// newly validated.
const ENFORCER_SYNC_TIMEOUT: Duration = Duration::from_secs(120);

fn assumeutxo_fixture_path(file: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("assumeutxo")
        .join(file)
}

/// Spawn bitcoind from the pre-setup dirs/ports and wait until it serves RPC.
fn spawn_bitcoind(
    setup: &PreSetup,
    extra_args: &[&str],
    res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
) -> anyhow::Result<(Bitcoind, AbortOnDrop<()>, bins::BitcoinCli)> {
    spawn_bitcoind_at(
        setup,
        setup.directories.bitcoin_dir.clone(),
        extra_args,
        res_tx,
    )
}

/// [`spawn_bitcoind`] with an explicit data dir, so a test can replace the
/// node behind the enforcer while keeping the enforcer's data dir.
fn spawn_bitcoind_at(
    setup: &PreSetup,
    data_dir: std::path::PathBuf,
    extra_args: &[&str],
    res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
) -> anyhow::Result<(Bitcoind, AbortOnDrop<()>, bins::BitcoinCli)> {
    let mut bitcoind = new_bitcoind(
        &setup.bin_paths,
        BitcoindKind::Patched,
        data_dir,
        &setup.reserved_ports,
        setup.network,
        None,
    )?;
    // `-txindex` requires full block data; neither node in these tests has it
    // (and it conflicts with `-prune`).
    bitcoind.txindex = false;
    let task = bitcoind.spawn_command_with_args::<String, _, _, _, _>(
        [],
        extra_args.iter().map(|arg| arg.to_string()),
        move |err| {
            let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
        },
    );
    let bitcoin_cli = bitcoind.new_bitcoin_cli(setup.bin_paths.bitcoin_cli()?.clone());
    Ok((bitcoind, task, bitcoin_cli))
}

struct SpawnedEnforcer {
    enforcer: Enforcer,
    /// Aborts (kills) the enforcer on drop.
    _task: AbortOnDrop<()>,
    /// Receives an error when the enforcer process exits.
    res_rx: mpsc::UnboundedReceiver<anyhow::Result<()>>,
}

/// Start a fresh (empty data dir) wallet-less enforcer against `bitcoind`.
fn spawn_enforcer(setup: &PreSetup, bitcoind: &Bitcoind) -> anyhow::Result<SpawnedEnforcer> {
    let enforcer = Enforcer {
        path: setup.bin_paths.bip300301_enforcer()?.clone(),
        data_dir: setup.directories.enforcer_dir.clone(),
        enable_mempool: false,
        enable_wallet: false,
        enable_block_template_server: false,
        coinbase_recipient: None,
        node_blocks_dir: None,
        node_mempool_dat: None,
        node_rpc_user: bitcoind.rpc_user.clone(),
        node_rpc_pass: bitcoind.rpc_pass.clone(),
        node_rpc_port: bitcoind.rpc_port,
        node_zmq_sequence_port: bitcoind.zmq_sequence_port,
        serve_grpc_port: setup.reserved_ports.enforcer_serve_grpc.port(),
        serve_rpc_port: setup.reserved_ports.enforcer_serve_rpc.port(),
        wallet_electrum_rpc_port: 0,
        wallet_electrum_http_port: 0,
    };
    let (res_tx, res_rx) = mpsc::unbounded::<anyhow::Result<()>>();
    let task = enforcer.spawn_command_with_args::<_, String, _, _, _>(
        [(
            "RUST_LOG",
            "h2=info,hyper_util=info,jsonrpsee-client=debug,jsonrpsee-http=debug,connectrpc=debug,trace",
        )],
        [],
        move |err| {
            let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
        },
    );
    Ok(SpawnedEnforcer {
        enforcer,
        _task: task,
        res_rx,
    })
}

/// Read the enforcer's combined stdout and stderr dumps.
fn read_enforcer_output(setup: &PreSetup) -> anyhow::Result<String> {
    let mut output = String::new();
    for file in ["stdout.txt", "stderr.txt"] {
        output.push_str(&std::fs::read_to_string(
            setup.directories.enforcer_dir.join(file),
        )?);
    }
    Ok(output)
}

/// Start a fresh wallet-less enforcer against `bitcoind`, wait for the
/// process to exit, and return its combined stdout/stderr.
async fn run_enforcer_until_exit(setup: &PreSetup, bitcoind: &Bitcoind) -> anyhow::Result<String> {
    let SpawnedEnforcer {
        enforcer: _,
        _task,
        mut res_rx,
    } = spawn_enforcer(setup, bitcoind)?;

    let exit_err = tokio::time::timeout(ENFORCER_EXIT_TIMEOUT, res_rx.next())
        .await
        .map_err(|_elapsed| {
            anyhow::anyhow!("enforcer did not exit within {ENFORCER_EXIT_TIMEOUT:?}")
        })?
        .ok_or_else(|| anyhow::anyhow!("enforcer result stream closed unexpectedly"))?
        .expect_err("enforcer task only reports errors");
    tracing::info!("enforcer exited: {exit_err:#}");

    read_enforcer_output(setup)
}

/// Path to the `loadtxoutset` snapshot for the fixture chain, generating it
/// if not cached yet.
///
/// The snapshot is not committed to the repo: it is derived from the
/// committed (text) blocks file by feeding the blocks to a throwaway
/// bitcoind and calling `dumptxoutset` at the fixture tip. The result is
/// verified against the fixture's expected UTXO-set hash and cached across
/// runs in a gitignored directory, so only the first run pays the
/// generation cost. A corrupt cache file cannot cause silent breakage:
/// `loadtxoutset` re-verifies the content hash against the node's
/// chainparams entry.
async fn assumeutxo_snapshot(
    setup: &PreSetup,
    fixture: &SnapshotFixture,
    blocks: &[&str],
) -> anyhow::Result<std::path::PathBuf> {
    let cache_dir = assumeutxo_fixture_path(".cache");
    let snapshot_path = cache_dir.join(format!("utxos-{}.dat", fixture.txoutset_hash));
    if snapshot_path.exists() {
        tracing::info!("Using cached UTXO snapshot: {}", snapshot_path.display());
        return Ok(snapshot_path);
    }

    tracing::info!("Generating UTXO snapshot from {}", fixture.blocks);
    let data_dir = temp_dir::TempDir::new()?;
    let listen_port = reserve_port::ReservedPort::random()?;
    let rpc_port = reserve_port::ReservedPort::random()?;
    let zmq_sequence_port = reserve_port::ReservedPort::random()?;
    let bitcoind = Bitcoind {
        path: setup.bin_paths.bitcoind()?.clone(),
        data_dir: data_dir.path().to_owned(),
        listen_port: listen_port.port(),
        network: setup.network.into(),
        onion_ports: None,
        rpc_user: "drivechain".to_owned(),
        rpc_pass: "integrationtesting".to_owned(),
        rpc_port: rpc_port.port(),
        rpc_host: "127.0.0.1".to_owned(),
        signet_challenge: None,
        accept_nonstd_txns: BitcoindKind::Patched.accept_nonstd_txns(),
        txindex: false,
        zmq_sequence_port: zmq_sequence_port.port(),
    };
    let (res_tx, _res_rx) = mpsc::unbounded::<anyhow::Result<()>>();
    // Killed on drop, once the snapshot is safely copied out of its data dir
    let _bitcoind_task =
        bitcoind.spawn_command_with_args::<String, String, _, _, _>([], [], move |err| {
            let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
        });
    let bitcoin_cli = bitcoind.new_bitcoin_cli(setup.bin_paths.bitcoin_cli()?.clone());
    let () = wait_for_bitcoind_ready(&bitcoin_cli).await?;
    let () = submit_blocks(&bitcoin_cli, blocks).await?;

    let dump: serde_json::Value = serde_json::from_str(
        &bitcoin_cli
            .command::<String, _, _, _, _>([], "dumptxoutset", ["utxos.dat", "latest"])
            .run_utf8()
            .await?,
    )?;
    anyhow::ensure!(
        dump["txoutset_hash"] == fixture.txoutset_hash,
        "generated snapshot does not match the expected UTXO-set hash {}, \
         was the blocks file regenerated without updating fixtures.json? \
         got: {dump}",
        fixture.txoutset_hash,
    );
    let dumped_path = dump["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing `path` in dumptxoutset response: {dump}"))?;

    std::fs::create_dir_all(&cache_dir)?;
    // Copy to a unique temp name, then rename: a concurrently running suite
    // must never observe a half-written snapshot at the final path.
    let tmp_path = cache_dir.join(format!(
        "utxos-{}.dat.tmp-{}",
        fixture.txoutset_hash,
        std::process::id()
    ));
    std::fs::copy(dumped_path, &tmp_path)?;
    std::fs::rename(&tmp_path, &snapshot_path)?;
    Ok(snapshot_path)
}

/// Submit raw blocks to the node, expecting each to be accepted.
async fn submit_blocks(bitcoin_cli: &bins::BitcoinCli, blocks_hex: &[&str]) -> anyhow::Result<()> {
    for block_hex in blocks_hex {
        // `submitblock` reports failure as a status string on stdout, and
        // success as no output at all.
        let output: String = bitcoin_cli
            .command::<String, _, _, _, _>([], "submitblock", [*block_hex])
            .run_utf8()
            .await?;
        anyhow::ensure!(output.is_empty(), "submitblock rejected a block: {output}");
    }
    Ok(())
}

/// Whether the node is still background-validating an assumeutxo snapshot:
/// `getchainstates` lists a snapshot chainstate that is not yet validated.
/// (Not `initialblockdownload`: the harness runs bitcoind with a huge
/// `-maxtipage`, so a node never reports IBD on these old test chains.)
async fn background_sync_in_progress(bitcoin_cli: &bins::BitcoinCli) -> anyhow::Result<bool> {
    let chainstates: serde_json::Value = serde_json::from_str(
        &bitcoin_cli
            .command::<String, _, String, _, _>([], "getchainstates", [])
            .run_utf8()
            .await?,
    )?;
    let in_progress = chainstates["chainstates"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("unexpected getchainstates response: {chainstates}"))?
        .iter()
        .any(|chainstate| {
            chainstate.get("snapshot_blockhash").is_some() && chainstate["validated"] == false
        });
    Ok(in_progress)
}

/// Poll the enforcer's `GetChainTip` until it reaches `height`.
async fn wait_for_enforcer_tip(
    client: &ValidatorServiceClient<HttpClient>,
    height: u32,
) -> anyhow::Result<()> {
    let poll = async {
        loop {
            match client.get_chain_tip(GetChainTipRequest::default()).await {
                Ok(tip) => {
                    let tip_height = tip
                        .into_owned()
                        .block_header_info
                        .into_option()
                        .ok_or_else(|| anyhow::anyhow!("missing block_header_info"))?
                        .height;
                    if tip_height >= height {
                        anyhow::ensure!(
                            tip_height == height,
                            "enforcer synced past the available blocks: \
                             {tip_height} > {height}"
                        );
                        return anyhow::Ok(());
                    }
                }
                // Not synced far enough to have a tip at all
                Err(err) if err.code == ErrorCode::Unavailable => (),
                Err(err) => return Err(anyhow::anyhow!("GetChainTip: {err}")),
            }
            sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::time::timeout(ENFORCER_SYNC_TIMEOUT, poll)
        .await
        .map_err(|_elapsed| {
            anyhow::anyhow!(
                "enforcer did not reach height {height} within {ENFORCER_SYNC_TIMEOUT:?}"
            )
        })?
}

/// Wait until the enforcer's stdout contains `needle`.
async fn wait_for_enforcer_log(setup: &PreSetup, needle: &str) -> anyhow::Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(30);
    let stdout_path = setup.directories.enforcer_dir.join("stdout.txt");
    let poll = async {
        loop {
            if let Ok(stdout) = std::fs::read_to_string(&stdout_path)
                && stdout.contains(needle)
            {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::time::timeout(TIMEOUT, poll)
        .await
        .map_err(|_elapsed| anyhow::anyhow!("enforcer did not log `{needle}` within {TIMEOUT:?}"))
}

/// The enforcer follows a node that is background-syncing after an assumeutxo
/// snapshot load, processing blocks as the node validates them.
///
/// From a user report: `loadtxoutset` moves the node's active tip to the
/// snapshot height while the blocks below it are not yet downloaded, and the
/// enforcer's initial sync used to die on the first `getblock` batch with
/// `Block not available (not fully downloaded)`.
pub async fn test_assumeutxo_node(setup: PreSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let (bitcoind, _bitcoind_task, bitcoin_cli) = spawn_bitcoind(&setup, &[], res_tx)?;
    let () = wait_for_bitcoind_ready(&bitcoin_cli).await?;

    // The fixture chain must match the node's regtest assumeutxo chainparams
    // entry, which differs between Core major versions
    // (see scripts/generate_assumeutxo_fixture.py).
    let fixture = load_snapshot_fixture(node_major_version(&bitcoin_cli).await?)?;

    // Give the node the fixture chain's headers only: this is the state of a
    // node that header-synced from the network but has no block data yet.
    tracing::info!("Submitting fixture headers");
    let blocks_hex = std::fs::read_to_string(assumeutxo_fixture_path(&fixture.blocks))?;
    let blocks: Vec<&str> = blocks_hex.lines().collect();
    for block_hex in &blocks {
        // A serialized header is the first 80 bytes of the serialized block.
        let header_hex = block_hex
            .get(..160)
            .ok_or_else(|| anyhow::anyhow!("malformed line in {}", fixture.blocks))?;
        let _output: String = bitcoin_cli
            .command::<String, _, _, _, _>([], "submitheader", [header_hex])
            .run_utf8()
            .await?;
    }

    tracing::info!("Loading UTXO snapshot");
    let utxos_dat = assumeutxo_snapshot(&setup, &fixture, &blocks).await?;
    let loaded: serde_json::Value = serde_json::from_str(
        &bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "loadtxoutset",
                [utxos_dat.to_string_lossy().to_string()],
            )
            .run_utf8()
            .await?,
    )?;
    anyhow::ensure!(
        loaded["base_height"] == fixture.base_height
            && loaded["tip_hash"] == fixture.base_blockhash,
        "unexpected snapshot base, wanted #{} `{}`, got: {loaded}",
        fixture.base_height,
        fixture.base_blockhash,
    );

    // The node is now in the reported state: active tip at the snapshot
    // height, background validation pending, with no block data below the
    // tip.
    let chain_info: serde_json::Value = serde_json::from_str(
        &bitcoin_cli
            .command::<String, _, String, _, _>([], "getblockchaininfo", [])
            .run_utf8()
            .await?,
    )?;
    anyhow::ensure!(
        chain_info["blocks"] == fixture.base_height,
        "expected tip {} after snapshot load, got: {chain_info}",
        fixture.base_height,
    );
    anyhow::ensure!(
        background_sync_in_progress(&bitcoin_cli).await?,
        "expected the node to be background-validating the snapshot after loading it"
    );
    let block_1_hash: String = bitcoin_cli
        .command::<String, _, _, _, _>([], "getblockhash", ["1"])
        .run_utf8()
        .await?;
    let getblock_err = bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [block_1_hash, "0".to_owned()])
        .run_utf8()
        .await
        .expect_err("blocks below the snapshot base must not be available yet");
    anyhow::ensure!(
        format!("{getblock_err:#}").contains("Block not available (not fully downloaded)"),
        "unexpected getblock error: {getblock_err:#}"
    );

    // The enforcer must come up and wait for the node's background sync
    // instead of dying on the first `getblock` batch.
    let mut spawned = spawn_enforcer(&setup, &bitcoind)?;
    wait_for_port(
        "127.0.0.1",
        spawned.enforcer.serve_grpc_port,
        Duration::from_secs(10),
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed waiting for enforcer gRPC port: {e}"))?;
    let () = wait_for_enforcer_log(&setup, "waiting for the node's background sync").await?;

    let validator_client = {
        let uri: http::Uri =
            format!("http://127.0.0.1:{}", spawned.enforcer.serve_grpc_port).parse()?;
        ValidatorServiceClient::new(HttpClient::plaintext(), ClientConfig::new(uri))
    };

    // Hand the node the first half of the block data. Its background
    // chainstate validates the blocks, and the enforcer must follow — while
    // the node is still mid background-validation.
    let half = blocks.len() / 2;
    tracing::info!("Submitting blocks 1-{half}");
    let () = submit_blocks(&bitcoin_cli, &blocks[..half]).await?;
    let () = wait_for_enforcer_tip(&validator_client, half as u32).await?;
    anyhow::ensure!(
        background_sync_in_progress(&bitcoin_cli).await?,
        "the node must still be background-validating the snapshot at this point"
    );

    // The rest of the blocks: the node finishes background validation, and
    // the enforcer reaches the snapshot height.
    tracing::info!("Submitting blocks {}-{}", half + 1, blocks.len());
    let () = submit_blocks(&bitcoin_cli, &blocks[half..]).await?;
    let () = wait_for_enforcer_tip(&validator_client, fixture.base_height).await?;

    // Through all of this, the enforcer process must have stayed alive.
    match spawned.res_rx.try_recv() {
        // No exit reported: the enforcer is still running.
        Err(_empty) => Ok(()),
        Ok(res) => anyhow::bail!(
            "enforcer exited during the background sync: {:#}",
            res.expect_err("enforcer task only reports errors")
        ),
    }
}

/// The enforcer refuses a node with pruning enabled at startup, with a clear
/// error naming the requirement (full block data) and the way out (restart
/// without `prune`, reindex).
///
/// `getblockchaininfo` reports `pruned: true` as soon as `-prune` is set,
/// before any block is actually discarded — and the enforcer refuses already
/// at that point: a prune-enabled node is guaranteed to eventually discard
/// blocks the enforcer needs.
pub async fn test_pruned_node(setup: PreSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let (bitcoind, _bitcoind_task, bitcoin_cli) = spawn_bitcoind(&setup, &["-prune=1"], res_tx)?;
    let () = wait_for_bitcoind_ready(&bitcoin_cli).await?;

    // A little chain so the refusal is provably not "nothing to sync".
    let _output: String = bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", ["10", UNSPENDABLE_ADDRESS])
        .run_utf8()
        .await?;

    let enforcer_output = run_enforcer_until_exit(&setup, &bitcoind).await?;
    anyhow::ensure!(
        enforcer_output.contains("Bitcoin Core node has pruning enabled"),
        "expected the startup pruning check to refuse the node with a clear error"
    );
    anyhow::ensure!(
        enforcer_output.contains("-reindex"),
        "expected the error to tell the user how to un-prune their node"
    );
    anyhow::ensure!(
        !enforcer_output.contains("Block not available"),
        "the enforcer must refuse the node up front, not die mid-sync on \
         a raw `getblock` error"
    );
    Ok(())
}

/// Read raw blocks `from_height..=to_height` off a node.
async fn read_blocks(
    bitcoin_cli: &bins::BitcoinCli,
    from_height: u32,
    to_height: u32,
) -> anyhow::Result<Vec<String>> {
    let mut blocks = Vec::new();
    for height in from_height..=to_height {
        let block_hash: String = bitcoin_cli
            .command::<String, _, _, _, _>([], "getblockhash", [height.to_string()])
            .run_utf8()
            .await?;
        blocks.push(
            bitcoin_cli
                .command::<String, _, _, _, _>([], "getblock", [block_hash, "0".to_owned()])
                .run_utf8()
                .await?,
        );
    }
    Ok(blocks)
}

/// An enforcer whose tip is already at or above the snapshot base must keep
/// following the chain while the node background-validates a snapshot.
///
/// This is what an operator gets by re-bootstrapping an existing node with
/// `loadtxoutset` and keeping their enforcer data dir. The node serves every
/// block from the snapshot base upwards for the whole background sync (the
/// snapshot chainstate downloaded and stored them on its way to the tip), so
/// there is nothing for the enforcer to wait for.
///
/// Verified against Bitcoin Core master (v31.99.0-67efced1fc83): with the
/// background chainstate at genesis and the snapshot chainstate at height
/// 399, `getblock` refuses heights 1, 149 and 299 with `Block not available
/// (not fully downloaded)` and serves heights 300, 349 and 399.
pub async fn test_assumeutxo_enforcer_above_snapshot_base(setup: PreSetup) -> anyhow::Result<()> {
    const EXTRA_BLOCKS: u32 = 100;

    // ---- phase 1: a plain node, and an enforcer synced to its tip ----
    let (res_tx, _res_rx) = mpsc::unbounded();
    let (full_node, full_node_task, full_node_cli) = spawn_bitcoind(&setup, &[], res_tx)?;
    let () = wait_for_bitcoind_ready(&full_node_cli).await?;

    let fixture = load_snapshot_fixture(node_major_version(&full_node_cli).await?)?;
    let blocks_hex = std::fs::read_to_string(assumeutxo_fixture_path(&fixture.blocks))?;
    let base_blocks: Vec<&str> = blocks_hex.lines().collect();
    let () = submit_blocks(&full_node_cli, &base_blocks).await?;

    // Extend past the snapshot base, so there is a range of blocks that the
    // assumeutxo node will be able to serve while its background chainstate
    // is still at genesis.
    let _generated: String = full_node_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [EXTRA_BLOCKS.to_string(), UNSPENDABLE_ADDRESS.to_owned()],
        )
        .run_utf8()
        .await?;
    let synced_height = fixture.base_height + EXTRA_BLOCKS;

    {
        let spawned = spawn_enforcer(&setup, &full_node)?;
        wait_for_port(
            "127.0.0.1",
            spawned.enforcer.serve_grpc_port,
            Duration::from_secs(10),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed waiting for enforcer gRPC port: {e}"))?;
        let uri: http::Uri =
            format!("http://127.0.0.1:{}", spawned.enforcer.serve_grpc_port).parse()?;
        let client = ValidatorServiceClient::new(HttpClient::plaintext(), ClientConfig::new(uri));
        let () = wait_for_enforcer_tip(&client, synced_height).await?;
        // `spawned` is dropped here: the enforcer is stopped, its data dir kept.
    }
    tracing::info!("enforcer synced to height {synced_height} against a plain node");

    // One more block, handed only to the assumeutxo node below: this is the
    // block the enforcer must fetch after the node is swapped.
    let _generated: String = full_node_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", ["1", UNSPENDABLE_ADDRESS])
        .run_utf8()
        .await?;
    let new_tip_height = synced_height + 1;

    let above_base = read_blocks(&full_node_cli, fixture.base_height + 1, new_tip_height).await?;
    let above_base: Vec<&str> = above_base.iter().map(String::as_str).collect();
    let snapshot = assumeutxo_snapshot(&setup, &fixture, &base_blocks).await?;

    // ---- phase 2: replace the node with one that is background-validating ----
    drop(full_node_task);
    drop(full_node);
    sleep(Duration::from_secs(2)).await;

    let assumeutxo_dir = setup
        .directories
        .base_dir
        .path()
        .join("bitcoind-assumeutxo");
    std::fs::create_dir_all(&assumeutxo_dir)?;
    let (au_res_tx, _au_res_rx) = mpsc::unbounded();
    let (au_node, _au_node_task, au_cli) =
        spawn_bitcoind_at(&setup, assumeutxo_dir, &[], au_res_tx)?;
    let () = wait_for_bitcoind_ready(&au_cli).await?;

    for block_hex in base_blocks.iter().chain(above_base.iter()) {
        let header_hex = block_hex
            .get(..160)
            .ok_or_else(|| anyhow::anyhow!("malformed block hex"))?;
        let _output: String = au_cli
            .command::<String, _, _, _, _>([], "submitheader", [header_hex])
            .run_utf8()
            .await?;
    }
    let _loaded: String = au_cli
        .command::<String, _, _, _, _>([], "loadtxoutset", [snapshot.to_string_lossy().to_string()])
        .run_utf8()
        .await?;
    // Only the blocks above the snapshot base. The background chainstate
    // stays at genesis, exactly as it would be early in a real background
    // sync.
    let () = submit_blocks(&au_cli, &above_base).await?;

    // The node itself confirms the split: nothing below the base, everything
    // from the base upwards.
    let base_plus_one: String = au_cli
        .command::<String, _, _, _, _>([], "getblockhash", [(fixture.base_height + 1).to_string()])
        .run_utf8()
        .await?;
    let _block: String = au_cli
        .command::<String, _, _, _, _>([], "getblock", [base_plus_one, "0".to_owned()])
        .run_utf8()
        .await
        .map_err(|err| {
            anyhow::anyhow!("blocks above the snapshot base must be available: {err:#}")
        })?;

    // ---- phase 3: the enforcer picks up where it left off ----
    let mut spawned = spawn_enforcer(&setup, &au_node)?;
    wait_for_port(
        "127.0.0.1",
        spawned.enforcer.serve_grpc_port,
        Duration::from_secs(10),
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed waiting for enforcer gRPC port: {e}"))?;

    let uri: http::Uri =
        format!("http://127.0.0.1:{}", spawned.enforcer.serve_grpc_port).parse()?;
    let client = ValidatorServiceClient::new(HttpClient::plaintext(), ClientConfig::new(uri));

    let reached = wait_for_enforcer_tip(&client, new_tip_height).await;
    if reached.is_err() {
        let output = read_enforcer_output(&setup)?;
        anyhow::ensure!(
            !output.contains("waiting for the node's background sync"),
            "the enforcer stalled waiting for the node's background sync at height \
             {new_tip_height}, but the node serves every block from the snapshot \
             base ({}) upwards",
            fixture.base_height,
        );
    }
    let () = reached?;

    match spawned.res_rx.try_recv() {
        Err(_empty) => Ok(()),
        Ok(res) => anyhow::bail!(
            "enforcer exited during the background sync: {:#}",
            res.expect_err("enforcer task only reports errors")
        ),
    }
}
