use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        mainchain::{
            CreateDepositTransactionRequest, CreateDepositTransactionResponse,
            CreateNewAddressRequest, ListUnspentOutputsRequest, SendTransactionRequest,
            SendTransactionResponse,
        },
    },
};
use futures::channel::mpsc;
use tokio::time::sleep;

use crate::{
    integration_test::{
        activate_sidechain, fund_enforcer, propose_sidechain, wait_for_wallet_sync,
    },
    mine::{mine, wait_for_tx_in_block_template},
    setup::{DummySidechain, PostSetup, Sidechain as _, wait_for_tx_in_mempool, wait_until},
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
    let () = wait_for_wallet_sync(post_setup).await?;

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
        let drain_txid = post_setup
            .wallet_service_client
            .send_transaction(SendTransactionRequest {
                drain_wallet_to: Some(drain_address),
                ..Default::default()
            })
            .await?
            .into_owned()
            .txid
            .ok_or_else(|| proto::Error::missing_field::<SendTransactionResponse>("txid"))?
            .decode::<SendTransactionResponse, _>("txid")?;
        // The drain must be in the mempool before we mine, or it won't confirm.
        let () = wait_for_tx_in_mempool(&post_setup.bitcoin_cli, &drain_txid).await?;
        let () = mine_to_core(post_setup, 1).await?;
        // Poll for the enforcer wallet to ingest the confirmed drain. A timeout
        // here is not fatal: a single drain can't always sweep the whole
        // wallet, so fall through and let the outer loop try another one.
        const INGEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
        const INGEST_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
        let deadline = std::time::Instant::now() + INGEST_TIMEOUT;
        loop {
            if unspent_output_count(post_setup).await? == 1 {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            sleep(INGEST_POLL_INTERVAL).await;
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
    let () = wait_for_tx_in_mempool(&post_setup.bitcoin_cli, &deposit_txid_1).await?;

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
    let () = wait_for_tx_in_mempool(&post_setup.bitcoin_cli, &deposit_txid_2).await?;

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

/// Block until the enforcer's block template selects all of `want` and none
/// of `not_want`. `Mode::GetBlockTemplate` only; the mempool mirror trails
/// bitcoind's mempool, so this must gate mining on the deposits. A template
/// that cannot be built at all is reported as the last error on timeout.
async fn wait_for_template_txs(
    post_setup: &PostSetup,
    want: Vec<bitcoin::Txid>,
    not_want: Vec<bitcoin::Txid>,
) -> anyhow::Result<()> {
    use cusf_enforcer_mempool::server::RpcClient as _;
    let gbt_client = post_setup.gbt_client.clone();
    wait_until(
        "the enforcer's block template to settle the competing deposits",
        move || {
            let gbt_client = gbt_client.clone();
            let want = want.clone();
            let not_want = not_want.clone();
            async move {
                let mut gbt_request = bitcoin_jsonrpsee::client::BlockTemplateRequest::default();
                gbt_request.capabilities.insert("coinbasetxn".to_owned());
                let template = crate::util::expect_block_template(
                    gbt_client.get_block_template(gbt_request).await?,
                )?;
                let txids: Vec<bitcoin::Txid> =
                    template.transactions.iter().map(|tx| tx.txid).collect();
                Ok(want.iter().all(|txid| txids.contains(txid))
                    && not_want.iter().all(|txid| !txids.contains(txid)))
            }
        },
    )
    .await
}

/// Two first deposits into the same empty treasury do not spend a treasury
/// UTXO, so they are not double-spends of each other and both sit in the
/// mempool at once. Only one of them can ever be connected, so a block
/// producer that cannot tell them apart offers a template containing both and
/// then fails to finalize it -- for this block, and for every block after the
/// winner confirms, because nothing evicts the loser.
pub async fn test_competing_deposits(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let sidechain = DummySidechain::setup((), &post_setup, res_tx).await?;
    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    let () = activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    let () = fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Activated sidechain and funded enforcer successfully");
    drop(sidechain);

    // As in `test_consecutive_deposits`: a single UTXO forces the second
    // deposit to be funded from the first deposit's change.
    let () = consolidate_to_single_utxo(&mut post_setup).await?;

    let deposit_txid_1 = create_deposit(&mut post_setup, "sidechain address 1").await?;
    let () = wait_for_tx_in_mempool(&post_setup.bitcoin_cli, &deposit_txid_1).await?;
    // The mirror has to hold the first deposit before the second is created,
    // so that it is the second that is seen to compete with the first.
    let () = wait_for_tx_in_block_template(&post_setup, &deposit_txid_1).await?;

    let deposit_txid_2 = create_deposit(&mut post_setup, "sidechain address 2").await?;
    let () = wait_for_tx_in_mempool(&post_setup.bitcoin_cli, &deposit_txid_2).await?;
    tracing::info!(%deposit_txid_1, %deposit_txid_2, "Both competing deposits are in the mempool");

    // The second deposit spends the first's change, so the only block either
    // can appear in is one holding the first alone.
    let () = wait_for_template_txs(&post_setup, vec![deposit_txid_1], vec![deposit_txid_2]).await?;

    let () = mine::<DummySidechain>(&mut post_setup, 1, None).await?;

    // The block was mined from that template, so it took the first deposit
    // alone, and the second is still an unconfirmed transaction that can never
    // be connected now that the treasury it would have created exists.
    let mempool = raw_mempool(&mut post_setup).await?;
    anyhow::ensure!(
        !mempool.contains(&deposit_txid_1.to_string()),
        "expected the winning deposit {deposit_txid_1} to be mined: {mempool:?}"
    );
    anyhow::ensure!(
        mempool.contains(&deposit_txid_2.to_string()),
        "expected the losing deposit {deposit_txid_2} to stay in bitcoind's mempool: {mempool:?}"
    );

    // Unless connecting the block evicted the loser from the enforcer's own
    // mempool mirror, every later template fails to finalize the same way the
    // first one would have.
    let () = wait_for_template_txs(&post_setup, vec![], vec![deposit_txid_2]).await?;
    tracing::info!("The losing deposit was evicted, and templates are still served");
    Ok(())
}
