use std::{
    env,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs},
    num::NonZeroU64,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use clap::{Args, Parser, ValueEnum};
use thiserror::Error;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::fmt::format as tracing_format;

use crate::types::NetworkParams;

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
        os => return Err(format!("Unsupported OS: {os}")),
    };

    Ok(dir)
}

// Sub-par location for the log file.
// https://github.com/LayerTwo-Labs/bip300301_enforcer/issues/133
const LOG_FILENAME: &str = "bip300301_enforcer.log";

// Sub-par location for the log dir.
// https://github.com/LayerTwo-Labs/bip300301_enforcer/issues/133
const DEFAULT_LOG_DIRNAME: &str = "logs";

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

impl LogFormat {
    fn default_log_suffix(&self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Compact | Self::Full | Self::Pretty => "log",
        }
    }
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

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum LogRotation {
    Daily,
    Hourly,
    Minutely,
    #[default]
    Never,
}

impl From<LogRotation> for Rotation {
    fn from(rotation: LogRotation) -> Self {
        match rotation {
            LogRotation::Daily => Self::DAILY,
            LogRotation::Hourly => Self::HOURLY,
            LogRotation::Minutely => Self::MINUTELY,
            LogRotation::Never => Self::NEVER,
        }
    }
}

#[derive(Clone, Args)]
pub struct LoggerConfig {
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
    /// Set a limit for the maximum number of log files that will be retained.
    /// Older log files will be deleted if the maximum number of log files has
    /// been reached.
    #[arg(long = "max-log-files", default_value_t = 10)]
    pub max_log_files: usize,

    /// Set a limit for the maximum size of a log file, in megabytes.
    /// If the log file exceeds this size, it will be rotated.
    #[arg(long = "max-log-file-size-mb", default_value_t = 100)]
    pub max_log_file_size_mb: u64,

    /// Log file directory.
    #[arg(long = "log-directory")]
    directory: Option<PathBuf>,
    /// Log file rotation frequency.
    /// If set, a new log file will be created at the specified interval.
    #[arg(default_value_t, long = "log-rotation", value_enum)]
    pub rotation: LogRotation,
}

/// Parse an address without committing to a network. The network is only known
/// once we've asked the node, so the check happens at startup instead.
fn parse_bitcoin_address_unchecked(
    s: &str,
) -> Result<bitcoin::Address<bitcoin::address::NetworkUnchecked>, String> {
    bitcoin::Address::from_str(s).map_err(|err| format!("invalid bitcoin address: {err}"))
}

