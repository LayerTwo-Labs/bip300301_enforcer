//! The validator must be able to synchronize headers from Bitcoin Core when
//! its optional REST interface is disabled.

use bip300301_enforcer_lib::bins::CommandExt as _;

use crate::setup::{PostSetup, read_enforcer_log, wait_for_validator_synced, wait_until};

pub const TEST_NAME: &str = "rest_disabled_header_sync";

async fn validator_tip_height(post_setup: &PostSetup) -> anyhow::Result<u32> {
    Ok(post_setup
        .validator_service_client
        .get_chain_tip(Default::default())
        .await?
        .into_owned()
        .block_header_info
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("validator chain tip has no block header info"))?
        .height)
}

pub async fn test_rest_disabled_header_sync(post_setup: PostSetup) -> anyhow::Result<()> {
    // Guard the premise: the harness normally starts Core with `-rest`, and
    // this trial appends `-norest` to override it.
    let rest_response = reqwest::get(format!(
        "http://127.0.0.1:{}/rest/chaininfo.json",
        post_setup.bitcoin_cli.rpc_port
    ))
    .await?;
    anyhow::ensure!(
        rest_response.status() == reqwest::StatusCode::NOT_FOUND,
        "Bitcoin Core REST must be disabled for this test, got HTTP {}",
        rest_response.status()
    );

    let node_height: u32 = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    let validator_tip = wait_for_validator_synced(&post_setup.validator_service_client).await?;
    anyhow::ensure!(
        validator_tip.height == node_height,
        "validator stopped at height {}, Bitcoin Core is at {node_height}",
        validator_tip.height
    );

    // Prove that the fallback remains live after startup, rather than only
    // accepting the chain that existed when the enforcer first connected.
    let mining_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?;
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", ["1", mining_address.trim()])
        .run_utf8()
        .await?;
    let new_node_height: u32 = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    anyhow::ensure!(
        new_node_height == node_height + 1,
        "expected one newly mined block, Core advanced from {node_height} to {new_node_height}"
    );
    wait_until(
        &format!("validator to reach newly mined block at height {new_node_height}"),
        || async { Ok(validator_tip_height(&post_setup).await? == new_node_height) },
    )
    .await?;

    let enforcer_log = read_enforcer_log(&post_setup.directories.enforcer_dir)?;
    anyhow::ensure!(
        enforcer_log
            .contains("Bitcoin Core REST server is disabled; falling back to JSON-RPC header sync"),
        "enforcer did not report using the JSON-RPC header fallback"
    );
    Ok(())
}
