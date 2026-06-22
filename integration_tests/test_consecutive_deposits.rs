use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        mainchain::{CreateDepositTransactionRequest, CreateDepositTransactionResponse},
    },
};
use futures::channel::mpsc;
use tokio::time::sleep;

use crate::{
    integration_test::{
        activate_sidechain, consolidate_to_single_utxo, fund_enforcer, propose_sidechain,
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
