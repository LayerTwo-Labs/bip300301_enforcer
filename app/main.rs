use std::{future::Future, net::SocketAddr, time::Duration};

use bdk_wallet::bip39::{Language, Mnemonic};
use bip300301_enforcer_lib::{
    cli::{self, LogFormatter, WalletSyncSource},
    errors::ErrorChain,
    p2p::compute_signet_magic,
    proto::{
        self,
        crypto::crypto_service_server::CryptoServiceServer,
        mainchain::{
            validator_service_server::ValidatorServiceServer,
            wallet_service_server::WalletServiceServer,
        },
    },
    rpc_client, server,
    validator::{
        Validator,
        main_rest_client::{MainRestClient, MainRestClientError},
    },
    wallet::{self, error::BitcoinCoreRPC},
};
use bitcoin::ScriptBuf;
use bitcoin_jsonrpsee::MainClient;
use clap::Parser;
use cusf_enforcer_mempool::mempool::{InitialSyncMempoolError, SyncTaskError};
use either::Either;
use futures::{FutureExt, SinkExt, StreamExt, TryFutureExt as _, channel::oneshot};
use http::{Request, header::HeaderName};
use jsonrpsee::{core::client::Error, server::middleware::rpc::RpcServiceBuilder};
use miette::{Diagnostic, IntoDiagnostic, Result, miette};
use reqwest::Url;
use thiserror::Error;
use tokio::{net::TcpStream, task::JoinHandle};
use tonic::{server::NamedService, transport::Server};
use tower::ServiceBuilder;
use tower_http::{
    request_id::{MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::{DefaultOnFailure, DefaultOnResponse, TraceLayer},
};
use tracing::Instrument;
use tracing_subscriber::{filter as tracing_filter, layer::SubscriberExt};
use wallet::Wallet;

mod file_descriptors;

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
    log_formatter: LogFormatter,
    log_level: tracing::Level,
    rolling_log_appender: tracing_appender::rolling::RollingFileAppender,
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

    // Ensure the appender is non-blocking!
    let (file_appender, guard) = tracing_appender::non_blocking(rolling_log_appender);

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
) -> Result<bitcoin_jsonrpsee::client::BlockTemplate, wallet::error::BitcoinCoreRPC>
where
    RpcClient: MainClient + Sync,
{
    let mut request = bitcoin_jsonrpsee::client::BlockTemplateRequest::default();
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

const REQUEST_ID_HEADER: &str = "x-request-id";

fn set_request_id_layer() -> SetRequestIdLayer<RequestIdMaker> {
    SetRequestIdLayer::new(HeaderName::from_static(REQUEST_ID_HEADER), RequestIdMaker)
}

fn propagate_request_id_layer() -> PropagateRequestIdLayer {
    PropagateRequestIdLayer::new(HeaderName::from_static(REQUEST_ID_HEADER))
}

#[derive(Debug, Clone)]
struct FailureHandler;
use tower_http::classify::GrpcFailureClass;

impl tower_http::trace::OnFailure<GrpcFailureClass> for FailureHandler {
    fn on_failure(&mut self, failure: GrpcFailureClass, latency: Duration, _span: &tracing::Span) {
        let code = match failure {
            GrpcFailureClass::Code(code) => tonic::Code::from_i32(code.into()),
            GrpcFailureClass::Error(err) => {
                tracing::warn!("unexpected gRPC failure class: {err}");
                tonic::Code::Internal
            }
        };
        tracing::error!(
            latency = ?latency,
            code = ?code,
            "gRPC server responding with error",
        );
    }
}

async fn spawn_json_rpc_server(
    validator: Either<Validator, Wallet>,
    serve_addr: SocketAddr,
) -> miette::Result<jsonrpsee::server::ServerHandle> {
    let methods = match validator {
        Either::Left(validator) => {
            server::validator::json_rpc::RpcServer::into_rpc(validator).into()
        }
        Either::Right(wallet) => {
            let mut methods: jsonrpsee::server::Methods =
                server::validator::json_rpc::RpcServer::into_rpc(wallet.validator().clone()).into();
            methods
                .merge(server::wallet::json_rpc::RpcServer::into_rpc(wallet))
                .into_diagnostic()?;
            methods
        }
    };

    tracing::info!("Listening for JSON-RPC on {}", serve_addr);

    // Ordering here matters! Order here is from official docs on request IDs tracings
    // https://docs.rs/tower-http/latest/tower_http/request_id/index.html#using-trace
    let tracer = tower::ServiceBuilder::new()
        .layer(set_request_id_layer())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(move |request: &http::Request<_>| {
                    let request_id = request
                        .headers()
                        .get(http::HeaderName::from_static(REQUEST_ID_HEADER))
                        .and_then(|h| h.to_str().ok())
                        .filter(|s| !s.is_empty());

                    tracing::span!(
                        tracing::Level::DEBUG,
                        "json_rpc_server",
                        request_id, // this is needed for the record call below to work
                    )
                })
                .on_request(())
                .on_eos(())
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO))
                .on_failure(DefaultOnFailure::new().level(tracing::Level::ERROR)),
        )
        .layer(propagate_request_id_layer())
        .into_inner();

    let http_middleware = tower::ServiceBuilder::new().layer(tracer);
    let rpc_middleware = RpcServiceBuilder::new().rpc_logger(1024);

    let handle = jsonrpsee::server::Server::builder()
        .set_http_middleware(http_middleware)
        .set_rpc_middleware(rpc_middleware)
        .build(serve_addr)
        .await
        .map_err(|err| miette!("initialize JSON-RPC server at `{serve_addr}`: {err:#}"))?
        .start(methods);
    Ok(handle)
}

