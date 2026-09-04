use std::{ffi::OsStr, future::Future, path::PathBuf, time::Duration};

use thiserror::Error;

use crate::proto::{StatusBuilder, ToStatus};

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

impl ToStatus for CommandError {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::FromUtf8(err) => StatusBuilder::new(err),
            Self::Stderr(_) => StatusBuilder::new(self),
            Self::Unknown(err) => StatusBuilder::new(err),
        }
    }
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
    pub rpc_user: Option<String>,
    /// Redacted by `Debug`, so a logged `BitcoinCli` cannot leak it.
    pub rpc_pass: Option<crate::cli::SecretString>,
    pub rpc_cookie_path: Option<String>,
    pub rpc_port: u16,
    pub rpc_host: String,
    pub rpc_wallet: Option<String>,
}

impl BitcoinCli {
    /// `rpc_pass` renders the `-rpcpassword` argument, and is `None` when
    /// there is no password to pass. Taking it as an argument is what lets a
    /// redacted rendering be built from the very same code as the argv that
    /// runs, so the two cannot drift apart.
    fn default_args_with_rpc_pass(&self, rpc_pass: Option<&str>) -> Vec<String> {
        let mut res = vec![
            format!("-chain={}", self.network.to_core_arg()),
            format!("-rpcport={}", self.rpc_port),
            format!("-rpcconnect={}", self.rpc_host),
        ];

        if let Some(rpc_cookie_path) = &self.rpc_cookie_path {
            res.push(format!("-rpccookiefile={rpc_cookie_path}"));
        }

        if let Some(rpc_user) = &self.rpc_user {
            res.push(format!("-rpcuser={rpc_user}"));
        }

        if let Some(rpc_pass) = rpc_pass {
            res.push(format!("-rpcpassword={rpc_pass}"));
        }

        if let Some(rpc_wallet) = &self.rpc_wallet {
            res.push(format!("-rpcwallet={rpc_wallet}"))
        }
        res
    }

    fn default_args(&self) -> Vec<String> {
        self.default_args_with_rpc_pass(self.rpc_pass.as_ref().map(|pass| pass.expose()))
    }

