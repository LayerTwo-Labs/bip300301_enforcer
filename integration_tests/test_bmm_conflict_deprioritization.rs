//! Two BMM requests for the same sidechain slot cannot share a block: the
//! coinbase can only commit to one of them. They spend different inputs, so
//! Bitcoin Core cannot see the conflict and would offer both in one
//! `getblocktemplate` — and any block built from that template is rejected.
//!
//! The enforcer's `accept_tx` reports the conflict to the mempool sync task,
//! which must deprioritize the lower-fee-rate request in Core
//! (`prioritisetransaction` with a large negative delta) so that Core's
//! template agrees with the enforcer's own template builder.
//!
//! This test submits two M8 BMM requests for the same slot with clearly
//! different fees, then asserts via `getprioritisedtransactions` that exactly
//! the low-fee request is deprioritized.

use std::{str::FromStr as _, time::Duration};

use bip300301_enforcer_lib::{bins::CommandExt as _, messages::M8BmmRequest, types::BmmCommitment};
use bitcoin::{
    Amount, BlockHash, OutPoint, Transaction, TxIn, TxOut, Txid, consensus::encode::serialize_hex,
    transaction::Version,
};
use serde::Deserialize;
use tokio::time::sleep;

use crate::{
    integration_test::{activate_sidechain, propose_sidechain},
    setup::{DummySidechain, PostSetup, Sidechain as _},
};

/// The bid burned in the M8 OP_RETURN output. Same for both requests; only
/// the miner fee differs.
const BID_SATS: u64 = 10_000;
const LOW_FEE: Amount = Amount::from_sat(2_000);
const HIGH_FEE: Amount = Amount::from_sat(50_000);

#[derive(Deserialize)]
struct Utxo {
    txid: String,
    vout: u32,
    amount: f64,
}

#[derive(Deserialize)]
struct SignResult {
    hex: String,
    complete: bool,
}

/// Build, sign (with bitcoind's wallet), and broadcast an M8 BMM request for
/// the given slot, spending `utxo` and paying `fee` to miners.
async fn submit_m8(
    post_setup: &PostSetup,
    prev_mainchain_block_hash: BlockHash,
    sidechain_block_hash: [u8; 32],
    utxo: &Utxo,
    fee: Amount,
) -> anyhow::Result<Txid> {
    let m8_script = M8BmmRequest::script_pubkey(
        DummySidechain::SIDECHAIN_NUMBER,
        BmmCommitment(sidechain_block_hash),
        prev_mainchain_block_hash,
    )?;
    let input_value = Amount::from_btc(utxo.amount)?;
    let bid = Amount::from_sat(BID_SATS);
    let unsigned_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_str(&utxo.txid)?,
                vout: utxo.vout,
            },
            ..TxIn::default()
        }],
        output: vec![
            // The M8 OP_RETURN must be the first output for `parse_m8_tx`.
            TxOut {
                script_pubkey: m8_script,
                value: bid,
            },
            TxOut {
                script_pubkey: post_setup.mining_address.script_pubkey(),
                value: input_value - bid - fee,
            },
        ],
    };
    let signed_hex = {
        let json = post_setup
            .bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "signrawtransactionwithwallet",
                [serialize_hex(&unsigned_tx)],
            )
            .run_utf8()
            .await?;
        let signed: SignResult = serde_json::from_str(&json)?;
        anyhow::ensure!(signed.complete, "signrawtransactionwithwallet incomplete");
        signed.hex
    };
    // maxburnamount must cover the bid burned in the OP_RETURN output. The
    // harness runs bitcoind with `-acceptnonstdtxn`, so the nonstandard M8
    // is accepted into the mempool.
    let txid = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "sendrawtransaction",
            [signed_hex, "0.10".to_owned(), "0.001".to_owned()],
        )
        .run_utf8()
        .await?;
    Ok(Txid::from_str(txid.trim())?)
}

/// Txids with a `prioritisetransaction` delta, per `getprioritisedtransactions`.
async fn prioritised_txids(post_setup: &PostSetup) -> anyhow::Result<Vec<Txid>> {
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getprioritisedtransactions", [])
        .run_utf8()
        .await?;
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&json)?;
    map.keys()
        .map(|s| Txid::from_str(s).map_err(anyhow::Error::from))
        .collect()
}

pub async fn test_bmm_conflict_deprioritization(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Proposed sidechain successfully");
    let () = activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Activated sidechain successfully");

    // Mature funds for bitcoind's own wallet, which funds and signs the two
    // M8 txs below.
    let mining_address = post_setup.mining_address.to_string();
    let _res: String = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", ["101".to_owned(), mining_address])
        .run_utf8()
        .await?;

    let tip_hash: BlockHash = {
        let hash = post_setup
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "getbestblockhash", [])
            .run_utf8()
            .await?;
        BlockHash::from_str(hash.trim())?
    };

    let utxos: Vec<Utxo> = {
        let json = post_setup
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "listunspent", [])
            .run_utf8()
            .await?;
        serde_json::from_str(&json)?
    };
    let mut spendable = utxos
        .into_iter()
        .filter(|utxo| Amount::from_btc(utxo.amount).is_ok_and(|amount| amount > HIGH_FEE * 2));
    let (utxo_low, utxo_high) = match (spendable.next(), spendable.next()) {
        (Some(a), Some(b)) => (a, b),
        _ => anyhow::bail!("need two spendable UTXOs in bitcoind's wallet"),
    };

    // Two BMM requests for the same slot (same sidechain number and prev
    // mainchain block hash), committing to different sidechain blocks. The
    // low-fee one is submitted first, so the conflict is discovered when the
    // high-fee one arrives — the incumbent must lose.
    let low_fee_txid = submit_m8(&post_setup, tip_hash, [0xaa; 32], &utxo_low, LOW_FEE).await?;
    let high_fee_txid = submit_m8(&post_setup, tip_hash, [0xbb; 32], &utxo_high, HIGH_FEE).await?;
    tracing::info!(
        %low_fee_txid,
        %high_fee_txid,
        "submitted two conflicting BMM requests"
    );

    // The enforcer's mempool sync must deprioritize the low-fee request in
    // bitcoind, and must leave the high-fee one alone.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let prioritised = prioritised_txids(&post_setup).await?;
        anyhow::ensure!(
            !prioritised.contains(&high_fee_txid),
            "the high-fee BMM request {high_fee_txid} should have won the \
             conflict, but was deprioritized in bitcoind"
        );
        if prioritised.contains(&low_fee_txid) {
            // Give a buggy implementation a moment to also deprioritize the
            // winner before declaring success.
            sleep(Duration::from_millis(500)).await;
            let prioritised = prioritised_txids(&post_setup).await?;
            anyhow::ensure!(
                !prioritised.contains(&high_fee_txid),
                "the high-fee BMM request {high_fee_txid} should have won the \
                 conflict, but was deprioritized in bitcoind"
            );
            tracing::info!(
                loser = %low_fee_txid,
                winner = %high_fee_txid,
                "low-fee BMM request was deprioritized in bitcoind"
            );
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "the low-fee BMM request {low_fee_txid} was never \
                 deprioritized in bitcoind, even though it conflicts with \
                 {high_fee_txid}: Core will offer both in one template, and \
                 a block carrying both is rejected. Currently prioritised: \
                 {prioritised:?}"
            );
        }
        sleep(Duration::from_millis(200)).await;
    }
}
