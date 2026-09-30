use bip300301_enforcer_lib::proto::{self, mainchain::GenerateToAddressRequest};
use futures::channel::mpsc;
use jsonrpsee::{core::client::ClientT as _, rpc_params};

use crate::{
    setup::{EnforcerWallet, Mode, PreSetup, SetupOpts},
    util::BitcoindClient,
};

pub async fn test_generate_to_address(setup: PreSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let setup_opts: SetupOpts = SetupOpts {
        enforcer_wallet: EnforcerWallet::Disabled,
        ..Default::default()
    };

    // Setup waits for the enforcer's template server, which `GenerateToAddress`
    // builds its blocks from.
    let post_setup = setup
        .setup(Mode::GetBlockTemplate, setup_opts, res_tx)
        .await?;

    // `GenerateToAddress` builds on the validator's tip, and a template from
    // bitcoind that is ahead of it is refused. Signet restores a pre-mined
    // chain, which the validator is still syncing when setup returns.
    let () = crate::integration_test::wait_for_validator_tip(&post_setup).await?;

    async fn block_count(bitcoind_client: &BitcoindClient) -> anyhow::Result<u64> {
        Ok(bitcoind_client
            .request("getblockcount", rpc_params![])
            .await?)
    }
    async fn tip_time(bitcoind_client: &BitcoindClient) -> anyhow::Result<u64> {
        let tip: bitcoin::BlockHash = bitcoind_client
            .request("getbestblockhash", rpc_params![])
            .await?;
        let header: serde_json::Value = bitcoind_client
            .request("getblockheader", rpc_params![tip])
            .await?;
        header["time"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("no time in block header: {header}"))
    }
    fn unix_now() -> anyhow::Result<u64> {
        Ok(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs())
    }

    let mining_address = post_setup.mining_address.clone();

    // A restored signet chain's tip can be days old, and each block stamped
    // while catching up only halves the gap. Mine until one is stamped with
    // the present, so the blocks below are mined back to back from a current
    // tip.
    const MAX_CATCH_UP_BLOCKS: usize = 64;
    for attempt in 1.. {
        let started = unix_now()?;
        let _resp = post_setup
            .mining_service_client
            .generate_to_address(GenerateToAddressRequest {
                blocks: proto::wrap_u32(1),
                address: mining_address.to_string(),
            })
            .await?;
        if tip_time(&post_setup.bitcoind_client).await? >= started {
            break;
        }
        anyhow::ensure!(
            attempt < MAX_CATCH_UP_BLOCKS,
            "tip still behind the clock after {MAX_CATCH_UP_BLOCKS} blocks"
        );
    }

    let start_height = block_count(&post_setup.bitcoind_client).await?;
    let start_tip_time = tip_time(&post_setup.bitcoind_client).await?;

    const BLOCKS: u32 = 3;
    let resp = post_setup
        .mining_service_client
        .generate_to_address(GenerateToAddressRequest {
            blocks: proto::wrap_u32(BLOCKS),
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
        block_hashes.len() == BLOCKS as usize,
        "expected {BLOCKS} block hashes, got {}",
        block_hashes.len()
    );

    // The node accepted the blocks: the chain advanced by `BLOCKS`, and its tip
    // is the last returned hash.
    let end_height = block_count(&post_setup.bitcoind_client).await?;
    anyhow::ensure!(
        end_height == start_height + BLOCKS as u64,
        "expected the chain to advance from {start_height} to {}, got {end_height}",
        start_height + BLOCKS as u64
    );
    let best_block_hash: bitcoin::BlockHash = post_setup
        .bitcoind_client
        .request("getbestblockhash", rpc_params![])
        .await?;
    anyhow::ensure!(
        Some(&best_block_hash) == block_hashes.last(),
        "expected the node tip to be the last generated block, got {best_block_hash}"
    );

    // Blocks mined back to back are not stamped ahead of the clock, beyond
    // the one second per block that keeps each after the last.
    let now = unix_now()?;
    let end_tip_time = tip_time(&post_setup.bitcoind_client).await?;
    let latest_allowed = now.max(start_tip_time) + BLOCKS as u64;
    anyhow::ensure!(
        end_tip_time <= latest_allowed,
        "expected the tip time to be at most {latest_allowed} (now {now}, start tip time \
         {start_tip_time}), got {end_tip_time}"
    );

    // The coinbase pays out to the requested address.
    let tip: serde_json::Value = post_setup
        .bitcoind_client
        .request("getblock", rpc_params![best_block_hash, 2])
        .await?;
    let coinbase_recipient = tip["tx"][0]["vout"][0]["scriptPubKey"]["address"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no address in coinbase output: {tip}"))?;
    anyhow::ensure!(
        coinbase_recipient == mining_address.to_string(),
        "expected the coinbase to pay `{mining_address}`, got `{coinbase_recipient}`"
    );

    // Zero blocks is rejected.
    let status = post_setup
        .mining_service_client
        .generate_to_address(GenerateToAddressRequest {
            blocks: proto::wrap_u32(0),
            address: mining_address.to_string(),
        })
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("GenerateToAddress succeeded with 0 blocks"))?;
    anyhow::ensure!(
        status.code == connectrpc::ErrorCode::InvalidArgument,
        "expected invalid argument for 0 blocks, got: {status}"
    );

    // A missing address is rejected.
    let status = post_setup
        .mining_service_client
        .generate_to_address(GenerateToAddressRequest {
            blocks: proto::wrap_u32(1),
            address: String::new(),
        })
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("GenerateToAddress succeeded without an address"))?;
    anyhow::ensure!(
        status.code == connectrpc::ErrorCode::InvalidArgument,
        "expected invalid argument for a missing address, got: {status}"
    );

    // An address for the wrong network is rejected.
    const MAINNET_ADDRESS: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
    let status = post_setup
        .mining_service_client
        .generate_to_address(GenerateToAddressRequest {
            blocks: proto::wrap_u32(1),
            address: MAINNET_ADDRESS.to_owned(),
        })
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("GenerateToAddress succeeded with a mainnet address"))?;
    anyhow::ensure!(
        status.code == connectrpc::ErrorCode::InvalidArgument,
        "expected invalid argument for a mainnet address, got: {status}"
    );

    // The rejection echoes the offending address, so the error body grows
    // with the input. A caller must still get `invalid_argument` back, not an
    // internal error from whatever sits between the handler and the wire.
    let oversized_address = "x".repeat(8 * 1024);
    let status = post_setup
        .mining_service_client
        .generate_to_address(GenerateToAddressRequest {
            blocks: proto::wrap_u32(1),
            address: oversized_address.clone(),
        })
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("GenerateToAddress succeeded with a garbage address"))?;
    anyhow::ensure!(
        status.code == connectrpc::ErrorCode::InvalidArgument,
        "expected invalid argument for an oversized address, got: {status}"
    );
    anyhow::ensure!(
        status
            .message
            .as_deref()
            .is_some_and(|message| message.contains(&oversized_address)),
        "expected the error to echo the oversized address, got: {status}"
    );

    drop(post_setup);
    Ok(())
}

