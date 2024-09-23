use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::PathBuf,
};

use clap::Parser;

#[derive(Parser)]
pub struct Cli {
    #[arg(default_value_t = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18_443)), long)]
    pub node_rpc_addr: SocketAddr,

    #[arg(default_value = "../data/bitcoin", long)]
    pub node_rpc_datadir: PathBuf,

    #[arg(default_value_t = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 50_051)), long)]
    pub serve_rpc_addr: SocketAddr,
}
