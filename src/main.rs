use std::net::SocketAddr;

use bip300301::MainClient;
use clap::Parser;
use futures::{future::Either, FutureExt as _, TryFutureExt as _};
use miette::{miette, IntoDiagnostic, Result};
use tokio::{signal::ctrl_c, spawn, task::JoinHandle};
use tonic::{server::NamedService, transport::Server};
use tower::ServiceBuilder;
use tower_http::trace::{DefaultOnFailure, DefaultOnResponse, TraceLayer};
use tracing_subscriber::{filter as tracing_filter, layer::SubscriberExt};

use bip300301_enforcer_lib::{
    cli,
    proto::{
        self,
        crypto::crypto_service_server::CryptoServiceServer,
        mainchain::{wallet_service_server::WalletServiceServer, Server as ValidatorServiceServer},
    },
    server,
    validator::Validator,
    wallet,
};

mod rpc_client;

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

async fn run_server(validator: Validator, wallet: Option<Wallet>, addr: SocketAddr) -> Result<()> {
    let tracer = ServiceBuilder::new()
        .layer(
            TraceLayer::new_for_grpc()
                .on_request(())
                .on_eos(())
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO))
                .on_failure(DefaultOnFailure::new().level(tracing::Level::ERROR)),
        )
        .into_inner();

    let crypto_service = CryptoServiceServer::new(server::CryptoServiceServer);
    let validator_service = ValidatorServiceServer::new(validator);

    let mut builder = Server::builder()
        .layer(tracer)
        .add_service(crypto_service)
        .add_service(validator_service);

    let mut reflection_service_builder = tonic_reflection::server::Builder::configure()
        .with_service_name(CryptoServiceServer::<server::CryptoServiceServer>::NAME)
        .with_service_name(ValidatorServiceServer::<Validator>::NAME)
        .register_encoded_file_descriptor_set(proto::ENCODED_FILE_DESCRIPTOR_SET);

    if let Some(wallet) = wallet {
        tracing::info!("gRPC: enabling wallet service");

        let wallet_service = WalletServiceServer::new(wallet);
        builder = builder.add_service(wallet_service);
        reflection_service_builder =
            reflection_service_builder.with_service_name(WalletServiceServer::<Wallet>::NAME);
    }

    tracing::info!("Listening for gRPC on {addr} with reflection");

    builder
        .add_service(reflection_service_builder.build_v1().into_diagnostic()?)
        .serve(addr)
        .map_err(|err| miette!("error in validator server: {err:#}"))
        .await
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Config::parse();
    set_tracing_subscriber(cli.log_level)?;

    tracing::info!(
        "starting up bip300301_enforcer with data directory {}",
        cli.data_dir.display()
    );

    let mainchain_client = rpc_client::create_client(&cli.node_rpc_opts)?;

    tracing::info!(
        "Created mainchain client from options: {}:{}@{}",
        cli.node_rpc_opts.user.as_deref().unwrap_or("cookie"),
        cli.node_rpc_opts
            .pass
            .as_deref()
            .map(|_| "*****")
            .unwrap_or("cookie"),
        cli.node_rpc_opts.addr,
    );

    let info = mainchain_client
        .get_blockchain_info()
        .await
        .map_err(|err| wallet::error::BitcoinCoreRPC {
            method: "getblockchaininfo".to_string(),
            error: err,
        })?;

    tracing::info!(
        network = %info.chain,
        blocks = %info.blocks,
        "Connected to mainchain client",
    );

    // Both wallet data and validator data are stored under the same root
    // directory. Add a subdirectories to clearly indicate which
    // is which.
    let validator_data_dir = cli.data_dir.join("validator").join(info.chain.to_string());
    let wallet_data_dir = cli.data_dir.join("wallet").join(info.chain.to_string());

    // Ensure that the data directories exists
    for data_dir in [validator_data_dir.clone(), wallet_data_dir.clone()] {
        std::fs::create_dir_all(data_dir).into_diagnostic()?;
    }

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

    let wallet: Option<wallet::Wallet> = if cli.enable_wallet {
        let wallet = Wallet::new(
            &wallet_data_dir,
            &cli.wallet_opts,
            mainchain_client,
            validator.clone(),
        )?;
        Some(wallet)
    } else {
        None
    };

    let _server_task: JoinHandle<()> = spawn(
        run_server(validator, wallet, cli.serve_rpc_addr)
            .unwrap_or_else(|err| tracing::error!("{err:#}")),
    );

    match futures::future::select(err_rx, ctrl_c().boxed()).await {
        Either::Left((Ok(err), _ctrl_c_handler)) => {
            tracing::error!("{err:#}")
        }
        Either::Left((Err(futures::channel::oneshot::Canceled), _ctrl_c_handler)) => {
            tracing::info!("Shutting down due to validator error")
        }
        Either::Right((Ok(()), _err_rx)) => {
            tracing::info!("Shutting down")
        }
        Either::Right((Err(ctrl_c_err), _err_rx)) => {
            tracing::error!("Shutting down due to error in ctrl-c handler: {ctrl_c_err:#}")
        }
    }
    Ok(())
}
