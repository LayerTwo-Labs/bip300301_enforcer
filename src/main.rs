use std::{net::SocketAddr, sync::Arc, time::Duration};

use clap::Parser;
use futures::{future::TryFutureExt, FutureExt, StreamExt};
use miette::{miette, IntoDiagnostic, Result};
use tokio::{spawn, task::JoinHandle, time::interval};
use tonic::{server::NamedService, transport::Server};
use tower::ServiceBuilder;
use tower_http::trace::{DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tracing_subscriber::{filter as tracing_filter, layer::SubscriberExt};

mod cli;
mod messages;
mod proto;
mod rpc_client;
mod server;
mod types;
mod validator;
mod wallet;
mod zmq;

use proto::mainchain::{
    wallet_service_server::WalletServiceServer, Server as ValidatorServiceServer,
};
use validator::Validator;
use wallet::Wallet;

/// Saturating predecessor of a log level
fn saturating_pred_level(log_level: tracing::Level) -> tracing::Level {
    match log_level {
        tracing::Level::TRACE => tracing::Level::DEBUG,
        tracing::Level::DEBUG => tracing::Level::INFO,
        tracing::Level::INFO => tracing::Level::WARN,
        tracing::Level::WARN => tracing::Level::ERROR,
        tracing::Level::ERROR => tracing::Level::ERROR,
    }
}

/// The empty string target `""` can be used to set a default level.
fn targets_directive_str<'a, Targets>(targets: Targets) -> String
where
    Targets: IntoIterator<Item = (&'a str, tracing::Level)>,
{
    targets
        .into_iter()
        .map(|(target, level)| {
            let level = level.as_str().to_ascii_lowercase();
            if target.is_empty() {
                level
            } else {
                format!("{target}={level}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

// Configure logger.
fn set_tracing_subscriber(log_level: tracing::Level) -> miette::Result<()> {
    let targets_filter = {
        let default_directives_str = targets_directive_str([
            ("", saturating_pred_level(log_level)),
            ("bip300301", log_level),
            ("jsonrpsee_core::tracing", log_level),
            ("bip300301_enforcer", log_level),
        ]);
        let directives_str = match std::env::var(tracing_filter::EnvFilter::DEFAULT_ENV) {
            Ok(env_directives) => format!("{default_directives_str},{env_directives}"),
            Err(std::env::VarError::NotPresent) => default_directives_str,
            Err(err) => return Err(err).into_diagnostic(),
        };
        tracing_filter::EnvFilter::builder()
            .parse(directives_str)
            .into_diagnostic()?
    };
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

async fn run_server(
    validator: Validator,
    wallet: Option<Arc<Wallet>>,
    addr: SocketAddr,
) -> Result<()> {
    let validator_service = ValidatorServiceServer::new(validator);

    let mut reflection_service_builder = tonic_reflection::server::Builder::configure()
        .with_service_name(ValidatorServiceServer::<Validator>::NAME)
        .register_encoded_file_descriptor_set(proto::ENCODED_FILE_DESCRIPTOR_SET);

    tracing::info!("Listening for gRPC on {addr} with reflection");

    let tracer = ServiceBuilder::new()
        .layer(
            TraceLayer::new_for_grpc()
                .on_request(())
                .on_eos(())
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO))
                .on_failure(DefaultOnFailure::new().level(tracing::Level::ERROR)),
        )
        .into_inner();

    let mut builder = Server::builder()
        .layer(tracer)
        .add_service(validator_service);

    if let Some(wallet) = wallet {
        tracing::info!("gRPC: enabling wallet service");

        let wallet_service = WalletServiceServer::new(Arc::clone(&wallet));
        builder = builder.add_service(wallet_service);

        reflection_service_builder =
            reflection_service_builder.with_service_name(WalletServiceServer::<Wallet>::NAME);

        let _sync_wallet: JoinHandle<()> = {
            let wallet = Arc::clone(&wallet);
            spawn(wallet_task(wallet).unwrap_or_else(|err| tracing::error!("{err:#}")))
        };
    }

    builder
        .add_service(reflection_service_builder.build_v1().into_diagnostic()?)
        .serve(addr)
        .map_err(|err| miette!("error in validator server: {err:#}"))
        .await
}

// TODO: return `Result<!, _>` once `never_type` is stabilized
async fn wallet_task(wallet: Arc<wallet::Wallet>) -> Result<(), miette::Report> {
    const SYNC_INTERVAL: Duration = Duration::from_secs(15);
    let mut interval_stream = tokio_stream::wrappers::IntervalStream::new(interval(SYNC_INTERVAL));
    while let Some(_tick) = interval_stream.next().await {
        match wallet.sync() {
            Ok(_) => (),
            Err(err) => tracing::error!("wallet sync error: {err:#}"),
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Config::parse();
    set_tracing_subscriber(cli.log_level)?;

    tracing::info!(
        "starting up bip300301_enforcer with data directory {}",
        cli.data_dir.display()
    );

    // Both wallet data and validator data are stored under the same root
    // directory. Add a subdirectories to clearly indicate which
    // is which.
    let validator_data_dir = cli.data_dir.join("validator");
    let wallet_data_dir = cli.data_dir.join("wallet");

    // Ensure that the data directories exists
    for data_dir in [validator_data_dir.clone(), wallet_data_dir.clone()] {
        std::fs::create_dir_all(data_dir).into_diagnostic()?;
    }

    let mainchain_client = rpc_client::create_client(&cli.node_rpc_opts)?;
    let (err_tx, err_rx) = futures::channel::oneshot::channel();
    let validator = Validator::new(
        mainchain_client.clone(),
        cli.node_zmq_addr_sequence,
        &validator_data_dir,
        |err| async {
            let _send_err: Result<(), _> = err_tx.send(err);
        },
    )
    .await
    .into_diagnostic()?;

    let wallet: Option<Arc<wallet::Wallet>> = if cli.enable_wallet {
        let wallet = Wallet::new(
            &wallet_data_dir,
            &cli.wallet_opts,
            mainchain_client,
            validator.clone(),
        )
        .map_err(|e| miette!("failed to create wallet: {:?}", e))
        .await?;
        Some(Arc::new(wallet))
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

    run_server(validator, wallet, cli.serve_rpc_addr).await
}
