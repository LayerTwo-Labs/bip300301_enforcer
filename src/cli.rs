use std::{
    env,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs},
    path::{Path, PathBuf},
};

use clap::{Args, Parser};
use thiserror::Error;

const DEFAULT_NODE_RPC_ADDR: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18443));

#[derive(Debug, Error)]
enum HostAddrError {
    #[error("Failed to resolve address")]
    FailedResolution,
    #[error("Failed to parse address")]
    InvalidAddress(#[source] std::io::Error),
}

fn parse_host_addr(s: &str) -> Result<SocketAddr, HostAddrError> {
    s.to_socket_addrs()
        .map_err(HostAddrError::InvalidAddress)?
        .next()
        .ok_or(HostAddrError::FailedResolution)
}

fn get_data_dir() -> Result<PathBuf, String> {
    const APP_NAME: &str = "bip300301_enforcer";

    let dir = match env::consts::OS {
        "linux" => {
            if let Ok(xdg_data_home) = env::var("XDG_DATA_HOME") {
                Path::new(&xdg_data_home).join(APP_NAME)
            } else {
                let home = env::var("HOME")
                    .map_err(|_| "HOME environment variable not set".to_string())?;
                Path::new(&home).join(".local").join("share").join(APP_NAME)
            }
        }
        "macos" => {
            let home =
                env::var("HOME").map_err(|_| "HOME environment variable not set".to_string())?;
            Path::new(&home)
                .join("Library")
                .join("Application Support")
                .join(APP_NAME)
        }
        "windows" => {
            let app_data = env::var("APPDATA")
                .map_err(|_| "APPDATA environment variable not set".to_string())?;
            Path::new(&app_data).join(APP_NAME)
        }
        os => return Err(format!("Unsupported OS: {}", os)),
    };

    Ok(dir)
}

#[derive(Args, Clone)]
pub struct NodeRpcConfig {
    #[arg(
        default_value_t = DEFAULT_NODE_RPC_ADDR,
        long = "node-rpc-addr",
        value_parser = parse_host_addr
    )]
    pub addr: SocketAddr,
    /// Path to Bitcoin Core cookie. Cannot be set together with user + password.
    #[arg(long = "node-rpc-cookie-path")]
    pub cookie_path: Option<String>,
    /// RPC user for Bitcoin Core. Implies also setting password.
    /// Cannot be set together with cookie path.
    #[arg(long = "node-rpc-user")]
    pub user: Option<String>,
    /// RPC password for Bitcoin Core. Implies also setting user. Cannot
    /// be set together with cookie path.
    #[arg(long = "node-rpc-pass")]
    pub pass: Option<String>,
}

#[derive(Clone, Args)]
pub struct MiningConfig {
    /// Path to the Python mining script from Bitcoin Core. If not set,
    /// the mining script is downloaded from GitHub.
    #[arg(long = "signet-miner-script-path")]
    pub signet_mining_script_path: Option<PathBuf>,

    /// If true, the signet mining script is run with `--debug` flag.
    #[arg(long = "signet-miner-script-debug", default_value_t = false)]
    pub signet_mining_script_debug: bool,

    /// Path to the Bitcoin Core `bitcoin-util` binary. Defaults to `bitcoin-util`.
    #[arg(
        long = "signet-miner-bitcoin-util-path",
        default_value = "bitcoin-util"
    )]
    pub bitcoin_util_path: PathBuf,

    /// Path to the Bitcoin Core `bitcoin-cli` binary. Defaults to `bitcoin-cli`.
    #[arg(long = "signet-miner-bitcoin-cli-path", default_value = "bitcoin-cli")]
    pub bitcoin_cli_path: PathBuf,
}

#[derive(Clone, Args)]
pub struct WalletConfig {
    /// If no host is provided, a default value is used based on the network
    /// we're on.
    ///
    /// Signet: drivechain.live, regtest: 127.0.0.1  
    #[arg(long = "wallet-electrum-host")]
    pub electrum_host: Option<String>,

    /// If no port is provided, a default value is used based on the network
    /// we're on.
    ///
    /// Signet: 50001, regtest: 60401
    #[arg(long = "wallet-electrum-port")]
    pub electrum_port: Option<u16>,

    /// If no existing wallet is found, automatically create and load
    /// a new, unencrypted wallet from a randomly generated BIP39 mnemonic.
    #[arg(long = "wallet-auto-create", default_value_t = false)]
    pub auto_create: bool,
}

const DEFAULT_SERVE_RPC_ADDR: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8122));
const DEFAULT_SERVE_GRPC_ADDR: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 50_051));

#[derive(Clone, Parser)]
pub struct Config {
    /// Directory to store wallet + drivechain + validator data.
    #[arg(default_value_os_t = get_data_dir().unwrap_or_else(|_| PathBuf::from("./datadir")), long)]
    pub data_dir: PathBuf,
    #[arg(long, default_value_t = false)]
    pub enable_wallet: bool,
    /// If enabled, maintains a mempool. If the wallet is enabled, serves
    /// getblocktemplate.
    #[arg(long, default_value_t = false)]
    pub enable_mempool: bool,
    /// Log level.
    /// Logs from most dependencies are filtered one level below the specified
    /// log level, if a lower level exists.
    /// For example, at the default log level `DEBUG`, logs from most
    /// dependencies are only emitted if their level is `INFO` or lower.
    #[arg(default_value_t = tracing::Level::DEBUG, long)]
    pub log_level: tracing::Level,

    /// File path to write logs to, in addition to stdout.
    /// If none is provided, logs are written to `bip300301_enforcer.log`
    /// in the data directory.
    #[arg(long)]
    pub log_file: Option<PathBuf>,

    #[command(flatten)]
    pub node_rpc_opts: NodeRpcConfig,
    /// Bitcoin node ZMQ endpoint for `sequence`
    #[arg(long)]
    pub node_zmq_addr_sequence: String,
    /// Serve RPCs such as `getblocktemplate` on this address
    #[arg(default_value_t = DEFAULT_SERVE_RPC_ADDR, long)]
    pub serve_rpc_addr: SocketAddr,
    /// Serve gRPCs on this address
    #[arg(default_value_t = DEFAULT_SERVE_GRPC_ADDR, long)]
    pub serve_grpc_addr: SocketAddr,
    #[command(flatten)]
    pub wallet_opts: WalletConfig,

    #[command(flatten)]
    pub mining_opts: MiningConfig,
}

impl Config {
    pub fn bitcoin_cli(&self, network: bitcoin::Network) -> crate::bins::BitcoinCli {
        crate::bins::BitcoinCli {
            path: self.mining_opts.bitcoin_cli_path.clone(),
            network,
            rpc_user: self.node_rpc_opts.user.clone().unwrap_or_default(),
            rpc_pass: self.node_rpc_opts.pass.clone().unwrap_or_default(),
            rpc_port: self.node_rpc_opts.addr.port(),
            rpc_host: self.node_rpc_opts.addr.ip().to_string(),
            rpc_wallet: None,
        }
    }
}
