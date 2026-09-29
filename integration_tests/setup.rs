//! Setup for an integration test

use std::{
    borrow::Borrow, collections::HashMap, ffi::OsStr, future::Future, net::SocketAddr,
    path::PathBuf, sync::LazyLock,
};

use anyhow::anyhow;
use bip300301_enforcer_lib::{
    proto::{
        self,
        mainchain::{
            BlockHeaderInfo, BroadcastWithdrawalBundleRequest, BroadcastWithdrawalBundleResponse,
            GetChainTipRequest, GetChainTipResponse,
        },
        mainchain_service::{
            BlockProducerServiceClient, MiningServiceClient, ValidatorServiceClient,
            WalletServiceClient,
        },
    },
    types::{BlindedM6, BlindedM6Error, M6id, SidechainNumber},
};
use bitcoin::{Address, BlockHash, Txid};
use connectrpc::{
    ConnectError,
    client::{ClientConfig, HttpClient},
};
use futures::{
    channel::{mpsc, oneshot},
    future,
};
use jsonrpsee::{core::client::ClientT as _, rpc_params};
use reserve_port::ReservedPort;
use temp_dir::TempDir;
use thiserror::Error;
use tokio::{
    net::{TcpListener, TcpStream},
    time::{Duration, sleep, timeout},
};

use crate::{
    signet_chain_params::{SIGNET_CACHED_CHAIN_BLOCKS, SIGNET_CHALLENGE_SECRET_KEY},
    signet_miner::{SignetMiner, TemplateSource},
    util::{
        AbortOnDrop, BinPaths, Bitcoind, BitcoindClient, Electrs, Enforcer, FileDumpConfig,
        TestFileRegistry,
    },
};

#[derive(Clone, Copy, Debug)]
pub enum Network {
    Regtest,
    Signet,
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Regtest => "Regtest",
            Self::Signet => "Signet",
        })
    }
}

impl From<Network> for bitcoin::Network {
    fn from(network: Network) -> Self {
        match network {
            Network::Regtest => Self::Regtest,
            Network::Signet => Self::Signet,
        }
    }
}

// Signet-specific setup
pub struct SignetSetup {
    secret_key: bitcoin::PrivateKey,
    signet_challenge: bitcoin::ScriptBuf,
    signet_challenge_addr: bitcoin::Address,
    signet_magic: bitcoin::p2p::Magic,
}

impl SignetSetup {
    fn new() -> anyhow::Result<Self> {
        let secret_key = bitcoin::PrivateKey::from_slice(
            &SIGNET_CHALLENGE_SECRET_KEY,
            bitcoin::NetworkKind::Test,
        )?;
        let cpk = bitcoin::CompressedPublicKey::from_private_key(
            &bitcoin::secp256k1::Secp256k1::new(),
            &secret_key,
        )?;
        let (signet_challenge, signet_challenge_addr) =
            crate::signet_chain_params::signet_challenge(&cpk);
        let signet_magic = bip300301_enforcer_lib::p2p::compute_signet_magic(&signet_challenge);
        tracing::info!(
            signet_challenge = %hex::encode(signet_challenge.as_bytes()),
            %signet_magic,
            mining_address = %signet_challenge_addr,
        );
        Ok(Self {
            secret_key,
            signet_challenge,
            signet_challenge_addr,
            signet_magic,
        })
    }

    /// Import the signet challenge key. The enforcer only mines on signet if
    /// the node wallet owns the challenge address.
    async fn init_bitcoind_wallet(&self, bitcoind_client: &BitcoindClient) -> anyhow::Result<()> {
        tracing::debug!("Importing secret key");
        let mining_descriptor = {
            use bdk_wallet::miniscript;
            let descriptor = bdk_wallet::descriptor!(wpkh(self.secret_key))?;
            descriptor.0.to_string_with_secret(&descriptor.1)
        };
        let import_descriptors_output: serde_json::Value = bitcoind_client
            .request(
                "importdescriptors",
                rpc_params![serde_json::json!([
                    {
                        "desc": mining_descriptor,
                        "timestamp": "now",
                        "active": false,
                    },
                ])],
            )
            .await?;
        if import_descriptors_output != serde_json::json!([{ "success": true }]) {
            anyhow::bail!("Importing descriptors failed: `{import_descriptors_output}`")
        }
        tracing::debug!(
            signet_challenge_addr = %self.signet_challenge_addr,
            "Checking that the signet challenge addr is loaded"
        );
        let getaddressinfo_output: serde_json::Value = bitcoind_client
            .request(
                "getaddressinfo",
                rpc_params![self.signet_challenge_addr.to_string()],
            )
            .await?;
        tracing::debug!(%getaddressinfo_output);
        Ok(())
    }

    fn miner(&self) -> SignetMiner {
        SignetMiner::new(self.secret_key.inner, self.signet_challenge.clone())
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MiningMode {
    GenerateBlocks,
    GetBlockTemplate,
}

#[derive(Clone, Copy, Debug)]
pub enum Mode {
    GetBlockTemplate,
    Mempool,
    NoMempool,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::GetBlockTemplate => "GetBlockTemplate",
            Self::Mempool => "Mempool",
            Self::NoMempool => "NoMempool",
        })
    }
}

impl Mode {
    pub fn enable_mempool(&self) -> bool {
        match self {
            Self::GetBlockTemplate | Self::Mempool => true,
            Self::NoMempool => false,
        }
    }

    pub fn mining_mode(&self) -> MiningMode {
        match self {
            Self::GetBlockTemplate => MiningMode::GetBlockTemplate,
            Self::Mempool | Self::NoMempool => MiningMode::GenerateBlocks,
        }
    }
}

#[derive(Debug)]
pub struct ReservedPorts {
    pub bitcoind_listen: ReservedPort,
    pub bitcoind_rpc: ReservedPort,
    pub bitcoind_zmq_sequence: ReservedPort,
    pub electrs_electrum_rpc: ReservedPort,
    pub electrs_electrum_http: ReservedPort,
    pub electrs_monitoring: ReservedPort,
    pub enforcer_serve_grpc: ReservedPort,
    pub enforcer_serve_rpc: ReservedPort,
}

/// Whether some process is already listening on `127.0.0.1:port`.
///
/// `reserve-port` calls a port free when it can `bind` it on `127.0.0.1` and
/// `[::1]`, which is not the same question as "is this port ours to serve on".
/// A process that binds the *wildcard* address (`*:port`) -- a container
/// runtime, a docker-desktop-style proxy, some other dev server -- does not
/// stop those binds from succeeding: the kernel lets a more specific address
/// coexist with a wildcard one. It does, however, answer every connection to
/// `127.0.0.1:port` until our own child binds the more specific address.
///
/// So a reserved port can be shadowed by a foreign listener, and the harness
/// cannot tell the difference by binding: [`wait_for_port`] sees the foreign
/// listener accept and reports the child ready, and the first gRPC request
/// goes to a peer that accepts the connection and then never answers -- a
/// request that hangs for its entire timeout instead of failing and being
/// retried.
///
/// A genuinely free port refuses connections. Anything that accepts one is
/// already shadowed.
fn port_is_shadowed(port: u16) -> bool {
    const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
    let addr = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    std::net::TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).is_ok()
}

/// [`ReservedPort::random`], skipping ports that a foreign listener already
/// answers on (see [`port_is_shadowed`]).
fn reserve_unshadowed_port() -> Result<ReservedPort, reserve_port::Error> {
    /// Shadowed ports are rare; needing more attempts than this means the
    /// scanned range is unusable, which is worth failing loudly over.
    const MAX_ATTEMPTS: usize = 32;

    for _ in 0..MAX_ATTEMPTS {
        let reserved = ReservedPort::random()?;
        if !port_is_shadowed(reserved.port()) {
            return Ok(reserved);
        }
        tracing::warn!(
            port = reserved.port(),
            "reserved port is already answered by another process on this machine; \
             taking another"
        );
        // Deliberately never dropped: `ReservedPort`'s `Drop` hands the port
        // back to `reserve-port`'s global finder, which would offer this same
        // shadowed port to the next test that asks.
        std::mem::forget(reserved);
    }
    Err(reserve_port::Error::FailedToReservePort)
}

impl ReservedPorts {
    pub fn new() -> Result<Self, reserve_port::Error> {
        Ok(Self {
            bitcoind_listen: reserve_unshadowed_port()?,
            bitcoind_rpc: reserve_unshadowed_port()?,
            bitcoind_zmq_sequence: reserve_unshadowed_port()?,
            electrs_electrum_rpc: reserve_unshadowed_port()?,
            electrs_electrum_http: reserve_unshadowed_port()?,
            electrs_monitoring: reserve_unshadowed_port()?,
            enforcer_serve_grpc: reserve_unshadowed_port()?,
            enforcer_serve_rpc: reserve_unshadowed_port()?,
        })
    }
}

