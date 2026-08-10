//! End-to-end check of `--node-mempool-dat`.
//!
//! Runs the real enforcer binary against a real bitcoind holding a real
//! mempool, and asserts on the enforcer's own logs that the fast path was
//! actually taken.
//!
//! Two enforcers are started in turn against the same node:
//!   1. pointed at the node's real `mempool.dat` -> must seed every tx and
//!      leave nothing for RPC
//!   2. pointed at a file of garbage             -> must warn, seed nothing,
//!      and still complete the sync over RPC
//!
//! The second run is the one that matters for safety: reading the dump is an
//! optimization, so a broken dump may cost performance but must never cost
//! correctness or availability.

use std::time::Duration;

use bip300301_enforcer_lib::bins::CommandExt as _;
use futures::channel::mpsc;

use crate::{
    setup::{
        BitcoindKind, PreSetup, new_bitcoind, read_enforcer_log, wait_for_bitcoind_ready,
        wait_for_enforcer_log, wait_for_port,
    },
    util::Enforcer,
};

/// Transactions to put in the node's mempool before dumping.
const TXS: usize = 15;

pub async fn test_mempool_dat_sync(setup: PreSetup) -> anyhow::Result<()> {
    let (res_tx, _) = mpsc::unbounded::<anyhow::Result<()>>();

    let bitcoind = new_bitcoind(
        &setup.bin_paths,
        BitcoindKind::Patched,
        setup.directories.bitcoin_dir.clone(),
        &setup.reserved_ports,
        setup.network,
        None,
    )?;

    let _bitcoind_task = bitcoind.spawn_command_with_args::<String, String, _, _, _>(
        [],
        ["-fallbackfee=0.0002".to_owned()],
        {
            let res_tx = res_tx.clone();
            move |err| {
                tracing::error!("Error starting bitcoind: {err:#}");
                let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
            }
        },
    );

    let bitcoin_cli = bitcoind.new_bitcoin_cli(setup.bin_paths.bitcoin_cli()?.clone());
    let () = wait_for_bitcoind_ready(&bitcoin_cli).await?;

    // Fund a wallet so there is something to spend, then build a mempool.
    // A fresh regtest datadir has no wallet loaded, so create one first.
    bitcoin_cli
        .command::<String, _, _, _, _>([], "createwallet", ["mempool-dat-test".to_owned()])
        .run_utf8()
        .await?;
    let address = bitcoin_cli
        .command::<String, _, _, _, _>([], "getnewaddress", Vec::<String>::new())
        .run_utf8()
        .await?;
    let address = address.trim().to_owned();
    bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", ["101".to_owned(), address.clone()])
        .run_utf8()
        .await?;
    for _ in 0..TXS {
        let _res = bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "sendtoaddress",
                [address.clone(), "0.001".to_owned()],
            )
            .run_utf8()
            .await?;
    }

    // The enforcer asks for this itself at startup, but doing it here too means
    // the file exists before the first enforcer is even spawned, so a failure
    // to seed cannot be blamed on a missing dump.
    bitcoin_cli
        .command::<String, _, _, _, _>([], "savemempool", Vec::<String>::new())
        .run_utf8()
        .await?;

    let dat_path = setup
        .directories
        .bitcoin_dir
        .join("regtest")
        .join("mempool.dat");
    anyhow::ensure!(
        dat_path.exists(),
        "expected a dump at {} after savemempool",
        dat_path.display()
    );

    let make_enforcer = |mempool_dat: Option<std::path::PathBuf>| Enforcer {
        path: setup.bin_paths.bip300301_enforcer().unwrap().clone(),
        data_dir: setup.directories.enforcer_dir.clone(),
        enable_mempool: true,
        enable_wallet: false,
        enable_block_template_server: false,
        coinbase_recipient: None,
        node_mempool_dat: mempool_dat,
        node_blocks_dir: None,
        node_rpc_user: bitcoind.rpc_user.clone(),
        node_rpc_pass: bitcoind.rpc_pass.clone(),
        node_rpc_port: bitcoind.rpc_port,
        node_zmq_sequence_port: bitcoind.zmq_sequence_port,
        serve_grpc_port: setup.reserved_ports.enforcer_serve_grpc.port(),
        serve_rpc_port: setup.reserved_ports.enforcer_serve_rpc.port(),
        wallet_electrum_rpc_port: 0,
        wallet_electrum_http_port: 0,
    };

    // ---- run 1: the real dump ----
    tracing::info!("starting enforcer with a valid mempool.dat");
    let enforcer = make_enforcer(Some(dat_path.clone()));
    let enforcer_task = enforcer.spawn_command_with_args::<String, String, _, _, _>([], [], {
        let res_tx = res_tx.clone();
        move |err| {
            let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
        }
    });
    wait_for_port(
        "127.0.0.1",
        enforcer.serve_grpc_port,
        Duration::from_secs(30),
    )
    .await?;

    let log = wait_for_enforcer_log(
        &setup.directories.enforcer_dir,
        "the seeded-from-dump line",
        |log| log.contains("seeded "),
    )
    .await?;

    let seeded_line = log
        .lines()
        .find(|line| line.contains("seeded "))
        .ok_or_else(|| anyhow::anyhow!("no seeded line"))?
        .to_owned();
    tracing::info!("fast path reported: {seeded_line}");

    // The dump was taken from this exact mempool, so nothing should be left for
    // RPC. This is the assertion that fails if the flag is silently ignored, or
    // if the file is parsed but the results are not used.
    anyhow::ensure!(
        seeded_line.contains("0 left for RPC"),
        "expected the dump to cover the whole mempool, got: {seeded_line}"
    );
    anyhow::ensure!(
        seeded_line.contains(&format!("seeded {TXS} of {TXS}")),
        "expected all {TXS} txs to be seeded, got: {seeded_line}"
    );

    // Stop this enforcer before starting the next: they share a data dir and a
    // gRPC port.
    drop(enforcer_task);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let log_before_garbage = read_enforcer_log(&setup.directories.enforcer_dir)?.len();

    // ---- run 2: a garbage dump ----
    tracing::info!("starting enforcer with a corrupt mempool.dat");
    let garbage_path = setup.directories.bitcoin_dir.join("mempool.dat.garbage");
    std::fs::write(&garbage_path, vec![0xCDu8; 8192])?;

    let enforcer = make_enforcer(Some(garbage_path));
    let _enforcer_task = enforcer.spawn_command_with_args::<String, String, _, _, _>([], [], {
        let res_tx = res_tx.clone();
        move |err| {
            let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
        }
    });

    // Availability is the point: an unreadable dump must not stop the enforcer
    // from coming up and serving.
    wait_for_port(
        "127.0.0.1",
        enforcer.serve_grpc_port,
        Duration::from_secs(30),
    )
    .await?;

    let log = wait_for_enforcer_log(
        &setup.directories.enforcer_dir,
        "the fallback warning",
        |log| log.len() > log_before_garbage && log.contains("could not read mempool dump"),
    )
    .await?;
    let new_log = &log[log_before_garbage.min(log.len())..];

    anyhow::ensure!(
        new_log.contains("could not read mempool dump"),
        "expected a warning about the unreadable dump"
    );
    // And it must genuinely fall back rather than sync an empty mempool: the
    // second run should report nothing seeded, leaving every tx to RPC.
    anyhow::ensure!(
        !new_log.contains("0 left for RPC"),
        "corrupt dump should have left work for RPC, but reported none"
    );

    Ok(())
}
