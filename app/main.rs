use std::{net::SocketAddr, time::Duration};

use bdk_wallet::bip39::{Language, Mnemonic};
use bip300301_enforcer_lib::{
    cli::{self, WalletSyncSource},
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
    types::Event,
    validator::{
        Validator,
        main_rest_client::{MainRestClient, MainRestClientError},
    },
    version,
    wallet::{self, error::BitcoinCoreRPC},
};
use bitcoin::ScriptBuf;
use bitcoin_jsonrpsee::{MainClient, jsonrpsee::http_client::transport};
use clap::Parser;
use either::Either;
use futures::{StreamExt, TryFutureExt as _, channel::oneshot};
use http::{Request, header::HeaderName};
use jsonrpsee::{core::client::Error, server::middleware::rpc::RpcServiceBuilder};
use miette::{Diagnostic, IntoDiagnostic, Result, WrapErr as _, miette};
use reqwest::Url;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tonic::{server::NamedService, transport::Server};
use tower::ServiceBuilder;
use tower_http::{
    request_id::{MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::{DefaultOnFailure, DefaultOnResponse, TraceLayer},
};
use tracing::Instrument;
use wallet::Wallet;

mod error;
mod file_descriptors;
mod logging;

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

async fn run_grpc_server(
    validator: Either<Validator, Wallet>,
    cancel: CancellationToken,
    addr: SocketAddr,
) -> Result<()> {
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
            server::validator::Server::new(validator.clone(), cancel.clone())
        }));

    let mut reflection_service_builder = tonic_reflection::server::Builder::configure()
        .with_service_name(CryptoServiceServer::<server::crypto::CryptoServiceServer>::NAME)
        .with_service_name(ValidatorServiceServer::<Validator>::NAME);
    for descriptor_set in proto::ENCODED_FILE_DESCRIPTOR_SETS {
        reflection_service_builder =
            reflection_service_builder.register_encoded_file_descriptor_set(descriptor_set);
    }

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
                .map_err(error::GrpcServer::Reflection)?,
        )
        .add_service(health_service);

    server
        .serve_with_shutdown(addr, cancel.clone().cancelled())
        .await
        .map_err(|err| miette::Report::from_err(error::GrpcServer::Serve { addr, source: err }))
}

