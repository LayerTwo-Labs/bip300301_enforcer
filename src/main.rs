use std::{net::SocketAddr, path::Path, time::Duration};

use clap::Parser;
use futures::{future::TryFutureExt, FutureExt, StreamExt};
use miette::{miette, IntoDiagnostic, Result};
use tokio::{spawn, task::JoinHandle, time::interval};
use tonic::transport::Server;
use tracing_subscriber::{filter as tracing_filter, layer::SubscriberExt};

mod cli;
mod proto;
mod rpc_client;
mod server;
mod types;
mod validator;
mod wallet;
mod zmq;

use proto::validator::Server as ValidatorServiceServer;
use server::Validator;
use wallet::Wallet;

// Configure logger.
fn set_tracing_subscriber(log_level: tracing::Level) -> miette::Result<()> {
    let targets_filter = tracing_filter::EnvFilter::builder()
        .with_default_directive(tracing_filter::LevelFilter::from_level(log_level).into())
        .from_env()
        .into_diagnostic()?;
    let stdout_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_file(true)
        .with_line_number(true);
    let tracing_subscriber = tracing_subscriber::registry()
        .with(targets_filter)
        .with(stdout_layer);
    tracing::subscriber::set_global_default(tracing_subscriber)
        .into_diagnostic()
        .map_err(|err| miette::miette!("setting default subscriber failed: {err:#}"))
}

async fn run_server(validator: Validator, addr: SocketAddr) -> Result<()> {
    tracing::info!("Listening for gRPC on {addr}");
    Server::builder()
        .add_service(ValidatorServiceServer::new(validator))
        .serve(addr)
        .map_err(|err| miette!("error in validator server: {err:#}"))
        .await
}

// TODO: return `Result<!, _>` once `never_type` is stabilized
async fn wallet_task(mut wallet: Wallet) -> Result<(), miette::Report> {
    const SYNC_INTERVAL: Duration = Duration::from_secs(15);
    let mut interval_stream = tokio_stream::wrappers::IntervalStream::new(interval(SYNC_INTERVAL));
    while let Some(_tick) = interval_stream.next().await {
        let () = wallet.sync()?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Config::parse();
    set_tracing_subscriber(cli.log_level)?;

    let mainchain_client = rpc_client::create_client(&cli.node_rpc_opts)?;
    let (err_tx, err_rx) = futures::channel::oneshot::channel();
    let validator = Validator::new(
        mainchain_client.clone(),
        cli.node_zmq_addr_sequence,
        Path::new("./"),
        |err| async {
            let _send_err: Result<(), _> = err_tx.send(err);
        },
    )
    .into_diagnostic()?;

    let wallet: Option<Wallet> = if cli.enable_wallet {
        Some(
            Wallet::new(
                cli.data_dir,
                &cli.wallet_opts,
                mainchain_client,
                validator.clone(),
            )
            .map_err(|e| miette!("failed to create wallet: {:?}", e))
            .await?,
        )
    } else {
        None
    };

    let _handle_validator_errors: JoinHandle<()> = spawn({
        err_rx.map(|err| {
            if let Ok(err) = err {
                tracing::error!("{err:#}");
            }
        })
    });
    let _sync_wallet: Option<JoinHandle<()>> = wallet
        .map(|wallet| spawn(wallet_task(wallet).unwrap_or_else(|err| tracing::error!("{err:#}"))));

    run_server(validator, cli.serve_rpc_addr).await
}
