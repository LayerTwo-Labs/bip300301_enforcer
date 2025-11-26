use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    future::Future,
    path::PathBuf,
    sync::Arc,
};

use futures::TryFutureExt as _;
use thiserror::Error;
use tokio::task::JoinHandle;

/// Information about a failed test
#[derive(Clone, Debug)]
pub struct TestFailure {
    pub test_name: String,
    pub error_message: String,
    pub output_files: Vec<FileWithConfig>,
}

/// Global collector for test failures
#[derive(Clone, Debug, Default)]
pub struct TestFailureCollector {
    failures: Arc<parking_lot::Mutex<Vec<TestFailure>>>,
}

impl TestFailureCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a test failure to the collection
    pub fn add_failure(&self, failure: TestFailure) {
        let mut failures = self.failures.lock();
        failures.push(failure);
    }

    /// Get all collected failures
    pub fn get_failures(&self) -> Vec<TestFailure> {
        let failures = self.failures.lock();
        failures.clone()
    }

    /// Format and display all collected failures
    #[allow(clippy::print_stderr)]
    pub fn display_all_failures(&self) {
        let failures = self.get_failures();
        if failures.is_empty() {
            return;
        }

        eprintln!("\n{}", "=".repeat(80));
        eprintln!("TEST FAILURE SUMMARY ({} failed tests)", failures.len());
        eprintln!("{}", "=".repeat(80));

        for (i, failure) in failures.iter().enumerate() {
            eprintln!("\n[{}] Test: {}", i + 1, failure.test_name);
            eprintln!("Error: {}", failure.error_message);

            if !failure.output_files.is_empty() {
                let output = format_test_output_files(&failure.test_name, &failure.output_files);
                if !output.is_empty() {
                    eprintln!("{output}");
                }
            }

            if i < failures.len() - 1 {
                eprintln!("{}", "-".repeat(40));
            }
        }

        eprintln!("\n{}", "=".repeat(80));
        eprintln!("END OF FAILURE SUMMARY");
        eprintln!("{}", "=".repeat(80));
    }
}

#[derive(Debug, Error)]
#[error("Error resolving environment variable (`{key}`): {err:#}")]
pub struct VarError {
    key: String,
    err: dotenvy::Error,
}

impl VarError {
    pub fn new(key: impl std::fmt::Display, err: dotenvy::Error) -> Self {
        Self {
            key: key.to_string(),
            err,
        }
    }
}

/// Fetches the environment variable key from the current process
pub fn get_env_var<K: AsRef<OsStr>>(key: K) -> Result<String, VarError> {
    dotenvy::var(&key).map_err(|err| VarError::new(key.as_ref().to_string_lossy(), err))
}

pub fn get_env_var_or<K: AsRef<OsStr>>(key: K, default: &str) -> Result<String, VarError> {
    match get_env_var(&key) {
        Ok(val) => Ok(val),
        Err(VarError {
            err: dotenvy::Error::EnvVar(std::env::VarError::NotPresent),
            ..
        }) => Ok(default.to_string()),
        Err(err) => Err(err),
    }
}

#[derive(Clone, Debug)]
pub struct BinPaths {
    pub bitcoind: PathBuf,
    pub bitcoin_cli: PathBuf,
    pub bitcoin_util: PathBuf,
    pub bip300301_enforcer: PathBuf,
    pub electrs: PathBuf,
    pub signet_miner: PathBuf,
}

impl BinPaths {
    /// Read from environment variables
    pub fn from_env() -> Result<Self, VarError> {
        Ok(Self {
            bitcoind: get_env_var("BITCOIND")?.into(),
            bitcoin_cli: get_env_var("BITCOIN_CLI")?.into(),
            bitcoin_util: get_env_var("BITCOIN_UTIL")?.into(),
            bip300301_enforcer: get_env_var_or(
                "BIP300301_ENFORCER",
                "./target/debug/bip300301_enforcer",
            )?
            .into(),
            electrs: get_env_var("ELECTRS")?.into(),
            signet_miner: get_env_var("SIGNET_MINER")?.into(),
        })
    }
}

/// Configuration for individual file dumping behavior
#[derive(Clone, Debug)]
pub struct FileDumpConfig {
    /// Number of lines to show from the end of large files (default: 100)
    pub lines: usize,
    /// Whether to show this file on test failure (default: true)
    pub show_on_failure: bool,
    /// Optional label for the file
    pub label: Option<String>,
}