async fn spawn_gbt_server<RpcClient>(
    server: cusf_enforcer_mempool::server::Server<Wallet, RpcClient>,
    serve_addr: SocketAddr,
) -> miette::Result<jsonrpsee::server::ServerHandle>
where
    RpcClient: bitcoin_jsonrpsee::MainClient + Send + Sync + 'static,
{
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

    let http_middleware = tower::ServiceBuilder::new()
        // Limit the gbt_server to 1 concurrent request, preventing runaway
        // callers (e.g. a stuck signet miner subprocess) from hammering
        // the endpoint with parallel requests.
        .layer(tower::limit::ConcurrencyLimitLayer::new(1))
        .layer(tracer);
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

async fn is_address_port_open(addr: &str) -> Result<bool, std::io::Error> {
    let addr = addr.strip_prefix("tcp://").unwrap_or_default();
    match tokio::time::timeout(Duration::from_millis(250), TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => Ok(true),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(false),
        Ok(Err(e)) => Err(e),
        Err(_) => Ok(false),
    }
}

/// Initialize a mempool sync. Returns the sync handle plus a receiver that
/// fires if the background sync task hits an error. Caller should
/// race the receiver against the shutdown signal
async fn sync_mempool<Enforcer, RpcClient>(
    mut enforcer: Enforcer,
    network: bitcoin::Network,
    rpc_client: RpcClient,
    zmq_addr_sequence: &str,
    cancel: CancellationToken,
) -> Result<
    (
        cusf_enforcer_mempool::mempool::MempoolSync<Enforcer>,
        oneshot::Receiver<error::MempoolTask<Enforcer>>,
    ),
    error::MempoolTask<Enforcer>,
>
where
    Enforcer: cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer + Send + Sync + 'static,
    RpcClient: bitcoin_jsonrpsee::client::MainClient + Send + Sync + 'static,
{
    tracing::debug!(%zmq_addr_sequence, "Ensuring ZMQ address for mempool sync is reachable");

    match is_address_port_open(zmq_addr_sequence).await {
        Ok(true) => (),
        Ok(false) => {
            let err = error::MempoolTask::ZmqNotReachable {
                zmq_addr_sequence: zmq_addr_sequence.to_owned(),
            };
            return Err(err);
        }
        Err(err) => {
            let err = error::MempoolTask::ZmqCheck {
                addr: zmq_addr_sequence.to_owned(),
                source: err,
            };
            return Err(err);
        }
    }

    let init_sync_mempool_future = cusf_enforcer_mempool::mempool::init_sync_mempool(
        &mut enforcer,
        network,
        &rpc_client,
        zmq_addr_sequence,
        cancel.cancelled(),
    )
    .inspect_ok(|_| tracing::info!(%zmq_addr_sequence,  "Initial mempool sync complete"))
    .instrument(tracing::info_span!("initial_mempool_sync"));

    let (sequence_stream, mempool, tx_cache) = match init_sync_mempool_future.await {
        Ok(res) => res,
        Err(err) => {
            let err = error::MempoolTask::InitialSync(err);
            tracing::error!("{:#}", ErrorChain::new(&err));
            return Err(err);
        }
    };

    let (err_tx, err_rx) = oneshot::channel();
    let mempool = cusf_enforcer_mempool::mempool::MempoolSync::new(
        enforcer,
        mempool,
        tx_cache,
        rpc_client,
        sequence_stream,
        |err| async move {
            let err = error::MempoolTask::SyncTask(err);
            let _send_err: Result<(), _> = err_tx.send(err);
        },
    );

    Ok((mempool, err_rx))
}

async fn wait_for_error_or_shutdown<E>(
    cancel: CancellationToken,
    err_rx: oneshot::Receiver<E>,
) -> Result<(), miette::Report>
where
    E: std::error::Error + Send + Sync + 'static,
{
    tokio::select! {
        () = cancel.cancelled() => Ok(()),
        recv = err_rx => match recv {
            Ok(err) => Err(miette::Report::from_err(err)),
            Err(oneshot::Canceled) => Ok(()),
        }
    }
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

async fn run_no_mempool_task(
    mut enforcer: Either<Validator, Wallet>,
    mainchain_client: bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
    zmq_addr_sequence: String,
    cancel: CancellationToken,
) -> Result<(), miette::Report> {
    tracing::info!("CUSF enforcer task w/o mempool: starting");
    cusf_enforcer_mempool::cusf_enforcer::task(
        &mut enforcer,
        &mainchain_client,
        &zmq_addr_sequence,
        cancel.cancelled(),
    )
    .await
    .into_diagnostic()
    .wrap_err("CUSF enforcer task w/o mempool")
}

async fn run_validator_mempool_task(
    validator: Validator,
    network: bitcoin::Network,
    mainchain_client: bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
    zmq_addr_sequence: String,
    cancel: CancellationToken,
) -> Result<(), miette::Report> {
    tracing::info!("mempool sync task w/validator: starting");
    let (_, err_rx) = sync_mempool(
        validator,
        network,
        mainchain_client,
        &zmq_addr_sequence,
        cancel.clone(),
    )
    .await
    .map_err(miette::Report::from_err)?;
    wait_for_error_or_shutdown(cancel, err_rx).await
}

async fn run_wallet_mempool_task(
    wallet: Wallet,
    network: bitcoin::Network,
    mainchain_client: bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
    zmq_addr_sequence: String,
    cli: cli::Config,
    cancel: CancellationToken,
) -> Result<(), miette::Report> {
    // A pre-requisite for the mempool sync task is that the wallet is
    // initialized and unlocked. Give a nice error message if this is not
    // the case!
    if !wallet.is_initialized().await {
        #[derive(Debug, Diagnostic, Error)]
        #[error(
            "Wallet-based mempool sync requires an initialized wallet! Create one with the CreateWallet RPC method."
        )]
        #[diagnostic(code(wallet_not_initialized))]
        struct WalletNotInitialized;
        return Err(WalletNotInitialized.into());
    }

    let mining_reward_address = match cli.mining_opts.coinbase_recipient.clone() {
        Some(addr) => addr,
        None => wallet.get_new_address().await.map_err(|err| {
            miette::Report::from_err(err).wrap_err("failed to get mining reward address")
        })?,
    };
    let network_info = mainchain_client
        .get_network_info()
        .await
        .map_err(|err| miette::Report::from_err(err).wrap_err("failed to get network info"))?;

    let (mempool, err_rx) = sync_mempool(
        wallet,
        network,
        mainchain_client.clone(),
        &zmq_addr_sequence,
        cancel.clone(),
    )
    .await
    .map_err(miette::Report::from_err)?;

    // Bitcoin Core refuses to service block templates until IBD is finished. We should
    // therefore run this check AFTER the mempool sync task has finished, and then
    // gracefully handle the error.
    //
    // Once the mempool sync has finished, we're finished with syncing up until the
    // current Core tip. However, Core can still be stuck in IBD, hence the need for this loop
    // https://github.com/bitcoin/bitcoin/blob/6c4fe401e908cff1b67d80035b117aae15fe7db6/src/rpc/mining.cpp#L771-L773
    let sample_block_template = loop {
        // https://github.com/bitcoin/bitcoin/blob/6c4fe401e908cff1b67d80035b117aae15fe7db6/src/rpc/protocol.h#L58
        const RPC_CLIENT_NOT_CONNECTED: i32 = -9; // No P2P peers connected, refuses block template requests
        const RPC_IN_WARMUP: i32 = -10; // In IBD etc, refuses block template requests
        match get_block_template(&mainchain_client, network).await {
            Ok(block_template) => break block_template,
            Err(wallet::error::BitcoinCoreRPC {
                method: _,
                error: jsonrpsee::core::client::Error::Call(err),
            }) if err.code() == RPC_IN_WARMUP => {
                tracing::debug!(
                    err = format!("{}: {}", err.code(), err.message()),
                    "Transient Bitcoin Core error, retrying before spawning GBT server...",
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
            Err(wallet::error::BitcoinCoreRPC {
                method: _,
                error: jsonrpsee::core::client::Error::Call(err),
            }) if err.code() == RPC_CLIENT_NOT_CONNECTED => {
                #[derive(Debug, Diagnostic, Error)]
                #[error(
                    "Bitcoin Core has no P2P peers connected and refuses to serve block template requests"
                )]
                #[diagnostic(code(bip300301_enforcer::bitcoin_core_not_connected))]
                struct NotConnected;

                return Err(NotConnected.into());
            }
            Err(err) => {
                return Err(
                    miette::Report::from_err(err).wrap_err("failed to get sample block template")
                );
            }
        }
    };

    let gbt_cache_lifetime = cli
        .gbt_cache_lifetime_s
        .map(|secs| Duration::from_secs(secs.get()));
    let gbt_server = cusf_enforcer_mempool::server::Server::new(
        mining_reward_address.script_pubkey(),
        mempool,
        network,
        network_info,
        mainchain_client,
        gbt_cache_lifetime,
        sample_block_template,
    )
    .into_diagnostic()?;
    let server_handle = spawn_gbt_server(gbt_server, cli.serve_rpc_addr).await?;

    let result = wait_for_error_or_shutdown(cancel, err_rx).await;

    tracing::debug!("stopping `getblocktemplate` JSON-RPC server");

    // This should never fail. The only failure mode is the server
    // already being stopped, and we have full control over that.
    if let Err(err) = server_handle.stop() {
        tracing::error!("error stopping `getblocktemplate` JSON-RPC server: {err:#}");
    }
    result
}