fn parse_network_magic(s: &str) -> Result<[u8; 4], String> {
    let bytes = hex::decode(s).map_err(|err| format!("invalid hex: {err}"))?;
    bytes
        .try_into()
        .map_err(|_| format!("expected 4 bytes (8 hex chars), got {}", s.len() / 2))
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
pub struct NodeBlocksDirConfig {
    /// Path to the Bitcoin Core blocks directory.
    #[arg(long = "node-blocks-dir")]
    pub dir: Option<PathBuf>,
    /// Path to the node's `mempool.dat`. When set, the initial mempool sync
    /// reads transactions from this file instead of fetching every one of them
    /// over RPC, and falls back to RPC for whatever the file cannot supply.
    #[arg(long = "node-mempool-dat")]
    pub mempool_dat: Option<PathBuf>,
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
    // Doc comments on these fields become `--help` text, so this note is a
    // plain comment: the `SecretString` type is what redacts this value
    // everywhere, including the startup configuration dump.
    #[arg(long = "node-rpc-pass")]
    pub pass: Option<SecretString>,
}

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
pub enum NetworkPreset {
    /// Dry run forknet v4: mainnet fork at block 961632. Hours-scale thresholds
    Drynet4,
    /// Alphanet forknet: mainnet fork at block 963648. Hours-scale thresholds
    Alphanet,
    /// Integration-test-only preset: SHORT thresholds with BIP300/301
    /// activating at height 10, so tests can exercise the activation-height
    /// machinery on a fresh chain. Hidden from --help
    #[value(hide = true)]
    TestActivation,
}

impl NetworkPreset {
    pub const fn params(self) -> NetworkParams {
        match self {
            Self::Drynet4 => NetworkParams::drynet4(),
            Self::Alphanet => NetworkParams::alphanet(),
            Self::TestActivation => NetworkParams::test_activation(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, ValueEnum)]
pub enum WalletSyncSource {
    /// Communicates over the Electrum protocol.
    Electrum,
    #[default]
    /// Communicates over REST to a Esplora server (i.e. mempool.space API)
    Esplora,
    /// The wallet is only synced by new blocks coming in.
    Disabled,
}

#[derive(Clone, Args)]
pub struct WalletConfig {
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
    /// Signet: https://explorer.signet.drivechain.info/api
    /// Mainnet: https://explorer.forknet.drivechain.info/api
    /// Regtest: http://localhost:3003
    #[arg(long = "wallet-esplora-url")]
    pub esplora_url: Option<url::Url>,
    /// If no host is provided, a default value is used based on the network
    /// we're on.
    ///
    /// Signet: node.signet.drivechain.info, Mainnet: node.forknet.drivechain.info, regtest: 127.0.0.1
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
    #[arg(long = "wallet-sync-source", default_value_t = WalletSyncSource::default(), value_enum)]
    pub sync_source: WalletSyncSource,

    /// How many blocks the wallet is willing to catch up on one block at a
    /// time. Past this it advances its chain with a single checkpoint and
    /// recovers the transactions in the skipped range with a full scan
    /// against the sync backend instead.
    ///
    /// Hidden: for testing purposes only
    #[arg(
        long = "wallet-max-block-by-block-replay",
        default_value_t = 2_000,
        hide = true
    )]
    pub max_block_by_block_replay: u32,

    /// SLIP-44 coin type for the BIP 84 account path, overriding the one the
    /// network implies. Drives a descriptor mismatch on a network where the
    /// coin type never changed and one could not otherwise arise.
    ///
    /// Hidden: for testing purposes only
    #[arg(long = "wallet-derivation-coin-type", hide = true)]
    pub derivation_coin_type: Option<u32>,

    /// Path to a file containing exactly 12 space-separated BIP39 mnemonic words.
    #[arg(long = "wallet-seed-file", conflicts_with = "auto_create")]
    pub mnemonic_path: Option<PathBuf>,
}

#[derive(miette::Diagnostic, Debug, Error)]
pub enum RollingLoggerError {
    #[error(transparent)]
    Init(#[from] tracing_appender::rolling::InitError),
    #[error("Log file name must be valid UTF-8")]
    InvalidFileName,
    #[error("Log path has no file name")]
    NoFileName,
    #[error("Log file path has no parent")]
    NoParent,
}

const DEFAULT_SERVE_RPC_ADDR: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8122));
const DEFAULT_SERVE_GRPC_ADDR: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 50_051));

fn get_long_version() -> clap::builder::Str {
    format!(
        "v{}
 commit: {}
 binary: bip300301_enforcer",
        env!("CARGO_PKG_VERSION"),
        env!("GIT_HASH"),
    )
    .into()
}