impl Default for FileDumpConfig {
    fn default() -> Self {
        Self {
            lines: 10,
            show_on_failure: true,
            label: None,
        }
    }
}

impl FileDumpConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_lines(mut self, lines: usize) -> Self {
        self.lines = lines;
        self
    }

    pub fn with_label<S: Into<String>>(mut self, label: S) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn hide_on_failure(mut self) -> Self {
        self.show_on_failure = false;
        self
    }
}

/// A file with its associated dump configuration
#[derive(Clone, Debug)]
pub struct FileWithConfig {
    pub path: PathBuf,
    pub config: FileDumpConfig,
}

/// Registry to track stdout/stderr files for active tests
#[derive(Clone, Debug, Default)]
pub struct TestFileRegistry {
    inner: Arc<parking_lot::Mutex<HashMap<String, Vec<FileWithConfig>>>>,
}

impl TestFileRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a single file with its configuration for a test
    pub fn register_file<P: Into<PathBuf>>(
        &self,
        test_name: &str,
        path: P,
        config: FileDumpConfig,
    ) {
        let mut registry = self.inner.lock();
        let file_with_config = FileWithConfig {
            path: path.into(),
            config,
        };
        registry
            .entry(test_name.to_string())
            .or_default()
            .push(file_with_config);
    }

    /// Get all registered files with their configurations for a test
    fn get_files(&self, test_name: &str) -> Vec<FileWithConfig> {
        let registry = self.inner.lock();
        registry.get(test_name).cloned().unwrap_or_default()
    }
}

pub struct AsyncTrial<Fut> {
    name: String,
    test: Fut,
    file_registry: TestFileRegistry,
    failure_collector: TestFailureCollector,
}

impl<Fut> AsyncTrial<Fut> {
    pub fn new<Name>(
        name: Name,
        test: Fut,
        file_registry: TestFileRegistry,
        failure_collector: TestFailureCollector,
    ) -> Self
    where
        Name: AsRef<str>,
    {
        Self {
            name: name.as_ref().to_owned(),
            test,
            file_registry,
            failure_collector,
        }
    }

    // Run the trial on the provided runtime
    pub fn run_blocking<Err>(self, rt_handle: tokio::runtime::Handle) -> libtest_mimic::Trial
    where
        libtest_mimic::Failed: From<Err>,
        Fut: Future<Output = Result<(), Err>> + Send + 'static,
        Err: std::fmt::Display,
    {
        let span = tracing::info_span!("test", name = %self.name);
        let test_name = self.name.clone();
        let file_registry = self.file_registry;
        let failure_collector = self.failure_collector;

        libtest_mimic::Trial::test(self.name, move || {
            span.in_scope(|| {
                // Use spawn_blocking to avoid nested runtime issues
                std::thread::spawn(move || {
                    rt_handle.block_on(async {
                        let result = self.test.map_err(libtest_mimic::Failed::from).await;

                        // Collect failure information instead of immediately printing
                        if result.is_err() {
                            let files = file_registry.get_files(&test_name);

                            let error_message = match &result {
                                Err(failed) => format!("{failed:?}"),
                                Ok(_) => "Unknown error".to_string(),
                            };

                            let failure = TestFailure {
                                test_name: test_name.clone(),
                                error_message,
                                output_files: files,
                            };

                            failure_collector.add_failure(failure);
                        }
                        result
                    })
                })
                .join()
                .unwrap()
            })
        })
    }
}

/// Read file contents with optional tail functionality for large files
pub fn read_file_with_tail(
    path: &std::path::Path,
    config: &FileDumpConfig,
) -> Result<String, std::io::Error> {
    // Read last N lines
    let content = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();

    if lines.len() <= config.lines {
        Ok(content)
    } else {
        let tail_start = lines.len() - config.lines;
        let tail_content = lines[tail_start..].join("\n");
        Ok(format!(
            "... [showing last {} lines of {} total lines] ...\n{}",
            config.lines,
            lines.len(),
            tail_content
        ))
    }
}