#[derive(Debug, Diagnostic, Error)]
enum GrpcServerError {
    #[error("unable to serve gRPC at `{addr}`")]
    #[diagnostic(code(grpc_server::serve))]
    Serve {
        addr: SocketAddr,
        source: tonic::transport::Error,
    },
    #[error("unable to build reflection service")]
    #[diagnostic(code(grpc_server::reflection))]
    Reflection(#[from] tonic_reflection::server::Error),
}

async fn run_grpc_server<F: Future<Output = ()>>(
    validator: Either<Validator, Wallet>,
    shutdown_tx: futures::channel::mpsc::Sender<()>,
    shutdown_signal: F,
    addr: SocketAddr,
) -> Result<(), GrpcServerError> {
    // Ordering here matters! Order here is from official docs on request IDs tracings
    // https://docs.rs/tower-http/latest/tower_http/request_id/index.html#using-trace
    let tracer = ServiceBuilder::new()
        .layer(set_request_id_layer())
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
                        "grpc_server",
                        method = %request.method(),
                        uri = %request.uri(),
                        request_id , // this is needed for the record call below to work
                    )
                })
                .on_request(())
                .on_eos(())
                // Set this to a low log level. Quickly leads to enormous log files, as our GUI
                // implementations are sending a lof of requests /all/ the time.
                .on_response(DefaultOnResponse::new().level(tracing::Level::TRACE))
                .on_failure(FailureHandler),
        )
        .layer(propagate_request_id_layer())
        .into_inner();

    let crypto_service = CryptoServiceServer::new(server::crypto::CryptoServiceServer);
    let mut builder = Server::builder()
        .layer(tracer)
        .add_service(crypto_service)
        .add_service(ValidatorServiceServer::new({
            let validator = match validator {
                Either::Left(ref validator) => validator,
                Either::Right(ref wallet) => wallet.validator(),
            };
            server::validator::Server::new(validator.clone(), shutdown_tx.clone())
        }));

    let mut reflection_service_builder = tonic_reflection::server::Builder::configure()
        .with_service_name(CryptoServiceServer::<server::crypto::CryptoServiceServer>::NAME)
        .with_service_name(ValidatorServiceServer::<Validator>::NAME)
        .register_encoded_file_descriptor_set(proto::ENCODED_FILE_DESCRIPTOR_SET);

    if let Either::Right(wallet) = validator.clone() {
        tracing::info!("gRPC: enabling wallet service");
        let wallet_service = WalletServiceServer::new(wallet);
        builder = builder.add_service(wallet_service);
        reflection_service_builder =
            reflection_service_builder.with_service_name(WalletServiceServer::<Wallet>::NAME);
    }

    let (health_reporter, health_service) = tonic_health::server::health_reporter();

    // Set all services to have the "serving" status.
    // TODO: somehow expose the health reporter to the running services, and
    // dynamically update if we're running into issues.
    for service in [
        ValidatorServiceServer::<Validator>::NAME,
        WalletServiceServer::<Wallet>::NAME,
        CryptoServiceServer::<server::crypto::CryptoServiceServer>::NAME,
    ] {
        tracing::debug!("Setting health status for service: {service}");
        health_reporter
            .set_service_status(service, tonic_health::ServingStatus::Serving)
            .await;
    }

    tracing::info!("Listening for gRPC on {addr} with reflection");

    let server = builder
        .add_service(
            reflection_service_builder
                .build_v1()
                .map_err(GrpcServerError::Reflection)?,
        )
        .add_service(health_service);

    server
        .serve_with_shutdown(addr, shutdown_signal)
        .await
        .map_err(|err| GrpcServerError::Serve { addr, source: err })
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

    // Ordering here matters! Order here is from official docs on request IDs tracings
    // https://docs.rs/tower-http/latest/tower_http/request_id/index.html#using-trace
    let tracer = tower::ServiceBuilder::new()
        .layer(set_request_id_layer())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(move |request: &http::Request<_>| {
                    let request_id = request
                        .headers()
                        .get(http::HeaderName::from_static(REQUEST_ID_HEADER))
                        .and_then(|h| h.to_str().ok())
                        .filter(|s| !s.is_empty());

                    tracing::span!(
                        tracing::Level::DEBUG,
                        "gbt_server",
                        request_id, // this is needed for the record call below to work
                    )
                })
                .on_request(())
                .on_eos(())
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO))
                .on_failure(DefaultOnFailure::new().level(tracing::Level::ERROR)),
        )
        .layer(propagate_request_id_layer())
        .into_inner();

    let http_middleware = tower::ServiceBuilder::new().layer(tracer);
    let rpc_middleware = RpcServiceBuilder::new().rpc_logger(1024);

    use cusf_enforcer_mempool::server::RpcServer;
    let handle = jsonrpsee::server::Server::builder()
        .set_http_middleware(http_middleware)
        .set_rpc_middleware(rpc_middleware)
        .build(serve_addr)
        .await
        .map_err(|err| miette!("initialize JSON-RPC server at `{serve_addr}`: {err:#}"))?
        .start(rpc_server);
    Ok(handle)
}

