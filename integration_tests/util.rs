use std::{
    ffi::{OsStr, OsString},
    future::Future,
    path::PathBuf,
};

use clap::Args;
use futures::TryFutureExt as _;
use temp_dir::TempDir;

#[derive(Args, Clone, Debug)]
pub struct BinPaths {
    #[arg(long)]
    pub bitcoind: PathBuf,
    #[arg(long)]
    pub bitcoin_cli: PathBuf,
    #[arg(long)]
    pub bitcoin_util: PathBuf,
    #[arg(long)]
    pub bip300301_enforcer: PathBuf,
    #[arg(long)]
    pub electrs: PathBuf,
    #[arg(long)]
    pub signet_miner: PathBuf,
}

pub struct AsyncTrial<Fut> {
    name: String,
    test: Fut,
}

impl<Fut> AsyncTrial<Fut> {
    pub fn new<Name>(name: Name, test: Fut) -> Self
    where
        Name: AsRef<str>,
    {
        Self {
            name: name.as_ref().to_owned(),
            test,
        }
    }

    // Run the trial on the provided runtime
    pub fn run_blocking<Err>(self, rt_handle: tokio::runtime::Handle) -> libtest_mimic::Trial
    where
        libtest_mimic::Failed: From<Err>,
        Fut: Future<Output = Result<(), Err>> + Send + 'static,
    {
        let span = tracing::info_span!("test", name = %self.name);
        libtest_mimic::Trial::test(self.name, move || {
            span.in_scope(|| {
                rt_handle.block_on(async { self.test.map_err(libtest_mimic::Failed::from).await })
            })
        })
    }
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
    pub signet_challenge: Option<bitcoin::ScriptBuf>,
    pub txindex: bool,
    pub zmq_sequence_port: u16,
}

impl Bitcoind {
    pub fn await_command_with_args<Env, Arg, Envs, Args>(
        &self,
        envs: Envs,
        args: Args,
    ) -> impl Future<Output = anyhow::Error> + 'static
    where
        Arg: AsRef<OsStr>,
        Env: AsRef<OsStr>,
        Envs: IntoIterator<Item = (Env, Env)>,
        Args: IntoIterator<Item = Arg>,
    {
        let mut default_args = vec![
            "-acceptnonstdtxn".to_owned(),
            format!("-chain={}", self.network.to_core_arg()),
            format!("-datadir={}", self.data_dir.display()),
            format!("-bind=127.0.0.1:{}", self.listen_port),
            format!("-rpcuser={}", self.rpc_user),
            format!("-rpcpassword={}", self.rpc_pass),
            format!("-rpcport={}", self.rpc_port),
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
        await_command_with_args(&self.data_dir, self.path.clone(), envs, args)
    }
}

/// Run command with args, dumping stderr/stdout to `dir`` on exit
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
        let stderr_file = match std::fs::File::create_new(stderr_fp.clone()) {
            Ok(stderr_file) => stderr_file,
            Err(err) => {
                let err = anyhow::Error::from(err);
                return anyhow::anyhow!(
                    "error creating stderr file for command `{command}`: `{err:#}`"
                );
            }
        };
        cmd.stderr(std::process::Stdio::from(stderr_file));
        let stdout_file = match std::fs::File::create_new(stdout_fp.clone()) {
            Ok(stdout_file) => stdout_file,
            Err(err) => {
                let err = anyhow::Error::from(err);
                return anyhow::anyhow!(
                    "error creating stdout file for command `{command}`: `{err:#}`"
                );
            }
        };
        cmd.stdout(std::process::Stdio::from(stdout_file));
        let mut cmd = match cmd.spawn() {
            Ok(cmd) => cmd,
            Err(err) => {
                let err = anyhow::Error::from(err);
                return anyhow::anyhow!("Spawning command {command} failed: `{err:#}`");
            }
        };
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

        let mut msg = format!("Command `{command}` finished: `{}`", exit_status,);

        if let Ok(stderr) = std::fs::read_to_string(stderr_fp).map(|s| s.trim().to_owned()) {
            if !stderr.is_empty() {
                msg.push_str(&format!("\nStderr:\n{}", stderr));
            }
        }
        anyhow::anyhow!(msg)
    }
}

