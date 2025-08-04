use bip300301_enforcer_lib::{bins::CommandExt, proto::common::ConsensusHex};
use futures::{StreamExt, channel::mpsc};
use tokio::time::sleep;
use tracing::Instrument;

use crate::{
    integration_test::{activate_sidechain, fund_enforcer, propose_sidechain},
    mine,
    setup::{DummySidechain, Mode, Network, PostSetup, Sidechain, setup},
    util::{self, BinPaths},
};

async fn create_bmm_request_tx(
    post_setup: &mut PostSetup,
    sidechain_block_hash: &[u8; 32],
) -> anyhow::Result<bitcoin::Txid> {
    let block_hash: bitcoin::BlockHash = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?
        .parse()?;
    let op_return_data_hex = format!(
        "00bf00{:02x}{}{}",
        DummySidechain::SIDECHAIN_NUMBER.0,
        hex::encode(sidechain_block_hash),
        hex::encode({
            let block_hash_bytes: &[u8; 32] = block_hash.as_ref();
            block_hash_bytes
        }),
    );
    let tx_hex = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "createrawtransaction",
            [
                "[]".to_owned(),
                format!(r#"{{"data": "{op_return_data_hex}"}}"#),
            ],
        )
        .run_utf8()
        .await?;
    let tx_json_str = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "fundrawtransaction",
            [
                tx_hex,
                r#"{"changePosition": 1, "fee_rate": 10000}"#.to_owned(),
            ],
        )
        .run_utf8()
        .await?;
    let tx_json = serde_json::from_str::<serde_json::Value>(&tx_json_str)?;
    let tx_hex = tx_json["hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Expected a hex string in tx hex"))?;
    let signed_tx_json_str = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "signrawtransactionwithwallet", [tx_hex])
        .run_utf8()
        .await?;
    let signed_tx_json = serde_json::from_str::<serde_json::Value>(&signed_tx_json_str)?;
    let tx_hex = signed_tx_json["hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Expected a hex string in signed tx hex"))?;
    let txid = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "sendrawtransaction", [tx_hex])
        .run_utf8()
        .await?
        .parse()?;
    Ok(txid)
}

async fn test_peer_bmm_request_task(
    bin_paths: &BinPaths,
    network: Network,
    mode: Mode,
    res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let mut post_setup = setup(bin_paths, network, mode, res_tx.clone()).await?;
    let sidechain = DummySidechain::setup((), &post_setup, res_tx).await?;
    tracing::info!("Setup successfully");
    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Proposed sidechain successfully");
    let () = activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Activated sidechain successfully");
    let () = fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Funded enforcer successfully");

    let sidechain_block_hash: [u8; 32] = {
        use bitcoin::hashes::Hash;
        bitcoin::hashes::sha256::Hash::hash(b"dummy sidechain block").to_byte_array()
    };
    let _txid = create_bmm_request_tx(&mut post_setup, &sidechain_block_hash).await?;
    tracing::info!("Created BMM request tx successfully");
    // Wait for mempool inclusion
    sleep(std::time::Duration::from_secs(1)).await;
    // Mine a block and check that the BMM request worked
    let () = mine::mine_check_block_events::<_, DummySidechain>(
        &mut post_setup,
        1,
        None,
        |_, block_info| {
            let bmm_commitment = block_info
                .bmm_commitment
                .ok_or_else(|| anyhow::anyhow!("Expected a BMM commitment"))?;
            let expected_bmm_commitment = ConsensusHex::encode(&sidechain_block_hash);
            anyhow::ensure!(bmm_commitment == expected_bmm_commitment);
            Ok(())
        },
    )
    .await?;
    tracing::info!("Included BMM request tx successfully");

    drop(sidechain);
    tracing::info!("Removing {}", post_setup.out_dir.path().display());
    drop(post_setup.tasks);
    // Wait for tasks to die
    sleep(std::time::Duration::from_secs(1)).await;
    post_setup.out_dir.cleanup()?;
    Ok(())
}

/// Test a deposit-withdraw round-trip.
/// * Proposes and activates a sidechain
/// * Creates two deposits
/// * If mode is not GBT, creates a withdrawal that will be allowed to expire
/// * Creates and handles a withdrawal
pub async fn test_peer_bmm_request(
    bin_paths: BinPaths,
    network: Network,
    mode: Mode,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let _test_task: util::AbortOnDrop<()> = tokio::task::spawn({
        let res_tx = res_tx.clone();
        async move {
            let res = test_peer_bmm_request_task(&bin_paths, network, mode, res_tx.clone()).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .in_current_span()
    })
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("Unexpected end of test task result stream"))?
}
