use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bdk_wallet::bip39::{Language, Mnemonic};
use bip300301_enforcer_lib::{
    cli::{self, WalletSyncSource},
    errors::ErrorChain,
    p2p::compute_signet_magic,
    proto::{
        crypto_service::{CRYPTO_SERVICE_SERVICE_NAME, CryptoServiceExt},
        mainchain_service::{
            VALIDATOR_SERVICE_SERVICE_NAME, ValidatorServiceExt, WALLET_SERVICE_SERVICE_NAME,
            WalletServiceExt,
        },
    },
    rpc_client, server,
    types::NetworkParams,
    validator::{
        SyncStateSummary, Validator,
        main_rest_client::{MainRestClient, MainRestClientError},
    },
    version,
    wallet::{self, error::BitcoinCoreRPC},
};
use bitcoin::ScriptBuf;
use bitcoin_jsonrpsee::{MainClient, jsonrpsee::http_client::transport};
use clap::Parser;
use connectrpc::Router;
use connectrpc_health::{HealthExt, HealthService, StaticChecker};
use connectrpc_reflection::Reflector;
use either::Either;
use futures::{FutureExt as _, TryFutureExt as _, channel::oneshot};
use http::{Request, header::HeaderName};
use jsonrpsee::{core::client::Error, server::middleware::rpc::RpcServiceBuilder};
use miette::{Diagnostic, IntoDiagnostic, Result, WrapErr as _, miette};
use reqwest::Url;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tower_http::{
    request_id::{MakeRequestId, RequestId, SetRequestIdLayer},
    trace::{DefaultOnFailure, TraceLayer},
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

/// Build the shared HTTP-level tower stack for our jsonrpsee servers
/// (request-id stamping + propagation + per-request tracing span +
/// structured response event).
///
/// Returns the *inner* `tower::Layer` (post-`into_inner()`), so callers
/// can compose it with additional layers (e.g. `ConcurrencyLimitLayer`).
///
/// A macro rather than a fn because (a) `tracing::span!` needs a literal
/// span name and (b) the layered `ServiceBuilder<Stack<..>>` return type
/// is impractical to spell out.
macro_rules! jsonrpsee_tracer {
    ($span_name:literal) => {{
        // Ordering here matters! Order here is from official docs on request IDs tracings
        // https://docs.rs/tower-http/latest/tower_http/request_id/index.html#using-trace
        tower::ServiceBuilder::new()
            .layer(set_request_id_layer())
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(|request: &http::Request<_>| {
                        let request_id = request
                            .headers()
                            .get(http::HeaderName::from_static(REQUEST_ID_HEADER))
                            .and_then(|h| h.to_str().ok())
                            .filter(|s| !s.is_empty());
                        tracing::span!(tracing::Level::DEBUG, $span_name, request_id)
                    })
                    .on_request(())
                    .on_eos(())
                    .on_response(
                        |response: &http::Response<_>,
                         duration: std::time::Duration,
                         _span: &tracing::Span| {
                            tracing::info!(
                                server = $span_name,
                                status = response.status().as_u16(),
                                duration_ms = duration.as_millis(),
                                "served JSON-RPC request",
                            );
                        },
                    )
                    .on_failure(DefaultOnFailure::new().level(tracing::Level::ERROR)),
            )
            .layer(tower_http::request_id::PropagateRequestIdLayer::new(
                http::HeaderName::from_static(REQUEST_ID_HEADER),
            ))
            .into_inner()
    }};
}

