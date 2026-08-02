use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{self, mainchain::GenerateToAddressRequest},
};
use futures::channel::mpsc;

use crate::setup::{EnforcerWallet, Mode, PreSetup, SetupOpts};

pub async fn test_generate_to_address(setup: PreSetup, mode: Mode) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let setup_opts: SetupOpts = SetupOpts {
        enforcer_wallet: EnforcerWallet::Disabled,
        ..Default::default()
    };

    let post_setup = setup.setup(mode, setup_opts, res_tx).await?;

    // In GetBlockTemplate mode, `GenerateToAddress` fetches its block template
    // from the enforcer's own `getblocktemplate` server, which only comes up
    // once the initial mempool sync is done. Wait for it, so the requests
    // below don't race server startup.
    if matches!(mode, Mode::GetBlockTemplate) {
        crate::setup::wait_for_port(
            "127.0.0.1",
            post_setup.reserved_ports.enforcer_serve_rpc.port(),
            std::time::Duration::from_secs(30),
        )
        .await?;
    }

    async fn block_count(
        bitcoin_cli: &bip300301_enforcer_lib::bins::BitcoinCli,
    ) -> anyhow::Result<u64> {
        let count = bitcoin_cli
            .command::<String, _, String, _, _>([], "getblockcount", [])
            .run_utf8()
            .await?;
        Ok(count.trim().parse()?)
    }
    let start_height = block_count(&post_setup.bitcoin_cli).await?;

    const BLOCKS: u32 = 3;
    let mining_address = post_setup.mining_address.clone();
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
    let end_height = block_count(&post_setup.bitcoin_cli).await?;
    anyhow::ensure!(
        end_height == start_height + BLOCKS as u64,
        "expected the chain to advance from {start_height} to {}, got {end_height}",
        start_height + BLOCKS as u64
    );
    let best_block_hash = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?
        .trim()
        .parse::<bitcoin::BlockHash>()?;
    anyhow::ensure!(
        Some(&best_block_hash) == block_hashes.last(),
        "expected the node tip to be the last generated block, got {best_block_hash}"
    );

    // The coinbase pays out to the requested address.
    let tip_json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "getblock",
            [best_block_hash.to_string(), "2".to_owned()],
        )
        .run_utf8()
        .await?;
    let tip: serde_json::Value = serde_json::from_str(&tip_json)?;
    let coinbase_recipient = tip["tx"][0]["vout"][0]["scriptPubKey"]["address"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no address in coinbase output: {tip_json}"))?;
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

    drop(post_setup);
    Ok(())
}
