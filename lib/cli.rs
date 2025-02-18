use std::{
    borrow::Cow,
    env,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs},
    path::{Path, PathBuf},
};

use clap::{Args, Parser, ValueEnum};
use thiserror::Error;
use tracing_subscriber::fmt::format as tracing_format;

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

// Sub-par location for the log file.
// https://github.com/LayerTwo-Labs/bip300301_enforcer/issues/133
const DEFAULT_LOG_FILENAME: &str = "bip300301_enforcer.log";

/// Possible formats for log output.
#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum LogFormat {
    /// See https://docs.rs/tracing-subscriber/latest/tracing_subscriber/fmt/format/struct.Compact.html
    #[default]
    Compact,
    /// See https://docs.rs/tracing-subscriber/latest/tracing_subscriber/fmt/format/struct.Full.html
    Full,
    /// See https://docs.rs/tracing-subscriber/latest/tracing_subscriber/fmt/format/struct.Json.html
    Json,
    /// See https://docs.rs/tracing-subscriber/latest/tracing_subscriber/fmt/format/struct.Pretty.html
    Pretty,
}

/// Log formatter, equivalent to [`tracing_subscriber::fmt::format::Format`]
#[derive(Clone, Copy, Debug)]
pub struct LogFormatter {
    format: LogFormat,
    display_filename: Option<bool>,
    display_line_number: Option<bool>,
}

impl LogFormatter {
    pub fn with_file(mut self, display_filename: bool) -> Self {
        self.display_filename = Some(display_filename);
        self
    }

    pub fn with_line_number(mut self, display_line_number: bool) -> Self {
        self.display_line_number = Some(display_line_number);
        self
    }

    fn set_format_opts<F, T>(
        &self,
        mut format: tracing_format::Format<F, T>,
    ) -> tracing_format::Format<F, T> {
        if let Some(display_filename) = self.display_filename {
            format = format.with_file(display_filename);
        }
        if let Some(display_line_number) = self.display_line_number {
            format = format.with_line_number(display_line_number);
        }
        format
    }
}

impl From<LogFormat> for LogFormatter {
    fn from(format: LogFormat) -> Self {
        Self {
            format,
            display_filename: None,
            display_line_number: None,
        }
    }
}

impl<C, N> tracing_subscriber::fmt::FormatEvent<C, N> for LogFormatter
where
    C: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, C, N>,
        writer: tracing_format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        use tracing_subscriber::fmt::format::{Format, Full};
        let format: Format<Full> = Format::default();
        match self.format {
            LogFormat::Compact => self
                .set_format_opts(format.compact())
                .format_event(ctx, writer, event),
            LogFormat::Full => self
                .set_format_opts(format)
                .format_event(ctx, writer, event),
            LogFormat::Json => self
                .set_format_opts(format.json())
                .format_event(ctx, writer, event),
            LogFormat::Pretty => self
                .set_format_opts(format.pretty())
                .format_event(ctx, writer, event),
        }
    }
}

impl<'writer> tracing_subscriber::fmt::FormatFields<'writer> for LogFormatter {
    fn format_fields<R: tracing_subscriber::field::RecordFields>(
        &self,
        writer: tracing_format::Writer<'writer>,
        fields: R,
    ) -> std::fmt::Result {
        use tracing_subscriber::fmt::format::{DefaultFields, JsonFields, Pretty};
        match self.format {
            LogFormat::Compact | LogFormat::Full => {
                DefaultFields::new().format_fields(writer, fields)
            }
            LogFormat::Json => JsonFields::new().format_fields(writer, fields),
            LogFormat::Pretty => Pretty::default().format_fields(writer, fields),
        }
    }
}

#[derive(Clone, Args)]
pub struct LoggerConfig {
    /// File path to write logs to, in addition to stdout.
    /// If none is provided, logs are written to `bip300301_enforcer.log`
    /// in the data directory.
    #[arg(long = "log-file")]
    file: Option<PathBuf>,
    /// Format for log output.
    #[arg(default_value_t, long = "log-format", value_enum)]
    format: LogFormat,
    /// Log level.
    /// Logs from most dependencies are filtered one level below the specified
    /// log level, if a lower level exists.
    /// For example, at the default log level `DEBUG`, logs from most
    /// dependencies are only emitted if their level is `INFO` or lower.
    /// Logger output is further configurable via the `RUST_LOG` environment
    /// variable, using a directive of the form specified in
    /// https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html#directives
    #[arg(default_value_t = tracing::Level::DEBUG, long = "log-level")]
    pub level: tracing::Level,
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

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum WalletSyncSource {
    #[default]
    /// Communicates over the Electrum protocol.
    Electrum,
    /// Communicates over REST to a Esplora server (i.e. mempool.space APIt
    Esplora,
}

#[derive(Clone, Args)]
pub struct WalletConfig {
    /// If true, the wallet will perform a full scan of the blockchain on startup, before
    /// proceeding with the normal operations of the wallet.
    #[arg(long = "wallet-full-scan", default_value_t = false)]
    pub full_scan: bool,
    /// If no existing wallet is found, automatically create and load
    /// a new, unencrypted wallet from a randomly generated BIP39 mnemonic.
    #[arg(
        long = "wallet-auto-create",
        default_value_t = false,
        conflicts_with = "mnemonic_path"
    )]
    pub auto_create: bool,
    /// URL of the Esplora server to use for the wallet.
    ///
    /// Signet: http://172.105.148.135:3000
    /// Regtest: http://localhost:3003
    #[arg(long = "wallet-esplora-url")]
    pub esplora_url: Option<url::Url>,
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

    /// Skip the periodic wallet sync task. This can be useful if
    /// the wallet is large and periodic syncs are not feasible.
    #[arg(long = "wallet-skip-periodic-sync", default_value_t = false)]
    pub skip_periodic_sync: bool,
    /// The source of the wallet sync.
    #[arg(long = "wallet-sync-source", default_value_t = WalletSyncSource::Electrum, value_enum)]
    pub sync_source: WalletSyncSource,

    /// Path to a file containing exactly 12 space-separated BIP39 mnemonic words.
    #[arg(long = "wallet-seed-file", conflicts_with = "auto_create")]
    pub mnemonic_path: Option<PathBuf>,
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
    #[command(flatten)]
    pub logger_opts: LoggerConfig,
    #[command(flatten)]
    pub mining_opts: MiningConfig,
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

    pub fn log_file(&self) -> Cow<'_, Path> {
        match &self.logger_opts.file {
            Some(log_file) => Cow::Borrowed(log_file),
            None => Cow::Owned(self.data_dir.join(DEFAULT_LOG_FILENAME)),
        }
    }

    pub fn log_formatter(&self) -> LogFormatter {
        self.logger_opts.format.into()
    }
}