async fn connect_rpc_access_log(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    /// Cap how much error-response body we buffer for logging.
    const ERROR_BODY_LOG_LIMIT: usize = 4 * 1024;

    let uri = req.uri().clone();
    let request_id_header = http::HeaderName::from_static(REQUEST_ID_HEADER);
    let request_id = req.headers().get(&request_id_header).cloned();

    let started = std::time::Instant::now();
    let mut response = next.run(req).await;
    let duration_ms = started.elapsed().as_millis();
    let http_status = response.status();

    // Stamp the request id on the response so the caller sees it back.
    // Connect dispatchers don't preserve headers added by inner tower
    // layers, so we mirror it here unconditionally.
    if let Some(ref id) = request_id {
        response.headers_mut().insert(request_id_header, id.clone());
    }

    let request_id = request_id
        .as_ref()
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default()
        .to_owned();

    // Connect procedure: `/package.Service/Method`. Logged as-is rather
    // than split — the connect router may be mounted under a non-root
    // path, in which case the URL path carries a prefix we don't want to
    // mis-interpret as the service component.
    let procedure = uri.path().to_owned();

    // On non-2xx, buffer the body so we can pull the Connect error code
    // and message out of it. Bodies are small JSON like
    // `{"code":"invalid_argument","message":"..."}`.
    if http_status.is_client_error() || http_status.is_server_error() {
        let (parts, body) = response.into_parts();
        let body_bytes = axum::body::to_bytes(body, ERROR_BODY_LOG_LIMIT)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(
                    procedure, %http_status, duration_ms, request_id,
                    "connect_rpc: failed to buffer error body: {err:#}",
                );
                axum::body::Bytes::from_static(b"{}")
            });
        let (connect_code, message) =
            match buffa::serde_json::from_slice::<connectrpc::ConnectError>(&body_bytes) {
                Ok(err) => (
                    err.code.as_str().to_owned(),
                    err.message.unwrap_or_default(),
                ),
                Err(_) => (
                    "unknown".to_owned(),
                    String::from_utf8_lossy(&body_bytes).into_owned(),
                ),
            };
        if http_status.is_server_error() {
            tracing::error!(
                procedure, code = %connect_code, duration_ms,
                request_id, message = %message, "connect_rpc",
            );
        } else {
            tracing::warn!(
                procedure, code = %connect_code, duration_ms,
                request_id, message = %message, "connect_rpc",
            );
        }
        return axum::response::Response::from_parts(parts, axum::body::Body::from(body_bytes));
    }

    tracing::info!(
        procedure,
        code = "ok",
        duration_ms,
        request_id,
        "connect_rpc",
    );
    response
}

/// Treat a POST with no body the same as a POST with `{}`, so
/// `curl $url` works the same as `curl -d '{}' $url`
/// for RPCs without parameters.
///
/// Streaming RPCs uses `application/connect+json` /
/// `application/connect+proto`, so we leave those alone.
async fn fill_empty_json_body(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let is_unary_json = req.method() == http::Method::POST
        && req
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("application/json")
            });
    // Missing or zero Content-Length on a unary JSON POST → assume empty body.
    // (A well-behaved JSON client always sends Content-Length for a fixed payload.)
    let is_empty = req
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .is_none_or(|v| v.to_str().ok().and_then(|s| s.parse::<u64>().ok()) == Some(0));
    if !is_unary_json || !is_empty {
        return next.run(req).await;
    }

    let (mut parts, _body) = req.into_parts();
    parts.headers.insert(
        http::header::CONTENT_LENGTH,
        http::HeaderValue::from_static("2"),
    );
    next.run(axum::extract::Request::from_parts(
        parts,
        axum::body::Body::from("{}"),
    ))
    .await
}

/// For Connect unary GETs, default `encoding` to `json` and `message` to `{}`
/// when the client omitted them. Other query params are left untouched.
/// This makes it possible to do a simple curl invocation of GET endpoints:
///
///     curl -H 'content-type: application/json' http://localhost:50051/cusf.mainchain.v1.WalletService/GetBalance
///
/// As opposed to:
///     curl -H 'content-type: application/json' http://localhost:50051/cusf.mainchain.v1.WalletService/GetBalance?encoding=json=message={}
async fn fill_connect_get_defaults(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if req.method() != http::Method::GET {
        return next.run(req).await;
    }

    let existing = req.uri().query().unwrap_or("");
    let mut has_encoding = false;
    let mut has_message = false;
    for pair in existing.split('&').filter(|s| !s.is_empty()) {
        let key = pair.split('=').next().unwrap_or("");
        match key {
            "encoding" => has_encoding = true,
            "message" => has_message = true,
            _ => {}
        }
    }
    if has_encoding && has_message {
        return next.run(req).await;
    }

    let mut new_query = existing.to_string();
    if !has_encoding {
        if !new_query.is_empty() {
            new_query.push('&');
        }
        new_query.push_str("encoding=json");
    }
    if !has_message {
        if !new_query.is_empty() {
            new_query.push('&');
        }
        let empty_message: String = url::form_urlencoded::byte_serialize(b"{}").collect();
        new_query.push_str(&format!("message={empty_message}"));
    }

    let mut uri_parts = req.uri().clone().into_parts();
    let path = uri_parts
        .path_and_query
        .as_ref()
        .map(|pq| pq.path())
        .unwrap_or("/");
    uri_parts.path_and_query = Some(
        format!("{path}?{new_query}")
            .parse()
            .expect("valid path+query"),
    );
    let new_uri = http::Uri::from_parts(uri_parts).expect("valid uri");

    let (mut parts, body) = req.into_parts();
    parts.uri = new_uri;
    next.run(axum::extract::Request::from_parts(parts, body))
        .await
}