pub fn new_bitcoind(
    bin_paths: &BinPaths,
    bitcoind_kind: BitcoindKind,
    data_dir: PathBuf,
    reserved_ports: &ReservedPorts,
    network: Network,
    signet_setup: Option<&SignetSetup>,
) -> Result<Bitcoind, crate::util::VarError> {
    Ok(Bitcoind {
        path: bitcoind_path(bin_paths, bitcoind_kind)?.clone(),
        data_dir,
        listen_port: reserved_ports.bitcoind_listen.port(),
        network: network.into(),
        onion_ports: None,
        rpc_user: "drivechain".to_owned(),
        rpc_pass: "integrationtesting".to_owned(),
        rpc_port: reserved_ports.bitcoind_rpc.port(),
        rpc_host: "127.0.0.1".to_owned(),
        signet_challenge: signet_setup
            .as_ref()
            .map(|setup| setup.signet_challenge.clone()),
        txindex: true,
        zmq_sequence_port: reserved_ports.bitcoind_zmq_sequence.port(),
    })
}

/// Waits for a TCP port to become available by attempting to connect periodically.
pub async fn wait_for_port(
    host: &str,
    port: u16,
    timeout_duration: Duration,
) -> anyhow::Result<()> {
    let target_addr_str = format!("{host}:{port}");
    let target_addr: SocketAddr = target_addr_str
        .parse()
        .map_err(|_| anyhow!("Invalid address format {host}:{port}"))?;
    let check_interval = Duration::from_millis(100);

    let task = async {
        loop {
            match TcpStream::connect(target_addr).await {
                Ok(_) => {
                    tracing::debug!("Port {port} on {host} is open.");
                    return Ok(());
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionRefused
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // Port not open yet, wait and retry
                    tracing::trace!("Port {port} on {host} not open yet ({e}), waiting...");
                    sleep(check_interval).await;
                }
                Err(e) => {
                    // Other IO error occurred
                    tracing::warn!(
                        "Error connecting to {host}:{port} while waiting: {e}. Retrying..."
                    );
                    // Still retry, maybe it's a transient issue
                    sleep(check_interval).await;
                }
            }
        }
    };

    match timeout(timeout_duration, task).await {
        Ok(Ok(())) => Ok(()), // Inner Ok(()) means success
        Ok(Err(e)) => Err(e), // Propagate inner error (though our loop logic makes this unlikely)
        Err(_) => Err(anyhow!(
            "Timeout waiting for port {host}:{port} to open after {timeout_duration:?}"
        )),
    }
}

/// Inverse of [`wait_for_port`]: wait until a port can be bound again.
/// `AbortOnDrop`'s `Drop` impl only calls `JoinHandle::abort`, which schedules
/// cancellation but doesn't guarantee the underlying child process (and the
/// port it holds) is actually gone by the time `drop` returns -- so a kill
/// immediately followed by a respawn on the same port can race the old
/// process's teardown. Poll for the port to actually free up instead of
/// guessing at a fixed delay.
///
/// This binds rather than connecting, because binding is the question the
/// caller actually has: can the process I am about to spawn take this port?
/// Every child here binds a specific `127.0.0.1` address, so the bind fails
/// while one still holds the port and succeeds once it is gone.
///
/// A connect probe cannot tell our child from an unrelated process listening on
/// the *wildcard* address, which answers connections to `127.0.0.1` forever and
/// so hangs the wait until it times out. That is not hypothetical: BSD
/// `SO_REUSEADDR` semantics let a specific bind coexist with a wildcard one, so
/// a port held that way still looks free to `reserve-port` and does get handed
/// out. A container publishing `0.0.0.0:8090` is enough to do it.
pub async fn wait_for_port_free(
    host: &str,
    port: u16,
    timeout_duration: Duration,
) -> anyhow::Result<()> {
    let target_addr_str = format!("{host}:{port}");
    let target_addr: SocketAddr = target_addr_str
        .parse()
        .map_err(|_| anyhow!("Invalid address format {host}:{port}"))?;
    let check_interval = Duration::from_millis(50);

    let task = async {
        loop {
            match TcpListener::bind(target_addr).await {
                // Hand it straight back: the caller is about to spawn something
                // that needs to bind it.
                Ok(listener) => {
                    drop(listener);
                    return;
                }
                Err(_) => {
                    tracing::trace!(
                        "Port {port} on {host} still held, waiting for it to free up..."
                    );
                    sleep(check_interval).await;
                }
            }
        }
    };

    match timeout(timeout_duration, task).await {
        Ok(()) => {
            tracing::debug!("Port {port} on {host} is free.");
            Ok(())
        }
        Err(_) => Err(anyhow!(
            "Timeout waiting for port {host}:{port} to free up after {timeout_duration:?}"
        )),
    }
}

/// Polls bitcoind via `getblockchaininfo` until it responds successfully.
/// The RPC port opens before bitcoind is ready to serve commands, so a TCP
/// probe alone is not enough.
pub async fn wait_for_bitcoind_ready(bitcoind_client: &BitcoindClient) -> anyhow::Result<()> {
    // When the whole suite runs at once, many bitcoind/electrs/enforcer
    // processes cold-start together. Apply a generous limit here that
    // doesn't crash long running tests, but catches stuck ones.
    const TIMEOUT: Duration = Duration::from_secs(120);
    let task = async {
        loop {
            match bitcoind_client
                .request::<serde_json::Value, _>("getblockchaininfo", rpc_params![])
                .await
            {
                Ok(_) => return,
                Err(e) => {
                    tracing::trace!("bitcoind not ready yet ({e}), waiting...");
                    sleep(WAIT_POLL_INTERVAL).await;
                }
            }
        }
    };
    timeout(TIMEOUT, task)
        .await
        .map_err(|_| anyhow!("Timeout waiting for bitcoind to become ready after {TIMEOUT:?}"))
}

/// Watches a spawned process for exiting, so that waiting for it to become
/// ready fails with the process's own error as soon as it dies, rather than
/// running into the wait's timeout.
pub struct ExitWatch {
    name: &'static str,
    exited: Option<oneshot::Receiver<String>>,
}

impl ExitWatch {
    /// The watch, and the error handler to spawn the process with. The
    /// handler still forwards the exit to `res_tx`.
    pub fn new(
        name: &'static str,
        res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> (Self, impl FnOnce(anyhow::Error) + Send + 'static) {
        let (exited_tx, exited_rx) = oneshot::channel();
        let on_exit = move |err: anyhow::Error| {
            let _unsent: Result<(), _> = exited_tx.send(format!("{err:#}"));
            let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
        };
        let watch = Self {
            name,
            exited: Some(exited_rx),
        };
        (watch, on_exit)
    }

    /// Await `ready`, unless the process exits first.
    pub async fn unless_exited<T>(
        &mut self,
        ready: impl Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let Some(mut exited) = self.exited.take() else {
            return ready.await;
        };
        let mut ready = std::pin::pin!(ready);
        tokio::select! {
            res = &mut ready => {
                self.exited = Some(exited);
                res
            }
            exit = &mut exited => match exit {
                Ok(err) => Err(anyhow!("{} exited before it was ready: {err}", self.name)),
                // Dropped without being called: the task was aborted, which
                // says nothing about the process.
                Err(oneshot::Canceled) => ready.await,
            },
        }
    }
}

/// Polls the validator via `get_chain_tip` until it reports a tip, and returns
/// it. The enforcer starts serving gRPC before the validator has finished its
/// initial sync, and RPCs that need the mainchain tip fail with `Unavailable`
/// (or `ValidatorNotSynced`) until then, so waiting for the port alone is not
/// enough.
pub async fn wait_for_validator_synced(
    client: &ValidatorServiceClient<Transport>,
) -> anyhow::Result<proto::mainchain::BlockHeaderInfo> {
    const TIMEOUT: Duration = Duration::from_secs(120);
    const CHECK_INTERVAL: Duration = Duration::from_millis(100);
    /// Budget for a single attempt. Connect RPCs carry no client-side deadline
    /// by default, so a request to a peer that accepts the connection and then
    /// never answers -- a foreign listener shadowing the port, an enforcer
    /// still binding it -- would otherwise spend the whole `TIMEOUT` inside one
    /// call and never reach the retry below. Bounding each attempt turns that
    /// into one lost interval, and the retry opens a fresh connection.
    const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
    let task = async {
        loop {
            let attempt = timeout(
                ATTEMPT_TIMEOUT,
                client.get_chain_tip(GetChainTipRequest::default()),
            );
            match attempt.await {
                Ok(Ok(resp)) => {
                    return resp
                        .into_owned()
                        .block_header_info
                        .into_option()
                        .ok_or_else(|| anyhow!("no block header info in chain tip"));
                }
                // Not ready yet. `Unavailable` means the validator is still
                // syncing; a transport error means the gRPC server isn't
                // actually serving yet, even though the port accepted a TCP
                // connection. With the whole suite starting processes at once
                // both are routine, so retry either until the timeout rather
                // than failing a test on a startup hiccup.
                Ok(Err(err)) => {
                    tracing::trace!("Validator not ready yet ({err}), waiting...");
                    sleep(CHECK_INTERVAL).await;
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        "chain tip request got no response within {ATTEMPT_TIMEOUT:?}; \
                         retrying on a fresh connection"
                    );
                }
            }
        }
    };
    timeout(TIMEOUT, task)
        .await
        .map_err(|_| anyhow!("Timeout waiting for validator to sync after {TIMEOUT:?}"))?
}

/// Default budget for the `wait_for_*` helpers below. Generous, because the
/// whole suite runs in parallel and the machine is saturated; these are
/// deadlines that catch a genuinely stuck test, not expected wait times.
pub const WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Interval between polls for conditions checked in-process or over an already
/// open connection (gRPC, a file read). Short, because these are cheap and the
/// conditions are normally already true on the first check.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Interval between polls for conditions that are expensive to check, such as
/// re-reading a large log. Polling these as fast as the cheap checks would put
/// more load on an already-saturated machine than it saves in latency.
pub const WAIT_POLL_INTERVAL_SLOW: Duration = Duration::from_millis(250);