/// Format and display stdout/stderr files for a test
pub fn format_test_output_files(test_name: &str, files: &[FileWithConfig]) -> String {
    if files.is_empty() {
        return String::new();
    }

    let mut output = format!("\n=== Test '{test_name}' Output Files ===");

    for file in files {
        // Skip files that are configured to be hidden
        if !file.config.show_on_failure {
            continue;
        }

        let mut file_with_config_label = file.path.display().to_string();

        if let Some(label) = &file.config.label {
            file_with_config_label = format!("{label} ({file_with_config_label})");
        }

        match read_file_with_tail(&file.path, &file.config) {
            Ok(content) if !content.trim().is_empty() => {
                // Only show the label if the content is not empty
                output.push_str(&format!("\n--- {file_with_config_label} ---\n"));

                output.push_str(&content);
                if !content.ends_with('\n') {
                    output.push('\n');
                }

                // Include the label in the end marker as well!
                output.push_str(&format!("--- End {file_with_config_label} ---\n"));
            }
            Ok(_) => {}
            Err(err) => {
                output.push_str(&format!(
                    "Error reading file `{path}`: {err}\n",
                    path = file.path.display()
                ));
            }
        }
    }

    output
}

/// Wrapper around `JoinHandle` that aborts the task on drop
#[derive(Debug)]
#[repr(transparent)]
pub struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> AbortOnDrop<T> {
    // TODO: is this OK? Claude dreamed it up!
    pub fn into_inner(self) -> JoinHandle<T> {
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: We wrapped self in ManuallyDrop, so Drop won't run
        unsafe { std::ptr::read(&this.0) }
    }
}

impl<T> std::ops::Deref for AbortOnDrop<T> {
    type Target = JoinHandle<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort()
    }
}

impl<T> From<JoinHandle<T>> for AbortOnDrop<T> {
    fn from(task: JoinHandle<T>) -> Self {
        Self(task)
    }
}

fn open_file_or_create_new(path: &std::path::Path) -> Result<std::fs::File, std::io::Error> {
    std::fs::File::create_new(path).or_else(|err| {
        if err.kind() == std::io::ErrorKind::AlreadyExists {
            std::fs::File::open(path)
        } else {
            Err(err)
        }
    })
}

/// Run command with args, dumping stderr/stdout to `dir` on exit
pub fn await_command_with_args<Cmd, Env, Arg, Envs, Args>(
    dir: &std::path::Path,
    command: Cmd,
    envs: Envs,
    args: Args,
) -> impl Future<Output = anyhow::Error> + 'static
where
    Cmd: AsRef<OsStr>,
    Arg: AsRef<OsStr>,
    Env: AsRef<OsStr>,
    Envs: IntoIterator<Item = (Env, Env)>,
    Args: IntoIterator<Item = Arg>,
{
    let mut cmd = tokio::process::Command::new(command.as_ref());
    cmd.envs(envs);
    cmd.args(args);
    cmd.kill_on_drop(true);
    let command: String = command.as_ref().to_string_lossy().to_string();
    let stderr_fp = dir.join("stderr.txt");
    let stdout_fp = dir.join("stdout.txt");
    async move {
        let stderr_file = match open_file_or_create_new(&stderr_fp) {
            Ok(stderr_file) => stderr_file,
            Err(err) => {
                let err = anyhow::Error::from(err);
                return anyhow::anyhow!(
                    "error creating stderr file for command `{command}`: `{err:#}`"
                );
            }
        };
        cmd.stderr(std::process::Stdio::from(stderr_file));
        let stdout_file = match open_file_or_create_new(&stdout_fp) {
            Ok(stdout_file) => stdout_file,
            Err(err) => {
                let err = anyhow::Error::from(err);
                return anyhow::anyhow!(
                    "error creating stdout file for command `{command}`: `{err:#}`"
                );
            }
        };
        cmd.stdout(std::process::Stdio::from(stdout_file));
        tracing::debug!(
            stdout_file = stdout_fp.to_string_lossy().to_string(),
            stderr_file = stderr_fp.to_string_lossy().to_string(),
            "Running command: {cmd:?}",
        );
        let mut cmd = match cmd.spawn() {
            Ok(cmd) => cmd,
            Err(err) => {
                let err = anyhow::Error::from(err);
                return anyhow::anyhow!("Spawning command {command} failed: `{err:#}`");
            }
        };

        tracing::debug!("Waiting for `{command}` to finish");
        let exit_status = match cmd.wait().await {
            Ok(exit_status) => exit_status,
            Err(err) => {
                let err = anyhow::Error::from(err);
                return anyhow::anyhow!("Command {command} failed: `{err:#}`",);
            }
        };
        tracing::error!(
            message = format!(
                "Command `{command}` exited with status `{}`!",
                exit_status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or("unknown".to_owned())
            ),
            stdout_file = stdout_fp.to_string_lossy().to_string(),
            stderr_file = stderr_fp.to_string_lossy().to_string(),
        );

        let mut msg = format!("Command `{command}` finished: `{exit_status}`");

        if let Ok(stderr) = std::fs::read_to_string(stderr_fp).map(|s| s.trim().to_owned())
            && !stderr.is_empty()
        {
            msg.push_str(&format!("\nStderr:\n{stderr}"));
        }
        anyhow::anyhow!(msg)
    }
}