/// Without the enforcer's block template server there is nothing to build a
/// block from that follows drivechain rules, so `GenerateToAddress` is refused
/// before anything is mined.
pub async fn test_generate_to_address_requires_template_server(
    setup: PreSetup,
) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let setup_opts: SetupOpts = SetupOpts {
        enforcer_wallet: EnforcerWallet::Disabled,
        ..Default::default()
    };
    let post_setup = setup.setup(Mode::NoMempool, setup_opts, res_tx).await?;
    let () = crate::integration_test::wait_for_validator_tip(&post_setup).await?;
    let height_before: u64 = post_setup
        .bitcoind_client
        .request("getblockcount", rpc_params![])
        .await?;

    let status = post_setup
        .mining_service_client
        .generate_to_address(GenerateToAddressRequest {
            blocks: proto::wrap_u32(1),
            address: post_setup.mining_address.to_string(),
        })
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("GenerateToAddress succeeded without a template server"))?;
    anyhow::ensure!(
        status.code == connectrpc::ErrorCode::FailedPrecondition,
        "expected failed precondition without a template server, got: {status}"
    );
    let height_after: u64 = post_setup
        .bitcoind_client
        .request("getblockcount", rpc_params![])
        .await?;
    anyhow::ensure!(
        height_after == height_before,
        "expected no block to be mined, the chain went from {height_before} to {height_after}"
    );

    drop(post_setup);
    Ok(())
}
