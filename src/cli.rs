use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use clap::Parser;

#[derive(Parser)]
pub struct Config {
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
}
