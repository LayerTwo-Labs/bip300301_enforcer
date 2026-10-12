//! On eCash builds wallet transactions carry the replay-protection
//! `nLockTime` and a non-final input; elsewhere they don't.

use std::collections::HashMap;

use bip300301_enforcer_lib::proto::{
    self,
    mainchain::{
        CreateDepositTransactionRequest, CreateDepositTransactionResponse, SendTransactionRequest,
        SendTransactionResponse,
    },
};
use futures::channel::mpsc;
use jsonrpsee::{core::client::ClientT as _, rpc_params};
use serde::Deserialize;

use crate::{
    integration_test::{
        activate_sidechain, fund_enforcer, propose_sidechain, wait_for_wallet_sync,
    },
    mine::{MiningPolicy, mine},
    setup::{DummySidechain, PostSetup, Sidechain as _, wait_for_tx_in_mempool},
};

const SEQUENCE_FINAL: u32 = 0xFFFF_FFFF;

#[derive(Deserialize)]
struct RawTxInput {
    sequence: u32,
}

#[derive(Deserialize)]
struct RawTx {
    locktime: u32,
    vin: Vec<RawTxInput>,
    #[serde(default)]
    confirmations: u32,
}

async fn raw_tx(post_setup: &PostSetup, txid: bitcoin::Txid) -> anyhow::Result<RawTx> {
    Ok(post_setup
        .bitcoind_client
        .request("getrawtransaction", rpc_params![txid, true])
        .await?)
}

fn expected_lock_time() -> anyhow::Result<Option<u32>> {
    crate::setup::bitcoind_replay_protection_lock_time()
        .map(|lock_time| {
            lock_time.parse().map_err(|err| {
                anyhow::anyhow!("invalid BITCOIND_REPLAY_PROTECTION_LOCK_TIME `{lock_time}`: {err}")
            })
        })
        .transpose()
}

async fn check_replay_protection(
    post_setup: &PostSetup,
    what: &str,
    txid: bitcoin::Txid,
    expected: Option<u32>,
) -> anyhow::Result<()> {
    let tx = raw_tx(post_setup, txid).await?;
    match expected {
        Some(expected) => {
            anyhow::ensure!(
                tx.locktime == expected,
                "{what} {txid} has nLockTime {}, want the replay-protection value {expected}",
                tx.locktime
            );
            anyhow::ensure!(
                tx.vin.iter().any(|input| input.sequence < SEQUENCE_FINAL),
                "{what} {txid} has only final inputs, so Bitcoin ignores its nLockTime and it \
                 can replay"
            );
        }
        None => {
            let tip_height: u32 = post_setup
                .bitcoind_client
                .request("getblockcount", rpc_params![])
                .await?;
            anyhow::ensure!(
                tx.locktime <= tip_height,
                "{what} {txid} has nLockTime {} on a non-eCash network (tip {tip_height}); \
                 replay protection must not apply here",
                tx.locktime
            );
        }
    }
    tracing::info!(%txid, lock_time = tx.locktime, "{what} has the expected nLockTime");
    Ok(())
}

async fn send(post_setup: &mut PostSetup) -> anyhow::Result<bitcoin::Txid> {
    let txid = post_setup
        .wallet_service_client
        .send_transaction(SendTransactionRequest {
            destinations: HashMap::from([(post_setup.receive_address.to_string(), 1_000_000_u64)])
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .await?
        .into_owned()
        .txid
        .ok_or_else(|| proto::Error::missing_field::<SendTransactionResponse>("txid"))?
        .decode::<SendTransactionResponse, _>("txid")?;
    let () = wait_for_tx_in_mempool(&post_setup.bitcoind_client, &txid).await?;
    Ok(txid)
}

async fn deposit(post_setup: &mut PostSetup, address: &str) -> anyhow::Result<bitcoin::Txid> {
    let txid = post_setup
        .wallet_service_client
        .create_deposit_transaction(CreateDepositTransactionRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            address: proto::wrap_string(address),
            value_sats: proto::wrap_u64(21_000_000),
            fee_sats: proto::wrap_u64(1_000_000),
        })
        .await?
        .into_owned()
        .txid
        .ok_or_else(|| proto::Error::missing_field::<CreateDepositTransactionResponse>("txid"))?
        .decode::<CreateDepositTransactionResponse, _>("txid")?;
    let () = wait_for_tx_in_mempool(&post_setup.bitcoind_client, &txid).await?;
    Ok(txid)
}

async fn mine_and_confirm(
    post_setup: &mut PostSetup,
    txids: &[(&str, bitcoin::Txid)],
) -> anyhow::Result<()> {
    let () = mine::<DummySidechain>(post_setup, 1, MiningPolicy::SILENT).await?;
    for (what, txid) in txids {
        let confirmations = raw_tx(post_setup, *txid).await?.confirmations;
        anyhow::ensure!(
            confirmations > 0,
            "{what} {txid} was not mined into the next block"
        );
    }
    let () = wait_for_wallet_sync(post_setup).await?;
    Ok(())
}

pub async fn test_replay_protection(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let sidechain = DummySidechain::setup((), &post_setup, res_tx).await?;
    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    let () = activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    let () = fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    drop(sidechain);

    let send_txid = send(&mut post_setup).await?;
    let () = check_replay_protection(&post_setup, "send", send_txid, expected_lock_time()?).await?;
    let first_deposit_txid = deposit(&mut post_setup, "replay protection 1").await?;
    let () = check_replay_protection(
        &post_setup,
        "first deposit",
        first_deposit_txid,
        expected_lock_time()?,
    )
    .await?;
    let () = mine_and_confirm(
        &mut post_setup,
        &[("send", send_txid), ("first deposit", first_deposit_txid)],
    )
    .await?;

    // Spends the Ctip, which stays final.
    let ctip_deposit_txid = deposit(&mut post_setup, "replay protection 2").await?;
    let () = check_replay_protection(
        &post_setup,
        "Ctip-spending deposit",
        ctip_deposit_txid,
        expected_lock_time()?,
    )
    .await?;
    let () = mine_and_confirm(
        &mut post_setup,
        &[("Ctip-spending deposit", ctip_deposit_txid)],
    )
    .await?;
    Ok(())
}
