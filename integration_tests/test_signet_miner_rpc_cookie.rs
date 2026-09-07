//! The enforcer's signet miner must get the node RPC credentials through a
//! cookie file, not through the `bitcoin-cli` invocation in its argv. Drives
//! `GenerateToAddress` on signet, the one path that spawns the miner, and
//! checks the logged invocation, the cookie file, and the enforcer's output.

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    cli::RPC_COOKIE_FILENAME,
    proto::{self, mainchain::GenerateToAddressRequest},
};
use futures::channel::mpsc;

use crate::{
    setup::{Mode, PostSetup, PreSetup, SetupOpts, wait_for_block_templates, wait_until},
    util::{assert_absent, enforcer_output},
};

/// Logged with the full miner command right before it is spawned.
const MINER_INVOCATION_MARKER: &str = "Running signet miner:";

async fn block_count(
    bitcoin_cli: &bip300301_enforcer_lib::bins::BitcoinCli,
) -> anyhow::Result<u64> {
    let count = bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?;
    Ok(count.trim().parse()?)
}

/// Polled: the rolling log can lag the RPC response.
async fn wait_for_miner_invocation(post_setup: &PostSetup) -> anyhow::Result<String> {
    let mut invocation = None;
    wait_until("the enforcer to log its signet miner invocation", || {
        let found = enforcer_output(&post_setup.directories.enforcer_dir).map(|files| {
            files.iter().find_map(|(_, contents)| {
                contents
                    .lines()
                    .find(|line| line.contains(MINER_INVOCATION_MARKER))
                    .map(str::to_owned)
            })
        });
        let result = match found {
            Ok(Some(line)) => {
                invocation = Some(line);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(err) => Err(err),
        };
        async move { result }
    })
    .await?;
    invocation.ok_or_else(|| anyhow::anyhow!("miner invocation matched but was not captured"))
}

pub async fn test_signet_miner_rpc_cookie(setup: PreSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    // Otherwise the enforcer clones the miner from GitHub and looks for the
    // binaries on `PATH`, which CI lacks.
    let enforcer_args = {
        let bin_paths = &setup.bin_paths;
        vec![
            format!(
                "--signet-miner-script-path={}",
                std::path::absolute(bin_paths.signet_miner()?)?.display()
            ),
            format!(
                "--signet-miner-bitcoin-cli-path={}",
                std::path::absolute(bin_paths.bitcoin_cli()?)?.display()
            ),
            format!(
                "--signet-miner-bitcoin-util-path={}",
                std::path::absolute(bin_paths.bitcoin_util()?)?.display()
            ),
        ]
    };
    let setup_opts: SetupOpts = SetupOpts {
        enforcer_args,
        ..Default::default()
    };
    let post_setup = setup
        .setup(Mode::GetBlockTemplate, setup_opts, res_tx)
        .await?;

    // The miner takes its template from the enforcer's own server.
    wait_for_block_templates(&post_setup.gbt_client).await?;

    let start_height = block_count(&post_setup.bitcoin_cli).await?;
    let mining_address = post_setup.mining_address.clone();
    let resp = post_setup
        .mining_service_client
        .generate_to_address(GenerateToAddressRequest {
            blocks: proto::wrap_u32(1),
            address: mining_address.to_string(),
        })
        .await?
        .into_owned();
    let block_hashes = resp
        .block_hashes
        .into_iter()
        .map(|block_hash| {
            block_hash.decode::<proto::mainchain::GenerateToAddressResponse, bitcoin::BlockHash>(
                "block_hashes",
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        block_hashes.len() == 1,
        "expected 1 block hash, got {}",
        block_hashes.len()
    );

    // The block landed, so `bitcoin-cli` authenticated with the cookie.
    let end_height = block_count(&post_setup.bitcoin_cli).await?;
    anyhow::ensure!(
        end_height == start_height + 1,
        "expected the chain to advance from {start_height} to {}, got {end_height}",
        start_height + 1
    );
    let best_block_hash = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?
        .trim()
        .parse::<bitcoin::BlockHash>()?;
    anyhow::ensure!(
        Some(&best_block_hash) == block_hashes.first(),
        "expected the node tip to be the generated block, got {best_block_hash}"
    );

    // The logged `--cli=` string is where the password used to show up. Never
    // echo it into a failure message.
    let invocation = wait_for_miner_invocation(&post_setup).await?;
    anyhow::ensure!(
        invocation.contains("--cli="),
        "the logged miner invocation carries no `--cli=`"
    );
    for leaked in ["-rpcpassword=", "-rpcuser="] {
        anyhow::ensure!(
            !invocation.contains(leaked),
            "`{leaked}` reached the miner's argv; see the enforcer log"
        );
    }
    let cookie_path = post_setup
        .directories
        .enforcer_dir
        .join(RPC_COOKIE_FILENAME);
    let cookie_arg = format!("-rpccookiefile={}", cookie_path.display());
    anyhow::ensure!(
        invocation.contains(&cookie_arg),
        "the miner's bitcoin-cli must be pointed at `{cookie_arg}`; see the enforcer log"
    );

    // The cookie itself.
    let rpc_user = post_setup
        .bitcoin_cli
        .rpc_user
        .clone()
        .ok_or_else(|| anyhow::anyhow!("harness has no rpc user"))?;
    let rpc_pass = post_setup
        .bitcoin_cli
        .rpc_pass
        .clone()
        .ok_or_else(|| anyhow::anyhow!("harness has no rpc password"))?;
    let cookie = std::fs::read_to_string(&cookie_path)
        .map_err(|err| anyhow::anyhow!("reading {}: {err}", cookie_path.display()))?;
    anyhow::ensure!(
        cookie == format!("{rpc_user}:{}", rpc_pass.expose()),
        "the cookie does not hold the node's credentials"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&cookie_path)?.permissions().mode() & 0o777;
        anyhow::ensure!(mode == 0o600, "the cookie must be 0600, got {mode:o}");
    }

    // Nothing the enforcer wrote carries the password.
    let files = enforcer_output(&post_setup.directories.enforcer_dir)?;
    assert_absent(&files, rpc_pass.expose(), "the node RPC password")?;

    drop(post_setup);
    Ok(())
}