async fn start_gbt_server(
    mining_reward_address: bitcoin::Address,
    network: bitcoin::Network,
    network_info: bitcoin_jsonrpsee::client::NetworkInfo,
    sample_block_template: bitcoin_jsonrpsee::client::BlockTemplate,
    mempool: cusf_enforcer_mempool::mempool::MempoolSync<Wallet>,
    serve_addr: SocketAddr,
) -> miette::Result<jsonrpsee::server::ServerHandle> {
    let gbt_server = cusf_enforcer_mempool::server::Server::new(
        mining_reward_address.script_pubkey(),
        mempool,
        network,
        network_info,
        sample_block_template,
    )
    .into_diagnostic()?;
    let gbt_server_handle = spawn_gbt_server(gbt_server, serve_addr).await?;
    Ok(gbt_server_handle)
}

async fn is_address_port_open(addr: &str) -> Result<bool, std::io::Error> {
    let addr = addr.strip_prefix("tcp://").unwrap_or_default();
    match tokio::time::timeout(Duration::from_millis(250), TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => Ok(true),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(false),
        Ok(Err(e)) => Err(e),
        Err(_) => Ok(false),
    }
}

#[derive(educe::Educe, Diagnostic, Error)]
#[educe(Debug(bound(SyncTaskError<Enforcer>: std::fmt::Debug)))]
enum MempoolTaskError<Enforcer>
where
    Enforcer: cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer + 'static,
{
    #[error("mempool initial sync error")]
    InitialSync(#[source] InitialSyncMempoolError<Enforcer>),
    #[error("mempool task sync error")]
    SyncTask(#[source] SyncTaskError<Enforcer>),
    #[error("failed to check if ZMQ address is reachable: failed to connect to {addr}")]
    ZmqCheck {
        addr: String,
        source: std::io::Error,
    },
    #[error("ZMQ address for mempool sync is not reachable: {zmq_addr_sequence}")]
    ZmqNotReachable { zmq_addr_sequence: String },
}

#[derive(educe::Educe, Diagnostic, Error)]
#[educe(Debug(bound(SyncTaskError<Enforcer>: std::fmt::Debug)))]
enum TaskError<Enforcer>
where
    Enforcer: cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer + 'static,
{
    #[error("CUSF enforcer task w/mempool error")]
    Mempool(#[from] MempoolTaskError<Enforcer>),
    #[error("CUSF enforcer task w/o mempool error")]
    NoMempool(#[from] cusf_enforcer_mempool::cusf_enforcer::TaskError<Enforcer>),
}

async fn sync_mempool<Enforcer, RpcClient, Signal>(
    mut enforcer: Enforcer,
    rpc_client: RpcClient,
    zmq_addr_sequence: &str,
    err_tx: oneshot::Sender<MempoolTaskError<Enforcer>>,
    shutdown_signal: Signal,
) -> Result<cusf_enforcer_mempool::mempool::MempoolSync<Enforcer>, MempoolTaskError<Enforcer>>
where
    Enforcer: cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer + Send + Sync + 'static,
    RpcClient: bitcoin_jsonrpsee::client::MainClient + Send + Sync + 'static,
    Signal: Future<Output = ()> + Send,
{
    tracing::debug!(%zmq_addr_sequence, "Ensuring ZMQ address for mempool sync is reachable");

    match is_address_port_open(zmq_addr_sequence).await {
        Ok(true) => (),
        Ok(false) => {
            let err = MempoolTaskError::ZmqNotReachable {
                zmq_addr_sequence: zmq_addr_sequence.to_owned(),
            };
            return Err(err);
        }
        Err(err) => {
            let err = MempoolTaskError::ZmqCheck {
                addr: zmq_addr_sequence.to_owned(),
                source: err,
            };
            return Err(err);
        }
    }

    let init_sync_mempool_future = cusf_enforcer_mempool::mempool::init_sync_mempool(
        &mut enforcer,
        &rpc_client,
        zmq_addr_sequence,
        shutdown_signal,
    )
    .inspect_ok(|_| tracing::info!(%zmq_addr_sequence,  "Initial mempool sync complete"))
    .instrument(tracing::info_span!("initial_mempool_sync"));

    let (sequence_stream, mempool, tx_cache) = match init_sync_mempool_future.await {
        Ok(res) => res,
        Err(err) => {
            let err = MempoolTaskError::InitialSync(err);
            tracing::error!("{:#}", ErrorChain::new(&err));
            return Err(err);
        }
    };

    let mempool = cusf_enforcer_mempool::mempool::MempoolSync::new(
        enforcer,
        mempool,
        tx_cache,
        rpc_client,
        sequence_stream,
        |err| async move {
            let err = MempoolTaskError::SyncTask(err);
            let _send_err: Result<(), _> = err_tx.send(err);
        },
    );

    Ok(mempool)
}

#[derive(Debug, Diagnostic, Error)]
enum EnforcerTaskErr {
    #[error(transparent)]
    MempoolValidator(#[from] MempoolTaskError<Validator>),
    #[error(transparent)]
    MempoolWallet(#[from] MempoolTaskError<Wallet>),
    #[error(transparent)]
    NoMempool(#[from] TaskError<Either<Validator, Wallet>>),
}

enum EnforcerTaskErrRx {
    MempoolValidator(oneshot::Receiver<MempoolTaskError<Validator>>),
    MempoolWallet(oneshot::Receiver<MempoolTaskError<Wallet>>),
    NoMempool(oneshot::Receiver<TaskError<Either<Validator, Wallet>>>),
}

impl EnforcerTaskErrRx {
    async fn receive(self) -> Result<EnforcerTaskErr, oneshot::Canceled> {
        match self {
            Self::MempoolValidator(err_rx) => err_rx.await.map(EnforcerTaskErr::MempoolValidator),
            Self::MempoolWallet(err_rx) => err_rx.await.map(EnforcerTaskErr::MempoolWallet),
            Self::NoMempool(err_rx) => err_rx.await.map(EnforcerTaskErr::NoMempool),
        }
    }
}

/// Error receivers for main task
struct ErrRxs {
    enforcer_task: EnforcerTaskErrRx,
    grpc_server: oneshot::Receiver<GrpcServerError>,
    shutdown_signal: oneshot::Receiver<()>,
    shutdown_tx: futures::channel::mpsc::Sender<()>,
}

async fn get_zmq_addr_sequence(
    mainchain_client: bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
) -> Result<String> {
    let notifications = mainchain_client
        .get_zmq_notifications()
        .await
        .map_err(|err| BitcoinCoreRPC {
            method: "getzmqnotifications".to_string(),
            error: err,
        })?;

    let Some(address) = notifications
        .iter()
        .find(|n| n.notification_type == "pubsequence")
        .map(|n| n.address.clone())
    else {
        #[derive(Debug, Diagnostic, Error)]
        #[error(
            "unable to find ZMQ notification for `pubsequence` in `getzmqnotifications` response"
        )]
        #[diagnostic(
            help(
                "Your Bitcoin Core instance is not configured to send ZMQ notifications for the `pubsequence` notification type"
            ),
            code(bip300301_enforcer::zmq_pubsequence_notification_missing),
            url(
                "https://github.com/layerTwo-Labs/bip300301_enforcer?tab=readme-ov-file#requirements"
            )
        )]
        struct ZmqNotificationMissing;

        return Err(ZmqNotificationMissing.into());
    };
    Ok(address)
}

/// Returns a join handle for the main task, a shared future for the shutdown signal,
/// and error receivers for the main task sub components
async fn spawn_task(
    enforcer: Either<Validator, Wallet>,
    cli: cli::Config,
    mainchain_client: bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
    network: bitcoin::Network,
) -> Result<(
    JoinHandle<Result<()>>,
    futures::future::Shared<impl Future<Output = ()>>,
    ErrRxs,
)> {
    let (enforcer_task_err_tx, enforcer_task_err_rx) = oneshot::channel();

    let (shutdown_signal_tx, shutdown_signal_rx) = oneshot::channel();
    let (shutdown_tx, mut shutdown_rx) = futures::channel::mpsc::channel(1);

    let node_zmq_addr_sequence = match cli.node_zmq_addr_sequence {
        Some(node_zmq_addr_sequence) => node_zmq_addr_sequence,
        None => get_zmq_addr_sequence(mainchain_client.clone()).await?,
    };

    let shutdown_signal = async move {
        shutdown_rx
            .next()
            .await
            .and_then(|_| {
                tracing::debug!("received on shutdown channel, sending signal");
                shutdown_signal_tx
                    .send(())
                    .inspect(|_| tracing::trace!("sent shutdown signal"))
                    .inspect_err(|_| {
                        tracing::error!("unable to send shutdown signal, receiver was dropped")
                    })
                    .ok()
            })
            .unwrap_or(())
    }
    .shared();

    let (task_handle, enforcer_task_err_rx) = match (cli.enable_mempool, enforcer.clone()) {
        (false, enforcer) => {
            let shutdown_signal = shutdown_signal.clone();
            let task_handle = tokio::task::spawn(async move {
                tracing::info!("CUSF enforcer task w/o mempool: starting");

                let mut enforcer = enforcer.clone();
                let task = cusf_enforcer_mempool::cusf_enforcer::task(
                    &mut enforcer,
                    &mainchain_client,
                    &node_zmq_addr_sequence,
                    shutdown_signal,
                );

                if let Err(err) = task.await {
                    let err = TaskError::NoMempool(err);
                    let _send_err: Result<(), _> = enforcer_task_err_tx.send(err);
                }
                Ok(())
            });
            (
                task_handle,
                EnforcerTaskErrRx::NoMempool(enforcer_task_err_rx),
            )
        }
        (true, Either::Left(validator)) => {
            let (enforcer_task_err_tx, enforcer_task_err_rx) = oneshot::channel();
            let shutdown_signal = shutdown_signal.clone();
            let task_handle = tokio::task::spawn(async move {
                tracing::info!("mempool sync task w/validator: starting");
                let _mempool = sync_mempool(
                    validator,
                    mainchain_client,
                    &node_zmq_addr_sequence,
                    enforcer_task_err_tx,
                    shutdown_signal,
                )
                .await?;
                Ok(())
            });
            (
                task_handle,
                EnforcerTaskErrRx::MempoolValidator(enforcer_task_err_rx),
            )
        }
        (true, Either::Right(wallet)) => {
            tracing::info!("mempool sync task w/wallet: starting");
            let (enforcer_task_err_tx, enforcer_task_err_rx) = oneshot::channel();
            let shutdown_signal = shutdown_signal.clone();
            let task_handle = tokio::task::spawn(async move {
                // A pre-requisite for the mempool sync task is that the wallet is
                // initialized and unlocked. Give a nice error message if this is not
                // the case!
                if !wallet.is_initialized().await {
                    return Err(miette!(
                        "Wallet-based mempool sync requires an initialized wallet! Create one with the CreateWallet RPC method."
                    ));
                }

                let mining_reward_address = match cli.mining_opts.coinbase_recipient {
                    Some(mining_reward_address) => Ok(mining_reward_address),
                    None => wallet.get_new_address().await,
                };

                let mining_reward_address = match mining_reward_address {
                    Ok(mining_reward_address) => mining_reward_address,
                    Err(err) => {
                        let err = miette::Report::from_err(err);
                        return Err(err.wrap_err("failed to get mining reward address"));
                    }
                };
                let network_info = match mainchain_client.get_network_info().await {
                    Ok(network_info) => network_info,
                    Err(err) => {
                        let err = miette::Report::from_err(err);
                        return Err(err.wrap_err("failed to get network info"));
                    }
                };
                let sample_block_template =
                    match get_block_template(&mainchain_client, network).await {
                        Ok(block_template) => block_template,
                        Err(err) => {
                            let err = miette::Report::from_err(err);
                            return Err(err.wrap_err("failed to get sample block template"));
                        }
                    };
                let mempool = sync_mempool(
                    wallet,
                    mainchain_client,
                    &node_zmq_addr_sequence,
                    enforcer_task_err_tx,
                    shutdown_signal.clone(),
                )
                .await?;

                let server_handle = start_gbt_server(
                    mining_reward_address,
                    network,
                    network_info,
                    sample_block_template,
                    mempool,
                    cli.serve_rpc_addr,
                )
                .await?;

                shutdown_signal.clone().await;

                tracing::debug!("stopping `getblocktemplate` JSON-RPC server");

                // This should never fail. The only failure mode is the server
                // already being stopped, and we have full control over that.
                if let Err(err) = server_handle.stop() {
                    tracing::error!("error stopping `getblocktemplate` JSON-RPC server: {err:#}");
                }
                Ok(())
            });
            (
                task_handle,
                EnforcerTaskErrRx::MempoolWallet(enforcer_task_err_rx),
            )
        }
    };

    let (grpc_server_err_tx, grpc_server_err_rx) = oneshot::channel();
    let _grpc_server_task: JoinHandle<()> = {
        let shutdown_signal = shutdown_signal.clone();
        let shutdown_tx = shutdown_tx.clone();
        tokio::task::spawn(
            run_grpc_server(enforcer, shutdown_tx, shutdown_signal, cli.serve_grpc_addr)
                .inspect(|_| tracing::info!("gRPC server finished"))
                .unwrap_or_else(|err| {
                    let _send_err = grpc_server_err_tx.send(err);
                }),
        )
    };

    let err_rxs = ErrRxs {
        enforcer_task: enforcer_task_err_rx,
        grpc_server: grpc_server_err_rx,
        shutdown_signal: shutdown_signal_rx,
        shutdown_tx: shutdown_tx.clone(),
    };
    Ok((task_handle, shutdown_signal, err_rxs))
}

#[tokio::main]
async fn main() -> Result<()> {
    let (mut self_interrupt_tx, mut self_interrupt_rx) = futures::channel::mpsc::unbounded();

    // We want to get panics properly logged, with request IDs and all that jazz.
    //
    // Save the original panic hook.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or("unknown".into());

        let payload = match info.payload().downcast_ref::<&str>() {
            Some(s) => s.to_string(),
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => s.clone(),
                None => format!("{:#?}", info.payload()).to_string(),
            },
        };
        tracing::error!(location, "Panicked during execution: `{payload}`");
        default_hook(info); // Panics are bad. re-throw!
    }));

    let cli = cli::Config::parse();
    // Assign the tracing guard to a variable so that it is dropped when the end of main is reached.
    let _tracing_guard = set_tracing_subscriber(
        cli.log_formatter(),
        cli.logger_opts.level,
        cli.rolling_log_appender()?,
    )?;
    tracing::info!(
        data_dir = %cli.data_dir.display(),
        log_dir = %cli.log_dir().display(),
        git_hash = cli.git_hash(),
        build = if cfg!(debug_assertions) { "debug" } else { "release" },
        "Starting up bip300301_enforcer",
    );

    let raw_url = format!("http://{}", cli.node_rpc_opts.addr);
    let mainchain_rest_client = MainRestClient::new(
        Url::parse(&raw_url)
            .map_err(|err| miette!("invalid mainchain REST URL `{raw_url}`: {err:#}"))?,
    );

    let ts = tokio::time::Instant::now();
    match mainchain_rest_client.get_chain_info().await {
        Ok(_) => {
            tracing::info!(
                "verified mainchain REST server is enabled in {:?}",
                ts.elapsed()
            );
        }
        Err(MainRestClientError::RestServerNotEnabled) => {
            return Err(miette!(
                "Mainchain REST server at `{raw_url}` is not enabled! Do this with the `-rest` flag or `rest=1` in your Bitcoin Core configuration file"
            ));
        }
        Err(err) => {
            return Err(miette::Report::from_err(err));
        }
    }
    let mainchain_client =
        rpc_client::create_client(&cli.node_rpc_opts, cli.enable_wallet && cli.enable_mempool)?;
    tracing::info!(
        "created mainchain JSON-RPC client from options: {}:*****@{}",
        cli.node_rpc_opts.user.as_deref().unwrap_or("cookie"),
        cli.node_rpc_opts.addr,
    );

    let mut info = None;
    while info.is_none() {
        // From Bitcoin Core src/rpc/protocol.h
        const RPC_IN_WARMUP: i32 = -28;

        // If Bitcoin Core is booting up, we don't want to fail hard.
        // Check for errors that should go away after a little while,
        // and tolerate those.
        match mainchain_client.get_blockchain_info().await {
            Ok(inner_info) => {
                info = Some(inner_info);
                Ok(())
            }

            Err(Error::Call(err)) if err.code() == RPC_IN_WARMUP => {
                tracing::debug!(
                    err = format!("{}: {}", err.code(), err.message()),
                    "Transient Bitcoin Core error, retrying...",
                );
                Ok(())
            }

            Err(err) => Err(wallet::error::BitcoinCoreRPC {
                method: "getblockchaininfo".to_string(),
                error: err,
            }),
        }
        .map_err(|err| miette!("failed to get blockchain info: {err:#}"))?;

        let delay = tokio::time::Duration::from_millis(250);
        tokio::time::sleep(delay).await;
    }

    let Some(info) = info else {
        return Err(miette!(
            "was never able to query bitcoin core blockchain info"
        ));
    };

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

    let validator = Validator::new(
        mainchain_client.clone(),
        mainchain_rest_client,
        &validator_data_dir,
        info.chain,
    )
    .into_diagnostic()?;

    let signet_challenge = if info.chain == bitcoin::Network::Signet {
        let block_template = get_block_template(&mainchain_client, info.chain).await?;
        let Some(signet_challenge) = block_template.signet_challenge else {
            return Err(miette!("signet challenge not found in block template"));
        };

        // We cannot verify that there's one specific signet challenge being used here,
        // because the user might want to run their own signet. However, if they're on
        // the standard signet, they're doing something wrong.
        let standard_signet_challenge = {
            const STANDARD_SIGNET_CHALLENGE_HEX: &str = "512103ad5e0edad18cb1f0fc0d28a3d4f1f3e445640337489abb10404f2d1e086be430210359ef5021964fe22d6f8e05b2463c9540ce96883fe3b278760f048f5189f2e6c452ae";
            ScriptBuf::from_hex(STANDARD_SIGNET_CHALLENGE_HEX)
                .expect("standard signet challenge is invalid")
        };

        if signet_challenge == standard_signet_challenge {
            #[derive(Debug, Diagnostic, Error)]
            #[error(
                "You're trying to run the enforcer against the standard signet chain! This is not what you want."
            )]
            #[diagnostic(
                help("either run against the L2L signet, or your own custom signet"),
                code(bip300301_enforcer::standard_signet),
                url(
                    "https://github.com/layerTwo-Labs/bip300301_enforcer?tab=readme-ov-file#requirements"
                )
            )]
            struct StandardSignetError;

            return Err(StandardSignetError.into());
        }

        Some(signet_challenge)
    } else {
        None
    };

    let enforcer: Either<Validator, Wallet> = if cli.enable_wallet {
        // The wallet needs the txindex in order to operate. Will lead to obscure errors later
        // if we fail RPC requests due to the index not being there.
        //
        // TODO: should actually move away from needed txindex, but that's for another day.
        // TODO: we could check if the index is synced here. not necessary?
        let index_info = mainchain_client.get_index_info().await.map_err(|err| {
            wallet::error::BitcoinCoreRPC {
                method: "getindexinfo".to_string(),
                error: err,
            }
        })?;
        if !index_info.contains_key("txindex") {
            return Err(miette!("`txindex` is not enabled on the mainchain client"));
        }

        let magic = signet_challenge
            .map(|signet_challenge| compute_signet_magic(&signet_challenge))
            .unwrap_or_else(|| info.chain.magic());
        let wallet = Wallet::new(
            &wallet_data_dir,
            &cli,
            mainchain_client.clone(),
            validator,
            magic,
        )
        .await?;

        let (mnemonic, auto_create) = match (
            cli.wallet_opts.mnemonic_path.clone(),
            cli.wallet_opts.auto_create,
        ) {
            (Some(mnemonic_path), _) => {
                tracing::debug!("Reading mnemonic from file: {}", mnemonic_path.display());

                let mnemonic_str =
                    std::fs::read_to_string(mnemonic_path.clone()).map_err(|err| {
                        miette!(
                            "failed to read mnemonic file `{}`: {}",
                            mnemonic_path.display(),
                            err
                        )
                    })?;

                let mnemonic = Mnemonic::parse_in(Language::English, &mnemonic_str)
                    .map_err(|err| miette!("invalid mnemonic: {}", err))?;

                (Some(mnemonic), true)
            }
            (_, true) => (None, true),
            _ => (None, false),
        };

        if !wallet.is_initialized().await && auto_create {
            tracing::info!("auto-creating new wallet");
            wallet.create_wallet(mnemonic, None).await?;
        }

        Either::Right(wallet)
    } else {
        Either::Left(validator)
    };
    // Start JSON-RPC server
    let json_rpc_server_handle = spawn_json_rpc_server(enforcer.clone(), cli.serve_json_rpc_addr)
        .await
        .map_err(|err| miette!("Failed to spawn JSON-RPC server: {err:#}"))?;

    let (main_task_handle, shutdown_signal, mut err_rxs) =
        spawn_task(enforcer.clone(), cli.clone(), mainchain_client, info.chain).await?;

    let json_rpc_handle: JoinHandle<Result<(), miette::Report>> = {
        let shutdown_signal = shutdown_signal.clone();
        tokio::spawn(async move {
            shutdown_signal.await;

            if let Err(err) = json_rpc_server_handle.stop() {
                #[derive(Debug, Diagnostic, Error)]
                #[error("error stopping JSON-RPC server: {err:#}")]
                #[diagnostic(code(bip300301_enforcer::json_rpc_server::stop))]
                struct StopError {
                    #[source]
                    err: jsonrpsee::server::AlreadyStoppedError,
                }
                return Err(miette::Report::from_err(StopError { err }));
            }

            Ok(())
        })
    };

    let mut wallet_sync_task_handle: Option<JoinHandle<Result<(), miette::Report>>> = None;

    if let Either::Right(wallet) = enforcer.clone() {
        // Big wallets (thousands of UTXOs) can get really bad performance for the
        // periodic sync. Therefore we expose a knob to disable it.
        let sync_source_disabled = cli.wallet_opts.sync_source == WalletSyncSource::Disabled;

        if cli.wallet_opts.full_scan {
            wallet.full_scan().await?;
        }

        if !cli.wallet_opts.skip_periodic_sync && !sync_source_disabled {
            let wallet = wallet.clone();
            let shutdown_signal = shutdown_signal.clone();
            let handle = tokio::spawn(async move { wallet.sync_task(shutdown_signal).await });
            wallet_sync_task_handle = Some(handle);
        }
    }

    let exit_after_sync_task = match cli.exit_after_sync {
        Some(exit_after_sync) => {
            let exit_after_sync = if exit_after_sync != 0 {
                exit_after_sync
            } else {
                info.blocks
            };

            let validator = match &enforcer {
                Either::Left(validator) => validator.clone(),
                Either::Right(wallet) => wallet.validator().clone(),
            };
            let handle = tokio::spawn(async move {
                tracing::info!(
                    "Waiting for sync to height {} before exiting",
                    exit_after_sync
                );

                let mut events = std::pin::pin!(validator.subscribe_events());
                while let Some(event) = events.next().await {
                    use bip300301_enforcer_lib::types::Event;

                    if let Ok(Event::ConnectBlock { header_info, .. }) = event {
                        if header_info.height >= exit_after_sync {
                            tracing::info!(
                                "Synced to block height {}, exiting",
                                header_info.height
                            );
                            let _ = self_interrupt_tx.send(()).await;

                            tracing::debug!("Sent self interrupt signal");
                            break;
                        }
                    }
                }

                Ok(())
            });

            Some(handle)
        }

        None => None,
    };

    struct TaskHandles {
        main_task: JoinHandle<Result<(), miette::Report>>,
        wallet_sync_task: Option<JoinHandle<Result<(), miette::Report>>>,
        json_rpc_handle: JoinHandle<Result<(), miette::Report>>,
        exit_after_sync_task: Option<JoinHandle<Result<(), miette::Report>>>,
    }

    impl TaskHandles {
        async fn try_join_all(self) -> Result<(), miette::Report> {
            let Self {
                main_task,
                wallet_sync_task,
                json_rpc_handle,
                exit_after_sync_task,
            } = self;

            let mut tasks: Vec<std::pin::Pin<Box<dyn Future<Output = Result<_, _>> + Send>>> = vec![
                Box::pin(main_task.map(|res| {
                    tracing::info!("main task finished");
                    res
                })),
                Box::pin(json_rpc_handle.map(|res| {
                    tracing::info!("JSON-RPC server finished");
                    res
                })),
            ];

            if let Some(wallet_sync_task) = wallet_sync_task {
                tasks.push(Box::pin(wallet_sync_task.map(|res| {
                    tracing::info!("wallet sync task finished");
                    res
                })));
            }

            if let Some(exit_after_sync_task) = exit_after_sync_task {
                tasks.push(Box::pin(exit_after_sync_task.map(|res| {
                    tracing::info!("exit after sync task finished");
                    res
                })));
            }

            for result in futures::future::try_join_all(tasks)
                .await
                .into_iter()
                .flatten()
            {
                if let Err(err) = result {
                    tracing::error!("task failed: {err:#}");
                }
            }

            Ok(())
        }
    }

    // If other tasks also need to be gracefully waited for on shutdown,
    // add them here.
    let mut handles = TaskHandles {
        main_task: main_task_handle,
        wallet_sync_task: wallet_sync_task_handle,
        json_rpc_handle,
        exit_after_sync_task,
    };

    async fn graceful_shutdown(
        shutdown_tx: &mut futures::channel::mpsc::Sender<()>,
        handles: TaskHandles,
    ) {
        // If we've not yet sent the shutdown signal, do so now. In the case of an interrupt signal,
        // this branch will hit
        if (shutdown_tx.send(()).await).is_ok() {
            tracing::debug!("shutdown: sent signal");
        }

        if let Err(err) = tokio::time::timeout(
            tokio::time::Duration::from_secs(1),
            // Wait for all handles to finish
            handles.try_join_all(),
        )
        .await
        .map_err(|err| {
            #[derive(Debug, Diagnostic, Error)]
            #[error("shutdown: error while waiting for tasks to finish")]
            #[diagnostic(code(bip300301_enforcer::shutdown))]
            struct ShutdownError {
                #[source]
                err: tokio::time::error::Elapsed,
            }
            ShutdownError { err }
        }) {
            // why isn't the source displayed here?
            tracing::error!("{err:#}");
        };
    }

    tokio::select! {

        _ = err_rxs.shutdown_signal => {
            tracing::info!("Shutting down due to shutdown signal");
            graceful_shutdown(&mut err_rxs.shutdown_tx, handles).await;
            Ok(())
        }

        task_err = &mut handles.main_task => {
            match task_err {
                Ok(Ok(())) => {
                    tracing::info!("Task completed, exiting with zero exit code ");
                    Ok(())
                }
                Ok(Err(err)) => Err(err),
                Err(join_error) => {
                    Err(miette!(
                        "main task panicked or was cancelled: {join_error:#}"
                    ))
                }
            }
        }
        enforcer_task_err = err_rxs.enforcer_task.receive() => {
            match enforcer_task_err {
                Ok(err) => {
                    let err = miette::Error::from(err);
                    tracing::error!("Received enforcer task error: {err:#}");
                    if cfg!(target_os = "macos") && format!("{err:#}").contains("Too many open files") {
                        tracing::error!(err = %err, "too many open files, dumping all open file descriptors");
                        match file_descriptors::list_open_descriptors_macos() {
                            Ok(open_fds) => {
                                tracing::error!("open file descriptors: {:#?}", open_fds);
                            }
                            Err(err) => {
                                tracing::error!(err = %err, "failed to list open file descriptors");
                            }
                        }
                    }
                    Err(err)
                }
                Err(err) => {
                    let err = miette!("Unable to receive error from enforcer task: {err:#}");
                    Err(err)
                }
            }
        }
        grpc_server_err = err_rxs.grpc_server => {
            match grpc_server_err {
                Ok(err) => {
                    let err = miette::Report::from_err(err);
                    Err(err.wrap_err("gRPC server error"))
                }
                Err(err) => {
                    let err = miette!("Unable to receive error from gRPC server: {err:#}");
                    Err(err)
                }
            }
        }
        signal = self_interrupt_rx.next() => {
            match signal {
                Some(()) => {
                    tracing::info!("Shutting down due to self interrupt");
                    graceful_shutdown(&mut err_rxs.shutdown_tx, handles).await;
                    Ok(())
                }
                None => {
                    tracing::error!("Unable to receive self interrupt signal");
                    Err(miette!("Unable to receive self interrupt signal"))
                }
            }

        }
        signal = tokio::signal::ctrl_c() => {
            match signal {
                Ok(()) => {
                    tracing::info!("Shutting down due to process interruption");
                    graceful_shutdown(&mut err_rxs.shutdown_tx, handles).await;
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
