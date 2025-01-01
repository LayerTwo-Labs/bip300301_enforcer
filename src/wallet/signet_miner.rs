use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct SignetMiner {
    pub path: PathBuf,
    pub bitcoin_cli: Option<String>,
    pub bitcoin_util: PathBuf,
    pub nbits: Option<[u8; 4]>,
    pub getblocktemplate_command: Option<String>,
    /// Only used with custom getblocktemplate command
    pub coinbasetxn: bool,
}

impl SignetMiner {
    pub fn command(
        &self,
        command_args: Vec<&str>,
        subcommand: &str,
        subcommand_args: Vec<&str>,
    ) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.path);
        command.arg(format!(
            "--cli={}",
            self.bitcoin_cli
                .clone()
                .unwrap_or("bitcoin-cli".to_string())
        ));

        // Unless debug is explicitly set, we want to run in quiet mode. Otherwise
        // we'll get lots of error logs about stderr not being empty.
        if !command_args.contains(&"--debug") {
            command.arg("--quiet");
        }
        command.args(command_args);
        let generate = subcommand == "generate";
        command.arg(subcommand);
        command.arg(format!("--grind-cmd={} grind", self.bitcoin_util.display()));
        if generate {
            if let Some(nbits) = self.nbits {
                command.arg(format!("--nbits={}", hex::encode(nbits)));
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
