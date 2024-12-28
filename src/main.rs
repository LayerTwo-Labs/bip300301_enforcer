use std::{future::Future, net::SocketAddr};

use bip300301::MainClient;
use clap::Parser;
use either::Either;
use futures::{FutureExt as _, TryFutureExt as _};
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

async fn run_grpc_server(validator: Either<Validator, Wallet>, addr: SocketAddr) -> Result<()> {
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
    let mut builder = Server::builder().layer(tracer).add_service(crypto_service);

    let mut reflection_service_builder = tonic_reflection::server::Builder::configure()
        .with_service_name(CryptoServiceServer::<server::CryptoServiceServer>::NAME)
        .with_service_name(ValidatorServiceServer::<Validator>::NAME)
        .register_encoded_file_descriptor_set(proto::ENCODED_FILE_DESCRIPTOR_SET);

    match validator {
        Either::Left(validator) => {
            builder = builder.add_service(ValidatorServiceServer::new(validator));
        }
        Either::Right(wallet) => {
            let validator = wallet.validator().clone();
            builder = builder.add_service(ValidatorServiceServer::new(validator));
            tracing::info!("gRPC: enabling wallet service");
            let wallet_service = WalletServiceServer::new(wallet);
            builder = builder.add_service(wallet_service);
            reflection_service_builder =
                reflection_service_builder.with_service_name(WalletServiceServer::<Wallet>::NAME);
        }
    };

    tracing::info!("Listening for gRPC on {addr} with reflection");

    builder
        .add_service(reflection_service_builder.build_v1().into_diagnostic()?)
        .serve(addr)
        .map_err(|err| miette!("error in grpc server: {err:#}"))
        .await
}

