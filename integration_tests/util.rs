use std::{
    ffi::{OsStr, OsString},
    future::Future,
    path::PathBuf,
};

use bip300301_enforcer_lib::bins;
use clap::Args;
use futures::TryFutureExt as _;

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

pub trait CommandExt {
    async fn run(&mut self) -> anyhow::Result<Vec<u8>>;

    // capture as utf8
    async fn run_utf8(&mut self) -> anyhow::Result<String> {
        let bytes = self.run().await?;
        let mut res = String::from_utf8(bytes)?;
        res = res.trim().to_owned();
        Ok(res)
    }
}

impl CommandExt for tokio::process::Command {
    async fn run(&mut self) -> anyhow::Result<Vec<u8>> {
        let output = self.output().await?;
        if output.status.success() {
            if !output.stderr.is_empty() {
                let stderr = match String::from_utf8(output.stderr) {
                    Ok(err_msgs) => err_msgs,
                    Err(err) => hex::encode(err.into_bytes()),
                };
                tracing::warn!("Command ran successfully, but stderr was not empty: `{stderr}`")
            }
            return Ok(output.stdout);
        }
        match String::from_utf8(output.stderr) {
            Ok(err_msg) => Err(anyhow::anyhow!("Command failed with error: `{err_msg}`")),
            Err(err) => {
                let stderr_hex = hex::encode(err.into_bytes());
                Err(anyhow::anyhow!(
                    "Command failed with stderr hex: `{stderr_hex}`"
                ))
            }
        }
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
    pub fn spawn_command_with_args<Env, Arg, Envs, Args, F>(
        &self,
        envs: Envs,
        args: Args,
        err_handler: F,
    ) -> bins::AbortOnDrop<()>
    where
        Arg: AsRef<OsStr>,
        Env: AsRef<OsStr>,
        Envs: IntoIterator<Item = (Env, Env)>,
        Args: IntoIterator<Item = Arg>,
        F: FnOnce(anyhow::Error) + Send + 'static,
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
        bins::spawn_command_with_args(&self.db_dir, self.path.clone(), envs, args, err_handler)
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
    pub fn spawn_command_with_args<Env, Arg, Envs, Args, F>(
        &self,
        envs: Envs,
        args: Args,
        err_handler: F,
    ) -> bins::AbortOnDrop<()>
    where
        Arg: AsRef<OsStr>,
        Env: AsRef<OsStr>,
        Envs: IntoIterator<Item = (Env, Env)>,
        Args: IntoIterator<Item = Arg>,
        F: FnOnce(anyhow::Error) + Send + 'static,
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
        bins::spawn_command_with_args(&self.data_dir, self.path.clone(), envs, args, err_handler)
    }
}
