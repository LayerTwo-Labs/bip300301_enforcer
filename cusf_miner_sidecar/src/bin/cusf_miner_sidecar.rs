//! HTTP entrypoint for the CUSF miner sidecar (localhost only).

use std::net::SocketAddr;

use clap::Parser;
use cusf_miner_sidecar::{
    MinerSidecar, RouterConfig, RpcConfig, bind_addr, is_loopback_ip, router,
};
use tracing_subscriber::EnvFilter;

/// Hold enforcer-format txs and mine them into stock bitcoind via submitblock.
///
/// Binds **127.0.0.1 only** by default — not for public exposure.
/// RPC password is never logged.
#[derive(Parser)]
#[command(name = "cusf_miner_sidecar", version, about)]
struct Args {
    /// HTTP listen port (always on 127.0.0.1 unless --allow-non-loopback).
    #[arg(long, default_value_t = 18_450)]
    port: u16,

    /// Explicit bind address. Default is 127.0.0.1. Non-loopback requires
    /// `--allow-non-loopback` (not recommended).
    #[arg(long)]
    bind: Option<SocketAddr>,

    /// Permit binding outside loopback (insecure for multi-tenant use).
    #[arg(long, default_value_t = false)]
    allow_non_loopback: bool,

    /// bitcoind JSON-RPC host:port (must be a full SocketAddr, e.g. 127.0.0.1:18443).
    #[arg(long, default_value = "127.0.0.1:18443")]
    bitcoind_rpc: String,

    /// bitcoind RPC username.
    #[arg(long, default_value = "user")]
    rpc_user: String,

    /// bitcoind RPC password.
    #[arg(long, default_value = "pass")]
    rpc_pass: String,

    /// Address for coinbase payout (must be valid for the node's network).
    #[arg(long)]
    coinbase_address: String,
}

// Manual Debug so clap secrets never print via accidental {:?} on Args.
impl std::fmt::Debug for Args {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Args")
            .field("port", &self.port)
            .field("bind", &self.bind)
            .field("allow_non_loopback", &self.allow_non_loopback)
            .field("bitcoind_rpc", &self.bitcoind_rpc)
            .field("rpc_user", &self.rpc_user)
            .field("rpc_pass", &"<redacted>")
            .field("coinbase_address", &self.coinbase_address)
            .finish()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let bind = args.bind.unwrap_or_else(|| bind_addr(args.port));
    if !is_loopback_ip(bind.ip()) {
        if !args.allow_non_loopback {
            anyhow::bail!(
                "refusing to bind non-loopback address {bind} without --allow-non-loopback \
                 (sidecar is local-only; inventory is unauthenticated)"
            );
        }
        tracing::warn!(
            %bind,
            path = "cusf_sidecar",
            "binding non-loopback address with --allow-non-loopback; \
             inventory HTTP is unauthenticated — do not expose publicly"
        );
    }

    let addr = RpcConfig::parse_addr(&args.bitcoind_rpc)?;
    let rpc = RpcConfig::new(addr, args.rpc_user, args.rpc_pass);
    let coinbase: bitcoin::Address = args
        .coinbase_address
        .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
        .map_err(|e| anyhow::anyhow!("invalid --coinbase-address: {e}"))?
        .assume_checked();

    let sidecar = MinerSidecar::from_rpc_config(&rpc, &coinbase)
        .map_err(|e| anyhow::anyhow!("bitcoind RPC client: {e}"))?;

    let app = router(sidecar.app_state(), RouterConfig);
    tracing::info!(
        %bind,
        bitcoind = %args.bitcoind_rpc,
        path = "cusf_sidecar",
        "CUSF miner sidecar listening (localhost-only by default)"
    );

    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