#[derive(Clone, Debug)]
pub struct Electrs {
    pub path: PathBuf,
    pub db_dir: PathBuf,
    pub config: PathBuf,
    pub daemon_dir: PathBuf,
    pub daemon_p2p_port: u16,
    pub daemon_rpc_port: u16,
    pub electrum_rpc_port: u16,
    pub monitoring_port: u16,
    pub network: bitcoin::Network,
    pub signet_magic: Option<[u8; 4]>,
}

impl Electrs {
    pub fn await_command_with_args<Env, Arg, Envs, Args>(
        &self,
        envs: Envs,
        args: Args,
    ) -> impl Future<Output = anyhow::Error> + 'static
    where
        Arg: AsRef<OsStr>,
        Env: AsRef<OsStr>,
        Envs: IntoIterator<Item = (Env, Env)>,
        Args: IntoIterator<Item = Arg>,
    {
        let mut default_args = vec![
            "--db-dir".to_owned(),
            format!("{}", &self.db_dir.display()),
            "--daemon-dir".to_owned(),
            format!("{}", &self.daemon_dir.display()),
            "--daemon-p2p-addr".to_owned(),
            format!("127.0.0.1:{}", self.daemon_p2p_port),
            "--daemon-rpc-addr".to_owned(),
            format!("127.0.0.1:{}", self.daemon_rpc_port),
            "--electrum-rpc-addr".to_owned(),
            format!("127.0.0.1:{}", self.electrum_rpc_port),
            "--monitoring-addr".to_owned(),
            format!("127.0.0.1:{}", self.monitoring_port),
            "--network".to_owned(),
            self.network.to_core_arg().to_owned(),
            "--conf".to_owned(),
            format!("{}", &self.config.display()),
            "--log-filters".to_owned(),
            "\"DEBUG\"".to_owned(),
        ];
        if let Some(signet_magic) = self.signet_magic {
            default_args.push("--signet-magic".to_owned());
            default_args.push(hex::encode(signet_magic));
        }
        let args = default_args
            .into_iter()
            .map(OsString::from)
            .chain(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        await_command_with_args(&self.db_dir, self.path.clone(), envs, args)
    }
}

#[derive(Clone, Debug)]
pub struct Enforcer {
    pub path: PathBuf,
    pub data_dir: PathBuf,
    pub enable_mempool: bool,
    pub node_rpc_user: String,
    pub node_rpc_pass: String,
    pub node_rpc_port: u16,
    pub node_zmq_sequence_port: u16,
    pub serve_grpc_port: u16,
    pub serve_rpc_port: u16,
    pub wallet_electrum_port: u16,
}

impl Enforcer {
    pub fn await_command_with_args<Env, Arg, Envs, Args>(
        &self,
        envs: Envs,
        args: Args,
    ) -> impl Future<Output = anyhow::Error> + 'static
    where
        Arg: AsRef<OsStr>,
        Env: AsRef<OsStr>,
        Envs: IntoIterator<Item = (Env, Env)>,
        Args: IntoIterator<Item = Arg>,
    {
        let mut default_args = vec![
            "--data-dir".to_owned(),
            format!("{}", self.data_dir.display()),
            "--node-rpc-addr".to_owned(),
            format!("127.0.0.1:{}", self.node_rpc_port),
            "--node-rpc-user".to_owned(),
            self.node_rpc_user.clone(),
            "--node-rpc-pass".to_owned(),
            self.node_rpc_pass.clone(),
            "--node-zmq-addr-sequence".to_owned(),
            format!("tcp://127.0.0.1:{}", self.node_zmq_sequence_port),
            "--enable-wallet".to_owned(),
            "--log-level".to_owned(),
            "trace".to_owned(),
            "--serve-grpc-addr".to_owned(),
            format!("127.0.0.1:{}", self.serve_grpc_port),
            "--serve-rpc-addr".to_owned(),
            format!("127.0.0.1:{}", self.serve_rpc_port),
            "--wallet-electrum-host".to_owned(),
            "127.0.0.1".to_owned(),
            "--wallet-electrum-port".to_owned(),
            self.wallet_electrum_port.to_string(),
            "--wallet-auto-create".to_owned(),
        ];
        if self.enable_mempool {
            default_args.push("--enable-mempool".to_owned());
        }
        let args = default_args
            .into_iter()
            .map(OsString::from)
            .chain(args.into_iter().map(|arg| arg.as_ref().to_owned()));
        await_command_with_args(&self.data_dir, self.path.clone(), envs, args)
    }
}