/// Spawn a task that awaits command with args,
/// dumping stderr/stdout to `dir` on exit, and handling errors via the
/// provided function.
pub fn spawn_command_with_args<Cmd, Env, Arg, Envs, Args, F>(
    dir: &std::path::Path,
    command: Cmd,
    envs: Envs,
    args: Args,
    err_handler: F,
) -> AbortOnDrop<()>
where
    Cmd: AsRef<OsStr>,
    Arg: AsRef<OsStr>,
    Env: AsRef<OsStr>,
    Envs: IntoIterator<Item = (Env, Env)>,
    Args: IntoIterator<Item = Arg>,
    F: FnOnce(anyhow::Error) + Send + 'static,
{
    let fut = await_command_with_args(dir, command, envs, args);
    tokio::task::spawn(async {
        use tracing::Instrument as _;
        let err = fut.in_current_span().await;
        tracing::error!("Command failed with error: {err:#}");
        err_handler(err)
    })
    .into()
}

#[derive(Clone, Debug)]
pub struct Bitcoind {
    pub path: PathBuf,
    pub data_dir: PathBuf,
    pub listen_port: u16,
    pub network: bitcoin::Network,
    // Ports to listen on tor network, and control tor.
    // If set to None, listening on tor is disabled
    pub onion_ports: Option<(u16, u16)>,
    pub rpc_user: String,
    pub rpc_pass: String,
    pub rpc_port: u16,
    pub rpc_host: String,
    pub signet_challenge: Option<bitcoin::ScriptBuf>,
    pub txindex: bool,
    pub zmq_sequence_port: u16,
}

