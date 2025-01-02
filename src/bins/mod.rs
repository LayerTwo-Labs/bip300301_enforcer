use std::{
    ffi::{OsStr, OsString},
    future::Future,
    path::PathBuf,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct BitcoinCli {
    pub path: PathBuf,
    pub network: bitcoin::Network,
    pub rpc_user: String,
    pub rpc_pass: String,
    pub rpc_port: u16,
    pub rpc_wallet: Option<String>,
}

impl BitcoinCli {
    fn default_args(&self) -> Vec<String> {
        let mut res = vec![
            format!("-chain={}", self.network.to_core_arg()),
            format!("-rpcuser={}", self.rpc_user),
            format!("-rpcpassword={}", self.rpc_pass),
            format!("-rpcport={}", self.rpc_port),
        ];
        if let Some(rpc_wallet) = &self.rpc_wallet {
            res.push(format!("-rpcwallet={rpc_wallet}"))
        }
        res
    }

    pub fn command<CmdArg, Subcommand, SubcommandArg, CmdArgs, SubcommandArgs>(
        &self,
        command_args: CmdArgs,
        subcommand: Subcommand,
        subcommand_args: SubcommandArgs,
    ) -> tokio::process::Command
    where
        CmdArg: AsRef<OsStr>,
        Subcommand: AsRef<OsStr>,
        SubcommandArg: AsRef<OsStr>,
        CmdArgs: IntoIterator<Item = CmdArg>,
        SubcommandArgs: IntoIterator<Item = SubcommandArg>,
    {
        let mut command = tokio::process::Command::new(&self.path);
        command.args(self.default_args());
        command.args(command_args);
        command.arg(subcommand);
        command.args(subcommand_args);
        command
    }

    /// Display without chain argument.
    /// Required by signet miner
    pub fn display_without_chain(&self) -> String {
        let mut command_fragments = vec![format!("{}", self.path.display())];
        command_fragments.extend(
            self.default_args()
                .into_iter()
                .filter(|arg| !arg.starts_with("-chain=")),
        );
        command_fragments.join(" ")
    }
}

#[derive(Clone, Debug)]
pub struct BitcoinUtil {
    pub path: PathBuf,
    pub network: bitcoin::Network,
}

impl BitcoinUtil {
    pub fn command<CmdArg, Subcommand, SubcommandArg, CmdArgs, SubcommandArgs>(
        &self,
        command_args: CmdArgs,
        subcommand: Subcommand,
        subcommand_args: SubcommandArgs,
    ) -> tokio::process::Command
    where
        CmdArg: AsRef<OsStr>,
        Subcommand: AsRef<OsStr>,
        SubcommandArg: AsRef<OsStr>,
        CmdArgs: IntoIterator<Item = CmdArg>,
        SubcommandArgs: IntoIterator<Item = SubcommandArg>,
    {
        let mut command = tokio::process::Command::new(&self.path);
        command.arg(format!("-chain={}", self.network.to_core_arg()));
        command.args(command_args);
        command.arg(subcommand);
        command.args(subcommand_args);
        command
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

#[derive(Debug, Clone)]
pub struct SignetMiner {
    /// Path to the Python mining script from Bitcoin Core.
    pub path: PathBuf,

    /// Command to use for executing `bitcoin-cli`. If not set,
    /// we'll simply use `bitcoin-cli`.
    pub bitcoin_cli: Option<String>,

    /// Path to `bitcoin-util` command.
    pub bitcoin_util: Option<PathBuf>,

    pub block_interval: Option<Duration>,

    /// If set to None, we'll pass `--min-nbits` to the mining script
    pub nbits: Option<[u8; 4]>,
    pub getblocktemplate_command: Option<String>,
    /// Only used with custom getblocktemplate command
    pub coinbasetxn: bool,

    // Enable debug mode when running the miner
    pub debug: bool,
}

impl SignetMiner {
    pub fn command(&self, subcommand: &str, subcommand_args: Vec<&str>) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.path);
        command.arg(format!(
            "--cli={}",
            self.bitcoin_cli
                .clone()
                .unwrap_or("bitcoin-cli".to_string())
        ));

        // Unless debug is explicitly set, we want to run in quiet mode. Otherwise
        // we'll get lots of error logs about stderr not being empty.
        command.arg(if self.debug { "--debug" } else { "--quiet" });

        let generate = subcommand == "generate";
        command.arg(subcommand);
        command.arg(format!(
            "--grind-cmd={} grind",
            self.bitcoin_util
                .as_ref()
                .map(|cmd| cmd.display().to_string())
                .unwrap_or("bitcoin-util".to_string())
        ));
        if generate {
            if let Some(block_interval) = self.block_interval {
                command.arg(format!("--block-interval={}", block_interval.as_secs_f32()));
            }
            if let Some(nbits) = self.nbits {
                command.arg(format!("--nbits={}", hex::encode(nbits)));
            } else {
                command.arg("--min-nbits");
            }
            if let Some(getblocktemplate_command) = &self.getblocktemplate_command {
                command.arg(format!(
                    "--getblocktemplate-command={getblocktemplate_command}"
                ));
                if self.coinbasetxn {
                    command.arg("--coinbasetxn");
                }
            }
        }
        command.args(subcommand_args);
        command
    }
}