async fn spawn_gbt_server(
    server: cusf_enforcer_mempool::server::Server<Wallet>,
    serve_addr: SocketAddr,
) -> anyhow::Result<jsonrpsee::server::ServerHandle> {
    let rpc_server = server.into_rpc();

    tracing::info!(
        "Listening for JSON-RPC on {} with method(s): {}",
        serve_addr,
        rpc_server
            .method_names()
            .map(|m| format!("`{m}`"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    use cusf_enforcer_mempool::server::RpcServer;
    let handle = jsonrpsee::server::Server::builder()
        .build(serve_addr)
        .await?
        .start(rpc_server);
    Ok(handle)
}

async fn run_gbt_server(
    mining_reward_address: bitcoin::Address,
    network: bitcoin::Network,
    network_info: bip300301::client::NetworkInfo,
    sample_block_template: bip300301::client::BlockTemplate,
    mempool: cusf_enforcer_mempool::mempool::MempoolSync<Wallet>,
    serve_addr: SocketAddr,
) -> anyhow::Result<()> {
    let gbt_server = cusf_enforcer_mempool::server::Server::new(
        mining_reward_address.script_pubkey(),
        mempool,
        network,
        network_info,
        sample_block_template,
    )?;
    let gbt_server_handle = spawn_gbt_server(gbt_server, serve_addr).await?;
    let () = gbt_server_handle.stopped().await;
    Ok(())
}

async fn mempool_task<Enforcer, RpcClient, F, Fut>(
    mut enforcer: Enforcer,
    rpc_client: RpcClient,
    node_zmq_addr_sequence: &str,
    err_tx: futures::channel::oneshot::Sender<anyhow::Error>,
    f: F,
) where
    Enforcer: cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer + Send + Sync + 'static,
    RpcClient: bip300301::client::MainClient + Send + Sync + 'static,
    F: FnOnce(cusf_enforcer_mempool::mempool::MempoolSync<Enforcer>) -> Fut,
    Fut: Future<Output = ()>,
{
    let (sequence_stream, mempool, tx_cache) =
        match cusf_enforcer_mempool::mempool::init_sync_mempool(
            &mut enforcer,
            &rpc_client,
            node_zmq_addr_sequence,
        )
        .await
        {
            Ok(res) => res,
            Err(err) => {
                let err = anyhow::Error::from(err);
                let _send_err: Result<(), _> = err_tx.send(err);
                return;
            }
        };
    tracing::info!("Initial mempool sync complete");
    let mempool = cusf_enforcer_mempool::mempool::MempoolSync::new(
        enforcer,
        mempool,
        tx_cache,
        rpc_client,
        sequence_stream,
        |err| async {
            let err = anyhow::Error::from(err);
            let _send_err: Result<(), _> = err_tx.send(err);
        },
    );
    f(mempool).await
}

fn task(
    validator: Validator,
    cli: cli::Config,
    mainchain_client: bip300301::jsonrpsee::http_client::HttpClient,
    network: bitcoin::Network,
    wallet_data_dir: &std::path::Path,
) -> Result<(
    JoinHandle<()>,
    futures::channel::oneshot::Receiver<anyhow::Error>,
)> {
    use either::Either;
    let enforcer: Either<Validator, Wallet> = if cli.enable_wallet {
        let wallet = Wallet::new(
            wallet_data_dir,
            &cli.wallet_opts,
            mainchain_client.clone(),
            validator,
        )?;
        Either::Right(wallet)
    } else {
        Either::Left(validator)
    };
    let (err_tx, err_rx) = futures::channel::oneshot::channel();
    let _grpc_server_task: JoinHandle<()> = spawn(
        run_grpc_server(enforcer.clone(), cli.serve_grpc_addr)
            .unwrap_or_else(|err| tracing::error!("{err:#}")),
    );
    let res = match (cli.enable_mempool, enforcer) {
        (false, enforcer) => cusf_enforcer_mempool::cusf_enforcer::spawn_task(
            enforcer,
            mainchain_client,
            cli.node_zmq_addr_sequence,
            |err| async {
                let err = anyhow::Error::from(err);
                let _send_err: Result<(), _> = err_tx.send(err);
            },
        ),
        (true, Either::Left(validator)) => spawn(async move {
            mempool_task(
                validator,
                mainchain_client,
                &cli.node_zmq_addr_sequence,
                err_tx,
                |_mempool| futures::future::pending(),
            )
            .await
        }),
        (true, Either::Right(wallet)) => {
            let mining_reward_address = wallet.get_new_address()?;
            spawn(async move {
                let network_info = match mainchain_client.get_network_info().await {
                    Ok(network_info) => network_info,
                    Err(err) => {
                        let err = anyhow::Error::from(err);
                        tracing::error!("{err:#}");
                        return;
                    }
                };
                let sample_block_template = {
                    let mut request = bip300301::client::BlockTemplateRequest::default();
                    if network == bitcoin::Network::Signet {
                        request.rules.push("signet".to_owned())
                    }
                    match mainchain_client.get_block_template(request).await {
                        Ok(block_template) => block_template,
                        Err(err) => {
                            let err = anyhow::Error::from(err);
                            tracing::error!("failed to get sample block template: {err:#}");
                            return;
                        }
                    }
                };
                mempool_task(
                    wallet,
                    mainchain_client,
                    &cli.node_zmq_addr_sequence,
                    err_tx,
                    |mempool| async {
                        match run_gbt_server(
                            mining_reward_address,
                            network,
                            network_info,
                            sample_block_template,
                            mempool,
                            cli.serve_rpc_addr,
                        )
                        .await
                        {
                            Ok(()) => (),
                            Err(err) => tracing::error!("{err:#}"),
                        }
                    },
                )
                .await
            })
        }
    };
    Ok((res, err_rx))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Config::parse();
    set_tracing_subscriber(cli.log_level)?;

    tracing::info!(
        "starting up bip300301_enforcer with data directory {}",
        cli.data_dir.display()
    );

    let mainchain_client =
        rpc_client::create_client(&cli.node_rpc_opts, cli.enable_wallet && cli.enable_mempool)?;

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

    let validator = Validator::new(mainchain_client.clone(), &validator_data_dir)
        .await
        .into_diagnostic()?;

    let (_task, err_rx) = task(
        validator,
        cli,
        mainchain_client,
        info.chain,
        &wallet_data_dir,
    )?;

    match futures::future::select(err_rx, ctrl_c().boxed()).await {
        futures::future::Either::Left((Ok(err), _ctrl_c_handler)) => {
            tracing::error!("{err:#}")
        }
        futures::future::Either::Left((
            Err(futures::channel::oneshot::Canceled),
            _ctrl_c_handler,
        )) => {
            tracing::info!("Shutting down due to validator error")
        }
        futures::future::Either::Right((Ok(()), _err_rx)) => {
            tracing::info!("Shutting down")
        }
        futures::future::Either::Right((Err(ctrl_c_err), _err_rx)) => {
            tracing::error!("Shutting down due to error in ctrl-c handler: {ctrl_c_err:#}")
        }
    }
    Ok(())
}
