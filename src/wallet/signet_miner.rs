use std::{path::PathBuf, time::Duration};

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
