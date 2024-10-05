use std::{net::SocketAddr, path::Path, sync::Arc};

mod cli;
mod gen;
mod jsonrpc;
mod server;
mod types;
mod validator;
mod wallet;

use clap::Parser;

use futures::{future::try_join_all, TryFutureExt};
use gen::validator::validator_service_server::ValidatorServiceServer;
use miette::{miette, Result};

use server::Validator;

use tonic::transport::Server;
use wallet::Wallet;

async fn run_server(bip300: &Validator, addr: SocketAddr) -> Result<()> {
    log::info!("Listening for gRPC on {addr}");
    Server::builder()
        .add_service(ValidatorServiceServer::new(bip300.clone()))
        .serve(addr)
        .map_err(|err| miette!("error in validator server: {err:#}"))
        .await
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let cli = Arc::new(cli::Config::parse());

    let validator = Validator::new(Path::new("./")).map(Arc::new)?;

    // Takes in data from the blockchain and updates the validator state
    let run_validator_task = tokio::spawn({
        log::info!("spawning validator task");

        let validator = Arc::clone(&validator);
        let cli = Arc::clone(&cli);
        async move { validator.run(&cli).await }
    });

    let mut tasks: Vec<tokio::task::JoinHandle<Result<(), miette::Error>>> = Vec::new();
    tasks.push(run_validator_task);

    let run_validator_server_task = tokio::spawn({
        log::info!("spawning validator server task");

        let validator = Arc::clone(&validator);
        let cli = Arc::clone(&cli);
        async move { run_server(&validator, cli.serve_rpc_addr).await }
    });
    tasks.push(run_validator_server_task);

    // "Start" the wallet. We're going to add a server here, and run this in a spawned task.
    // That requires a bit more:
    // 1. Proper configuration for connecting the wallet to the blockchain
    // 2. A server for the wallet
    //
    // The point here is to prove that we can conditionally start a task.
    if cli.enable_wallet {
        let validator = Arc::clone(&validator);
        let wallet = Wallet::new(&cli, &validator)
            .map_err(|e| miette!("failed to create wallet: {:?}", e))
            .await?;

        let run_wallet_task = tokio::spawn({
            log::info!("spawning wallet task");
            async move {
                // this prints the wallet balance
                // TODO: take the print statements ouf of the wallet, and into a return value
                wallet
                    .get_balance()
                    .map_err(|e| miette!("failed to get wallet balance: {:?}", e))?;
                Ok(())
            }
        });

        tasks.push(run_wallet_task);
    }

    // Wait for the first error or for all tasks to complete
    let result = try_join_all(tasks.into_iter().map(|t| {
        Box::pin(async move {
            t.await
                .unwrap_or_else(|e| Err(miette!("Task panicked: {}", e)))
        })
    }))
    .await;

    // Check if there was an error
    result?;
    Ok(())
}