async fn run_connect_server(
    validator: Either<Validator, Wallet>,
    cancel: CancellationToken,
    addr: SocketAddr,
) -> Result<(), error::ConnectServer> {
    let (inner_validator, wallet) = match validator {
        Either::Left(v) => (v, None),
        Either::Right(w) => (w.validator().clone(), Some(w)),
    };
    let router = Router::new();
    let router = Arc::new(server::crypto::CryptoServiceServer).register(router);
    let router = Arc::new(server::validator::Server::new(
        inner_validator,
        cancel.clone(),
    ))
    .register(router);

    let mut services: Vec<&'static str> =
        vec![CRYPTO_SERVICE_SERVICE_NAME, VALIDATOR_SERVICE_SERVICE_NAME];
    let router = if let Some(wallet) = wallet {
        services.push(WALLET_SERVICE_SERVICE_NAME);
        Arc::new(wallet).register(router)
    } else {
        router
    };

    let health_checker = Arc::new(StaticChecker::with_services(services));
    let router = Arc::new(HealthService::from_arc(Arc::clone(&health_checker))).register(router);

    // gRPC server reflection (v1 + v1alpha), so tools like grpcurl/buf curl can
    // discover and call our services without local proto files. Backed by the
    // descriptor pool embedded in the buffa-generated code (`reflect_mode=bridge`
    // in buf.gen.yaml). Each generated package embeds the full file closure, so
    // any one package's pool covers every service.
    let reflector = Reflector::from_descriptor_pool(Arc::clone(
        bip300301_enforcer_lib::proto::mainchain::descriptor_pool(),
    ))
    .map_err(error::ConnectServer::Reflection)?;
    let router = connectrpc_reflection::install(router, reflector);

    let app = axum::Router::new()
        .fallback_service(router.into_axum_service())
        .layer(
            tower::ServiceBuilder::new()
                .layer(set_request_id_layer())
                .layer(axum::middleware::from_fn(connect_rpc_access_log))
                .layer(axum::middleware::from_fn(fill_empty_json_body))
                .layer(axum::middleware::from_fn(fill_connect_get_defaults)),
        );

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|err| error::ConnectServer::Bind { addr, source: err })?;

    tracing::info!("Listening for Connect RPC on {addr}");

    let cancel_clone = cancel.clone();
    let res = axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel_clone.cancelled().await })
        .await
        .map_err(|err| error::ConnectServer::Serve { addr, source: err });

    // Mark all services as not serving when we're shutting down
    health_checker.shutdown();

    res
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

    let http_middleware = tower::ServiceBuilder::new()
        // Limit the gbt_server to 1 concurrent request, preventing runaway
        // callers (e.g. a stuck signet miner subprocess) from hammering
        // the endpoint with parallel requests.
        .layer(tower::limit::ConcurrencyLimitLayer::new(1))
        .layer(jsonrpsee_tracer!("gbt_server"));
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
        rpc_client,
        zmq_addr_sequence,
        Box::pin(cancel.cancelled_owned()).fuse(),
    )
    .inspect_ok(|_| tracing::info!(%zmq_addr_sequence,  "Initial mempool sync complete"))
    .instrument(tracing::info_span!("initial_mempool_sync"));

    let synced = match init_sync_mempool_future.await {
        Ok(res) => res,
        Err(err) => {
            let err = error::MempoolTask::InitialSync(err);
            tracing::error!("{:#}", ErrorChain::new(&err));
            return Err(err);
        }
    };

    let (err_tx, err_rx) = oneshot::channel();
    let mempool =
        cusf_enforcer_mempool::mempool::MempoolSync::new(enforcer, synced, |err| async move {
            let err = error::MempoolTask::SyncTask(err);
            let _send_err: Result<(), _> = err_tx.send(err);
        });

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
    mainchain_client: bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
    zmq_addr_sequence: String,
    cancel: CancellationToken,
) -> Result<(), miette::Report> {
    tracing::info!("mempool sync task w/validator: starting");
    let (_, err_rx) = sync_mempool(
        validator,
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

fn report_sync_state(validator: &Validator, state_file: &Path) -> Result<SyncStateSummary> {
    match validator.sync_state_summary() {
        Ok(summary) => {
            tracing::info!(
                state_digest = %summary.digest(),
                tip_hash = %summary.tip_hash,
                tip_height = summary.tip_height,
                sidechain_proposals = summary.sidechains.len(),
                active_sidechains = summary.active_sidechain_count(),
                pending_withdrawal_bundles = summary.pending_withdrawal_count(),
                "writing consensus state summary to {}", state_file.display(),
            );
            let json = summary.to_json_pretty();
            std::fs::write(state_file, format!("{json}\n")).map_err(|err| {
                miette::Report::from_err(err).wrap_err("write sync state summary")
            })?;
            Ok(summary)
        }
        err => err,
    }
}

fn load_consensus_reference(path: &Path) -> Result<SyncStateSummary, miette::Report> {
    let contents = std::fs::read_to_string(path)
        .map_err(|err| miette::Report::from_err(err).wrap_err("load consensus reference"))?;

    SyncStateSummary::from_json(&contents)
        .map_err(|err| miette!("consensus-state file {} is invalid: {err}", path.display()))
}

fn verify_against_reference(
    produced: SyncStateSummary,
    reference: &Option<SyncStateSummary>,
) -> Result<(), miette::Report> {
    use std::fmt::Write as _;

    let Some(reference) = reference else {
        return Ok(());
    };

    if produced == *reference {
        tracing::info!(
            digest = %produced.digest(),
            tip_height = produced.tip_height,
            "consensus-state verification PASSED: state matches reference exactly",
        );
        return Ok(());
    }

    // Short hash/digest prefixes keep every report line on one line
    let short = |v: &dyn std::fmt::Display| -> String { v.to_string().chars().take(12).collect() };

    let mut report = String::new();
    let _ = writeln!(report, "consensus-state verification FAILED");
    let _ = writeln!(
        report,
        "  current   tip:    height {} ({}..)",
        produced.tip_height,
        short(&produced.tip_hash)
    );
    let _ = writeln!(
        report,
        "  reference tip:    height {} ({}..)",
        reference.tip_height,
        short(&reference.tip_hash)
    );
    let _ = writeln!(
        report,
        "  current   digest: {}..",
        short(&produced.digest())
    );
    let _ = write!(
        report,
        "  reference digest: {}..",
        short(&reference.digest())
    );

    let sidechain_diffs = produced.sidechain_diffs(reference);
    if !sidechain_diffs.is_empty() {
        let _ = write!(report, "\nsidechain differences:");
        for diff in sidechain_diffs {
            let _ = write!(report, "\n  - {diff}");
        }
    }

    Err(miette!("{report}"))
}

/// Drive a single bounded sync of `validator` forward to `goal_height`, logging
/// progress and a blocks/sec summary. Shared by the bounded-sync CLI modes
/// (`--exit-after-sync`, `--verify-consensus-state`, `--freeze-at-height`);
/// `label` names the mode in log and error messages. Returns the validator's
/// height once the sync completes.
async fn sync_to_height(
    validator: &mut Validator,
    goal_height: u32,
    label: &str,
    mainchain_client: &bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
    cancel: &CancellationToken,
) -> Result<Option<u32>, miette::Report> {
    use cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer as _;

    let start_height = validator.try_get_block_height().ok().flatten();

    if let Some(current) = start_height
        && current > goal_height
    {
        return Err(miette!(
            "data dir is already synced to height {current}, past the requested height \
                {goal_height}"
        ));
    }

    let target_hash = mainchain_client
        .getblockhash(goal_height as usize)
        .await
        .map_err(|err| {
            miette::Report::from_err(err)
                .wrap_err(format!("{label}: failed to resolve block #{goal_height}"))
        })?;

    tracing::info!(
        goal_height,
        target_hash = %target_hash,
        ?start_height,
        "{label}: syncing to height #{goal_height} ({target_hash})",
    );

    let started_at = tokio::time::Instant::now();
    validator
        .sync_to_tip(cancel.cancelled(), target_hash)
        .await?;
    let elapsed = started_at.elapsed();

    let final_height = validator.try_get_block_height().ok().flatten();
    let blocks_synced = match (start_height, final_height) {
        (Some(start), Some(end)) => u64::from(end.saturating_sub(start)),
        (None, Some(end)) => u64::from(end) + 1,
        _ => 0,
    };
    let blocks_per_sec = blocks_synced as f64 / elapsed.as_secs_f64();
    let elapsed = jiff::SignedDuration::try_from(elapsed).unwrap_or_default();
    tracing::info!(
        "{label}: synced {blocks_synced} blocks to height {} in {elapsed} ({blocks_per_sec:.1} blocks/sec)",
        final_height.map_or_else(|| "?".to_owned(), |h| h.to_string()),
    );

    Ok(final_height)
}

async fn run_exit_after_sync(
    mut validator: Validator,
    goal_height: u32,
    state_file: PathBuf,
    reference: Option<SyncStateSummary>,
    mainchain_client: bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
    cancel: CancellationToken,
) -> Result<(), miette::Report> {
    sync_to_height(
        &mut validator,
        goal_height,
        "exit-after-sync",
        &mainchain_client,
        &cancel,
    )
    .await?;

    let produced = report_sync_state(&validator, &state_file)?;
    verify_against_reference(produced, &reference)
}

async fn run_freeze_at_height(
    mut validator: Validator,
    goal_height: u32,
    mainchain_client: bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
    cancel: CancellationToken,
) -> Result<(), miette::Report> {
    sync_to_height(
        &mut validator,
        goal_height,
        "freeze-at-height",
        &mainchain_client,
        &cancel,
    )
    .await?;

    tracing::info!("freeze-at-height: node stays queryable until shutdown (Ctrl-C)");

    cancel.cancelled().await;
    Ok(())
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

    // Validate the verification reference early, before creating node connections etc.
    let verify_reference = cli
        .verify_consensus_state
        .clone()
        .map(|path| {
            let reference = load_consensus_reference(path.as_ref())?;
            tracing::info!(
                reference = %path.display(),
                tip_height = reference.tip_height,
                reference_digest = %reference.digest(),
                "Loaded consensus-state reference, will sync to #{} and verify",
                reference.tip_height,
            );
            Ok::<_, miette::Report>(reference)
        })
        .transpose()?;

    let raw_url = format!("http://{}", cli.node_rpc_opts.addr);
    let mainchain_rest_client = MainRestClient::new(
        Url::parse(&raw_url)
            .map_err(|err| miette!("invalid mainchain REST URL `{raw_url}`: {err:#}"))?,
    );

    let ts = tokio::time::Instant::now();
    let mut retries = 0;
    loop {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
        const MAX_RETRIES: u32 = 5;

        match mainchain_rest_client.get_chain_info().await {
            Ok(_) => {
                tracing::info!(
                    "verified mainchain REST server is enabled in {:?}",
                    ts.elapsed()
                );
                break;
            }
            Err(err @ MainRestClientError::RestServerNotEnabled) => {
                return Err(miette::Report::from_err(err));
            }
            // Bitcoin Core responds 503 on the REST interface while warming
            // up. Tolerate this, the same way we tolerate RPC_IN_WARMUP on
            // the JSON-RPC interface below.
            Err(MainRestClientError::Http(err))
                if err.status() == Some(reqwest::StatusCode::SERVICE_UNAVAILABLE) =>
            {
                if retries >= MAX_RETRIES {
                    return Err(miette::Report::from_err(err).wrap_err(format!(
                        "mainchain REST server still unavailable after {MAX_RETRIES} retries"
                    )));
                }
                retries += 1;
                tracing::debug!(
                    err = %err,
                    "Mainchain REST server not ready yet, retrying ({retries}/{MAX_RETRIES})...",
                );
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(err) => {
                let wrapped = miette::Report::from_err(err)
                    .wrap_err("unable to check availability of mainchain REST server");

                return Err(wrapped);
            }
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

    // Resolve BIP300 thresholds + enforcement activation height: from the
    // explicit --network-preset if given, otherwise from the node's network.
    let network_params = match cli.network_preset {
        Some(preset) => {
            let params = preset.params();
            tracing::info!(
                ?preset,
                bip300_activation_height = params.bip300_activation_height,
                thresholds = ?params.thresholds,
                "Applying network parameter preset"
            );
            params
        }
        None => NetworkParams::for_network(info.chain),
    };

    // Both wallet data and validator data are stored under the same root
    // directory. Add a subdirectories to clearly indicate which
    // is which. Presets get their own namespace (e.g. `main-drynet1`)
    let chain_dir_name = match network_params.datadir_suffix {
        Some(suffix) => format!("{}-{}", info.chain, suffix),
        None => info.chain.to_string(),
    };
    let validator_data_dir = cli.data_dir.join("validator").join(&chain_dir_name);
    let wallet_data_dir = cli.data_dir.join("wallet").join(&chain_dir_name);

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
        network_params,
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

    // `--exit-after-sync` / `--verify-consensus-state` / `--freeze-at-height`
    // drive a single bounded sync to a specific height (spawned below) instead
    // of the normal task that chases the live chain tip. Skip the tip-chasing
    // sync in that mode so the two don't race on the validator. The periodic
    // wallet sync still runs (it only reads the validator, same as in normal
    // operation alongside the tip-chasing writer).
    let exit_after_sync_mode =
        cli.exit_after_sync.is_some() || cli.verify_consensus_state.is_some();

    let bounded_sync_mode = exit_after_sync_mode || cli.freeze_at_height.is_some();

    // The mempool / block-template server is brought up by the tip-chasing
    // task, which is skipped in bounded-sync mode. It cannot run alongside a
    // bounded sync: the mempool init forces the validator to Bitcoin Core's
    // live tip (see cusf_enforcer::initial_sync), which is exactly what these
    // modes avoid. Warn rather than silently ignoring the flag.
    if bounded_sync_mode && cli.enable_mempool {
        tracing::warn!(
            "--enable-mempool has no effect in bounded-sync mode (--exit-after-sync / \
             --verify-consensus-state / --freeze-at-height): the mempool / block-template \
             server requires syncing to the live chain tip, which these modes do not do. \
             The validator gRPC, JSON-RPC, and wallet endpoints are still available",
        );
    }

    if !bounded_sync_mode {
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
                    let res =
                        run_validator_mempool_task(validator, mainchain_client, zmq, cancel).await;
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
            let res = run_connect_server(enforcer, cancel, addr)
                .await
                .map_err(miette::Report::from_err);
            ("Connect RPC server", res)
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

    if exit_after_sync_mode {
        let validator = match &enforcer {
            Either::Left(validator) => validator.clone(),
            Either::Right(wallet) => wallet.validator().clone(),
        };

        // The reference (if any) was loaded + validated at startup.
        let (goal_height, reference) = match verify_reference {
            Some(reference) => (reference.tip_height, Some(reference)),
            None => {
                let exit_after_sync = cli.exit_after_sync.unwrap_or(0);
                let goal_height = if exit_after_sync != 0 {
                    exit_after_sync
                } else {
                    info.blocks
                };
                (goal_height, None)
            }
        };

        let state_file = cli.data_dir.join("consensus-state.json");
        let mainchain_client = mainchain_client.clone();
        let cancel = cancel.clone();
        tasks.spawn(async move {
            let res = run_exit_after_sync(
                validator,
                goal_height,
                state_file,
                reference,
                mainchain_client,
                cancel,
            )
            .await;
            ("exit-after-sync task", res)
        });
    }

    if let Some(goal_height) = cli.freeze_at_height {
        let validator = match &enforcer {
            Either::Left(validator) => validator.clone(),
            Either::Right(wallet) => wallet.validator().clone(),
        };
        let mainchain_client = mainchain_client.clone();
        let cancel = cancel.clone();
        tasks.spawn(async move {
            let res = run_freeze_at_height(validator, goal_height, mainchain_client, cancel).await;
            ("freeze-at-height task", res)
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
