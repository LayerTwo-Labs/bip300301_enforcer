//! Setup for an integration test

use std::{
    borrow::Borrow,
    collections::HashMap,
    ffi::OsStr,
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, LazyLock},
};

use anyhow::anyhow;
use bip300301_enforcer_lib::{
    bins::{self, CommandExt as _},
    proto::{
        self,
        mainchain::{BroadcastWithdrawalBundleRequest, BroadcastWithdrawalBundleResponse},
        mainchain_service::{ValidatorServiceClient, WalletServiceClient},
    },
    types::{BlindedM6, BlindedM6Error, M6id, SidechainNumber},
};
use bitcoin::{Address, Txid};
use connectrpc::{
    ConnectError,
    client::{ClientConfig, HttpClient},
};
use futures::{channel::mpsc, future};
use reserve_port::ReservedPort;
use temp_dir::TempDir;
use thiserror::Error;
use tokio::{
    net::TcpStream,
    time::{Duration, sleep, timeout},
};

use crate::util::{AbortOnDrop, BinPaths, Bitcoind, Electrs, Enforcer, VarError};

#[derive(strum::Display, Clone, Copy, Debug)]
pub enum Network {
    Regtest,
    Signet,
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
        let secret_key = bitcoin::PrivateKey::generate(bitcoin::NetworkKind::Test);
        let cpk = bitcoin::CompressedPublicKey::from_private_key(
            &bitcoin::secp256k1::Secp256k1::new(),
            &secret_key,
        )?;
        let signet_challenge = bitcoin::Script::builder()
            .push_opcode(bitcoin::opcodes::all::OP_PUSHNUM_1)
            .push_slice(cpk.to_bytes())
            .push_opcode(bitcoin::opcodes::all::OP_PUSHNUM_1)
            .push_opcode(bitcoin::opcodes::all::OP_CHECKMULTISIG)
            .into_script();
        let signet_challenge_addr =
            bitcoin::Address::from_script(&cpk.p2wpkh_script_code(), &bitcoin::params::SIGNET)?;
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

