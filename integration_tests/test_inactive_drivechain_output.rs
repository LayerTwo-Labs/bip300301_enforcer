use std::{str::FromStr as _, time::Duration};

use bip300301_enforcer_lib::types::SidechainNumber;
use bitcoin::{
    Amount, BlockHash, OutPoint, Transaction, TxIn, TxOut, Txid, consensus::encode::serialize_hex,
    transaction::Version,
};
use jsonrpsee::{core::client::ClientT as _, rpc_params};
use serde::Deserialize;

use crate::{
    block_verdict::{Expect, assert_enforcer_verdict},
    setup::PostSetup,
};

const INACTIVE_SLOT: SidechainNumber = SidechainNumber(42);

const FUNDING_BLOCKS: u32 = 101;

const TX_FEE: Amount = Amount::from_sat(1_000);

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

#[derive(Deserialize)]
struct GenerateBlockResult {
    hash: String,
}

pub async fn test_inactive_slot_drivechain_output(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let mining_address = post_setup.mining_address.to_string();

    let _block_hashes: Vec<BlockHash> = post_setup
        .bitcoind_client
        .request(
            "generatetoaddress",
            rpc_params![FUNDING_BLOCKS, mining_address.clone()],
        )
        .await?;

    let utxos: Vec<Utxo> = post_setup
        .bitcoind_client
        .request("listunspent", rpc_params![])
        .await?;
    let (utxo, input_value) = utxos
        .into_iter()
        .find_map(|u| {
            let amount = Amount::from_btc(u.amount).ok()?;
            (amount > TX_FEE).then_some((u, amount))
        })
        .ok_or_else(|| anyhow::anyhow!("no spendable UTXO in bitcoind wallet"))?;

    let change_address: String = post_setup
        .bitcoind_client
        .request("getnewaddress", rpc_params![])
        .await?;
    let change_address =
        bitcoin::Address::from_str(&change_address)?.require_network(post_setup.network.into())?;

    // Build a tx whose first output is a zero-value OP_DRIVECHAIN output for an
    // inactive slot
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
            TxOut {
                script_pubkey: crate::setup::op_drivechain()?.script(INACTIVE_SLOT),
                value: Amount::ZERO,
            },
            TxOut {
                script_pubkey: change_address.script_pubkey(),
                value: input_value - TX_FEE,
            },
        ],
    };

    let signed_hex = {
        let signed: SignResult = post_setup
            .bitcoind_client
            .request(
                "signrawtransactionwithwallet",
                rpc_params![serialize_hex(&unsigned_tx)],
            )
            .await?;
        anyhow::ensure!(signed.complete, "signrawtransactionwithwallet incomplete");
        signed.hex
    };

    // `generateblock` mines a block containing the raw tx, bypassing mempool
    // standardness (which rejects OP_DRIVECHAIN as a non-standard script) while
    // still enforcing consensus rules
    let block_hash = {
        let result: GenerateBlockResult = post_setup
            .bitcoind_client
            .request("generateblock", rpc_params![mining_address, [signed_hex]])
            .await?;
        BlockHash::from_str(&result.hash)?
    };
    tracing::info!(%block_hash, "mined block with inactive-slot OP_DRIVECHAIN output");

    // Asserting acceptance also waits for the enforcer to validate past this
    // block's height, which subsumes catching up on the funding blocks above.
    assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Accepted,
        Duration::from_secs(30),
    )
    .await
}