#[derive(Clone, Parser)]
#[clap(version, long_version = get_long_version())]
pub struct Config {
    /// Directory to store wallet + drivechain + validator data.
    #[arg(default_value_os_t = get_data_dir().unwrap_or_else(|_| PathBuf::from("./datadir")), long)]
    pub data_dir: PathBuf,
    #[arg(long, default_value_t = false)]
    pub enable_wallet: bool,
    /// If enabled, maintains a mempool. Required for serving block templates and
    /// for the wallet to track unconfirmed transactions.
    #[arg(long, default_value_t = false)]
    pub enable_mempool: bool,
    /// Serve `getblocktemplate` to miners. Requires `--enable-mempool`.
    ///
    /// Works without a wallet.
    ///
    /// Without a wallet, `--coinbase-recipient` is required.
    #[arg(long, default_value_t = false, requires = "enable_mempool")]
    pub enable_block_template_server: bool,
    /// Address that receives the block reward.
    ///
    /// Applies to served `getblocktemplate` and the signet mining script.
    /// Validated against the node's network at startup. If the
    /// wallet is enabled and this is unset, the reward goes to a fresh address
    /// from the wallet.
    #[arg(long, value_parser = parse_bitcoin_address_unchecked)]
    pub coinbase_recipient: Option<bitcoin::Address<bitcoin::address::NetworkUnchecked>>,
    #[command(flatten)]
    pub logger_opts: LoggerConfig,
    /// GBT cache lifetime, in seconds
    #[arg(long)]
    pub gbt_cache_lifetime_s: Option<NonZeroU64>,
    #[command(flatten)]
    pub mining_opts: MiningConfig,
    #[command(flatten)]
    pub node_rpc_opts: NodeRpcConfig,
    #[command(flatten)]
    pub node_blocks_dir_opts: NodeBlocksDirConfig,
    /// Apply a network parameter preset By default parameters derive from the
    /// node's reported network.
    #[arg(long, value_enum)]
    pub network_preset: Option<NetworkPreset>,
    /// P2P message-start bytes as hex, e.g. `eca5d404`. Overrides the value
    /// from `--network-preset` and automatic signet-challenge derivation.
    //
    // Hidden because a preset is the intended way to get this. This CLI opt is
    // only needed for tests and dev purposes.
    #[arg(long, value_parser = parse_network_magic, hide = true)]
    pub network_magic: Option<[u8; 4]>,
    /// Bitcoin node ZMQ endpoint for `sequence`. If not set, we try to find
    /// it via `bitcoin-cli getzmqnotifications`.
    #[arg(long)]
    pub node_zmq_addr_sequence: Option<String>,
    /// Broadcast Deposit/Withdrawal/BMM request txs via p2p to this peer.
    /// Accepts `IP:PORT` or `HOST:PORT`
    /// On L2L Signet, txs are broadcast to the signet mining server by
    /// default.
    /// This option can be specified multiple times.
    #[arg(long)]
    pub p2p_broadcast_addr: Vec<crate::p2p::BroadcastAddr>,
    /// Serve RPCs such as `getblocktemplate` on this address
    #[arg(default_value_t = DEFAULT_SERVE_RPC_ADDR, long)]
    pub serve_rpc_addr: SocketAddr,
    /// Serve gRPCs on this address
    #[arg(default_value_t = DEFAULT_SERVE_GRPC_ADDR, long)]
    pub serve_grpc_addr: SocketAddr,
    #[command(flatten)]
    pub wallet_opts: WalletConfig,

    /// Exit after syncing to the specified block height. If set to 0, we exit
    /// after syncing to the tip. On exit, a summary of the sync (blocks
    /// synced, elapsed time, blocks/sec) is logged.
    #[arg(long)]
    pub exit_after_sync: Option<u32>,

    /// Path to a reference `consensus-state.json` (from a prior
    /// `--exit-after-sync` run). Syncs to that file's tip height and verifies
    /// the resulting consensus state matches it, exiting non-zero on any
    /// mismatch. Cannot be combined with `--exit-after-sync`.
    #[arg(long, conflicts_with = "exit_after_sync")]
    pub verify_consensus_state: Option<PathBuf>,

    /// Sync to the specified block height, then stop syncing and keep the node
    /// running so it can be queried at that frozen height.
    #[arg(long, conflicts_with_all = ["exit_after_sync", "verify_consensus_state"])]
    pub freeze_at_height: Option<u32>,

    /// Assert that the connected Bitcoin Core node is running this major
    /// version (e.g. `30`). If not set, the enforcer accepts any major in
    /// its built-in list of supported versions.
    #[arg(long)]
    pub bitcoin_core_expected_version: Option<u32>,

    /// Skip the Bitcoin Core version compatibility check at startup.
    #[arg(
        long,
        default_value_t = false,
        conflicts_with = "bitcoin_core_expected_version"
    )]
    pub bitcoin_core_skip_version_check: bool,
}

/// Written in place of a sensitive value.
const REDACTED: &str = "[redacted]";

/// Written in place of an argument that was neither given nor defaulted.
const UNSET: &str = "<unset>";

/// A configuration value that must never be written to a log.
///
/// The annotation lives on the field, as its type. Two things follow from
/// that, neither of which depends on anyone maintaining a list of names:
///
/// - `Debug` and `Display` print `[redacted]`, so a secret cannot escape
///   through a stray `{:?}` on a whole config struct.
/// - [`log_effective_config`] asks clap what type each argument parses into,
///   so typing a field `SecretString` is what makes it redacted in the dump.
///
/// Read the value back with [`SecretString::expose`], which is deliberately
/// noisy at the call site.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Read the underlying secret. Every call is a place a secret could
    /// escape, so keep them few and obvious.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Parses a [`SecretString`]. Exists so that clap builds a value parser whose
