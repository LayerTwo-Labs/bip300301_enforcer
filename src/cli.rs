use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs},
    path::PathBuf,
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

#[derive(Clone, Args)]
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
pub struct WalletConfig {
    #[arg(default_value = "drivechain.live", long = "wallet-electrum-host")]
    pub electrum_host: String,
    #[arg(default_value = "50001", long = "wallet-electrum-port")]
    pub electrum_port: u16,
}

const DEFAULT_SERVE_RPC_ADDR: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 50_051));

#[derive(Clone, Parser)]
pub struct Config {
    /// Directory to store wallet + drivechain data.
    /// TODO: find a sensible default outside of the repo.
    #[arg(default_value = "./datadir", long)]
    pub data_dir: PathBuf,
    #[arg(long)]
    pub enable_wallet: bool,
    /// Log level.
    /// Logs from most dependencies are filtered one level below the specified
    /// log level, if a lower level exists.
    /// For example, at the default log level `DEBUG`, logs from most
    /// dependencies are only emitted if their level is `INFO` or lower.
    #[arg(default_value_t = tracing::Level::DEBUG, long)]
    pub log_level: tracing::Level,
    #[command(flatten)]
    pub node_rpc_opts: NodeRpcConfig,
    /// Bitcoin node ZMQ endpoint for `sequence`
    #[arg(long)]
    pub node_zmq_addr_sequence: String,
    #[arg(default_value_t = DEFAULT_SERVE_RPC_ADDR, long)]
    pub serve_rpc_addr: SocketAddr,
    #[command(flatten)]
    pub wallet_opts: WalletConfig,
}