    /// Initialize bitcoind wallet
    async fn init_bitcoind_wallet(&self, bitcoin_cli: &bins::BitcoinCli) -> anyhow::Result<()> {
        tracing::debug!("Importing secret key");
        let mining_descriptor = {
            use bdk_wallet::miniscript;
            let descriptor = bdk_wallet::descriptor!(wpkh(self.secret_key))?;
            descriptor.0.to_string_with_secret(&descriptor.1)
        };
        let multisig_descriptor = {
            let descriptor = bdk_wallet::descriptor!(bare(multi(1, self.secret_key)))?;
            descriptor.0.to_string_with_secret(&descriptor.1)
        };
        let import_descriptors_output = bitcoin_cli
            .command::<String, _, String, _, _>(
                [],
                "importdescriptors",
                [serde_json::json!([
                    {
                        "desc": mining_descriptor,
                        "timestamp": "now",
                        "active": false,
                    },
                    {
                        "desc": multisig_descriptor,
                        "timestamp": "now",
                        "active": false,
                    },
                ])
                .to_string()],
            )
            .run_utf8()
            .await?;
        let expected_import_descriptors_output = serde_json::json!([
            { "success": true }, { "success": true }
        ]);
        if serde_json::from_str::<serde_json::Value>(&import_descriptors_output)?
            != expected_import_descriptors_output
        {
            anyhow::bail!("Importing descriptors failed: `{import_descriptors_output}`")
        }
        tracing::debug!(
            signet_challenge_addr = %self.signet_challenge_addr,
            "Checking that the signet challenge addr is loaded"
        );
        let getaddressinfo_output = bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "getaddressinfo",
                [self.signet_challenge_addr.to_string()],
            )
            .run_utf8()
            .await?;
        tracing::debug!(%getaddressinfo_output);
        Ok(())
    }

    async fn calibrate_signet(&self, signet_miner: &mut bins::SignetMiner) -> anyhow::Result<()> {
        let calibrate_output = signet_miner
            .command("calibrate", vec!["--seconds=1"])
            .run_utf8()
            .await?;
        let nbits_hex = {
            calibrate_output
                .strip_prefix("nbits=")
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Missing nbits prefix from calibration output: `{calibrate_output}`",
                    )
                })?
                .split_once(" ")
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Missing nbits suffix from calibration output: `{calibrate_output}`",
                    )
                })?
                .0
                .to_owned()
        };
        signet_miner.nbits = Some(hex::FromHex::from_hex(&nbits_hex)?);
        Ok(())
    }

    /// Configure signet miner to use enforcer's GBT server
    fn configure_miner(
        signet_miner: &mut bins::SignetMiner,
        out_dir: &TempDir,
        enforcer: &Enforcer,
    ) -> anyhow::Result<()> {
        let gbt_script_file = out_dir.path().join("gbt-script.sh");
        tracing::info!("GBT script: {}", gbt_script_file.display());
        let gbt_script = format!(
            r#"#!/bin/sh
            REQUEST='{{"jsonrpc":"2.0","id":0,"method":"getblocktemplate","params":['$1']}}'
            RESPONSE=$(curl 127.0.0.1:{} --no-progress-meter -H "Content-Type: application/json" --data-binary "${{REQUEST}}")
            RESULT=$(echo "${{RESPONSE}}" | jq '.result')
            echo "${{RESULT}}""#,
            enforcer.serve_rpc_port
        );
        std::fs::write(&gbt_script_file, gbt_script)?;
        cfg_if::cfg_if! {
            if #[cfg(target_family = "unix")] {
                use std::os::unix::fs::PermissionsExt as _;
                let mut perms = std::fs::metadata(&gbt_script_file)?.permissions();
                // Add execute permission (equivalent to chmod +x)
                perms.set_mode(perms.mode() | 0o111);
                std::fs::set_permissions(&gbt_script_file, perms)?;
            }
        }
        signet_miner.coinbasetxn = true;
        signet_miner.getblocktemplate_command = Some(format!("{}", gbt_script_file.display()));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MiningMode {
    GenerateBlocks,
    GetBlockTemplate,
}

