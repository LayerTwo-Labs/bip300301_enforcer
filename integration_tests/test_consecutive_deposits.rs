use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        mainchain::{
            CreateDepositTransactionRequest, CreateDepositTransactionResponse,
            CreateNewAddressRequest, ListUnspentOutputsRequest, SendTransactionRequest,
        },
    },
};
use futures::channel::mpsc;
use tokio::time::sleep;

use crate::{
    integration_test::{
        activate_sidechain, fund_enforcer, propose_sidechain, wait_for_wallet_sync,
    },
    setup::{DummySidechain, PostSetup, Sidechain as _},
};

const DEPOSIT_AMOUNT: bitcoin::Amount = bitcoin::Amount::from_sat(21_000_000);
const DEPOSIT_FEE: bitcoin::Amount = bitcoin::Amount::from_sat(1_000_000);

/// Create a deposit via the wallet gRPC, without mining a block.
async fn create_deposit(
    post_setup: &mut PostSetup,
    sidechain_address: &str,
) -> anyhow::Result<bitcoin::Txid> {
    let deposit_txid = post_setup
        .wallet_service_client
        .create_deposit_transaction(CreateDepositTransactionRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            address: proto::wrap_string(sidechain_address),
            value_sats: proto::wrap_u64(DEPOSIT_AMOUNT.to_sat()),
            fee_sats: proto::wrap_u64(DEPOSIT_FEE.to_sat()),
        })
        .await?
        .into_owned()
        .txid
        .ok_or_else(|| proto::Error::missing_field::<CreateDepositTransactionResponse>("txid"))?
        .decode::<CreateDepositTransactionResponse, _>("txid")?;
    Ok(deposit_txid)
}

async fn raw_mempool(post_setup: &mut PostSetup) -> anyhow::Result<Vec<String>> {
    let mempool_json = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getrawmempool", [])
        .run_utf8()
        .await?;
    Ok(serde_json::from_str(&mempool_json)?)
}

async fn unspent_output_count(post_setup: &mut PostSetup) -> anyhow::Result<usize> {
    let utxos = post_setup
        .wallet_service_client
        .list_unspent_outputs(ListUnspentOutputsRequest {})
        .await?
        .into_owned();
    Ok(utxos.outputs.len())
}

/// Mine `blocks` to Bitcoin Core's own address (not the enforcer wallet), so
/// the coinbases do not add new UTXOs to the enforcer wallet.
async fn mine_to_core(post_setup: &mut PostSetup, blocks: u32) -> anyhow::Result<()> {
    let core_address = post_setup.receive_address.to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", [blocks.to_string(), core_address])
        .run_utf8()
        .await?;
    Ok(())
}

/// Collapse the entire wallet into a single confirmed UTXO.
async fn consolidate_to_single_utxo(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    // The funding coinbases are mostly immature (coinbase maturity is 100
    // blocks), so a drain can only sweep the few mature ones. Advance the
    // chain so every funding coinbase matures and a single drain can sweep the
    // whole wallet.
    let () = mine_to_core(post_setup, 100).await?;
    let () = wait_for_wallet_sync().await?;

    for _ in 0..6 {
        if unspent_output_count(post_setup).await? <= 1 {
            return Ok(());
        }
        let drain_address = post_setup
            .wallet_service_client
            .create_new_address(CreateNewAddressRequest {})
            .await?
            .into_owned()
            .address;
        let _drain = post_setup
            .wallet_service_client
            .send_transaction(SendTransactionRequest {
                drain_wallet_to: Some(drain_address),
                ..Default::default()
            })
            .await?;
        sleep(std::time::Duration::from_secs(1)).await;
        let () = mine_to_core(post_setup, 1).await?;
        // Poll for the enforcer wallet to ingest the confirmed drain.
        for _ in 0..10 {
            sleep(std::time::Duration::from_secs(2)).await;
            if unspent_output_count(post_setup).await? == 1 {
                return Ok(());
            }
        }
    }
    anyhow::bail!(
        "failed to consolidate wallet to a single UTXO, still have {}",
        unspent_output_count(post_setup).await?
    )
}

pub async fn test_consecutive_deposits(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let sidechain = DummySidechain::setup((), &post_setup, res_tx).await?;
    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Proposed sidechain successfully");
    let () = activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Activated sidechain successfully");
    let () = fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Funded enforcer successfully");
    drop(sidechain);

    // Collapse the wallet into a single UTXO so the two deposits below have no
    // choice but to compete for the same funding input.
    let () = consolidate_to_single_utxo(&mut post_setup).await?;
    tracing::info!("Consolidated wallet into a single UTXO");

    // First deposit: always succeeds, spending the sole UTXO.
    let deposit_txid_1 = create_deposit(&mut post_setup, "sidechain address 1").await?;
    tracing::info!(%deposit_txid_1, "Created first deposit");
    // Wait for the deposit tx to enter the mempool.
    sleep(std::time::Duration::from_secs(1)).await;

    // Second deposit, without a block in between. This must succeed: it should
    // be funded from the first deposit's change output, not by reselecting the
    // first deposit's (now spent) input.
    let deposit_txid_2 = create_deposit(&mut post_setup, "sidechain address 2")
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "second consecutive deposit failed to broadcast \
                 (the wallet reused the first deposit's funding input, so Core \
                 rejected it as an RBF replacement): {err:#}"
            )
        })?;
    tracing::info!(%deposit_txid_2, "Created second deposit");
    sleep(std::time::Duration::from_secs(1)).await;

    anyhow::ensure!(
        deposit_txid_1 != deposit_txid_2,
        "expected two distinct deposit txids"
    );

    // Both deposits must coexist in the mempool. If the second had replaced the
    // first (RBF), only one would be present.
    let mempool = raw_mempool(&mut post_setup).await?;
    anyhow::ensure!(
        mempool.contains(&deposit_txid_1.to_string()),
        "first deposit {deposit_txid_1} missing from mempool (was it replaced?): {mempool:?}"
    );
    anyhow::ensure!(
        mempool.contains(&deposit_txid_2.to_string()),
        "second deposit {deposit_txid_2} missing from mempool: {mempool:?}"
    );
    tracing::info!("Both consecutive deposits are in the mempool");
    Ok(())
}
