use std::{future::Future, net::SocketAddr, path::Path, time::Duration};

use bip300301::MainClient;
use clap::Parser;
use either::Either;
use futures::{channel::oneshot, TryFutureExt as _};
use http::{header::HeaderName, Request};

use miette::{miette, IntoDiagnostic, Result};
use tokio::{net::TcpStream, signal::ctrl_c, spawn, task::JoinHandle};
use tonic::{server::NamedService, transport::Server};
use tower::ServiceBuilder;
use tower_http::{
    request_id::{MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::{DefaultOnFailure, DefaultOnResponse, TraceLayer},
};
use tracing::Instrument;
use tracing_subscriber::{filter as tracing_filter, layer::SubscriberExt};

use bip300301_enforcer_lib::{
    cli::{self, LogFormatter},
    p2p::compute_signet_magic,
    proto::{
        self,
        crypto::crypto_service_server::CryptoServiceServer,
        mainchain::{wallet_service_server::WalletServiceServer, Server as ValidatorServiceServer},
    },
    rpc_client, server,
    validator::Validator,
    wallet,
};

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

// Configure logger. The returned guard should be dropped when the program
// exits.
fn set_tracing_subscriber(
    log_file: &Path,
    log_formatter: LogFormatter,
    log_level: tracing::Level,
) -> miette::Result<tracing_appender::non_blocking::WorkerGuard> {
    let targets_filter = {
        let default_directives_str = targets_directive_str([
            ("", saturating_pred_level(log_level)),
            ("bip300301", log_level),
            ("cusf_enforcer_mempool", log_level),
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
    // If no writer is provided (as here!), logs end up at stdout.
    let mut stdout_layer = tracing_subscriber::fmt::layer()
        .event_format(log_formatter.with_file(true).with_line_number(true))
        .fmt_fields(log_formatter);
    let is_terminal = std::io::IsTerminal::is_terminal(&stdout_layer.writer()());
    stdout_layer.set_ansi(is_terminal);

    let log_file_dir = log_file.parent().ok_or(miette!("log file has no parent"))?;
    let log_file_name = log_file
        .file_name()
        .ok_or(miette!("log file has no name"))?;

    let file_appender = tracing_appender::rolling::never(log_file_dir, log_file_name);

    // Ensure the appender is non-blocking!
    let (file_appender, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .event_format(log_formatter.with_file(true).with_line_number(true))
        .fmt_fields(log_formatter)
        .with_ansi(false);
    let tracing_subscriber = tracing_subscriber::registry()
        .with(targets_filter)
        .with(stdout_layer)
        .with(file_layer);

    tracing::subscriber::set_global_default(tracing_subscriber)
        .into_diagnostic()
        .map_err(|err| miette::miette!("setting default subscriber failed: {err:#}"))?;

    Ok(guard)
}

async fn get_block_template<RpcClient>(
    rpc_client: &RpcClient,
    network: bitcoin::Network,
) -> Result<bip300301::client::BlockTemplate, wallet::error::BitcoinCoreRPC>
where
    RpcClient: MainClient + Sync,
{
    let mut request = bip300301::client::BlockTemplateRequest::default();
    if network == bitcoin::Network::Signet {
        request.rules.push("signet".to_owned())
    }
    rpc_client
        .get_block_template(request)
        .await
        .map_err(|err| wallet::error::BitcoinCoreRPC {
            method: "getblocktemplate".to_string(),
            error: err,
        })
}

#[derive(Clone, Debug)]
struct RequestIdMaker;

impl MakeRequestId for RequestIdMaker {
    fn make_request_id<B>(&mut self, _: &Request<B>) -> Option<RequestId> {
        use uuid::Uuid;
        // the 'simple' format renders the UUID with no dashes, which
        // makes for easier copy/pasting.
        let id = Uuid::new_v4();
        let id = id.as_simple();
        let id = format!("req_{id}"); // prefix all IDs with "req_", to make them easier to identify

        let Ok(header_value) = http::HeaderValue::from_str(&id) else {
            return None;
        };

        Some(RequestId::new(header_value))
    }
}

async fn run_grpc_server(validator: Either<Validator, Wallet>, addr: SocketAddr) -> Result<()> {
    const REQUEST_ID_HEADER: &str = "x-request-id";

    // Ordering here matters! Order here is from official docs on request IDs tracings
    // https://docs.rs/tower-http/latest/tower_http/request_id/index.html#using-trace
    let tracer = ServiceBuilder::new()
        .layer(SetRequestIdLayer::new(
            HeaderName::from_static(REQUEST_ID_HEADER),
            RequestIdMaker,
        ))
        .layer(
            TraceLayer::new_for_grpc()
                .make_span_with(move |request: &Request<_>| {
                    let request_id = request
                        .headers()
                        .get(HeaderName::from_static(REQUEST_ID_HEADER))
                        .and_then(|h| h.to_str().ok())
                        .filter(|s| !s.is_empty());

                    tracing::span!(
                        tracing::Level::DEBUG,
                        "request",
                        method = %request.method(),
                        uri = %request.uri(),
                        request_id , // this is needed for the record call below to work
                    )
                })
                .on_request(())
                .on_eos(())
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO))
                .on_failure(DefaultOnFailure::new().level(tracing::Level::ERROR)),
        )
        .layer(PropagateRequestIdLayer::new(HeaderName::from_static(
            REQUEST_ID_HEADER,
        )))
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

    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();

    // Set all services to have the "serving" status.
    // TODO: somehow expose the health reporter to the running services, and
    // dynamically update if we're running into issues.
    for service in [
        ValidatorServiceServer::<Validator>::NAME,
        WalletServiceServer::<Wallet>::NAME,
        CryptoServiceServer::<server::CryptoServiceServer>::NAME,
    ] {
        tracing::debug!("Setting health status for service: {service}");
        health_reporter
            .set_service_status(service, tonic_health::ServingStatus::Serving)
            .await;
    }

    tracing::info!("Listening for gRPC on {addr} with reflection");

    builder
        .add_service(reflection_service_builder.build_v1().into_diagnostic()?)
        .add_service(health_service)
        .serve(addr)
        .map_err(|err| miette!("serve gRPC at `{addr}`: {err:#}"))
        .await
}

async fn spawn_gbt_server(
    server: cusf_enforcer_mempool::server::Server<Wallet>,
    serve_addr: SocketAddr,
) -> miette::Result<jsonrpsee::server::ServerHandle> {
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
        .await
        .map_err(|err| miette!("initialize JSON-RPC server at `{serve_addr}`: {err:#}"))?
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
) -> miette::Result<()> {
    let gbt_server = cusf_enforcer_mempool::server::Server::new(
        mining_reward_address.script_pubkey(),
        mempool,
        network,
        network_info,
        sample_block_template,
    )
    .into_diagnostic()?;
    let gbt_server_handle = spawn_gbt_server(gbt_server, serve_addr).await?;
    let () = gbt_server_handle.stopped().await;
    Ok(())
}

async fn is_address_port_open(addr: &str) -> Result<bool> {
    let addr = addr.strip_prefix("tcp://").unwrap_or_default();
    match tokio::time::timeout(Duration::from_millis(250), TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => Ok(true),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(false),
        Ok(Err(e)) => Err(miette!("failed to connect to {addr}: {e:#}")),
        Err(_) => Ok(false),
    }
}

async fn mempool_task<Enforcer, RpcClient, F, Fut>(
    mut enforcer: Enforcer,
    rpc_client: RpcClient,
    zmq_addr_sequence: &str,
    err_tx: oneshot::Sender<miette::Report>,
    f: F,
) where
    Enforcer: cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer + Send + Sync + 'static,
    RpcClient: bip300301::client::MainClient + Send + Sync + 'static,
    F: FnOnce(cusf_enforcer_mempool::mempool::MempoolSync<Enforcer>) -> Fut,
    Fut: Future<Output = ()>,
{
    tracing::debug!(%zmq_addr_sequence, "Ensuring ZMQ address for mempool sync is reachable");

    match is_address_port_open(zmq_addr_sequence).await {
        Ok(true) => (),
        Ok(false) => {
            let err = miette::miette!(
                "ZMQ address for mempool sync is not reachable: {zmq_addr_sequence}"
            );
            let _send_err: Result<(), _> = err_tx.send(err);
            return;
        }
        Err(err) => {
            let err = miette::miette!("failed to check if ZMQ address is reachable: {err:#}");
            let _send_err: Result<(), _> = err_tx.send(err);
            return;
        }
    }

    let init_sync_mempool_future = cusf_enforcer_mempool::mempool::init_sync_mempool(
        &mut enforcer,
        &rpc_client,
        zmq_addr_sequence,
    )
    .inspect_ok(|_| tracing::info!(%zmq_addr_sequence,  "Initial mempool sync complete"))
    .instrument(tracing::info_span!("initial_mempool_sync"));

    let (sequence_stream, mempool, tx_cache) = match init_sync_mempool_future.await {
        Ok(res) => res,
        Err(err) => {
            let err = miette::miette!("mempool: initial sync error: {err:#}");
            let _send_err: Result<(), _> = err_tx.send(err);
            return;
        }
    };
    let mempool = cusf_enforcer_mempool::mempool::MempoolSync::new(
        enforcer,
        mempool,
        tx_cache,
        rpc_client,
        sequence_stream,
        |err| async move {
            let err = miette::Report::from_err(err);
            let err = miette::miette!("mempool: task sync error: {err:#}");
            let _send_err: Result<(), _> = err_tx.send(err);
        },
    );
    f(mempool).await
}

/// Error receivers for main task
struct ErrRxs {
    enforcer_task: oneshot::Receiver<miette::Report>,
    grpc_server: oneshot::Receiver<miette::Report>,
}

fn task(
    enforcer: Either<Validator, Wallet>,
    cli: cli::Config,
    mainchain_client: bip300301::jsonrpsee::http_client::HttpClient,
    network: bitcoin::Network,
) -> Result<(JoinHandle<()>, ErrRxs)> {
    let (grpc_server_err_tx, grpc_server_err_rx) = oneshot::channel();
    let _grpc_server_task: JoinHandle<()> = spawn(
        run_grpc_server(enforcer.clone(), cli.serve_grpc_addr).unwrap_or_else(|err| {
            let _send_err = grpc_server_err_tx.send(err);
        }),
    );

    let (enforcer_task_err_tx, enforcer_task_err_rx) = oneshot::channel();
    let res = match (cli.enable_mempool, enforcer) {
        (false, enforcer) => cusf_enforcer_mempool::cusf_enforcer::spawn_task(
            enforcer,
            mainchain_client,
            cli.node_zmq_addr_sequence,
            |err| async move {
                let err = miette::miette!("CUSF enforcer task w/o mempool: {err:#}");
                let _send_err: Result<(), _> = enforcer_task_err_tx.send(err);
            },
        ),
        (true, Either::Left(validator)) => spawn(async move {
            tracing::info!("mempool sync task w/validator: starting");
            mempool_task(
                validator,
                mainchain_client,
                &cli.node_zmq_addr_sequence,
                enforcer_task_err_tx,
                |_mempool| futures::future::pending(),
            )
            .await
        }),
        (true, Either::Right(wallet)) => {
            tracing::info!("mempool sync task w/wallet: starting");

            // A pre-requisite for the mempool sync task is that the wallet is
            // initialized and unlocked. Give a nice error message if this is not
            // the case!
            if !wallet.is_initialized() {
                return Err(miette!("Wallet-based mempool sync requires an initialized wallet! Create one with the CreateWallet RPC method."));
            }

            let mining_reward_address = wallet.get_new_address()?;
            spawn(async move {
                let network_info = match mainchain_client.get_network_info().await {
                    Ok(network_info) => network_info,
                    Err(err) => {
                        let err = miette::Report::from_err(err);
                        tracing::error!("failed to get network info: {err:#}");
                        return;
                    }
                };
                let sample_block_template =
                    match get_block_template(&mainchain_client, network).await {
                        Ok(block_template) => block_template,
                        Err(err) => {
                            tracing::error!("failed to get sample block template: {err:#}");
                            return;
                        }
                    };
                mempool_task(
                    wallet,
                    mainchain_client,
                    &cli.node_zmq_addr_sequence,
                    enforcer_task_err_tx,
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
                            Err(err) => tracing::error!("JSON-RPC server error: {err:#}"),
                        }
                    },
                )
                .await
            })
        }
    };
    let err_rxs = ErrRxs {
        enforcer_task: enforcer_task_err_rx,
        grpc_server: grpc_server_err_rx,
    };
    Ok((res, err_rxs))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Config::parse();
    let log_file = cli.log_file();
    // Assign the tracing guard to a variable so that it is dropped when the end of main is reached.
    let _tracing_guard =
        set_tracing_subscriber(&log_file, cli.log_formatter(), cli.logger_opts.level)?;
    tracing::info!(
        data_dir = %cli.data_dir.display(),
        log_file = %log_file.display(),
        "Starting up bip300301_enforcer",
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

    let validator = Validator::new(mainchain_client.clone(), &validator_data_dir, info.chain)
        .into_diagnostic()?;

    let enforcer: Either<Validator, Wallet> = if cli.enable_wallet {
        let block_template = get_block_template(&mainchain_client, info.chain).await?;
        let magic = block_template
            .signet_challenge
            .map(|signet_challenge| compute_signet_magic(&signet_challenge))
            .unwrap_or_else(|| info.chain.magic());
        let wallet = tokio::task::block_in_place(|| {
            Wallet::new(
                &wallet_data_dir,
                &cli,
                mainchain_client.clone(),
                validator,
                magic,
            )
        })?;

        if !wallet.is_initialized() && cli.wallet_opts.auto_create {
            tracing::info!("auto-creating new wallet");
            let mnemonic = None;
            let password = None;
            wallet.create_wallet(mnemonic, password)?;
        }

        Either::Right(wallet)
    } else {
        Either::Left(validator)
    };

    let (_task, err_rxs) = task(enforcer.clone(), cli, mainchain_client, info.chain)?;

    tokio::select! {
        enforcer_task_err = err_rxs.enforcer_task => {
            match enforcer_task_err {
                Ok(err) => {
                    tracing::error!("Received error: {err:#}");
                    Err(miette!(err))
                }
                Err(err) => {
                    let err = miette!("Unable to receive error from enforcer task: {err:#}");
                    tracing::error!("{err:#}");
                    Err(err)
                }
            }
        }
        grpc_server_err = err_rxs.grpc_server => {
            match grpc_server_err {
                Ok(err) => {
                    tracing::error!("Received error: {err:#}");
                    Err(miette!(err))
                }
                Err(err) => {
                    let err = miette!("Unable to receive error from grpc server: {err:#}");
                    tracing::error!("{err:#}");
                    Err(err)
                }
            }
        }
        signal = ctrl_c() => {
            match signal {
                Ok(()) => {
                    tracing::info!("Shutting down due to process interruption");
                    Ok(())
                }
                Err(err) => {
                    tracing::error!("Unable to receive interruption signal: {err:#}");
                    Err(miette!("Unable to receive interruption signal: {err:#}"))
                }
            }
        }
    }
}