/// Poll `check` until it reports the condition has been reached, erroring out
/// with `what` in the message if it hasn't happened within `WAIT_TIMEOUT`.
///
/// Prefer this over sleeping a fixed duration: it returns as soon as the state
/// is actually observable (normally on the first poll) instead of paying a
/// worst-case guess every run, and it fails loudly rather than silently
/// continuing against state that was never reached.
///
/// A failing `check` counts as "not yet", not as a test failure: processes are
/// still coming up while the suite runs in parallel, so an RPC can legitimately
/// be refused or time out on the first attempts. Whatever it failed with last
/// is reported if the deadline runs out.
pub async fn wait_until<Check, Fut>(what: &str, check: Check) -> anyhow::Result<()>
where
    Check: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<bool>>,
{
    wait_until_every(what, WAIT_POLL_INTERVAL, check).await
}

/// [`wait_until`], with an explicit poll interval. Use
/// [`WAIT_POLL_INTERVAL_SLOW`] for checks that are expensive to repeat.
pub async fn wait_until_every<Check, Fut>(
    what: &str,
    poll_interval: Duration,
    mut check: Check,
) -> anyhow::Result<()>
where
    Check: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<bool>>,
{
    let deadline = tokio::time::Instant::now() + WAIT_TIMEOUT;
    let mut last_err: Option<anyhow::Error> = None;
    loop {
        // Bound each individual check by whatever budget is left, so a single
        // hung RPC can't outlive the deadline.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, check()).await {
            Ok(Ok(true)) => return Ok(()),
            Ok(Ok(false)) => tracing::trace!("still waiting for {what}..."),
            Ok(Err(err)) => {
                tracing::trace!("still waiting for {what} (check failed: {err:#})");
                last_err = Some(err);
            }
            Err(_elapsed) => break,
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        sleep(poll_interval.min(remaining)).await;
    }
    Err(match last_err {
        Some(err) => err.context(format!(
            "Timed out after {WAIT_TIMEOUT:?} waiting for {what}; last check failed"
        )),
        None => anyhow!("Timeout waiting for {what} after {WAIT_TIMEOUT:?}"),
    })
}

/// Wait until `txid` is in `bitcoind_client`'s node's mempool.
///
/// The wallet broadcasts asynchronously, so a tx is not necessarily in the
/// node's mempool by the time the RPC that created it returns.
pub async fn wait_for_tx_in_mempool(
    bitcoind_client: &BitcoindClient,
    txid: &bitcoin::Txid,
) -> anyhow::Result<()> {
    wait_until(&format!("tx `{txid}` to enter the mempool"), || async {
        Ok(bitcoind_client
            .request::<serde_json::Value, _>("getmempoolentry", rpc_params![txid])
            .await
            .is_ok())
    })
    .await
}

/// Reported by the enforcer's `getblocktemplate` while its mempool syncs.
const RPC_CLIENT_IN_INITIAL_DOWNLOAD: i32 = -10;

/// Block until the enforcer's `getblocktemplate` endpoint serves templates,
/// rather than reporting that it is still syncing. Any other answer counts as
/// ready; a transport error is not an answer, and is retried.
pub async fn wait_for_block_templates(
    gbt_client: &jsonrpsee::http_client::HttpClient,
) -> anyhow::Result<()> {
    use cusf_enforcer_mempool::server::RpcClient as _;

    wait_until("the enforcer to serve block templates", || async {
        let request = bitcoin_jsonrpsee::client::BlockTemplateRequest::default();
        match gbt_client.get_block_template(request).await {
            Ok(_) => Ok(true),
            Err(jsonrpsee::core::client::Error::Call(err)) => {
                Ok(err.code() != RPC_CLIENT_IN_INITIAL_DOWNLOAD)
            }
            Err(err) => Err(err.into()),
        }
    })
    .await
}

/// Concatenate the enforcer's rolling log files.
///
/// Not `stdout.txt`: that only holds the run the harness spawned most recently,
/// whereas the rolling log in the data dir spans restarts — which is what a
/// test asserting across a restart needs.
///
/// Errors if there is no content yet, so callers polling for a line get a
/// "not yet" rather than silently matching against an empty string.
pub fn read_enforcer_log(enforcer_dir: &std::path::Path) -> anyhow::Result<String> {
    let log_dir = enforcer_dir.join("logs");
    let mut combined = String::new();
    for entry in std::fs::read_dir(&log_dir)? {
        let path = entry?.path();
        if path.is_file() {
            combined.push_str(&std::fs::read_to_string(&path)?);
        }
    }
    anyhow::ensure!(
        !combined.is_empty(),
        "no enforcer log content found in {}",
        log_dir.display()
    );
    Ok(combined)
}

/// Poll the enforcer's log until `pred` matches, returning the log that
/// satisfied it.
///
/// A failed read counts as "not yet": the log directory does not exist until
/// the enforcer has started, and a test may begin polling before that.
pub async fn wait_for_enforcer_log<Pred>(
    enforcer_dir: &std::path::Path,
    what: &str,
    mut pred: Pred,
) -> anyhow::Result<String>
where
    Pred: FnMut(&str) -> bool,
{
    wait_until(what, || {
        // The read is synchronous, so do it here and hand `wait_until` a ready
        // future; a read error propagates as a failed check, which it treats as
        // "not yet" and reports if the deadline runs out.
        let matched = read_enforcer_log(enforcer_dir).map(|log| pred(&log));
        async move { matched }
    })
    .await?;
    // Re-read rather than threading the matched contents out of the closure.
    // The log is append-only, so this can only be a superset of what matched.
    read_enforcer_log(enforcer_dir)
}

/// Wait until a sidechain proposal for `sidechain_number` has been persisted by
/// the block producer, so that the next block it builds carries the M1.
///
/// `CreateSidechainProposal` persists before it returns, but the unary response
/// to a server-streaming call resolves on headers, so poll the state the next
/// block template actually reads rather than assuming the write landed.
pub async fn wait_for_pending_proposal(
    client: &BlockProducerServiceClient<Transport>,
    sidechain_number: SidechainNumber,
) -> anyhow::Result<()> {
    use proto::mainchain::GetBlockProducerStateRequest;
    let slot = u32::from(sidechain_number.0);
    wait_until(
        &format!("sidechain proposal for slot {slot} to be persisted"),
        || async {
            let state = client
                .get_block_producer_state(GetBlockProducerStateRequest::default())
                .await?
                .into_owned();
            Ok(state.pending_proposals.into_iter().any(|proposal| {
                proto::unwrap_u32(proposal.sidechain_number).is_some_and(|number| number == slot)
            }))
        },
    )
    .await
}

/// Per-run state that bitcoind rewrites on startup, or that would leak one
/// run's runtime details into the next. Excluded when snapshotting a datadir
/// for reuse.
const DATADIR_VOLATILE_NAMES: &[&str] = &[
    ".cookie",
    ".lock",
    "anchors.dat",
    "banlist.json",
    "bitcoind.pid",
    "debug.log",
    "fee_estimates.dat",
    "mempool.dat",
    "peers.dat",
    "stderr.txt",
    "stdout.txt",
];

