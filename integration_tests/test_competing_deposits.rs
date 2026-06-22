//! Reproduces a mining deadlock observed on signet: two deposits into the same
//! empty treasury, created without a block in between.
//!
//! The block producer must instead exclude the offending tx from the block
//! proposal, so that one unconfirmable tx cannot stop block production.
//!
//! We also verify that this works for basic mempool packages.

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        mainchain::{
            CreateDepositTransactionRequest, CreateDepositTransactionResponse, block_info,
        },
    },
};
use futures::channel::mpsc;

use crate::{
    integration_test::{
        activate_sidechain, consolidate_to_single_utxo, fund_enforcer, propose_sidechain,
    },
    mine::{mine_check_block_events, wait_for_tx_in_block_template},
    setup::{DummySidechain, PostSetup, Sidechain as _},
};

const DEPOSIT_AMOUNT: bitcoin::Amount = bitcoin::Amount::from_sat(21_000_000);
const DEPOSIT_FEE: bitcoin::Amount = bitcoin::Amount::from_sat(1_000_000);

/// Create a deposit via the wallet gRPC, without mining a block
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

/// The txids spent by `txid`'s inputs, via Bitcoin Core's `getrawtransaction`.
async fn input_txids(
    post_setup: &mut PostSetup,
    txid: bitcoin::Txid,
) -> anyhow::Result<Vec<bitcoin::Txid>> {
    #[derive(serde::Deserialize)]
    struct Vin {
        txid: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct RawTx {
        vin: Vec<Vin>,
    }
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "getrawtransaction",
            [txid.to_string(), "true".to_owned()],
        )
        .run_utf8()
        .await?;
    let raw: RawTx = serde_json::from_str(&json)?;
    Ok(raw
        .vin
        .into_iter()
        .filter_map(|vin| vin.txid)
        .filter_map(|txid| txid.parse().ok())
        .collect())
}

pub async fn test_competing_unconfirmed_deposits(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let sidechain = DummySidechain::setup((), &post_setup, res_tx).await?;
    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Proposed sidechain successfully");
    let () = activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Activated sidechain successfully");
    let () = fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Funded enforcer successfully");
    drop(sidechain);

    // Collapse the wallet to a single UTXO so the second deposit can only be
    // funded from the first deposit's change, making it a mempool child of the
    // first
    let () = consolidate_to_single_utxo(&mut post_setup).await?;
    tracing::info!("Consolidated wallet into a single UTXO");

    let sidechain_address = "sidechain address";
    let deposit_txid_1 = create_deposit(&mut post_setup, sidechain_address).await?;
    tracing::info!(%deposit_txid_1, "Created first deposit");
    let deposit_txid_2 = create_deposit(&mut post_setup, sidechain_address).await?;
    tracing::info!(%deposit_txid_2, "Created second deposit");

    anyhow::ensure!(
        deposit_txid_1 != deposit_txid_2,
        "expected two distinct deposits"
    );
    // The crux of the scenario: the second deposit spends the first, so the two
    // can only be mined together — which is invalid, as the second does not
    // spend the treasury the first creates.
    anyhow::ensure!(
        input_txids(&mut post_setup, deposit_txid_2)
            .await?
            .contains(&deposit_txid_1),
        "expected the second deposit to spend the first deposit's change"
    );

    // Wait for the mineable first deposit to reach the enforcer's mempool. The
    // second is rejected as unconfirmable, so it never enters the template.
    let () = wait_for_tx_in_block_template(&mut post_setup, deposit_txid_1).await?;

    // Block production must not wedge: the block contains exactly one of the
    // competing deposits.
    let () =
        mine_check_block_events::<_, DummySidechain>(&mut post_setup, 1, None, |_, block_info| {
            match block_info.events.as_slice() {
                [
                    block_info::Event {
                        event: Some(block_info::event::Event::Deposit(_)),
                    },
                ] => Ok(()),
                events => anyhow::bail!("Expected exactly one deposit event, found `{events:?}`"),
            }
        })
        .await?;
    tracing::info!("Mined block with exactly one of the competing deposits");

    // The losing deposit never confirms, so the next block has no deposit.
    let () =
        mine_check_block_events::<_, DummySidechain>(&mut post_setup, 1, None, |_, block_info| {
            match block_info.events.as_slice() {
                [] => Ok(()),
                events => anyhow::bail!("Expected no events, found `{events:?}`"),
            }
        })
        .await?;
    Ok(())
}
