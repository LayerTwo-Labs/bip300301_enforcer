use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::PathBuf,
};

use bdk::bitcoin::Network;
use clap::Parser;

#[derive(Parser, Clone)]
pub struct Config {
    /// Directory to store wallet + drivechain data.
    /// TODO: find a sensible default outside of the repo.
    #[arg(default_value = "./datadir", long)]
    pub data_dir: PathBuf,

    #[arg(default_value = "localhost", long)]
    pub node_rpc_host: String,

    #[arg(default_value = "18443", long)]
    pub node_rpc_port: u16,

    /// Path to Bitcoin Core cookie. Cannot be set together with user + password.
    #[arg(long)]
    pub node_rpc_cookie_path: Option<String>,

    /// RPC user for Bitcoin Core. Implies also setting password.
    /// Cannot be set together with cookie path.
    #[arg(long)]
    pub node_rpc_user: Option<String>,

    /// RPC password for Bitcoin Core. Implies also setting user. Cannot
    /// be set together with cookie path.
    #[arg(long)]
    pub node_rpc_password: Option<String>,

    #[arg(default_value_t = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 50_051)), long)]
    pub serve_rpc_addr: SocketAddr,

    #[arg(long)]
    pub enable_wallet: bool,

    #[arg(long, default_value = "signet")]
    pub wallet_network: Network,

    #[arg(long, default_value = "drivechain.live")]
    pub wallet_electrum_host: String,

    #[arg(long, default_value = "50001")]
    pub wallet_electrum_port: u16,
}
