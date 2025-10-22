use std::time::Duration;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::mainchain::{GetChainTipRequest, validator_service_client::ValidatorServiceClient},
};
use futures::channel::mpsc;
use tokio::time::sleep;
use tonic::Code;

use crate::{
    setup::{PreSetup, new_bitcoin_cli, new_bitcoind, wait_for_port},
    util::Enforcer,
};

pub async fn test_file_based_block_parser(setup: PreSetup) -> anyhow::Result<()> {
    let (res_tx, _) = mpsc::unbounded::<anyhow::Result<()>>();

    let bitcoind = new_bitcoind(
        &setup.bin_paths,
        setup.directories.bitcoin_dir.clone(),
        &setup.reserved_ports,
        setup.network,
        None,
    );

    tracing::info!("Starting bitcoind for the first time");
    let first_bitcoind = bitcoind.spawn_command_with_args::<String, String, _, _, _>([], [], {
        let res_tx = res_tx.clone();
        move |err| {
            tracing::error!("Error starting bitcoind: {err:#}");
            let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
        }
    });

    // wait for startup
    sleep(std::time::Duration::from_secs(1)).await;

    let bitcoin_cli = new_bitcoin_cli(&bitcoind, setup.bin_paths.bitcoin_cli.clone());

    tracing::info!("Generating blocks");
    // just generate to a random regtest address. we don't actually need the coins!
    let address = "bcrt1qrmxr2qc8eedpqw8wsdtg4spzkcmcs2adkrc9rh";
    let generated_blocks: Vec<String> = {
        let res = bitcoin_cli
            .command::<String, _, _, _, _>([], "generatetoaddress", ["100", address])
            .run_utf8()
            .await?;

        serde_json::from_str(&res)?
    };

    // Flush the blocks to disk
    tracing::info!("Stopping bitcoind");
    bitcoin_cli
        .command::<String, _, String, _, _>([], "stop", [])
        .run_utf8()
        .await?;

    first_bitcoind.into_inner().await?;

    // Restart bitcoind
    tracing::info!("Restarting bitcoind");
    let _second_bitcoind = bitcoind.spawn_command_with_args::<String, String, _, _, _>([], [], {
        let res_tx = res_tx.clone();
        move |err| {
            let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
        }
    });

    // wait for startup
    sleep(std::time::Duration::from_secs(1)).await;

    let enforcer = Enforcer {
        path: setup.bin_paths.bip300301_enforcer.clone(),
        data_dir: setup.directories.enforcer_dir.clone(),
        enable_mempool: false,
        enable_wallet: false,
        node_blocks_dir: Some(
            setup
                .directories
                .bitcoin_dir
                .clone()
                .join("regtest")
                .join("blocks"),
        ),
        node_rpc_user: bitcoind.rpc_user,
        node_rpc_pass: bitcoind.rpc_pass,
        node_rpc_port: bitcoind.rpc_port,
        node_zmq_sequence_port: bitcoind.zmq_sequence_port,
        serve_grpc_port: setup.reserved_ports.enforcer_serve_grpc.port(),
        serve_json_rpc_port: setup.reserved_ports.enforcer_serve_json_rpc.port(),
        serve_rpc_port: setup.reserved_ports.enforcer_serve_rpc.port(),
        wallet_electrum_rpc_port: 0,
        wallet_electrum_http_port: 0,
    };

    tracing::info!("Starting enforcer");
    let _enforcer_task = enforcer.spawn_command_with_args::<_, String, _, _, _>(
        [(
            "RUST_LOG",
            "h2=info,hyper_util=info,jsonrpsee-client=debug,jsonrpsee-http=debug,tonic=debug,trace",
        )],
        [],
        move |err| {
            let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
        },
    );

    // Wait for enforcer gRPC port to open
    wait_for_port(
        "127.0.0.1",
        enforcer.serve_grpc_port,
        Duration::from_secs(10),
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed waiting for enforcer gRPC port: {e}"))?;

    // verify we were able to sync

    let mut client =
        ValidatorServiceClient::connect(format!("http://127.0.0.1:{}", enforcer.serve_grpc_port))
            .await?;

    let header_info = loop {
        match client.get_chain_tip(GetChainTipRequest {}).await {
            Ok(enforcer_tip) => {
                break enforcer_tip
                    .into_inner()
                    .block_header_info
                    .expect("no block header info");
            }
            // validator is not synced
            Err(err) if err.code() == Code::Unavailable => {
                tracing::debug!("Validator is not synced yet, retrying...");
                sleep(Duration::from_millis(100)).await;
                continue;
            }
            Err(err) => return Err(anyhow::anyhow!("Error getting enforcer tip: {err}")),
        };
    };

    // Verify we were able to sync to the correct height
    assert_eq!(header_info.height, 100);
    assert_eq!(generated_blocks.len(), 100);
    assert_eq!(
        *generated_blocks.last().expect("no last block"),
        header_info
            .block_hash
            .expect("no block hash")
            .hex
            .expect("no hex"),
    );

    // Verify we did it by parsing the block files
    // Kinda cooky: do it by parsing the stdout file of the enforcer

    let stdout_lines = {
        let stdout = std::fs::read_to_string(setup.directories.enforcer_dir.join("stdout.txt"))?;
        stdout
            .split("\n")
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    };

    assert!(
        stdout_lines
            .iter()
            .any(|line| line.contains("Completed block sync from files: 101 blocks processed"))
    );

    Ok(())
}
