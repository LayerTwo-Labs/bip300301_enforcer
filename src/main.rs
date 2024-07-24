use std::{net::SocketAddr, path::Path};

mod bip300;
mod server;
mod types;

use futures::{
    future::{self, Either},
    FutureExt, TryFutureExt,
};
use miette::{miette, IntoDiagnostic, Result};
use server::{validator::validator_server::ValidatorServer, Bip300};
use tonic::transport::Server;
use ureq_jsonrpc::Client;

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

async fn run_server(bip300: Bip300, addr: SocketAddr) -> Result<()> {
    println!("Listening for gRPC on {addr}");
    Server::builder()
        .add_service(ValidatorServer::new(bip300))
        .serve(addr)
        .map(|res| res.into_diagnostic())
        .await
}

#[tokio::main]
async fn main() -> Result<()> {
    let bip300 = Bip300::new(Path::new("./"))?;
    let task = bip300
        .run()
        .map(|res| res.into_diagnostic())
        .unwrap_or_else(|err| eprintln!("{err:#}"));
    let addr: SocketAddr = "[::1]:50051".parse().into_diagnostic()?;
    //let ((), ()) = future::try_join(task.map(Ok), run_server(bip300, addr)).await?;
    match future::select(task, run_server(bip300, addr).boxed()).await {
        Either::Left(((), server_task)) => {
            // continue to run server task
            server_task.await
        }
        Either::Right((res, _task)) => res,
    }
}