#[derive(strum::Display, Clone, Copy, Debug)]
pub enum Mode {
    GetBlockTemplate,
    Mempool,
    NoMempool,
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

impl ReservedPorts {
    pub fn new() -> Result<Self, reserve_port::Error> {
        Ok(Self {
            bitcoind_listen: ReservedPort::random()?,
            bitcoind_rpc: ReservedPort::random()?,
            bitcoind_zmq_sequence: ReservedPort::random()?,
            electrs_electrum_rpc: ReservedPort::random()?,
            electrs_electrum_http: ReservedPort::random()?,
            electrs_monitoring: ReservedPort::random()?,
            enforcer_serve_grpc: ReservedPort::random()?,
            enforcer_serve_rpc: ReservedPort::random()?,
        })
    }
}

pub fn new_bitcoind(
    bitcoind_path: PathBuf,
    data_dir: PathBuf,
    reserved_ports: &ReservedPorts,
    network: Network,
    signet_setup: Option<&SignetSetup>,
) -> Bitcoind {
    Bitcoind {
        path: bitcoind_path,
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
    }
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

/// Polls bitcoind via `getblockchaininfo` until it responds successfully.
/// The RPC port opens before bitcoind is ready to serve commands, so a TCP
/// probe alone is not enough.
pub async fn wait_for_bitcoind_ready(bitcoin_cli: &bins::BitcoinCli) -> anyhow::Result<()> {
    // When the whole suite runs at once, many bitcoind/electrs/enforcer
    // processes cold-start together. Apply a generous limit here that
    // doesn't crash long running tests, but catches stuck ones.
    const TIMEOUT: Duration = Duration::from_secs(120);
    const CHECK_INTERVAL: Duration = Duration::from_millis(200);
    let task = async {
        loop {
            match bitcoin_cli
                .clone()
                .command::<String, _, String, _, _>([], "getblockchaininfo", [])
                .run_utf8()
                .await
            {
                Ok(_) => return,
                Err(e) => {
                    tracing::trace!("bitcoind not ready yet ({e}), waiting...");
                    sleep(CHECK_INTERVAL).await;
                }
            }
        }
    };
    timeout(TIMEOUT, task)
        .await
        .map_err(|_| anyhow!("Timeout waiting for bitcoind to become ready after {TIMEOUT:?}"))
}

/// Running tasks, aborted on drop
pub struct Tasks {
    // MUST be dropped before electrs and bitcoind
    _enforcer: AbortOnDrop<()>,
    // MUST be dropped before bitcoind
    _electrs: AbortOnDrop<()>,
    _bitcoind: AbortOnDrop<()>,
}

type Transport = HttpClient;

/// Construct a connectrpc transport/config pair for a plaintext gRPC endpoint
/// served by our enforcer.
fn make_client(port: u16) -> anyhow::Result<(HttpClient, ClientConfig)> {
    let uri: http::Uri = format!("http://127.0.0.1:{port}")
        .parse()
        .map_err(|err| anyhow!("invalid client URI: {err}"))?;
    let http = HttpClient::plaintext();
    let config = ClientConfig::new(uri);
    Ok((http, config))
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
}

#[derive(Clone, Copy, Debug, Default)]
pub enum BitcoindKind {
    #[default]
    Patched,
    Unpatched,
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
}

type LazyLockBoxedSend<T> = LazyLock<T, Box<dyn FnOnce() -> T + Send>>;

pub struct PostSetup {
    pub network: Network,
    pub mode: Mode,
    pub bitcoin_cli: bins::BitcoinCli,
    bitcoin_util: LazyLockBoxedSend<Result<bins::BitcoinUtil, Arc<VarError>>>,
    // MUST occur before temp dirs and reserved ports in order to ensure that processes are dropped
    // before reserved ports are freed and temp dirs are cleared
    pub tasks: Tasks,
    /// Always `Some(_)` if `network == Network::Signet`, `None` otherwise
    pub signet_miner: Option<bins::SignetMiner>,
    pub gbt_client: jsonrpsee::http_client::HttpClient,
    pub validator_service_client: ValidatorServiceClient<Transport>,
    pub wallet_service_client: WalletServiceClient<Transport>,
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
    pub fn bitcoin_util(&self) -> Result<&bins::BitcoinUtil, Arc<VarError>> {
        self.bitcoin_util.as_ref().map_err(|err| err.clone())
    }

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

        tracing::debug!("Starting bitcoin node");
        let bitcoind = new_bitcoind(
            bitcoind_path(bin_paths, opts.bitcoind_kind)?.clone(),
            dirs.bitcoin_dir.clone(),
            &reserved_ports,
            network,
            signet_setup.as_ref(),
        );
        let bitcoind_task =
            bitcoind.spawn_command_with_args::<String, _, _, _, _>([], opts.bitcoind_args, {
                let res_tx = res_tx.clone();
                move |err| {
                    let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
                }
            });
        // wait for startup
        let mut bitcoin_cli = bitcoind.new_bitcoin_cli(bin_paths.bitcoin_cli()?.clone());
        wait_for_bitcoind_ready(&bitcoin_cli).await?;

        // Create a wallet and initialize it
        tracing::debug!("Creating wallet");
        let _create_wallet_output = bitcoin_cli
            .command::<String, _, _, _, _>([], "createwallet", ["integration-test"])
            .run_utf8()
            .await?;
        bitcoin_cli.rpc_wallet = Some("integration-test".to_owned());
        let mining_address = match signet_setup.as_ref() {
            Some(signet_setup) => {
                let () = signet_setup.init_bitcoind_wallet(&bitcoin_cli).await?;
                signet_setup.signet_challenge_addr.clone()
            }
            None => {
                tracing::debug!("Generating mining address");
                let mining_addr_str = bitcoin_cli
                    .command::<String, _, String, _, _>([], "getnewaddress", [])
                    .run_utf8()
                    .await?;
                mining_addr_str
                    .parse::<bitcoin::Address<_>>()?
                    .require_network(network.into())?
            }
        };
        tracing::debug!("Mining address: {mining_address}");
        tracing::debug!("Generating receiving address");
        let receive_address = {
            let receive_address_str = bitcoin_cli
                .command::<String, _, String, _, _>([], "getnewaddress", [])
                .run_utf8()
                .await?;
            tracing::debug!("Receiving address: {receive_address_str}");
            receive_address_str
                .parse::<Address<_>>()?
                .require_network(bitcoind.network)?
        };
        let mut signet_miner = if let Some(signet_setup) = signet_setup.as_ref() {
            let mut signet_miner = bins::SignetMiner {
                path: bin_paths.signet_miner()?.clone(),
                bitcoin_cli: bitcoin_cli.clone(),
                bitcoin_util: bin_paths.bitcoin_util()?.clone(),
                block_interval: None,
                coinbase_recipient: Some(mining_address.clone()),
                debug: false,
                nbits: None,
                getblocktemplate_command: None,
                coinbasetxn: false,
            };
            let () = signet_setup.calibrate_signet(&mut signet_miner).await?;
            Some(signet_miner)
        } else {
            None
        };
        // Mine 1 block
        tracing::debug!(%mining_address, "Mining 1 block");
        if let Some(signet_miner) = signet_miner.as_ref() {
            let mine_output = signet_miner
                .command("generate", vec!["--address", &mining_address.to_string()])
                .run_utf8()
                .await?;
            tracing::debug!("Checking that block was mined successfully");
            let blocks: u32 = bitcoin_cli
                .command::<String, _, String, _, _>([], "getblockcount", [])
                .run_utf8()
                .await?
                .parse()?;
            anyhow::ensure!(blocks == 1);
            tracing::debug!("Mined 1 block: `{mine_output}`");
        } else {
            let _output = bitcoin_cli
                .command::<String, _, _, _, _>(
                    [],
                    "generatetoaddress",
                    ["1", &mining_address.to_string()],
                )
                .run_utf8()
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
        let electrs_task = electrs.spawn_command_with_args::<String, String, _, _, _>([], [], {
            let res_tx = res_tx.clone();
            move |err| {
                let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
            }
        });
        // wait for electrs to start
        sleep(std::time::Duration::from_secs(1)).await;
        // Start BIP300301 Enforcer
        tracing::debug!("Starting bip300301_enforcer");
        let enforcer = Enforcer {
            path: bin_paths.bip300301_enforcer()?.clone(),
            data_dir: dirs.enforcer_dir.clone(),
            enable_mempool: mode.enable_mempool(),
            enable_wallet: true,
            node_blocks_dir: None,
            node_rpc_user: bitcoind.rpc_user,
            node_rpc_pass: bitcoind.rpc_pass,
            node_rpc_port: bitcoind.rpc_port,
            node_zmq_sequence_port: bitcoind.zmq_sequence_port,
            serve_grpc_port: reserved_ports.enforcer_serve_grpc.port(),
            serve_rpc_port: reserved_ports.enforcer_serve_rpc.port(),
            wallet_electrum_rpc_port: electrs.electrum_rpc_port,
            wallet_electrum_http_port: electrs.electrum_http_port,
        };
        let enforcer_task = enforcer.spawn_command_with_args(
            [(
                "RUST_LOG",
                "h2=info,hyper_util=info,jsonrpsee-client=debug,jsonrpsee-http=debug,connectrpc=debug,trace",
            )],
            opts.enforcer_args,
            move |err| {
                let _err: Result<(), _> = res_tx.unbounded_send(Err(err));
            },
        );
        let tasks = Tasks {
            _enforcer: enforcer_task,
            _electrs: electrs_task,
            _bitcoind: bitcoind_task,
        };
        // Wait for enforcer gRPC port to open
        wait_for_port(
            "127.0.0.1",
            enforcer.serve_grpc_port,
            Duration::from_secs(10),
        )
        .await
        .map_err(|e| anyhow!("Failed waiting for enforcer gRPC port: {e}"))?;

        let gbt_client = jsonrpsee::http_client::HttpClient::builder()
            .build(format!("http://127.0.0.1:{}", enforcer.serve_rpc_port))
            .map_err(|err| anyhow!("failed to create gbt client: {err:#}"))?;
        if let Some(signet_miner) = signet_miner.as_mut() {
            let () = SignetSetup::configure_miner(signet_miner, &dirs.base_dir, &enforcer)?;
        }
        let (http, config) = make_client(enforcer.serve_grpc_port)?;
        let validator_service_client = ValidatorServiceClient::new(http.clone(), config.clone());
        let wallet_service_client = WalletServiceClient::new(http, config);
        let bitcoin_util = {
            let path = match bin_paths.bitcoin_util() {
                Ok(path) => Ok(path.clone()),
                Err(err) => Err(Arc::new(err)),
            };
            let network = bitcoind.network;
            let closure = move || path.map(|path| bins::BitcoinUtil { path, network });
            LazyLock::new(Box::new(closure) as Box<_>)
        };
        Ok(PostSetup {
            network,
            mode,
            bitcoin_cli,
            bitcoin_util,
            tasks,
            signet_miner,
            gbt_client,
            validator_service_client,
            wallet_service_client,
            mining_address,
            receive_address,
            directories: dirs.clone(),
            reserved_ports,
        })
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

    /// Drain any currently-ready events from the channel (non-blocking).
    fn update_from_events(&mut self) -> Result<(), DummySidechainError> {
        use bip300301_enforcer_lib::proto::{
            self,
            mainchain::{
                SubscribeEventsResponse, WithdrawalBundleEvent,
                subscribe_events_response::{
                    self,
                    event::{ConnectBlock, Event},
                },
                withdrawal_bundle_event,
            },
        };
        use tokio::sync::mpsc::error::TryRecvError;
        let Some(rx) = self.event_rx.as_mut() else {
            return Err(DummySidechainError::EventStreamCancelled);
        };
        loop {
            let item = match rx.try_recv() {
                Ok(item) => item,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.event_rx = None;
                    return Err(DummySidechainError::EventStreamClosed);
                }
            };
            let SubscribeEventsResponse { event, .. } = match item {
                Ok(event) => event,
                Err(err) => {
                    self.event_rx = None;
                    return Err(err.into());
                }
            };
            let subscribe_events_response::Event { event, .. } = event
                .into_option()
                .ok_or_else(|| proto::Error::missing_field::<SubscribeEventsResponse>("event"))?;
            let event: subscribe_events_response::event::Event = event.ok_or_else(|| {
                proto::Error::missing_field::<subscribe_events_response::Event>("event")
            })?;
            match event {
                Event::ConnectBlock(connect_block_event) => {
                    let block_info = connect_block_event
                        .block_info
                        .into_option()
                        .ok_or_else(|| proto::Error::missing_field::<ConnectBlock>("block_info"))?;
                    'inner: for event in block_info.events {
                        let Some(wbe) = Self::extract_withdrawal_bundle_event(event)? else {
                            continue 'inner;
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
                                proto::Error::missing_field::<withdrawal_bundle_event::Event>(
                                    "event",
                                )
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
                }
                Event::DisconnectBlock(_) => (),
            }
        }
        Ok(())
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
        // Pump the connect-rust ServerStream into a tokio mpsc so the
        // existing non-blocking `try_recv`-based event drain works as before.
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
        let () = self.update_from_events()?;
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