/// output type is `SecretString`, which is how secret arguments are recognized
/// without matching on their names.
#[derive(Clone, Copy, Debug)]
pub struct SecretStringParser;

impl clap::builder::TypedValueParser for SecretStringParser {
    type Value = SecretString;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        match value.to_str() {
            Some(value) => Ok(SecretString::new(value)),
            // Deliberately does not quote the value back, the way clap's own
            // InvalidUtf8 message would.
            None => Err(clap::Error::raw(
                clap::error::ErrorKind::InvalidUtf8,
                "secret value is not valid UTF-8\n",
            )
            .with_cmd(cmd)),
        }
    }
}

impl clap::builder::ValueParserFactory for SecretString {
    type Parser = SecretStringParser;

    fn value_parser() -> Self::Parser {
        SecretStringParser
    }
}

/// Whether `arg`'s value is secret, determined by asking clap what type the
/// argument parses into rather than by matching on its name.
fn is_secret_arg(arg: &clap::Arg) -> bool {
    use clap::builder::{ValueParser, ValueParserFactory as _};

    let secret = ValueParser::from(SecretString::value_parser());
    arg.get_value_parser().type_id() == secret.type_id()
}

const REDACTED_URL_COMPONENT: &str = "redacted";

pub(crate) fn redact_embedded_credentials(value: &str) -> Option<String> {
    let mut url = url::Url::parse(value).ok()?;

    if url.password().is_some() {
        url.set_password(Some(REDACTED_URL_COMPONENT)).ok()?;
    } else if !url.username().is_empty() {
        url.set_username(REDACTED_URL_COMPONENT).ok()?;
    } else {
        return None;
    }

    Some(url.to_string())
}

/// Log the effective value of every argument, one line each, with secrets
/// masked. This mirrors the configuration dump Bitcoin Core writes at startup:
/// it makes a log self-describing, so a bug report says exactly what the node
/// was run with, defaults included.
pub fn log_effective_config(matches: &clap::ArgMatches) {
    for line in effective_config_lines(matches) {
        tracing::info!("Command-line arg: {line}");
    }
}

/// Render `name=value (source)` for every argument, sorted by flag name.
///
/// Works off the raw [`clap::ArgMatches`] rather than a [`Config`] so that
/// every argument is covered by construction: a new argument shows up here
/// without anyone remembering to add it. Kept separate from the logging so
/// that tests can assert on exactly what would be written.
fn effective_config_lines(matches: &clap::ArgMatches) -> Vec<String> {
    use clap::{CommandFactory as _, parser::ValueSource};

    let command = Config::command();
    // Sorted by the flag the operator actually types, not by clap's internal
    // id, which is the struct field name and orders the dump arbitrarily.
    let mut arguments: Vec<_> = command
        .get_arguments()
        .filter(|arg| !matches!(arg.get_id().as_str(), "help" | "version"))
        .map(|arg| {
            let id = arg.get_id().as_str();
            let name = arg
                .get_long()
                .map_or_else(|| id.to_owned(), |long| format!("--{long}"));
            (name, id, is_secret_arg(arg))
        })
        .collect();
    arguments.sort_unstable();

    let mut lines = Vec::with_capacity(arguments.len());
    for (name, id, secret) in arguments {
        let source = match matches.value_source(id) {
            Some(ValueSource::CommandLine) => "command line",
            Some(ValueSource::EnvVariable) => "env",
            Some(ValueSource::DefaultValue) => "default",
            _ => "unset",
        };
        let values: Vec<String> = match matches.try_get_raw(id) {
            Ok(Some(values)) => values
                .map(|value| value.to_string_lossy().into_owned())
                .collect(),
            // An argument that was never given and has no default.
            Ok(None) | Err(_) => Vec::new(),
        };
        let rendered = if values.is_empty() {
            UNSET.to_owned()
        } else if secret {
            REDACTED.to_owned()
        } else {
            let values: Vec<String> = values
                .iter()
                .map(|value| {
                    let value = redact_embedded_credentials(value).unwrap_or_else(|| value.clone());
                    format!("{value:?}")
                })
                .collect();
            // Bracket repeated arguments so a multi-valued one is not mistaken
            // for a single value that happens to contain a comma.
            match values.as_slice() {
                [single] => single.clone(),
                many => format!("[{}]", many.join(", ")),
            }
        };
        lines.push(format!("{name}={rendered} ({source})"));
    }
    lines
}