/// Recursively copy `src` into `dst`, skipping [`DATADIR_VOLATILE_NAMES`].
fn copy_datadir(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if DATADIR_VOLATILE_NAMES.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        let (src_path, dst_path) = (entry.path(), dst.join(&name));
        if entry.file_type()?.is_dir() {
            copy_datadir(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Written into the cached chain directory, recording what the chain actually
/// is. A chain mined for a different signet challenge is not merely stale --
/// bitcoind would reject every block in it -- so it is checked before use
/// rather than trusting whatever a cache (local or CI) restored.
const SIGNET_CHAIN_MARKER_FILE: &str = "chain-info.txt";

fn signet_chain_marker(signet_setup: &SignetSetup) -> String {
    format!(
        "challenge={}\nblocks={SIGNET_CACHED_CHAIN_BLOCKS}\n",
        hex::encode(signet_setup.signet_challenge.as_bytes()),
    )
}

/// Whether `dir` holds a chain usable by *this* build: present, and built for
/// the current challenge and block count.
fn usable_cached_signet_chain(dir: &std::path::Path, signet_setup: &SignetSetup) -> bool {
    if !dir.join("signet").is_dir() {
        return false;
    }
    let marker = std::fs::read_to_string(dir.join(SIGNET_CHAIN_MARKER_FILE)).unwrap_or_default();
    if marker != signet_chain_marker(signet_setup) {
        tracing::warn!(
            "Cached signet chain at `{}` was built for a different challenge or \
             block count; re-mining it.",
            dir.display()
        );
        return false;
    }
    true
}

/// Resolved once per process: several tests can reach [`Setup::setup`] at the
/// same time, and they must not mine into the same directory concurrently.
/// The first caller mines; the rest await its result.
static CACHED_SIGNET_CHAIN: LazyLock<tokio::sync::OnceCell<Option<PathBuf>>> =
    LazyLock::new(tokio::sync::OnceCell::new);

/// Path to the pre-mined signet chain, mining it first if it isn't there yet.
///
/// Mining signet blocks costs real proof-of-work, and a fresh chain needs 100+
/// blocks before any coinbase is spendable -- which cost more than the rest of
/// the test suite put together. Since [`SIGNET_CHALLENGE_SECRET_KEY`] is fixed,
/// that chain is identical every run, so it is mined once and reused: by later
/// runs on the same machine, and on CI by caching `SIGNET_CHAIN_DIR`.
///
/// Returns `None` when `SIGNET_CHAIN_DIR` is unset, i.e. when there is nowhere
/// to keep a chain. Signet tests then mine to coinbase maturity themselves:
/// much slower, but self-contained and correct.
async fn cached_signet_chain(
    bin_paths: &BinPaths,
    signet_setup: &SignetSetup,
) -> anyhow::Result<Option<PathBuf>> {
    CACHED_SIGNET_CHAIN
        .get_or_try_init(|| async {
            let Some(dir) = std::env::var_os("SIGNET_CHAIN_DIR").map(PathBuf::from) else {
                tracing::debug!("SIGNET_CHAIN_DIR is unset, not caching a signet chain");
                return Ok(None);
            };
            if !usable_cached_signet_chain(&dir, signet_setup) {
                let () = mine_cached_signet_chain(bin_paths, &dir, signet_setup).await?;
            }
            Ok(Some(dir))
        })
        .await
        .cloned()
}

/// Mine the cached signet chain into `out_dir`, replacing anything already
/// there.
///
/// Runs bitcoind on a throwaway datadir, mines
/// [`SIGNET_CACHED_CHAIN_BLOCKS`] blocks to an address the node wallet owns,
/// shuts bitcoind down cleanly, then snapshots the datadir.
async fn mine_cached_signet_chain(
    bin_paths: &BinPaths,
    out_dir: &std::path::Path,
    signet_setup: &SignetSetup,
) -> anyhow::Result<()> {
    tracing::info!(
        "Mining {SIGNET_CACHED_CHAIN_BLOCKS}-block signet chain into `{}`. \
         This is a one-off: later runs reuse it.",
        out_dir.display()
    );
    let reserved_ports = ReservedPorts::new()?;
    let dirs = Directories::new()?;
    let (res_tx, _res_rx) = mpsc::unbounded::<anyhow::Result<()>>();

    let mut bitcoind = new_bitcoind(
        bin_paths,
        BitcoindKind::Patched,
        dirs.bitcoin_dir.clone(),
        &reserved_ports,
        Network::Signet,
        Some(signet_setup),
    )?;
    // Match what the tests run with, so the snapshot doesn't force a reindex.
    bitcoind.txindex = true;
    let (mut bitcoind_exit, on_exit) = ExitWatch::new("bitcoind", res_tx);
    let bitcoind_task =
        bitcoind.spawn_command_with_args::<String, String, _, _, _>([], [], on_exit);
    let bitcoind_client = bitcoind.rpc_client()?;
    bitcoind_exit
        .unless_exited(wait_for_bitcoind_ready(&bitcoind_client))
        .await?;

    let _create_wallet_output: serde_json::Value = bitcoind_client
        .request("createwallet", rpc_params!["integration-test"])
        .await?;
    let () = signet_setup.init_bitcoind_wallet(&bitcoind_client).await?;

    // A fresh wallet address rather than `signet_challenge_addr`, keeping the
    // funds tests spend apart from the block-signing key.
    let mining_address = bitcoind_client
        .request::<String, _>("getnewaddress", rpc_params![])
        .await?
        .parse::<bitcoin::Address<_>>()?
        .require_network(bitcoin::Network::Signet)?;
    tracing::info!(%mining_address, "Mining cached chain's coinbases to the node wallet");

    // Mine against Bitcoin Core's own templates: these are plain funding
    // blocks, with no BIP300 messages that would need the enforcer.
    let signet_miner = signet_setup.miner();
    let payout = mining_address.script_pubkey();
    for height in 1..=SIGNET_CACHED_CHAIN_BLOCKS {
        let _block_hash = signet_miner
            .mine_block(
                &bitcoind_client,
                TemplateSource::Node {
                    payout: payout.clone(),
                },
            )
            .await?;
        if height % 25 == 0 || height == SIGNET_CACHED_CHAIN_BLOCKS {
            tracing::info!("Mined {height}/{SIGNET_CACHED_CHAIN_BLOCKS} signet blocks");
        }
    }
    let blocks: u32 = bitcoind_client
        .request("getblockcount", rpc_params![])
        .await?;
    anyhow::ensure!(
        blocks == SIGNET_CACHED_CHAIN_BLOCKS,
        "expected to mine {SIGNET_CACHED_CHAIN_BLOCKS} blocks, chain is at height {blocks}"
    );

    // Shut bitcoind down cleanly so the snapshot has a consistent chainstate
    // and a flushed wallet, rather than one that needs recovery on load.
    // bitcoind closes its RPC port early in shutdown and keeps flushing after,
    // so wait for the process itself to exit -- not for the port to free up.
    tracing::info!("Stopping bitcoind before snapshotting");
    let _stop_output: String = bitcoind_client.request("stop", rpc_params![]).await?;
    // An open connection keeps bitcoind serving `503`s through shutdown.
    drop(bitcoind_client);
    timeout(Duration::from_secs(120), bitcoind_task.into_inner())
        .await
        .map_err(|_elapsed| anyhow!("Timed out waiting for bitcoind to shut down"))??;

    if out_dir.exists() {
        std::fs::remove_dir_all(out_dir)?;
    }
    copy_datadir(&dirs.bitcoin_dir, out_dir)?;
    std::fs::write(
        out_dir.join(SIGNET_CHAIN_MARKER_FILE),
        signet_chain_marker(signet_setup),
    )?;
    tracing::info!(
        "Wrote cached signet chain ({SIGNET_CACHED_CHAIN_BLOCKS} blocks) to `{}`",
        out_dir.display()
    );
    Ok(())
}

/// Running tasks, aborted on drop
pub struct Tasks {
    // MUST be dropped before electrs and bitcoind. `Option` (rather than the
    // task unconditionally present) so `kill_enforcer`/`restart_enforcer` can
    // explicitly drop the old process before spawning a replacement bound to
    // the same ports.
    _enforcer: Option<AbortOnDrop<()>>,
    // MUST be dropped before bitcoind. Also `Option`, for the same reason as
    // `_enforcer` -- electrs sometimes needs restarting independently (it's
    // known to panic on some reorgs; an unrelated, pre-existing limitation
    // of the pinned binary, not the enforcer).
    _electrs: Option<AbortOnDrop<()>>,
    _bitcoind: AbortOnDrop<()>,
}

type Transport = HttpClient;

/// Construct a connectrpc transport/config pair for a plaintext gRPC endpoint
/// served by our enforcer.
fn client_config(port: u16) -> anyhow::Result<ClientConfig> {
    let uri: http::Uri = format!("http://127.0.0.1:{port}")
        .parse()
        .map_err(|err| anyhow!("invalid client URI: {err}"))?;
    Ok(ClientConfig::new(uri))
}

#[derive(Clone, Debug)]
pub struct Directories {
    pub base_dir: TempDir,
    pub bitcoin_dir: PathBuf,
    pub electrs_dir: PathBuf,
    pub enforcer_dir: PathBuf,
}

impl Directories {
    fn new() -> anyhow::Result<Self> {
        let base_dir = TempDir::new()?;
        // leak unless explicitly allowed to cleanup
        base_dir.leak();

        let bitcoin_dir = base_dir.path().join("bitcoind");

        let electrs_dir = base_dir.path().join("electrs");

        let enforcer_dir = base_dir.path().join("enforcer");

        for dir in [&bitcoin_dir, &electrs_dir, &enforcer_dir] {
            std::fs::create_dir(dir)?;
        }

        Ok(Directories {
            base_dir,
            bitcoin_dir,
            electrs_dir,
            enforcer_dir,
        })
    }

    /// Register this node's bitcoind and enforcer logs, so a failing test
    /// dumps them.
    ///
    /// `label_suffix` names the node in a test that runs more than one, and
    /// so would otherwise print four identically labelled logs.
    pub fn register_files(
        &self,
        file_registry: &TestFileRegistry,
        test_name: &str,
        label_suffix: Option<&str>,
    ) {
        let label = |what: &str| match label_suffix {
            Some(suffix) => format!("{what} ({suffix})"),
            None => what.to_owned(),
        };
        for (dir, what) in [
            (&self.bitcoin_dir, "Bitcoin Core"),
            (&self.enforcer_dir, "Enforcer"),
        ] {
            for stream in ["stdout", "stderr"] {
                file_registry.register_file(
                    test_name,
                    dir.join(format!("{stream}.txt")),
                    FileDumpConfig::new().with_label(label(&format!("{what} {stream}"))),
                );
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub enum BitcoindKind {
    #[default]
    Patched,
    Unpatched,
}

pub fn bitcoind_regtest_magic() -> Option<String> {
    std::env::var("BITCOIND_REGTEST_MAGIC")
        .ok()
        .filter(|magic| !magic.is_empty())
}

/// The opcode the bitcoind under test reserves for `OP_DRIVECHAIN`, as the
/// enforcer's `--op-drivechain` spells it (`nop5` or `nop8`). Unset for
/// builds that follow BIP300. Every enforcer the harness spawns gets it as
/// that flag; [`op_drivechain`] resolves it for tests that build treasury
/// scripts by hand.
pub fn bitcoind_op_drivechain() -> Option<String> {
    std::env::var("BITCOIND_OP_DRIVECHAIN")
        .ok()
        .filter(|opcode| !opcode.is_empty())
}

pub fn op_drivechain() -> anyhow::Result<bip300301_enforcer_lib::types::OpDrivechain> {
    use bip300301_enforcer_lib::types::OpDrivechain;
    match bitcoind_op_drivechain().as_deref() {
        None | Some("nop5") => Ok(OpDrivechain::NOP5),
        Some("nop8") => Ok(OpDrivechain::NOP8),
        Some(other) => {
            anyhow::bail!("unknown BITCOIND_OP_DRIVECHAIN `{other}` (expected nop5 or nop8)")
        }
    }
}

fn bitcoind_path(
    bin_paths: &BinPaths,
    bitcoind_kind: BitcoindKind,
) -> Result<&PathBuf, crate::util::VarError> {
    match bitcoind_kind {
        BitcoindKind::Patched => bin_paths.bitcoind(),
        BitcoindKind::Unpatched => bin_paths.bitcoind_unpatched(),
    }
}

/// Whether the enforcer runs with a wallet. Is an enum instead of a
/// bool to make `Default` derivable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EnforcerWallet {
    #[default]
    Enabled,
    Disabled,
}

#[derive(Default)]
pub struct SetupOpts<
    BitcoindArg = String,
    EnforcerArg = String,
    BitcoindArgs = Vec<BitcoindArg>,
    EnforcerArgs = Vec<EnforcerArg>,
> where
    BitcoindArg: AsRef<OsStr>,
    EnforcerArg: AsRef<OsStr>,
    BitcoindArgs: IntoIterator<Item = BitcoindArg>,
    EnforcerArgs: IntoIterator<Item = EnforcerArg>,
{
    pub bitcoind_args: BitcoindArgs,
    pub bitcoind_kind: BitcoindKind,
    pub enforcer_args: EnforcerArgs,
    pub enforcer_wallet: EnforcerWallet,
}

pub struct PostSetup {
    pub network: Network,
    pub mode: Mode,
    /// The node's RPC credentials and port, for processes the harness starts
    /// against it. Tests talk to the node over `bitcoind_client`.
    pub rpc_user: String,
    pub rpc_pass: String,
    pub rpc_port: u16,
    pub bitcoind_client: BitcoindClient,
    // MUST occur before temp dirs and reserved ports in order to ensure that processes are dropped
    // before reserved ports are freed and temp dirs are cleared
    pub tasks: Tasks,
    /// Always `Some(_)` if `network == Network::Signet`, `None` otherwise
    pub signet_miner: Option<SignetMiner>,
    pub gbt_client: jsonrpsee::http_client::HttpClient,
    pub validator_service_client: ValidatorServiceClient<Transport>,
    pub wallet_service_client: WalletServiceClient<Transport>,
    pub block_producer_service_client: BlockProducerServiceClient<Transport>,
    pub mining_service_client: MiningServiceClient<Transport>,
    pub mining_address: Address,
    pub receive_address: Address,
    // MUST occur after tasks in order to ensure that tasks are dropped
    // before temp dirs are cleared
    pub directories: Directories,
    // MUST occur after tasks in order to ensure that tasks are dropped
    // before reserved ports are freed
    pub reserved_ports: ReservedPorts,
}

impl PostSetup {
    pub async fn setup<BitcoindArg, EnforcerArg, BitcoindArgs, EnforcerArgs>(
        bin_paths: &BinPaths,
        mode: Mode,
        network: Network,
        reserved_ports: ReservedPorts,
        dirs: Directories,
        opts: SetupOpts<BitcoindArg, EnforcerArg, BitcoindArgs, EnforcerArgs>,
        res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> anyhow::Result<Self>
    where
        BitcoindArg: AsRef<OsStr>,
        EnforcerArg: AsRef<OsStr>,
        BitcoindArgs: IntoIterator<Item = BitcoindArg>,
        EnforcerArgs: IntoIterator<Item = EnforcerArg>,
    {
        tracing::info!("Running setup");
        let signet_setup = if let Network::Signet = network {
            Some(SignetSetup::new()?)
        } else {
            None
        };

        let enable_wallet = opts.enforcer_wallet == EnforcerWallet::Enabled;
        // No wallet/mempool constraint to assert: `MiningService` is served on
        // regtest and signet regardless of either, and the one mode that does
        // need a mempool (`GetBlockTemplate`) enables it by construction.

        // Start signet from the pre-mined chain when one is cached, so the
        // test doesn't have to re-do its proof-of-work. It ships bitcoind's
        // wallet too, already holding mature coinbases.
        let cached_signet_chain = match signet_setup.as_ref() {
            Some(signet_setup) => cached_signet_chain(bin_paths, signet_setup).await?,
            None => None,
        };
        if let Some(cached_chain) = &cached_signet_chain {
            tracing::debug!(
                "Restoring cached signet chain from `{}`",
                cached_chain.display()
            );
            copy_datadir(cached_chain, &dirs.bitcoin_dir)?;
        }
        let restored_signet_chain = cached_signet_chain.is_some();

        tracing::debug!("Starting bitcoin node");
        let mut bitcoind = new_bitcoind(
            bin_paths,
            opts.bitcoind_kind,
            dirs.bitcoin_dir.clone(),
            &reserved_ports,
            network,
            signet_setup.as_ref(),
        )?;
        bitcoind.txindex = enable_wallet;
        let (mut bitcoind_exit, on_exit) = ExitWatch::new("bitcoind", res_tx.clone());
        let bitcoind_task =
            bitcoind.spawn_command_with_args::<String, _, _, _, _>([], opts.bitcoind_args, on_exit);
        // wait for startup
        let bitcoind_client = bitcoind.rpc_client()?;
        bitcoind_exit
            .unless_exited(wait_for_bitcoind_ready(&bitcoind_client))
            .await?;

        // Create a wallet and initialize it. A restored chain already ships
        // one, holding the mature coinbases it was mined to.
        const WALLET_NAME: &str = "integration-test";
        if restored_signet_chain {
            tracing::debug!("Loading wallet from the restored chain");
            let loaded_wallets: Vec<String> = bitcoind_client
                .request("listwallets", rpc_params![])
                .await?;
            if !loaded_wallets.iter().any(|wallet| wallet == WALLET_NAME) {
                let _load_wallet_output: serde_json::Value = bitcoind_client
                    .request("loadwallet", rpc_params![WALLET_NAME])
                    .await?;
            }
        } else {
            tracing::debug!("Creating wallet");
            let _create_wallet_output: serde_json::Value = bitcoind_client
                .request("createwallet", rpc_params![WALLET_NAME])
                .await?;
        }
        let mining_address = match signet_setup.as_ref() {
            Some(signet_setup) => {
                if !restored_signet_chain {
                    let () = signet_setup.init_bitcoind_wallet(&bitcoind_client).await?;
                }
                signet_setup.signet_challenge_addr.clone()
            }
            None => {
                tracing::debug!("Generating mining address");
                let mining_addr_str: String = bitcoind_client
                    .request("getnewaddress", rpc_params![])
                    .await?;
                mining_addr_str
                    .parse::<bitcoin::Address<_>>()?
                    .require_network(network.into())?
            }
        };
        tracing::debug!("Mining address: {mining_address}");
        tracing::debug!("Generating receiving address");
        let receive_address = {
            let receive_address_str: String = bitcoind_client
                .request("getnewaddress", rpc_params![])
                .await?;
            tracing::debug!("Receiving address: {receive_address_str}");
            receive_address_str
                .parse::<Address<_>>()?
                .require_network(bitcoind.network)?
        };
        let signet_miner = signet_setup.as_ref().map(SignetSetup::miner);
        // Mine 1 block, so the chain is non-empty. A restored chain already
        // has blocks -- and mining a signet one costs real proof-of-work, so
        // don't.
        tracing::debug!(%mining_address, "Mining 1 block");
        if restored_signet_chain {
            let blocks: u32 = bitcoind_client
                .request("getblockcount", rpc_params![])
                .await?;
            anyhow::ensure!(
                blocks >= SIGNET_CACHED_CHAIN_BLOCKS,
                "restored signet chain is only {blocks} blocks, expected at least \
                 {SIGNET_CACHED_CHAIN_BLOCKS}"
            );
            tracing::debug!("Restored signet chain at height {blocks}");
        } else if let Some(signet_miner) = signet_miner.as_ref() {
            let block_hash = signet_miner
                .mine_block(
                    &bitcoind_client,
                    TemplateSource::Node {
                        payout: mining_address.script_pubkey(),
                    },
                )
                .await?;
            tracing::debug!(%block_hash, "Mined 1 block");
        } else {
            let _block_hashes: Vec<BlockHash> = bitcoind_client
                .request(
                    "generatetoaddress",
                    rpc_params![1, mining_address.to_string()],
                )
                .await?;
        }
        // Start electrs
        tracing::debug!("Starting electrs");
        let electrs = Electrs {
            path: bin_paths.electrs()?.clone(),
            db_dir: dirs.electrs_dir.clone(),
            auth: ("drivechain".to_owned(), "integrationtesting".to_owned()),
            daemon_dir: bitcoind.data_dir.join("path"),
            daemon_rpc_port: bitcoind.rpc_port,
            electrum_rpc_port: reserved_ports.electrs_electrum_rpc.port(),
            electrum_http_port: reserved_ports.electrs_electrum_http.port(),
            monitoring_port: reserved_ports.electrs_monitoring.port(),
            network: bitcoind.network,
            signet_magic: signet_setup.as_ref().map(|setup| setup.signet_magic),
        };
        let (mut electrs_exit, on_exit) = ExitWatch::new("electrs", res_tx.clone());
        let electrs_task =
            electrs.spawn_command_with_args::<String, String, _, _, _>([], [], on_exit);
        // Wait for electrs to start serving. The enforcer's wallet talks to
        // both the Electrum RPC and HTTP ports, so wait for each to accept
        // connections rather than guessing at a fixed startup delay.
        electrs_exit
            .unless_exited(async {
                for port in [electrs.electrum_rpc_port, electrs.electrum_http_port] {
                    wait_for_port("127.0.0.1", port, Duration::from_secs(60))
                        .await
                        .map_err(|err| anyhow!("Failed waiting for electrs port {port}: {err}"))?;
                }
                Ok(())
            })
            .await?;
        // Start BIP300301 Enforcer
        tracing::debug!("Starting bip300301_enforcer");
        let enforcer = Enforcer {
            path: bin_paths.bip300301_enforcer()?.clone(),
            data_dir: dirs.enforcer_dir.clone(),
            enable_mempool: mode.enable_mempool(),
            enable_wallet,
            enable_block_template_server: matches!(mode, Mode::GetBlockTemplate),
            coinbase_recipient: (!enable_wallet).then(|| mining_address.to_string()),
            node_blocks_dir: None,
            node_mempool_dat: None,
            node_rpc_user: bitcoind.rpc_user.clone(),
            node_rpc_pass: bitcoind.rpc_pass.clone(),
            node_rpc_port: bitcoind.rpc_port,
            node_zmq_sequence_port: bitcoind.zmq_sequence_port,
            serve_grpc_port: reserved_ports.enforcer_serve_grpc.port(),
            serve_rpc_port: reserved_ports.enforcer_serve_rpc.port(),
            wallet_electrum_rpc_port: electrs.electrum_rpc_port,
            wallet_electrum_http_port: electrs.electrum_http_port,
        };
        let (mut enforcer_exit, on_enforcer_exit) = ExitWatch::new("enforcer", res_tx);
        let enforcer_task = enforcer.spawn_command_with_args(
            [(
                "RUST_LOG",
                "h2=info,hyper_util=info,jsonrpsee-client=debug,jsonrpsee-http=debug,connectrpc=debug,trace",
            )],
            opts.enforcer_args,
            on_enforcer_exit,
        );
        let tasks = Tasks {
            _enforcer: Some(enforcer_task),
            _electrs: Some(electrs_task),
            _bitcoind: bitcoind_task,
        };
        // Wait for enforcer gRPC port to open
        enforcer_exit
            .unless_exited(async {
                wait_for_port(
                    "127.0.0.1",
                    enforcer.serve_grpc_port,
                    Duration::from_secs(60),
                )
                .await
                .map_err(|e| anyhow!("Failed waiting for enforcer gRPC port: {e}"))
            })
            .await?;

        let gbt_client = jsonrpsee::http_client::HttpClient::builder()
            .build(format!("http://127.0.0.1:{}", enforcer.serve_rpc_port))
            .map_err(|err| anyhow!("failed to create gbt client: {err:#}"))?;

        // The JSON-RPC (`getblocktemplate`) server only runs in the mode that
        // serves block templates, and it binds before the enforcer has synced.
        // Both the `gbt_client` above and the signet miner talk to it, so wait
        // for it to serve a template rather than racing the first request
        // against startup.
        if enforcer.enable_block_template_server {
            enforcer_exit
                .unless_exited(async {
                    wait_for_port(
                        "127.0.0.1",
                        enforcer.serve_rpc_port,
                        Duration::from_secs(60),
                    )
                    .await
                    .map_err(|e| anyhow!("Failed waiting for enforcer JSON-RPC port: {e}"))?;
                    wait_for_block_templates(&gbt_client).await
                })
                .await?;
        }
        // Use HTTP/2 to multiplex event subscriptions and other RPCs. With
        // pooled HTTP/1.1, opening another call while a stream is live can
        // hang before its response headers arrive. Separate pools per service
        // do not help when `mine_check_block_events` and `mine_gbt_check` open
        // nested subscriptions on the same validator client.
        let http = HttpClient::plaintext_http2_only();
        let config = client_config(enforcer.serve_grpc_port)?;
        let validator_service_client = ValidatorServiceClient::new(http.clone(), config.clone());
        let block_producer_service_client =
            BlockProducerServiceClient::new(http.clone(), config.clone());
        let mining_service_client = MiningServiceClient::new(http.clone(), config.clone());
        let wallet_service_client = WalletServiceClient::new(http, config);
        // The gRPC port opens before the validator has synced the blocks that
        // this setup generated. Wait for it, so that tests don't race the
        // initial sync.
        let _chain_tip = enforcer_exit
            .unless_exited(wait_for_validator_synced(&validator_service_client))
            .await?;
        Ok(PostSetup {
            network,
            mode,
            rpc_user: bitcoind.rpc_user.clone(),
            rpc_pass: bitcoind.rpc_pass.clone(),
            rpc_port: bitcoind.rpc_port,
            bitcoind_client,
            tasks,
            signet_miner,
            gbt_client,
            validator_service_client,
            wallet_service_client,
            block_producer_service_client,
            mining_service_client,
            mining_address,
            receive_address,
            directories: dirs.clone(),
            reserved_ports,
        })
    }

    /// Kill the running enforcer process (simulating a crash) without
    /// respawning it, and wait until its gRPC port is confirmed free. Used
    /// together with [`Self::restart_enforcer`] so a reorg can be driven
    /// entirely on bitcoind while the enforcer is down, forcing it to catch
    /// up over more than one block on the next restart rather than observing
    /// the reorg live via its ZMQ-fed background sync task.
    pub async fn kill_enforcer(&mut self) -> anyhow::Result<()> {
        if let Some(old) = self.tasks._enforcer.take() {
            drop(old);
        }
        wait_for_port_free(
            "127.0.0.1",
            self.reserved_ports.enforcer_serve_grpc.port(),
            Duration::from_secs(10),
        )
        .await
    }

    /// Respawn the enforcer from the same data-dir and ports (killing it
    /// first, if not already killed via [`Self::kill_enforcer`]).
    /// bitcoind/electrs are left running throughout. Existing gRPC clients
    /// reconnect automatically once the new process is listening.
    ///
    /// Like [`PreSetup::setup`], waits for block templates in
    /// [`Mode::GetBlockTemplate`], so that tests can mine right away.
    pub async fn restart_enforcer<EnforcerArg, EnforcerArgs>(
        &mut self,
        bin_paths: &BinPaths,
        enforcer_args: EnforcerArgs,
        res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> anyhow::Result<()>
    where
        EnforcerArg: AsRef<OsStr>,
        EnforcerArgs: IntoIterator<Item = EnforcerArg>,
    {
        let mut enforcer_exit = self
            .spawn_enforcer(bin_paths, enforcer_args, res_tx)
            .await?;
        if matches!(self.mode, Mode::GetBlockTemplate) {
            enforcer_exit
                .unless_exited(wait_for_block_templates(&self.gbt_client))
                .await?;
        }
        Ok(())
    }

    /// [`Self::restart_enforcer`], returning as soon as gRPC is up, while the
    /// enforcer is still syncing. For tests that probe it during that window.
    pub async fn respawn_enforcer<EnforcerArg, EnforcerArgs>(
        &mut self,
        bin_paths: &BinPaths,
        enforcer_args: EnforcerArgs,
        res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> anyhow::Result<()>
    where
        EnforcerArg: AsRef<OsStr>,
        EnforcerArgs: IntoIterator<Item = EnforcerArg>,
    {
        let _enforcer_exit = self
            .spawn_enforcer(bin_paths, enforcer_args, res_tx)
            .await?;
        Ok(())
    }

    async fn spawn_enforcer<EnforcerArg, EnforcerArgs>(
        &mut self,
        bin_paths: &BinPaths,
        enforcer_args: EnforcerArgs,
        res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> anyhow::Result<ExitWatch>
    where
        EnforcerArg: AsRef<OsStr>,
        EnforcerArgs: IntoIterator<Item = EnforcerArg>,
    {
        self.kill_enforcer().await?;

        let enforcer = Enforcer {
            path: bin_paths.bip300301_enforcer()?.clone(),
            data_dir: self.directories.enforcer_dir.clone(),
            enable_mempool: self.mode.enable_mempool(),
            enable_wallet: true,
            enable_block_template_server: matches!(self.mode, Mode::GetBlockTemplate),
            coinbase_recipient: None,
            node_blocks_dir: None,
            node_mempool_dat: None,
            node_rpc_user: self.rpc_user.clone(),
            node_rpc_pass: self.rpc_pass.clone(),
            node_rpc_port: self.rpc_port,
            node_zmq_sequence_port: self.reserved_ports.bitcoind_zmq_sequence.port(),
            serve_grpc_port: self.reserved_ports.enforcer_serve_grpc.port(),
            serve_rpc_port: self.reserved_ports.enforcer_serve_rpc.port(),
            wallet_electrum_rpc_port: self.reserved_ports.electrs_electrum_rpc.port(),
            wallet_electrum_http_port: self.reserved_ports.electrs_electrum_http.port(),
        };
        let (mut enforcer_exit, on_exit) = ExitWatch::new("enforcer", res_tx);
        let enforcer_task = enforcer.spawn_command_with_args(
            [(
                "RUST_LOG",
                "h2=info,hyper_util=info,jsonrpsee-client=debug,jsonrpsee-http=debug,connectrpc=debug,trace",
            )],
            enforcer_args,
            on_exit,
        );
        self.tasks._enforcer = Some(enforcer_task);

        enforcer_exit
            .unless_exited(async {
                wait_for_port(
                    "127.0.0.1",
                    enforcer.serve_grpc_port,
                    Duration::from_secs(10),
                )
                .await
                .map_err(|e| anyhow!("Failed waiting for restarted enforcer gRPC port: {e}"))
            })
            .await?;

        Ok(enforcer_exit)
    }

    /// Kill electrs without respawning it, and wait until its ports are
    /// confirmed free.
    pub async fn kill_electrs(&mut self) -> anyhow::Result<()> {
        if let Some(old) = self.tasks._electrs.take() {
            drop(old);
        }
        wait_for_port_free(
            "127.0.0.1",
            self.reserved_ports.electrs_electrum_http.port(),
            Duration::from_secs(10),
        )
        .await
    }

    /// Kill electrs (if not already killed via [`Self::kill_electrs`]) and
    /// respawn it from a freshly wiped db-dir. electrs (the pinned v3.2.0
    /// binary) is known to panic mid-index on some reorgs -- an unrelated,
    /// pre-existing limitation, not the enforcer -- and can't resume
    /// cleanly from a state it panicked while indexing.
    pub async fn restart_electrs(
        &mut self,
        bin_paths: &BinPaths,
        res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> anyhow::Result<()> {
        self.kill_electrs().await?;

        std::fs::remove_dir_all(&self.directories.electrs_dir).ok();
        std::fs::create_dir_all(&self.directories.electrs_dir)?;

        let electrs = Electrs {
            path: bin_paths.electrs()?.clone(),
            db_dir: self.directories.electrs_dir.clone(),
            auth: (self.rpc_user.clone(), self.rpc_pass.clone()),
            daemon_dir: self.directories.bitcoin_dir.join("path"),
            daemon_rpc_port: self.rpc_port,
            electrum_rpc_port: self.reserved_ports.electrs_electrum_rpc.port(),
            electrum_http_port: self.reserved_ports.electrs_electrum_http.port(),
            monitoring_port: self.reserved_ports.electrs_monitoring.port(),
            network: self.network.into(),
            // Only relevant for signet, which this helper isn't used by yet.
            signet_magic: None,
        };
        let (mut electrs_exit, on_exit) = ExitWatch::new("electrs", res_tx);
        let electrs_task =
            electrs.spawn_command_with_args::<String, String, _, _, _>([], [], on_exit);
        self.tasks._electrs = Some(electrs_task);

        electrs_exit
            .unless_exited(async {
                wait_for_port(
                    "127.0.0.1",
                    electrs.electrum_http_port,
                    Duration::from_secs(60),
                )
                .await
                .map_err(|e| anyhow!("Failed waiting for restarted electrs http port: {e}"))
            })
            .await?;

        Ok(())
    }
}

pub struct PreSetup<B = BinPaths> {
    pub bin_paths: B,
    pub network: Network,
    pub reserved_ports: ReservedPorts,
    pub directories: Directories,
}

impl<B> PreSetup<B> {
    pub fn new(bin_paths: B, network: Network) -> anyhow::Result<Self> {
        Ok(PreSetup {
            bin_paths,
            network,
            reserved_ports: ReservedPorts::new()?,
            directories: Directories::new()?,
        })
    }

    pub async fn setup<BitcoindArg, EnforcerArg, BitcoindArgs, EnforcerArgs>(
        self,
        mode: Mode,
        opts: SetupOpts<BitcoindArg, EnforcerArg, BitcoindArgs, EnforcerArgs>,
        res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> anyhow::Result<PostSetup>
    where
        B: Borrow<BinPaths>,
        BitcoindArg: AsRef<OsStr>,
        EnforcerArg: AsRef<OsStr>,
        BitcoindArgs: IntoIterator<Item = BitcoindArg>,
        EnforcerArgs: IntoIterator<Item = EnforcerArg>,
    {
        PostSetup::setup(
            self.bin_paths.borrow(),
            mode,
            self.network,
            self.reserved_ports,
            self.directories,
            opts,
            res_tx,
        )
        .await
    }
}

pub trait Sidechain: Sized {
    const SIDECHAIN_NUMBER: SidechainNumber;

    type Init;

    type SetupError: std::error::Error + Send + Sync + 'static;

    fn setup(
        init: Self::Init,
        post_setup: &PostSetup,
        res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> impl Future<Output = Result<Self, Self::SetupError>> + Send;

    type GetDepositAddressError: std::error::Error + Send + Sync + 'static;

    /// Get a sidechain address to deposit to
    fn get_deposit_address(
        &self,
    ) -> impl Future<Output = Result<String, Self::GetDepositAddressError>> + Send;

    type ConfirmDepositError: std::error::Error + Send + Sync + 'static;

    fn confirm_deposit(
        &mut self,
        post_setup: &mut PostSetup,
        address: &str,
        value: bitcoin::Amount,
        txid: bitcoin::Txid,
    ) -> impl Future<Output = Result<(), Self::ConfirmDepositError>> + Send;

    /// Create a withdrawal and broadcast the bundle
    type CreateWithdrawalError: std::error::Error + Send + Sync + 'static;

    fn create_withdrawal(
        &mut self,
        post_setup: &mut PostSetup,
        receive_address: &bitcoin::Address,
        value: bitcoin::Amount,
        fee: bitcoin::Amount,
    ) -> impl Future<Output = Result<M6id, Self::CreateWithdrawalError>> + Send;
}

#[derive(Debug, Error)]
pub enum DummySidechainError {
    #[error(transparent)]
    BlindedM6(#[from] BlindedM6Error),
    #[error(transparent)]
    Grpc(Box<ConnectError>),
    #[error("Event stream was cancelled due to earlier error")]
    EventStreamCancelled,
    #[error("Event stream was closed unexpectedly")]
    EventStreamClosed,
    #[error("Timed out waiting for the event stream to catch up to chain tip {tip}")]
    EventStreamLagged { tip: BlockHash },
}

impl From<ConnectError> for DummySidechainError {
    fn from(err: ConnectError) -> Self {
        Self::Grpc(Box::new(err))
    }
}

impl From<proto::Error> for DummySidechainError {
    fn from(err: proto::Error) -> Self {
        Self::Grpc(Box::new(err.into()))
    }
}

/// Dummy implementation of `Sidechain`
pub struct DummySidechain {
    /// If a withdrawal fails, add the value here until another withdrawal
    /// is created
    pending_withdrawal_value: bitcoin::Amount,
    /// If a withdrawal fails, add the fee here until another withdrawal
    /// is created
    pending_withdrawal_fee: bitcoin::Amount,
    withdrawal_bundles: HashMap<M6id, BlindedM6<'static>>,
    /// Hash of the block whose `ConnectBlock` event was most recently
    /// processed from the event stream. `None` until the first connect event
    /// is processed, and after a disconnect event (which does not carry the
    /// new tip hash).
    last_connected_block: Option<BlockHash>,
    /// Receiver for SubscribeEvents stream items. The producer is a
    /// background task spawned in `setup` that pumps a connectrpc
    /// `ServerStream` into this channel. `None` after the stream errors or
    /// closes.
    event_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<
            Result<proto::mainchain::SubscribeEventsResponse, ConnectError>,
        >,
    >,
}

impl DummySidechain {
    /// Timeout for the event stream to catch up to the enforcer's chain tip
    /// in `sync_events_to_tip`.
    const EVENT_SYNC_TIMEOUT: Duration = Duration::from_secs(30);

    /// Construct a blinded M6 tx
    fn blinded_m6<Payouts>(
        fee_sats: u64,
        payouts: Payouts,
    ) -> Result<BlindedM6<'static>, BlindedM6Error>
    where
        Payouts: IntoIterator<Item = bitcoin::TxOut>,
    {
        let fee_txout = {
            let script_pubkey = bitcoin::script::Builder::new()
                .push_opcode(bitcoin::opcodes::all::OP_RETURN)
                .push_slice(fee_sats.to_be_bytes())
                .into_script();
            bitcoin::TxOut {
                value: bitcoin::Amount::ZERO,
                script_pubkey,
            }
        };
        let outputs = Vec::from_iter(std::iter::once(fee_txout).chain(payouts));
        let tx = bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::blockdata::locktime::absolute::LockTime::ZERO,
            input: Vec::new(),
            output: outputs,
        };
        let res = BlindedM6::try_from(std::borrow::Cow::Owned(tx))?;
        Ok(res)
    }

    /// Extract withdrawal bundle events from block info events
    fn extract_withdrawal_bundle_event(
        block_event: proto::mainchain::block_info::Event,
    ) -> Result<Option<proto::mainchain::WithdrawalBundleEvent>, proto::Error> {
        use proto::mainchain::block_info::event::Event;
        let event = block_event.event.ok_or_else(|| {
            proto::Error::missing_field::<proto::mainchain::block_info::Event>("event")
        })?;
        match event {
            Event::Deposit(_) => Ok(None),
            Event::WithdrawalBundle(wbe) => Ok(Some(*wbe)),
        }
    }

    /// Process a single event from the event stream, recording the connected
    /// block and restoring the value and fee of failed withdrawal bundles.
    fn process_event(
        &mut self,
        resp: proto::mainchain::SubscribeEventsResponse,
    ) -> Result<(), DummySidechainError> {
        use bip300301_enforcer_lib::proto::mainchain::{
            SubscribeEventsResponse, WithdrawalBundleEvent,
            subscribe_events_response::{
                self,
                event::{ConnectBlock, Event},
            },
            withdrawal_bundle_event,
        };
        let SubscribeEventsResponse { event, .. } = resp;
        let subscribe_events_response::Event { event, .. } = event
            .into_option()
            .ok_or_else(|| proto::Error::missing_field::<SubscribeEventsResponse>("event"))?;
        let event: subscribe_events_response::event::Event = event.ok_or_else(|| {
            proto::Error::missing_field::<subscribe_events_response::Event>("event")
        })?;
        match event {
            Event::ConnectBlock(connect_block_event) => {
                let block_hash = connect_block_event
                    .header_info
                    .into_option()
                    .ok_or_else(|| proto::Error::missing_field::<ConnectBlock>("header_info"))?
                    .block_hash
                    .into_option()
                    .ok_or_else(|| proto::Error::missing_field::<BlockHeaderInfo>("block_hash"))?
                    .decode::<BlockHeaderInfo, BlockHash>("block_hash")?;
                let block_info = connect_block_event
                    .block_info
                    .into_option()
                    .ok_or_else(|| proto::Error::missing_field::<ConnectBlock>("block_info"))?;
                for event in block_info.events {
                    let Some(wbe) = Self::extract_withdrawal_bundle_event(event)? else {
                        continue;
                    };
                    let m6id = wbe
                        .m6id
                        .into_option()
                        .ok_or_else(|| {
                            proto::Error::missing_field::<WithdrawalBundleEvent>("m6id")
                        })?
                        .decode::<WithdrawalBundleEvent, Txid>("m6id")
                        .map(M6id)?;
                    let wbe_inner = wbe
                        .event
                        .into_option()
                        .ok_or_else(|| {
                            proto::Error::missing_field::<WithdrawalBundleEvent>("event")
                        })?
                        .event
                        .ok_or_else(|| {
                            proto::Error::missing_field::<withdrawal_bundle_event::Event>("event")
                        })?;
                    match wbe_inner {
                        withdrawal_bundle_event::event::Event::Failed(_) => {
                            let failed_withdrawal = &self.withdrawal_bundles[&m6id];
                            self.pending_withdrawal_fee += *failed_withdrawal.fee();
                            self.pending_withdrawal_value += *failed_withdrawal.payout();
                        }
                        withdrawal_bundle_event::event::Event::Submitted(_)
                        | withdrawal_bundle_event::event::Event::Succeeded(_) => (),
                    }
                }
                self.last_connected_block = Some(block_hash);
            }
            Event::DisconnectBlock(_) => {
                // The disconnect event carries only the disconnected block's
                // hash, not the new tip; the next connect event will set it.
                self.last_connected_block = None;
            }
        }
        Ok(())
    }

    /// Await and process events until the `ConnectBlock` event for `tip` has
    /// been processed.
    async fn process_events_until(&mut self, tip: BlockHash) -> Result<(), DummySidechainError> {
        while self.last_connected_block != Some(tip) {
            let Some(rx) = self.event_rx.as_mut() else {
                return Err(DummySidechainError::EventStreamCancelled);
            };
            let Some(item) = rx.recv().await else {
                self.event_rx = None;
                return Err(DummySidechainError::EventStreamClosed);
            };
            let resp = match item {
                Ok(resp) => resp,
                Err(err) => {
                    self.event_rx = None;
                    return Err(err.into());
                }
            };
            let () = self.process_event(resp)?;
        }
        Ok(())
    }

    /// Await and process events until this sidechain has seen the
    /// `ConnectBlock` event for the enforcer's current chain tip.
    ///
    /// The test harness observes events on its own `SubscribeEvents`
    /// subscription (e.g. in `mine_check_block_events`) and continues as soon
    /// as an event arrives there. This sidechain receives the same events on
    /// a separate subscription, which may still be in flight at that point,
    /// so draining only already-delivered events can miss events the harness
    /// has already acted on — e.g. a withdrawal bundle failure whose value
    /// must roll over into the next bundle. Syncing to the chain tip removes
    /// the race between the two subscriptions.
    async fn sync_events_to_tip(
        &mut self,
        post_setup: &PostSetup,
    ) -> Result<(), DummySidechainError> {
        let tip = post_setup
            .validator_service_client
            .get_chain_tip(GetChainTipRequest::default())
            .await?
            .into_owned()
            .block_header_info
            .into_option()
            .ok_or_else(|| proto::Error::missing_field::<GetChainTipResponse>("block_header_info"))?
            .block_hash
            .into_option()
            .ok_or_else(|| proto::Error::missing_field::<BlockHeaderInfo>("block_hash"))?
            .decode::<BlockHeaderInfo, BlockHash>("block_hash")?;
        match timeout(Self::EVENT_SYNC_TIMEOUT, self.process_events_until(tip)).await {
            Ok(res) => res,
            Err(_) => Err(DummySidechainError::EventStreamLagged { tip }),
        }
    }
}

impl Sidechain for DummySidechain {
    const SIDECHAIN_NUMBER: SidechainNumber = SidechainNumber(0);

    type Init = ();

    type SetupError = ConnectError;

    async fn setup(
        _: Self::Init,
        post_setup: &PostSetup,
        _: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> Result<Self, Self::SetupError> {
        use bip300301_enforcer_lib::proto::mainchain::SubscribeEventsRequest;
        let subscribe_events_request = SubscribeEventsRequest {
            sidechain_id: proto::wrap_u32(Self::SIDECHAIN_NUMBER.0.into()),
        };
        let mut stream = post_setup
            .validator_service_client
            .subscribe_events(subscribe_events_request)
            .await?;
        // Pump the connect-rust ServerStream into a tokio mpsc, so that
        // `sync_events_to_tip` can both drain ready events and await delivery
        // of pending ones.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match stream.message().await {
                    Ok(Some(view)) => {
                        if tx.send(Ok(view.to_owned_message())).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        drop(tx.send(Err(err)));
                        break;
                    }
                }
            }
        });
        Ok(Self {
            pending_withdrawal_fee: bitcoin::Amount::ZERO,
            pending_withdrawal_value: bitcoin::Amount::ZERO,
            withdrawal_bundles: HashMap::new(),
            last_connected_block: None,
            event_rx: Some(rx),
        })
    }

    type GetDepositAddressError = std::convert::Infallible;

    fn get_deposit_address(
        &self,
    ) -> impl Future<Output = Result<String, Self::GetDepositAddressError>> + Send {
        future::ok("sidechain address".to_owned())
    }

    type ConfirmDepositError = std::convert::Infallible;

    async fn confirm_deposit(
        &mut self,
        _: &mut PostSetup,
        _: &str,
        _: bitcoin::Amount,
        _: bitcoin::Txid,
    ) -> Result<(), Self::ConfirmDepositError> {
        Ok(())
    }

    type CreateWithdrawalError = DummySidechainError;

    async fn create_withdrawal(
        &mut self,
        post_setup: &mut PostSetup,
        receive_address: &bitcoin::Address,
        mut value: bitcoin::Amount,
        mut fee: bitcoin::Amount,
    ) -> Result<M6id, Self::CreateWithdrawalError> {
        let () = self.sync_events_to_tip(post_setup).await?;
        value += self.pending_withdrawal_value;
        self.pending_withdrawal_value = bitcoin::Amount::ZERO;
        fee += self.pending_withdrawal_fee;
        self.pending_withdrawal_fee = bitcoin::Amount::ZERO;
        let blinded_m6 = Self::blinded_m6(
            fee.to_sat(),
            [bitcoin::TxOut {
                script_pubkey: receive_address.script_pubkey(),
                value,
            }],
        )?;
        let m6id = blinded_m6.compute_m6id();
        tracing::debug!(
            %m6id,
            value = %value.display_dynamic(),
            fee = %value.display_dynamic(),
            "Creating Withdrawal"
        );
        let withdrawal_bundle_tx = blinded_m6.clone().tx().into_owned();
        self.withdrawal_bundles.insert(m6id, blinded_m6);
        let _resp: BroadcastWithdrawalBundleResponse = post_setup
            .wallet_service_client
            .broadcast_withdrawal_bundle(BroadcastWithdrawalBundleRequest {
                sidechain_id: proto::wrap_u32(Self::SIDECHAIN_NUMBER.0.into()),
                transaction: buffa::MessageField::some(buffa_types::google::protobuf::BytesValue {
                    value: bitcoin::consensus::serialize(&withdrawal_bundle_tx),
                    ..Default::default()
                }),
            })
            .await?
            .into_owned();
        Ok(m6id)
    }
}