impl Bitcoind {
    #[must_use]
    pub fn spawn_command_with_args<Env, Arg, Envs, Args, F>(
        &self,
        envs: Envs,
        args: Args,
        err_handler: F,
    ) -> AbortOnDrop<()>
    where
        Arg: AsRef<OsStr>,
        Env: AsRef<OsStr>,
        Envs: IntoIterator<Item = (Env, Env)>,
        Args: IntoIterator<Item = Arg>,
        F: FnOnce(anyhow::Error) + Send + 'static,
    {
        let mut default_args = vec![
            "-acceptnonstdtxn".to_owned(),
            format!("-chain={}", self.network.to_core_arg()),
            format!("-datadir={}", self.data_dir.display()),
            format!("-bind=127.0.0.1:{}", self.listen_port),
            format!("-rpcuser={}", self.rpc_user),
            format!("-rpcpassword={}", self.rpc_pass),
            format!("-rpcport={}", self.rpc_port),
            "-rest".to_owned(), // needed for batch requesting block headers
            "-server".to_owned(),
            format!("-zmqpubsequence=tcp://127.0.0.1:{}", self.zmq_sequence_port),
        ];
        match self.onion_ports {
            Some((listen_port, control_port)) => {
                default_args.push(format!("-bind=127.0.0.1:{listen_port}=onion"));
                default_args.push(format!("-torcontrol=127.0.0.1:{control_port}"));
            }
            None => {
                default_args.push("-listenonion=0".to_owned());
            }
        }
        if self.txindex {
            default_args.push("-txindex".to_owned());
        }
        if let (bitcoin::Network::Signet, Some(signet_challenge)) =
            (self.network, &self.signet_challenge)
        {
            let signet_challenge = hex::encode(signet_challenge.as_bytes());
            default_args.push(format!("-signetchallenge={signet_challenge}"))
        }
        let args = default_args
            .into_iter()
            .map(OsString::from)
            .chain(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        spawn_command_with_args(&self.data_dir, self.path.clone(), envs, args, err_handler)
    }
}

#[derive(Clone, Debug)]
pub struct Electrs {
    pub path: PathBuf,
    pub db_dir: PathBuf,
    pub daemon_dir: PathBuf,
    pub auth: (String, String), // username + password
    pub daemon_rpc_port: u16,
    pub electrum_rpc_port: u16,
    pub electrum_http_port: u16,
    pub monitoring_port: u16,
    pub network: bitcoin::Network,
    pub signet_magic: Option<bitcoin::p2p::Magic>,
}

impl Electrs {
    pub fn spawn_command_with_args<Env, Arg, Envs, Args, F>(
        &self,
        envs: Envs,
        args: Args,
        err_handler: F,
    ) -> AbortOnDrop<()>
    where
        Arg: AsRef<OsStr>,
        Env: AsRef<OsStr>,
        Envs: IntoIterator<Item = (Env, Env)>,
        Args: IntoIterator<Item = Arg>,
        F: FnOnce(anyhow::Error) + Send + 'static,
    {
        let mut default_args = vec![
            "-vv".to_owned(), // add more v's for even more verbosity
            format!("--db-dir={}", &self.db_dir.display()),
            format!("--daemon-dir={}", &self.daemon_dir.display()),
            format!("--daemon-rpc-addr=127.0.0.1:{}", self.daemon_rpc_port),
            format!("--electrum-rpc-addr=127.0.0.1:{}", self.electrum_rpc_port),
            format!("--http-addr=127.0.0.1:{}", self.electrum_http_port),
            format!("--monitoring-addr=127.0.0.1:{}", self.monitoring_port),
            format!("--network={}", self.network.to_core_arg()),
            format!("--cookie={}:{}", self.auth.0, self.auth.1),
            "--jsonrpc-import".to_owned(),
        ];
        if let Some(signet_magic) = self.signet_magic {
            default_args.push("--magic".to_owned());
            default_args.push(hex::encode(signet_magic));
        }
        let args = default_args
            .into_iter()
            .map(OsString::from)
            .chain(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        spawn_command_with_args(&self.db_dir, self.path.clone(), envs, args, err_handler)
    }
}

#[derive(Clone, Debug)]
pub struct Enforcer {
    pub path: PathBuf,
    pub data_dir: PathBuf,
    pub enable_mempool: bool,
    pub enable_wallet: bool,
    pub node_blocks_dir: Option<PathBuf>,
    pub node_rpc_user: String,
    pub node_rpc_pass: String,
    pub node_rpc_port: u16,
    pub node_zmq_sequence_port: u16,
    pub serve_grpc_port: u16,
    pub serve_json_rpc_port: u16,
    pub serve_rpc_port: u16,
    pub wallet_electrum_rpc_port: u16,
    pub wallet_electrum_http_port: u16,
}

impl Enforcer {
    #[must_use]
    pub fn spawn_command_with_args<Env, Arg, Envs, Args, F>(
        &self,
        envs: Envs,
        args: Args,
        err_handler: F,
    ) -> AbortOnDrop<()>
    where
        Arg: AsRef<OsStr>,
        Env: AsRef<OsStr>,
        Envs: IntoIterator<Item = (Env, Env)>,
        Args: IntoIterator<Item = Arg>,
        F: FnOnce(anyhow::Error) + Send + 'static,
    {
        let mut default_args = vec![
            format!("--data-dir={}", self.data_dir.display()),
            format!("--node-rpc-addr=127.0.0.1:{}", self.node_rpc_port),
            format!("--node-rpc-user={}", self.node_rpc_user),
            format!("--node-rpc-pass={}", self.node_rpc_pass),
            "--log-level=trace".to_owned(),
            format!("--serve-grpc-addr=127.0.0.1:{}", self.serve_grpc_port),
            format!(
                "--serve-json-rpc-addr=127.0.0.1:{}",
                self.serve_json_rpc_port
            ),
            format!("--serve-rpc-addr=127.0.0.1:{}", self.serve_rpc_port),
        ];

        if let Some(node_blocks_dir) = &self.node_blocks_dir {
            default_args.push(format!("--node-blocks-dir={}", node_blocks_dir.display()));
        }

        if self.enable_wallet {
            default_args.extend(vec![
                "--enable-wallet".to_owned(),
                "--wallet-auto-create".to_owned(),
                format!("--wallet-electrum-host=127.0.0.1"),
                format!("--wallet-electrum-port={}", self.wallet_electrum_rpc_port),
                format!(
                    "--wallet-esplora-url=http://127.0.0.1:{}",
                    self.wallet_electrum_http_port
                ),
                "--wallet-skip-periodic-sync".to_owned(),
            ]);
        }

        if self.enable_mempool {
            default_args.push("--enable-mempool".to_owned());
        }
        let args = default_args
            .into_iter()
            .map(OsString::from)
            .chain(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        spawn_command_with_args(&self.data_dir, self.path.clone(), envs, args, err_handler)
    }
}