    /// [`Self::default_args`], with the RPC password replaced by
    /// `[redacted]`. For logging only: the result is not runnable.
    fn default_args_redacted(&self) -> Vec<String> {
        self.default_args_with_rpc_pass(self.rpc_pass.as_ref().map(|_| crate::cli::REDACTED))
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
    /// The full invocation, for embedding in a script that shells out to
    /// `bitcoin-cli` the way the harness itself would.
    pub fn display(&self) -> String {
        std::iter::once(format!("{}", self.path.display()))
            .chain(self.default_args())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn display_without_chain_args(&self, default_args: Vec<String>) -> String {
        let mut command_fragments = vec![format!("{}", self.path.display())];
        command_fragments.extend(
            default_args
                .into_iter()
                .filter(|arg| !arg.starts_with("-chain=")),
        );
        command_fragments.join(" ")
    }

    pub fn display_without_chain(&self) -> String {
        self.display_without_chain_args(self.default_args())
    }

    /// [`Self::display_without_chain`], with the RPC password replaced by
    /// `[redacted]`. For logging only: the result is not runnable.
    pub fn display_redacted(&self) -> String {
        self.display_without_chain_args(self.default_args_redacted())
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
    // Address for block reward payment
    pub coinbase_recipient: Option<bitcoin::Address>,
    pub getblocktemplate_command: Option<String>,
    /// Only used with custom mining script. Enables support for coinbasetxn
    pub coinbasetxn: bool,
    /// Enable debug mode when running the miner
    pub debug: bool,
}

impl SignetMiner {
    /// `cli` renders the `--cli` argument. Taking it as an argument is what
    /// lets [`Self::display_redacted`] build the very same command as
    /// [`Self::command`], without the node RPC password in it.
    fn command_with_cli<Subcommand, SubcommandArg, SubcommandArgs>(
        &self,
        cli: String,
        subcommand: Subcommand,
        subcommand_args: SubcommandArgs,
    ) -> tokio::process::Command
    where
        Subcommand: AsRef<OsStr>,
        SubcommandArg: AsRef<OsStr>,
        SubcommandArgs: IntoIterator<Item = SubcommandArg>,
    {
        let mut command = tokio::process::Command::new(&self.path);
        command.arg(format!("--cli={cli}"));

        // Unless debug is explicitly set, we want to run in quiet mode. Otherwise
        // we'll get lots of error logs about stderr not being empty.
        command.arg(if self.debug { "--debug" } else { "--quiet" });

        let generate = subcommand.as_ref() == "generate";
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
            if let Some(coinbase_recipient) = &self.coinbase_recipient {
                command.arg(format!("--address={coinbase_recipient}"));
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

    pub fn command<Subcommand, SubcommandArg, SubcommandArgs>(
        &self,
        subcommand: Subcommand,
        subcommand_args: SubcommandArgs,
    ) -> tokio::process::Command
    where
        Subcommand: AsRef<OsStr>,
        SubcommandArg: AsRef<OsStr>,
        SubcommandArgs: IntoIterator<Item = SubcommandArg>,
    {
        self.command_with_cli(
            self.bitcoin_cli.display_without_chain(),
            subcommand,
            subcommand_args,
        )
    }

    /// The invocation [`Self::command`] builds, with the node RPC password
    /// replaced by `[redacted]`. `Debug` on the command itself prints the
    /// password, so this is what a log line gets.
    pub fn display_redacted<Subcommand, SubcommandArg, SubcommandArgs>(
        &self,
        subcommand: Subcommand,
        subcommand_args: SubcommandArgs,
    ) -> String
    where
        Subcommand: AsRef<OsStr>,
        SubcommandArg: AsRef<OsStr>,
        SubcommandArgs: IntoIterator<Item = SubcommandArg>,
    {
        let command = self.command_with_cli(
            self.bitcoin_cli.display_redacted(),
            subcommand,
            subcommand_args,
        );
        let command = command.as_std();
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use super::{BitcoinCli, SignetMiner};
    use crate::cli::{REDACTED, SecretString};

    const RPC_PASS: &str = "node-rpc-password";

    fn signet_miner() -> SignetMiner {
        SignetMiner {
            path: PathBuf::from("/signet/miner"),
            bitcoin_cli: BitcoinCli {
                path: PathBuf::from("/bin/bitcoin-cli"),
                network: bitcoin::Network::Signet,
                rpc_user: Some("user".to_owned()),
                rpc_pass: Some(SecretString::new(RPC_PASS)),
                rpc_cookie_path: None,
                rpc_port: 38332,
                rpc_host: "127.0.0.1".to_owned(),
                rpc_wallet: None,
            },
            bitcoin_util: PathBuf::from("/bin/bitcoin-util"),
            block_interval: Some(Duration::from_secs(60)),
            nbits: None,
            coinbase_recipient: None,
            getblocktemplate_command: Some("bitcoin-cli getblocktemplate".to_owned()),
            coinbasetxn: true,
            debug: false,
        }
    }

    /// The node RPC password has to reach the mining script through its
    /// `--cli` argument, but must never reach a log line.
    #[test]
    fn signet_miner_display_redacts_rpc_password() {
        let miner = signet_miner();
        let subcommand_args = ["--set-block-time=1"];

        let displayed = miner.display_redacted("generate", subcommand_args);
        assert!(
            !displayed.contains(RPC_PASS),
            "the node RPC password leaked into a rendering meant for logs"
        );
        assert!(
            displayed.contains(&format!("-rpcpassword={REDACTED}")),
            "expected a redacted password, got: {displayed}"
        );

        // The argv that actually runs still carries the password. Without
        // this, the assertions above would also pass on a command that never
        // authenticated at all.
        let command = miner.command("generate", subcommand_args);
        let argv = std::iter::once(command.as_std().get_program())
            .chain(command.as_std().get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            argv.contains(RPC_PASS),
            "the mining script is invoked without the node RPC password"
        );

        // Redaction is the only difference: a new argument added to
        // `command` cannot silently go missing from what gets logged.
        assert_eq!(displayed.replace(REDACTED, RPC_PASS), argv);
    }
}