async fn run_exit_after_sync(validator: Validator, goal_height: u32) -> Result<(), miette::Report> {
    tracing::info!("Waiting for sync to height {} before exiting", goal_height);

    let mut events = std::pin::pin!(validator.subscribe_events());
    tracing::debug!("Subscribed to validator events");

    loop {
        match events.next().await {
            Some(Ok(Event::ConnectBlock { header_info, .. }))
                if header_info.height >= goal_height =>
            {
                tracing::info!("Synced to block height {}, exiting", header_info.height);
                return Ok(());
            }

            Some(Ok(_)) => {}

            Some(Err(err)) => {
                return Err(miette::Report::from_err(err)
                    .wrap_err("exit-after-sync: error receiving event"));
            }

            None => {
                return Err(miette!("exit-after-sync: no event received"));
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
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
    let _tracing_guard = logging::set_tracing_subscriber(
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
        Err(err @ MainRestClientError::RestServerNotEnabled) => {
            return Err(miette::Report::from_err(err));
        }

        Err(err) => {
            let wrapped = miette::Report::from_err(err)
                .wrap_err("unable to check availability of mainchain REST server");

            return Err(wrapped);
        }
    }
    tracing::info!("verified mainchain REST server at `{raw_url}` is available");

    let mainchain_client = rpc_client::create_client(&cli.node_rpc_opts)?;
    tracing::info!(
        "created mainchain JSON-RPC client from options: {}:*****@{}",
        cli.node_rpc_opts.user.as_deref().unwrap_or("cookie"),
        cli.node_rpc_opts.addr,
    );

    let info = 'info: loop {
        // From Bitcoin Core src/rpc/protocol.h
        const RPC_IN_WARMUP: i32 = -28;

        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

        // If Bitcoin Core is booting up, we don't want to fail hard.
        // Check for errors that should go away after a little while,
        // and tolerate those.
        let err = match mainchain_client.get_blockchain_info().await {
            Ok(info) => break 'info info,
            Err(err) => err,
        };
        let () = match err {
            Error::Call(err) if err.code() == RPC_IN_WARMUP => {
                tracing::debug!(
                    err = format!("{}: {}", err.code(), err.message()),
                    "Transient Bitcoin Core error, retrying...",
                );
                Ok(())
            }
            Error::Transport(err)
                if err.downcast_ref::<transport::Error>().is_some_and(|e| {
                    matches!(e, transport::Error::Rejected { status_code: 401 })
                }) =>
            {
                let message = if let Some(user) = cli.node_rpc_opts.user {
                    format!("we tried connecting with RPC user '{user}' and a password, you probably have something different in your bitcoin.conf file")
                } else {
                    "check your Bitcoin Core RPC credentials".to_string()
                };
                #[derive(Debug, Diagnostic, Error)]
                #[error("Invalid Bitcoin Core RPC credentials")]
                #[diagnostic(code(bip300301_enforcer::rpc_credentials))]
                struct UnauthorizedError {
                    #[help]
                    message: String,
                }

                return Err(UnauthorizedError {
                    message,
                }
                .into());
            }
            err => Err(wallet::error::BitcoinCoreRPC {
                method: "getblockchaininfo".to_string(),
                error: err,
            }),
        }
        .map_err(|err| {
            miette::Report::from(err).wrap_err("unable to verify Bitcoin Core is ready")
        })?;

        tokio::time::sleep(RETRY_DELAY).await;
    };

    tracing::info!(
        network = %info.chain,
        blocks = %info.blocks,
        "Connected to mainchain client",
    );

    // Verify we're talking to a supported Bitcoin Core version.
    let detected_version = version::check_bitcoin_core_version(
        &mainchain_client,
        cli.bitcoin_core_expected_version,
        cli.bitcoin_core_skip_version_check,
    )
    .await?;
    if cli.bitcoin_core_skip_version_check {
        tracing::warn!(
            version = detected_version.version,
            subversion = %detected_version.subversion,
            "Bitcoin Core version check skipped (--bitcoin-core-skip-version-check)"
        );
    } else {
        tracing::info!(
            version = detected_version.version,
            subversion = %detected_version.subversion,
            major = detected_version.major,
            "Bitcoin Core version accepted"
        );
    }

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
        cli.node_blocks_dir_opts.dir.clone(),
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
            #[derive(Debug, Diagnostic, Error)]
            #[error("`txindex` is not enabled on the mainchain client")]
            #[diagnostic(code(bip300301_enforcer::txindex_not_enabled))]
            struct TxindexNotEnabled;

            return Err(TxindexNotEnabled.into());
        }

        let magic = signet_challenge
            .as_deref()
            .map(compute_signet_magic)
            .unwrap_or_else(|| info.chain.magic());
        let wallet = Wallet::new(
            &wallet_data_dir,
            &cli,
            mainchain_client.clone(),
            validator,
            magic,
            signet_challenge,
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

    let cancel = CancellationToken::new();

    let node_zmq_addr_sequence = match cli.node_zmq_addr_sequence.clone() {
        Some(addr) => addr,
        None => get_zmq_addr_sequence(mainchain_client.clone()).await?,
    };

    // Spawn the wallet's full scan before pushing anything into the
    // JoinSet. It is blocking on the wallet's behalf and any error should
    // short-circuit startup
    if let Either::Right(wallet) = &enforcer
        && cli.wallet_opts.full_scan
    {
        wallet.full_scan().await?;
    }

    // Task name -> task result
    let mut tasks: tokio::task::JoinSet<(&'static str, Result<(), miette::Report>)> =
        tokio::task::JoinSet::new();

    {
        let cancel = cancel.clone();
        let mainchain_client = mainchain_client.clone();
        let zmq = node_zmq_addr_sequence.clone();
        let network = info.chain;
        match (cli.enable_mempool, enforcer.clone()) {
            (false, enforcer_inner) => {
                tasks.spawn(async move {
                    let res =
                        run_no_mempool_task(enforcer_inner, mainchain_client, zmq, cancel).await;
                    ("enforcer task", res)
                });
            }
            (true, Either::Left(validator)) => {
                tasks.spawn(async move {
                    let res = run_validator_mempool_task(
                        validator,
                        network,
                        mainchain_client,
                        zmq,
                        cancel,
                    )
                    .await;
                    ("validator mempool task", res)
                });
            }
            (true, Either::Right(wallet)) => {
                let cli = cli.clone();
                tasks.spawn(async move {
                    let res = run_wallet_mempool_task(
                        wallet,
                        network,
                        mainchain_client,
                        zmq,
                        cli,
                        cancel,
                    )
                    .await;
                    ("wallet mempool task", res)
                });
            }
        }
    }

    {
        let cancel = cancel.clone();
        let enforcer = enforcer.clone();
        let addr = cli.serve_grpc_addr;
        tasks.spawn(async move {
            let res = run_grpc_server(enforcer, cancel, addr).await;
            ("gRPC server", res)
        });
    }

    {
        let cancel = cancel.clone();
        tasks.spawn(async move {
            cancel.cancelled().await;
            let res = json_rpc_server_handle
                .stop()
                .map_err(|err| miette!("error stopping JSON-RPC server: {err:#}"));
            ("JSON-RPC server", res)
        });
    }

    if let Either::Right(wallet) = enforcer.clone() {
        // Big wallets (thousands of UTXOs) can get really bad performance for the
        // periodic sync. Therefore we expose a knob to disable it.
        let sync_source_disabled = cli.wallet_opts.sync_source == WalletSyncSource::Disabled;

        if !cli.wallet_opts.skip_periodic_sync && !sync_source_disabled {
            let cancel = cancel.clone();
            tasks.spawn(async move {
                let res = wallet.sync_task(cancel).await;
                ("wallet sync task", res)
            });
        }
    }

    if let Some(exit_after_sync) = cli.exit_after_sync {
        let validator = match &enforcer {
            Either::Left(validator) => validator.clone(),
            Either::Right(wallet) => wallet.validator().clone(),
        };

        let goal_height = if exit_after_sync != 0 {
            // Sanity check that the user didn't give an exit height that's
            // below what we're currently at.

            let mainchain_tip = validator.try_get_block_height()?;
            if exit_after_sync < mainchain_tip.unwrap_or_default() {
                return Err(miette!(
                    "exit-after-sync height {} is below current height {}",
                    exit_after_sync,
                    mainchain_tip.unwrap_or_default()
                ));
            }

            exit_after_sync
        } else {
            info.blocks
        };

        tasks.spawn(async move {
            let res = run_exit_after_sync(validator, goal_height).await;
            ("exit-after-sync task", res)
        });
    }

    // First arm: whichever task resolves first decides the exit status. Any
    // `Ok` is treated as "this task is done, time to shut everything else
    // down". An `Err` propagates to the process exit.
    //
    // Second arm: ctrl_c triggers a normal shutdown.
    let result: Result<(), miette::Report> = tokio::select! {
        joined = tasks.join_next() => match joined {
            Some(Ok((name, Ok(())))) => {
                tracing::info!("{name} finished cleanly; shutting down");
                Ok(())
            }
            Some(Ok((name, Err(err)))) => {
                if cfg!(target_os = "macos") && format!("{err:#}").contains("Too many open files") {
                    tracing::error!(err = %err, "too many open files, dumping all open file descriptors");
                    match file_descriptors::list_open_descriptors_macos() {
                        Ok(open_fds) => tracing::error!("open file descriptors: {:#?}", open_fds),
                        Err(err) => tracing::error!(err = %err, "failed to list open file descriptors"),
                    }
                }
                Err(err.wrap_err(format!("{name} failed")))
            }
            Some(Err(join_err)) => {
                Err(miette!("a task panicked or was cancelled: {join_err:#}"))
            }
            None => Err(miette!("no tasks were spawned")),
        },
        signal = tokio::signal::ctrl_c() => match signal {
            Ok(()) => {
                tracing::info!("Shutting down due to process interruption");
                Ok(())
            }
            Err(err) => {
                tracing::error!("Unable to receive interruption signal: {err:#}");
                Err(miette!("Unable to receive interruption signal: {err:#}"))
            }
        },
    };

    // Drain: signal everyone, wait briefly, abort what won't quit
    cancel.cancel();
    let drain = async {
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((name, Ok(()))) => tracing::info!("{name} finished during shutdown"),
                Ok((name, Err(err))) => {
                    tracing::error!(task = name, "task failed during shutdown: {err:#}")
                }
                Err(join_err) => {
                    tracing::error!("task panicked during shutdown: {join_err:#}")
                }
            }
        }
    };
    if tokio::time::timeout(tokio::time::Duration::from_secs(1), drain)
        .await
        .is_err()
    {
        tracing::error!("shutdown timeout reached; aborting remaining tasks");
        tasks.abort_all();
    }

    result
}
