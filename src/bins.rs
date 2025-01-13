use std::{ffi::OsStr, future::Future, path::PathBuf, time::Duration};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CommandError {
    #[error(transparent)]
    FromUtf8(#[from] std::string::FromUtf8Error),
    #[error("{}", match std::str::from_utf8(.0) {
        Ok(err_msg) => format!("Command failed with error: `{err_msg}`"),
        Err(_) => {
            let stderr_hex = hex::encode(.0);
            format!("Command failed with stderr hex: `{stderr_hex}`")
        }
    })]
    Stderr(Vec<u8>),
    #[error(transparent)]
    Unknown(#[from] std::io::Error),
}

pub trait CommandExt {
    fn run(&mut self) -> impl Future<Output = Result<Vec<u8>, CommandError>> + Send;

    // capture as utf8
    fn run_utf8(&mut self) -> impl Future<Output = Result<String, CommandError>> + Send {
        let fut = self.run();
        async {
            let bytes = fut.await?;
            let mut res = String::from_utf8(bytes)?;
            res = res.trim().to_owned();
            Ok(res)
        }
    }
}

impl CommandExt for tokio::process::Command {
    async fn run(&mut self) -> Result<Vec<u8>, CommandError> {
        let output = self.output().await?;
        if output.status.success() {
            if !output.stderr.is_empty() {
                let stderr = match String::from_utf8(output.stderr) {
                    Ok(err_msgs) => err_msgs,
                    Err(err) => hex::encode(err.into_bytes()),
                };
                tracing::warn!("Command ran successfully, but stderr was not empty: `{stderr}`")
            }
            Ok(output.stdout)
        } else {
            Err(CommandError::Stderr(output.stderr))
        }
    }
}

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

#[derive(Debug, Clone)]
pub struct SignetMiner {
    /// Path to the Python mining script from Bitcoin Core.
    pub path: PathBuf,
    /// Command to use for executing `bitcoin-cli`.
    pub bitcoin_cli: BitcoinCli,
    /// Path to `bitcoin-util` command.
    pub bitcoin_util: PathBuf,
    pub block_interval: Option<Duration>,
    /// If None, pass `--min-nbits` to the mining script
    pub nbits: Option<[u8; 4]>,
    pub getblocktemplate_command: Option<String>,
    /// Only used with custom mining script. Enables support for coinbasetxn
    pub coinbasetxn: bool,
    /// Enable debug mode when running the miner
    pub debug: bool,
}

impl SignetMiner {
    pub fn command(&self, subcommand: &str, subcommand_args: Vec<&str>) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.path);
        command.arg(format!(
            "--cli={}",
            self.bitcoin_cli.display_without_chain()
        ));

        // Unless debug is explicitly set, we want to run in quiet mode. Otherwise
        // we'll get lots of error logs about stderr not being empty.
        command.arg(if self.debug { "--debug" } else { "--quiet" });

        let generate = subcommand == "generate";
        command.arg(subcommand);
        command.arg(format!("--grind-cmd={} grind", self.bitcoin_util.display()));
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
