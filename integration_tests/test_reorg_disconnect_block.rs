use std::str::FromStr;

use bip300301_enforcer_lib::{
    bins::{BitcoinCli, CommandExt as _},
    proto::mainchain::{GetChainTipRequest, validator_service_client::ValidatorServiceClient},
};
use bitcoin::BlockHash;
use futures::channel::mpsc;
use serde_json::Map;
use tonic::transport::Channel;

use crate::{
    setup::{Mode, Network, generate_regtest_block, setup},
    util::BinPaths,
};

// Basic test for reorging and disconnecting a block.
// TODO: add setup + assertions for different BIP300/301 scenarios
//   * a freshly enacted sidechain going back to pending
//   * proposal votes being reset
//   * a newly deleted sidechain going back to pending
//   * deposits being reset
//   * sidechain treasury values being reset
//   * withdrawal bundle votes being reset
pub async fn test_reorg_disconnect_block(bin_paths: BinPaths) -> anyhow::Result<()> {
    let (res_tx, _) = mpsc::unbounded();
    let mut post_setup = setup(&bin_paths, Network::Regtest, Mode::NoMempool, res_tx).await?;

    let new_block =
        generate_regtest_block(&post_setup.bitcoin_cli, &post_setup.mining_address).await?;

    let enforcer_tip = fetch_enforcer_tip(&mut post_setup.validator_service_client).await?;

    // invalidate the block in core
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "invalidateblock", [new_block.to_string()])
        .run_utf8()
        .await?;

    tracing::info!("invalidated block: {}", new_block.clone());

    let chain_info = fetch_mainchain_info(&post_setup.bitcoin_cli).await?;
    tracing::info!("chain info post `invalidateblock`: {:#?}", chain_info);

    assert!(chain_info.block_height == enforcer_tip.block_height - 1);
    assert!(chain_info.best_block_hash == enforcer_tip.prev_block_hash);

    tracing::info!("bitcoin core has reorged to {}", chain_info.best_block_hash);

    let enforcer_tip_post_invalidate =
        fetch_enforcer_tip(&mut post_setup.validator_service_client).await?;

    assert!(enforcer_tip_post_invalidate.block_hash == enforcer_tip.prev_block_hash);
    assert!(enforcer_tip_post_invalidate.block_height == enforcer_tip.block_height - 1);

    tracing::info!(
        "enforcer has reorged to {}",
        enforcer_tip_post_invalidate.block_hash
    );

    // If we generate a new block right away Bitcoin Core for some reason
    // will generate the same block hash as the one we just invalidated...
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let new_block =
        generate_regtest_block(&post_setup.bitcoin_cli, &post_setup.mining_address).await?;

    tracing::info!("mined new block post invalidate: {}", new_block);

    let chain_info_post_generate = fetch_mainchain_info(&post_setup.bitcoin_cli).await?;
    tracing::info!(
        "chain info post `generate`: {:#?}",
        chain_info_post_generate
    );
    assert!(chain_info_post_generate.block_height == enforcer_tip_post_invalidate.block_height + 1);
    assert!(chain_info_post_generate.best_block_hash == new_block);

    let enforcer_tip_post_generate =
        fetch_enforcer_tip(&mut post_setup.validator_service_client).await?;

    tracing::info!(
        "enforcer tip post `generate`: {:#?}",
        enforcer_tip_post_generate
    );
    assert!(enforcer_tip_post_generate.block_hash == chain_info_post_generate.best_block_hash);
    assert!(enforcer_tip_post_generate.block_height == chain_info_post_generate.block_height);

    Ok(())
}

#[derive(Debug)]
struct ChainInfo {
    block_height: u64,
    best_block_hash: BlockHash,
}

async fn fetch_mainchain_info(bitcoin_cli: &BitcoinCli) -> anyhow::Result<ChainInfo> {
    let raw = bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockchaininfo", [])
        .run_utf8()
        .await
        .map_err(|err| anyhow::anyhow!("error fetching chain info: {err:#}"))?;

    let chain_info = match serde_json::from_str::<Map<String, serde_json::Value>>(&raw) {
        Ok(chain_info) => chain_info,
        Err(err) => {
            return Err(anyhow::anyhow!(
                "error parsing chain info ({raw}) : {err:#}",
            ));
        }
    };

    Ok(ChainInfo {
        block_height: chain_info
            .get("blocks")
            .map(|v| v.as_u64().unwrap_or_default())
            .unwrap_or_default(),
        best_block_hash: BlockHash::from_str(
            chain_info
                .get("bestblockhash")
                .map(|v| v.as_str().unwrap_or_default())
                .unwrap_or_default(),
        )
        .expect("best block hash should be valid"),
    })
}

#[derive(Debug)]
struct EnforcerTip {
    block_hash: BlockHash,
    prev_block_hash: BlockHash,
    block_height: u64,
}

async fn fetch_enforcer_tip(
    validator_service_client: &mut ValidatorServiceClient<Channel>,
) -> anyhow::Result<EnforcerTip> {
    let chain_tip = validator_service_client
        .get_chain_tip(GetChainTipRequest {})
        .await?
        .into_inner();

    let header_info = chain_tip
        .block_header_info
        .expect("block header info should be present");

    let block_hash = header_info
        .block_hash
        .expect("block hash should be present")
        .hex
        .expect("block hash hex should be present");

    Ok(EnforcerTip {
        block_hash: BlockHash::from_str(&block_hash).expect("block hash should be valid"),
        prev_block_hash: BlockHash::from_str(
            &header_info
                .prev_block_hash
                .unwrap_or_default()
                .hex
                .unwrap_or_default(),
        )
        .expect("prev block hash should be valid"),
        block_height: header_info.height as u64,
    })
}