impl Config {
    /// Parse the command line, keeping the raw [`clap::ArgMatches`] alongside
    /// the parsed config so that [`log_effective_config`] can report where each
    /// value came from once a tracing subscriber has been installed.
    pub fn parse_with_matches() -> (Self, clap::ArgMatches) {
        use clap::{CommandFactory as _, FromArgMatches as _};

        let matches = Self::command().get_matches();
        match Self::from_arg_matches(&matches) {
            Ok(config) => (config, matches),
            // The same exit path `Parser::parse` takes on a bad command line.
            Err(err) => err.exit(),
        }
    }

    /// Returns the git hash that was set during build time
    pub fn git_hash(&self) -> &'static str {
        env!("GIT_HASH")
    }

    pub fn bitcoin_cli(&self, network: bitcoin::Network) -> crate::bins::BitcoinCli {
        crate::bins::BitcoinCli {
            path: self.mining_opts.bitcoin_cli_path.clone(),
            network,
            rpc_user: self.node_rpc_opts.user.clone(),
            rpc_pass: self.node_rpc_opts.pass.clone(),
            rpc_cookie_path: self.node_rpc_opts.cookie_path.clone(),
            rpc_port: self.node_rpc_opts.addr.port(),
            rpc_host: self.node_rpc_opts.addr.ip().to_string(),
            rpc_wallet: None,
        }
    }

    pub fn log_formatter(&self) -> LogFormatter {
        self.logger_opts.format.into()
    }

    /// How long the GBT server may serve a cached template before rebuilding,
    /// or `None` to never cache.
    pub fn gbt_cache_lifetime(&self) -> Option<Duration> {
        self.gbt_cache_lifetime_s
            .map(|secs| Duration::from_secs(secs.get()))
    }

    fn log_filename_suffix(&self) -> Option<&'static str> {
        match self.logger_opts.rotation {
            LogRotation::Never => None,
            LogRotation::Daily | LogRotation::Hourly | LogRotation::Minutely => {
                Some(self.logger_opts.format.default_log_suffix())
            }
        }
    }

    pub fn log_dir(&self) -> PathBuf {
        self.logger_opts
            .directory
            .clone()
            .unwrap_or(self.data_dir.join(DEFAULT_LOG_DIRNAME))
    }

    pub fn rolling_log_appender(&self) -> Result<RollingFileAppender, RollingLoggerError> {
        let rotation = Rotation::from(self.logger_opts.rotation)
            .with_max_bytes(self.logger_opts.max_log_file_size_mb * 1024 * 1024);

        let mut builder = RollingFileAppender::builder()
            .rotation(rotation)
            .filename_prefix(LOG_FILENAME)
            .max_log_files(self.logger_opts.max_log_files);
        if let Some(log_filename_suffix) = self.log_filename_suffix() {
            builder = builder.filename_suffix(log_filename_suffix);
        }

        builder
            .build(self.log_dir())
            .map_err(RollingLoggerError::Init)
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory as _, Parser as _, ValueEnum as _, parser::ValueSource};

    use super::{
        Config, NetworkPreset, REDACTED, SecretString, UNSET, is_secret_arg,
        redact_embedded_credentials,
    };

    /// Each preset's `--network-preset` spelling reaches the parameters it
    /// names, activation height included.
    #[test]
    fn network_presets_parse_to_their_params() {
        let params = |value: &str| {
            Config::try_parse_from(["bip300301_enforcer", &format!("--network-preset={value}")])
                .unwrap_or_else(|err| panic!("--network-preset={value} did not parse: {err}"))
                .network_preset
                .expect("--network-preset was given")
                .params()
        };
        assert_eq!(params("drynet4").bip300_activation_height, 961_632);
        assert_eq!(params("alphanet").bip300_activation_height, 963_648);
    }

    /// `--network-magic` overrides every automatic/configured source, since a
    /// forked build may rebrand the magic independently of the network preset
    /// or signet challenge.
    #[test]
    fn network_magic_flag_overrides_the_preset() {
        let parse = |args: &[&str]| {
            let mut argv = vec!["bip300301_enforcer"];
            argv.extend_from_slice(args);
            Config::try_parse_from(argv)
        };

        // drynet4's regtest bytes, which differ from the preset's mainnet ones.
        let cli =
            parse(&["--network-preset=drynet4", "--network-magic=eca5d434"]).expect("should parse");
        assert_eq!(cli.network_magic, Some([0xec, 0xa5, 0xd4, 0x34]));
        assert_eq!(
            cli.network_preset.unwrap().params().network_magic,
            Some([0xec, 0xa5, 0xd4, 0x04]),
            "the preset still carries its own value; main.rs is what prefers the flag"
        );

        // Usable without a preset at all — that is the regtest case.
        assert_eq!(
            parse(&["--network-magic=fabfb5da"])
                .expect("should parse")
                .network_magic,
            Some([0xfa, 0xbf, 0xb5, 0xda])
        );

        // Wrong length and non-hex are rejected rather than silently padded.
        assert!(parse(&["--network-magic=eca5d4"]).is_err());
        assert!(parse(&["--network-magic=eca5d40400"]).is_err());
        assert!(parse(&["--network-magic=nothex!!"]).is_err());
    }

    /// Presets share thresholds and even fork heights with each other, but
    /// never a datadir suffix: that suffix is the only thing keeping one
    /// preset's validator/wallet state out of another's, so a copy-pasted
    /// one would silently mix two chains' databases.
    #[test]
    fn every_preset_has_a_distinct_datadir_suffix() {
        let mut seen = std::collections::HashMap::new();
        for preset in NetworkPreset::value_variants() {
            let suffix = preset
                .params()
                .datadir_suffix
                .unwrap_or_else(|| panic!("{preset:?} must namespace its datadir"));
            if let Some(previous) = seen.insert(suffix, preset) {
                panic!("{preset:?} and {previous:?} both use datadir suffix `{suffix}`");
            }
        }
    }

    /// The wallet's replay limit decides whether a gap is crossed one block
    /// at a time or with a checkpoint and a full scan, and it is hidden, so
    /// nothing in an ordinary run would notice it drifting. The integration
    /// tests lower it to put gaps on both sides of it, which only works for
    /// as long as the flag keeps this spelling.
    #[test]
    fn block_by_block_replay_limit_is_overridable() {
        let parse = |args: &[&str]| {
            let mut argv = vec!["bip300301_enforcer"];
            argv.extend_from_slice(args);
            Config::try_parse_from(argv)
                .expect("should parse")
                .wallet_opts
                .max_block_by_block_replay
        };
        assert_eq!(parse(&[]), 2_000);
        assert_eq!(parse(&["--wallet-max-block-by-block-replay=50"]), 50);
    }

    /// The integration tests persist a wallet under one coin type and start
    /// the enforcer under another, which only works while the flag keeps
    /// this spelling.
    #[test]
    fn derivation_coin_type_is_overridable() {
        let parse = |args: &[&str]| {
            let mut argv = vec!["bip300301_enforcer"];
            argv.extend_from_slice(args);
            Config::try_parse_from(argv)
                .expect("should parse")
                .wallet_opts
                .derivation_coin_type
        };
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&["--wallet-derivation-coin-type=0"]), Some(0));
    }

    /// The annotation itself: typing a field `SecretString` is what makes the
    /// dump redact it, and typing it anything else is what makes the dump
    /// print it.
    #[test]
    fn secret_args_are_recognized_by_their_type() {
        let command = Config::command();
        let arg = |id: &str| {
            command
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .unwrap_or_else(|| panic!("no argument `{id}`"))
        };
        assert!(
            is_secret_arg(arg("pass")),
            "--node-rpc-pass is typed SecretString, so it must be recognized as secret",
        );
        assert!(
            !is_secret_arg(arg("user")),
            "--node-rpc-user is a plain String and must not be redacted",
        );
        // A path is not a secret; over-redacting one costs debuggability.
        assert!(!is_secret_arg(arg("mnemonic_path")));
        assert!(!is_secret_arg(arg("data_dir")));
    }

    /// A secret must not render its value through any of the usual escapes.
    #[test]
    fn secret_string_never_renders_its_value() {
        let secret = SecretString::new("hunter2");
        assert_eq!(format!("{secret:?}"), REDACTED);
        assert_eq!(format!("{secret}"), REDACTED);
        // Including when nested inside a derived `Debug`.
        assert!(!format!("{:?}", Some(secret.clone())).contains("hunter2"));
        assert_eq!(secret.expose(), "hunter2", "but it is still readable");
    }

    /// End to end over what actually gets written: the secret is masked, the
    /// non-secrets around it are not, and the plaintext appears nowhere.
    #[test]
    fn the_dump_masks_secrets_and_nothing_else() {
        let matches = Config::command().get_matches_from([
            "bip300301_enforcer",
            "--node-rpc-user=alice",
            "--node-rpc-pass=hunter2",
            "--wallet-esplora-url=https://bob:s3kr1t@esplora.example/api",
        ]);
        let lines = super::effective_config_lines(&matches);

        assert!(
            lines.contains(&format!("--node-rpc-pass={REDACTED} (command line)")),
            "expected a redacted password line, got: {lines:#?}"
        );
        assert!(lines.contains(&"--node-rpc-user=\"alice\" (command line)".to_owned()));
        assert!(
            lines.contains(
                &"--wallet-esplora-url=\"https://bob:redacted@esplora.example/api\" \
                  (command line)"
                    .to_owned()
            ),
            "URL credentials must be stripped, got: {lines:#?}"
        );

        let dump = lines.join("\n");
        assert!(!dump.contains("hunter2"), "the password leaked: {dump}");
        assert!(!dump.contains("s3kr1t"), "URL credentials leaked: {dump}");
    }

    /// Defaults and unset arguments are distinguishable in the dump, which is
    /// the point of reporting the value source.
    #[test]
    fn value_sources_distinguish_default_from_unset() {
        let matches = Config::command().get_matches_from(["bip300301_enforcer"]);

        assert_eq!(
            matches.value_source("sync_source"),
            Some(ValueSource::DefaultValue),
            "an argument with a default reports one",
        );
        assert_eq!(
            matches.value_source("pass"),
            None,
            "an unset optional argument reports no source",
        );
        assert!(
            matches.try_get_raw("pass").unwrap().is_none(),
            "an unset optional argument has no raw value, and renders as {UNSET}",
        );
        assert_eq!(REDACTED, "[redacted]");
    }

    /// Booleans reach the dump as raw values too, rather than vanishing.
    #[test]
    fn boolean_flags_have_raw_values() {
        let matches = Config::command().get_matches_from(["bip300301_enforcer", "--enable-wallet"]);
        let raw: Vec<_> = matches
            .try_get_raw("enable_wallet")
            .unwrap()
            .unwrap()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(raw, ["true"]);
    }

    /// `value_source` panics on an unknown id, so confirm the dump only ever
    /// asks about ids that really exist, for every argument, in one pass.
    #[test]
    fn dumping_the_config_does_not_panic() {
        let matches = Config::command().get_matches_from([
            "bip300301_enforcer",
            "--node-rpc-pass=hunter2",
            "--node-rpc-user=alice",
            "--enable-wallet",
        ]);
        super::log_effective_config(&matches);
    }

    #[test]
    fn url_credentials_are_stripped() {
        assert_eq!(
            redact_embedded_credentials("https://alice:hunter2@esplora.example/api").as_deref(),
            Some("https://alice:redacted@esplora.example/api"),
        );
        // Nothing to strip: caller keeps the value as-is.
        assert_eq!(
            redact_embedded_credentials("https://esplora.example/api"),
            None,
        );
        assert_eq!(redact_embedded_credentials("/not/a/url"), None);
    }

    /// An esplora endpoint may carry its API key in the userinfo slot with no
    /// password. There the lone component is the credential itself, not a
    /// principal worth keeping, so the whole of it goes.
    #[test]
    fn url_userinfo_without_password_is_a_credential() {
        assert_eq!(
            redact_embedded_credentials("https://SECRETTOKEN@esplora.example/api").as_deref(),
            Some("https://redacted@esplora.example/api"),
        );
    }

    /// Redaction must not swallow the parts that identify which backend the
    /// wallet is talking to -- those are the whole point of reporting it.
    #[test]
    fn redaction_preserves_the_endpoint_identity() {
        let redacted =
            redact_embedded_credentials("https://alice:hunter2@esplora.example:8443/api/v1")
                .expect("a password is present");
        assert_eq!(
            redacted, "https://alice:redacted@esplora.example:8443/api/v1",
            "scheme, username, host, port and path must all survive"
        );
    }
}
