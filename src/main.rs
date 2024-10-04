use std::{net::SocketAddr, path::Path, sync::Arc};

mod cli;
mod gen;
mod server;
mod types;
mod validator;
mod wallet;

use clap::Parser;

use futures::{future::select_all, TryFutureExt};
use gen::validator::validator_service_server::ValidatorServiceServer;
use miette::{miette, IntoDiagnostic, Result};

use server::Validator;

use tonic::transport::Server;
use ureq_jsonrpc::Client;
use wallet::Wallet;

fn _create_client(main_datadir: &Path) -> Result<Client> {
    let auth = std::fs::read_to_string(main_datadir.join("regtest/.cookie")).into_diagnostic()?;
    let mut auth = auth.split(':');
    let user = auth
        .next()
        .ok_or(miette!("failed to get rpcuser"))?
        .to_string();
    let password = auth
        .next()
        .ok_or(miette!("failed to get rpcpassword"))?
        .to_string();
    Ok(Client {
        host: "localhost".into(),
        port: 18443,
        user,
        password,
        id: "mainchain".into(),
    })
}

async fn run_server(bip300: &Validator, addr: SocketAddr) -> Result<()> {
    println!("Listening for gRPC on {addr}");
    Server::builder()
        .add_service(ValidatorServiceServer::new(bip300.clone()))
        .serve(addr)
        .map_err(|err| miette!("error in validator server: {err:#}"))
        .await
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Config::parse();
    let serve_rpc_addr = cli.serve_rpc_addr;

    let enabled_wallet = cli.enable_wallet;

    let validator = match Validator::new(Path::new("./")) {
        Ok(validator) => Arc::new(validator),
        Err(err) => {
            return Err(err);
        }
    };

    // Takes in data from the blockchain and updates the validator state
    let run_validator_task = tokio::spawn({
        let validator = Arc::clone(&validator);
        async move { validator.run(&cli).await }
    });

    let mut tasks: Vec<tokio::task::JoinHandle<Result<(), miette::Error>>> = Vec::new();
    tasks.push(run_validator_task);

    let run_validator_server_task = tokio::spawn({
        let validator = Arc::clone(&validator);
        async move { run_server(&validator, serve_rpc_addr).await }
    });
    tasks.push(run_validator_server_task);

    // "Start" the wallet. We're going to add a server here, and run this in a spawned task.
    // That requires a bit more:
    // 1. Proper configuration for connecting the wallet to the blockchain
    // 2. A server for the wallet
    //
    // The point here is to prove that we can conditionally start a task.
    if enabled_wallet {
        let validator = Arc::clone(&validator);
        let wallet = Wallet::new(Path::new("./wallet-db"), &validator)
            .map_err(|e| miette!("failed to create wallet: {:?}", e))
            .await?;

        let run_wallet_task = tokio::spawn({
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
    let result = select_all(tasks.into_iter().map(|t| {
        Box::pin(async move {
            t.await
                .unwrap_or_else(|e| Err(miette!("Task panicked: {}", e)))
        })
    }))
    .await;

    // Check if there was an error
    if let (Err(e), _, _) = result {
        return Err(e);
    }
    Ok(())

    /*
    match try_join_all(tasks.into_iter()).await {
        Ok(_) => {
            println!("Validator: tasks completed");
            Ok(())
        }
        Err(err) => {
            return Err(miette!("unable to run tasks: {err:#}"));
        }
        hm => {
            println!("hm: {:?}", hm);
            Ok(())
        }
    }*/
}
