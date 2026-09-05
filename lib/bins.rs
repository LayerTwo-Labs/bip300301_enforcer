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
    fn default_args(&self) -> Vec<String> {
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

        if let Some(rpc_pass) = &self.rpc_pass {
            res.push(format!("-rpcpassword={}", rpc_pass.expose()));
        }

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
    /// The full invocation, for embedding in a script that shells out to
    /// `bitcoin-cli` the way the harness itself would.
    pub fn display(&self) -> String {
        std::iter::once(format!("{}", self.path.display()))
            .chain(self.default_args())
            .collect::<Vec<_>>()
            .join(" ")
    }

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
    // Address for block reward payment
    pub coinbase_recipient: Option<bitcoin::Address>,
    pub getblocktemplate_command: Option<String>,
    /// Only used with custom mining script. Enables support for coinbasetxn
    pub coinbasetxn: bool,
    /// Enable debug mode when running the miner
    pub debug: bool,
}

impl SignetMiner {
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
        let mut command = tokio::process::Command::new(&self.path);
        command.arg(format!(
            "--cli={}",
            self.bitcoin_cli.display_without_chain()
        ));

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
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::cli::Config;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bip300301-bins-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_with_rpc_credentials(dir: &std::path::Path) -> Config {
        Config::try_parse_from([
            "bip300301_enforcer",
            &format!("--data-dir={}", dir.display()),
            "--node-rpc-user=alice",
            "--node-rpc-pass=hunter2",
        ])
        .expect("should parse")
    }

    /// Anything in an argument vector is readable by every local user through
    /// `/proc/<pid>/cmdline`, and the signet miner is handed the whole
    /// `bitcoin-cli` invocation as its `--cli=` argument, so the password must
    /// reach `bitcoin-cli` through a cookie file rather than an argument.
    #[test]
    fn rpc_credentials_never_reach_a_command_line() {
        let dir = temp_dir("rpc-cookie");
        let config = config_with_rpc_credentials(&dir);
        let bitcoin_cli = config.bitcoin_cli(bitcoin::Network::Signet);
        let command = bitcoin_cli.command(Vec::<&str>::new(), "getblockcount", Vec::<&str>::new());
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        let cookie_path = bitcoin_cli
            .rpc_cookie_path
            .clone()
            .expect("credentials must be passed as a cookie file");
        assert!(
            args.contains(&format!("-rpccookiefile={cookie_path}")),
            "the cookie file must reach bitcoin-cli, got: {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.starts_with("-rpcpassword=")),
            "the password leaked into argv: {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.starts_with("-rpcuser=")),
            "the user leaked into argv: {args:?}"
        );
        assert!(!args.iter().any(|arg| arg.contains("hunter2")));
        // The exact string the signet miner receives as `--cli=`, and keeps in
        // its own argv for the whole mining run.
        assert!(
            !bitcoin_cli.display_without_chain().contains("hunter2"),
            "the password leaked into the miner's `--cli=` argument"
        );

        assert_eq!(
            std::fs::read_to_string(&cookie_path).unwrap(),
            "alice:hunter2",
            "the cookie must be in the `user:pass` shape bitcoin-cli expects"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A cookie holding the RPC password must not be readable by other users.
    #[cfg(unix)]
    #[test]
    fn rpc_cookie_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("rpc-cookie-permissions");
        let config = config_with_rpc_credentials(&dir);
        let cookie_path = config
            .bitcoin_cli(bitcoin::Network::Signet)
            .rpc_cookie_path
            .expect("credentials must be passed as a cookie file");

        let metadata = std::fs::metadata(cookie_path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }
}
