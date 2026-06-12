//! Reproduces a mining wedge observed on signet: two deposits into the same
//! empty treasury, created without a block in between.
//!
//! The wallet builds deposits against confirmed chain state, so with the
//! first deposit unconfirmed the second is built as another "first deposit"
//! (BIP300 M5) — typically funded from the first deposit's unconfirmed
//! change, carrying its own OP_DRIVECHAIN output for the slot without
//! spending the first deposit's treasury output. Both txs are individually
//! valid against chain state, but in any block containing both, the first
//! creates the slot's Ctip and the second fails M5/M6 validation. If both
//! end up in the block template, every `getblocktemplate` call fails and
//! mining is wedged until the mempool is cleared.
//!
//! The block producer must instead exclude the offending tx from the block
//! proposal (via `CusfBlockProducer::validate_block_proposal`), so that one
//! unconfirmable tx cannot wedge block production.

use bip300301_enforcer_lib::proto::{
    self,
    mainchain::{CreateDepositTransactionRequest, CreateDepositTransactionResponse, block_info},
};
use futures::channel::mpsc;
use tokio::time::sleep;

use crate::{
    integration_test::{activate_sidechain, fund_enforcer, propose_sidechain},
    mine::mine_check_block_events,
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
            sidechain_id: Some(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            address: Some(sidechain_address.to_owned()),
            value_sats: Some(DEPOSIT_AMOUNT.to_sat()),
            fee_sats: Some(DEPOSIT_FEE.to_sat()),
        })
        .await?
        .into_inner()
        .txid
        .ok_or_else(|| proto::Error::missing_field::<CreateDepositTransactionResponse>("txid"))?
        .decode::<CreateDepositTransactionResponse, _>("txid")?;
    Ok(deposit_txid)
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

    let sidechain_address = "sidechain address";
    let deposit_txid_1 = create_deposit(&mut post_setup, sidechain_address).await?;
    tracing::info!(%deposit_txid_1, "Created first deposit");
    // Wait for the deposit tx to enter the mempool
    sleep(std::time::Duration::from_secs(1)).await;
    let deposit_txid_2 = create_deposit(&mut post_setup, sidechain_address).await?;
    tracing::info!(%deposit_txid_2, "Created second deposit");
    sleep(std::time::Duration::from_secs(1)).await;

    // Mining must not wedge on the competing deposits, and the block can
    // only contain one of them
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

    // Mining must stay unwedged now that the slot's Ctip exists and the
    // losing deposit is invalid
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
