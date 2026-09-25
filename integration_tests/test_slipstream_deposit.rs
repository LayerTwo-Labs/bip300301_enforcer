//! A BIP300 deposit reaches a block through slipstream: submitted straight to
//! the block template server's mempool with `submitslipstreamtx`, never to the
//! node's, and credited to the sidechain once mined.
//!
//! Also checks that the enforcer's own rules still apply to slipstream txs:
//! a treasury output that does not spend the sidechain's CTIP is refused,
//! although the node would find it consensus-valid.

use std::str::FromStr as _;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        mainchain::{GetCtipRequest, GetCtipResponse},
    },
};
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid, consensus::encode::serialize_hex,
    script::PushBytesBuf, transaction::Version,
};
use cusf_enforcer_mempool::{
    mempool::{SlipstreamRemoval, SlipstreamTxStatus},
    server::RpcClient as _,
};
use serde::Deserialize;

use crate::{
    integration_test::{activate_sidechain, propose_sidechain},
    mine::{self, MiningPolicy},
    setup::{DummySidechain, PostSetup, Sidechain as _, wait_until},
};

pub const TEST_NAME: &str = "slipstream_deposit";

const DEPOSIT_AMOUNT: Amount = Amount::from_sat(21_000_000);
const DEPOSIT_FEE: Amount = Amount::from_sat(10_000);
const SIDECHAIN_ADDRESS: &[u8] = b"slipstream sidechain address";

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

/// A spendable UTXO from the node's wallet, other than those in `skip`, worth
/// more than `min`
async fn node_wallet_utxo(
    post_setup: &PostSetup,
    min: Amount,
    skip: &[OutPoint],
) -> anyhow::Result<(OutPoint, Amount)> {
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "listunspent", [])
        .run_utf8()
        .await?;
    let utxos: Vec<Utxo> = serde_json::from_str(&json)?;
    utxos
        .into_iter()
        .find_map(|utxo| {
            let outpoint = OutPoint {
                txid: Txid::from_str(&utxo.txid).ok()?,
                vout: utxo.vout,
            };
            let amount = Amount::from_btc(utxo.amount).ok()?;
            (amount > min && !skip.contains(&outpoint)).then_some((outpoint, amount))
        })
        .ok_or_else(|| anyhow::anyhow!("no spendable UTXO over {min} in the node's wallet"))
}

async fn sign(post_setup: &PostSetup, tx: &Transaction) -> anyhow::Result<String> {
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "signrawtransactionwithwallet", [serialize_hex(tx)])
        .run_utf8()
        .await?;
    let signed: SignResult = serde_json::from_str(&json)?;
    anyhow::ensure!(signed.complete, "signrawtransactionwithwallet incomplete");
    Ok(signed.hex)
}

/// An M5 deposit to [`DummySidechain`] funded from `utxo`, with `ctip`, if
/// any, as the treasury UTXO it spends. `ctip: None` on a sidechain that has
/// one builds a tx that consensus accepts and BIP300 does not.
async fn deposit_hex(
    post_setup: &PostSetup,
    utxo: (OutPoint, Amount),
    ctip: Option<(OutPoint, Amount)>,
) -> anyhow::Result<String> {
    let change_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?;
    let change_address = bitcoin::Address::from_str(change_address.trim())?
        .require_network(post_setup.network.into())?;
    let (utxo_outpoint, utxo_value) = utxo;
    let old_ctip_value = ctip.map_or(Amount::ZERO, |(_, value)| value);
    let input = std::iter::once(utxo_outpoint)
        .chain(ctip.map(|(outpoint, _)| outpoint))
        .map(|previous_output| TxIn {
            previous_output,
            ..TxIn::default()
        })
        .collect();
    let tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input,
        output: vec![
            // BIP300 M5: the treasury output, then the address right after it
            crate::setup::op_drivechain()?.create_m5_deposit_output(
                DummySidechain::SIDECHAIN_NUMBER,
                old_ctip_value,
                DEPOSIT_AMOUNT,
            )?,
            TxOut {
                script_pubkey: ScriptBuf::new_op_return(PushBytesBuf::try_from(
                    SIDECHAIN_ADDRESS.to_vec(),
                )?),
                value: Amount::ZERO,
            },
            TxOut {
                script_pubkey: change_address.script_pubkey(),
                value: utxo_value - DEPOSIT_AMOUNT - DEPOSIT_FEE,
            },
        ],
    };
    sign(post_setup, &tx).await
}

async fn ctip(post_setup: &PostSetup) -> anyhow::Result<Option<(OutPoint, Amount)>> {
    let resp = post_setup
        .validator_service_client
        .get_ctip(GetCtipRequest {
            sidechain_number: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
        })
        .await?
        .into_owned();
    let Some(ctip) = resp.ctip.into_option() else {
        return Ok(None);
    };
    let txid = ctip
        .txid
        .into_option()
        .ok_or_else(|| proto::Error::missing_field::<GetCtipResponse>("ctip.txid"))?
        .decode::<GetCtipResponse, Txid>("ctip.txid")?;
    Ok(Some((
        OutPoint {
            txid,
            vout: ctip.vout,
        },
        Amount::from_sat(ctip.value),
    )))
}

async fn node_mempool(post_setup: &PostSetup) -> anyhow::Result<Vec<Txid>> {
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getrawmempool", [])
        .run_utf8()
        .await?;
    Ok(serde_json::from_str(&json)?)
}

/// Block until the block template server builds on the node's tip, which
/// after blocks mined straight through the node trails the node for a while.
async fn wait_for_template_on_node_tip(post_setup: &PostSetup) -> anyhow::Result<()> {
    let node_tip: bitcoin::BlockHash = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    wait_until(
        &format!("block templates to build on {node_tip}"),
        || async {
            let mut request = bitcoin_jsonrpsee::client::BlockTemplateRequest::default();
            request.capabilities.insert("coinbasetxn".to_owned());
            let template = crate::util::expect_block_template(
                post_setup.gbt_client.get_block_template(request).await?,
            )?;
            Ok(template.prev_blockhash == node_tip)
        },
    )
    .await
}

pub async fn test_slipstream_deposit(mut post_setup: PostSetup) -> anyhow::Result<()> {
    // Mature coinbases in the node's wallet, to fund the deposits from. The
    // deposits are built by hand because the enforcer's own wallet would
    // broadcast them.
    let _hashes: String = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            ["101".to_owned(), post_setup.mining_address.to_string()],
        )
        .run_utf8()
        .await?;
    let () = wait_for_template_on_node_tip(&post_setup).await?;
    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    let () = activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    anyhow::ensure!(
        ctip(&post_setup).await?.is_none(),
        "expected no treasury UTXO before the first deposit",
    );

    let min_utxo = DEPOSIT_AMOUNT + DEPOSIT_FEE;
    let utxo = node_wallet_utxo(&post_setup, min_utxo, &[]).await?;
    let deposit = deposit_hex(&post_setup, utxo, None).await?;
    let response = post_setup.gbt_client.submit_slipstream_tx(deposit).await?;
    anyhow::ensure!(
        response.accepted && response.fee_sat == Some(DEPOSIT_FEE.to_sat()),
        "slipstream deposit refused: {response:?}",
    );
    let deposit_txid = response.txid;
    tracing::info!(%deposit_txid, "submitted slipstream deposit");

    let () = mine::wait_for_tx_in_block_template(&post_setup, &deposit_txid).await?;
    anyhow::ensure!(
        !node_mempool(&post_setup).await?.contains(&deposit_txid),
        "the slipstream deposit reached the node's mempool",
    );
    let () = mine::mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;

    let new_ctip = ctip(&post_setup).await?;
    anyhow::ensure!(
        new_ctip
            == Some((
                OutPoint {
                    txid: deposit_txid,
                    vout: 0
                },
                DEPOSIT_AMOUNT
            )),
        "expected the slipstream deposit as the treasury UTXO, found {new_ctip:?}",
    );
    let status = post_setup
        .gbt_client
        .get_slipstream_tx(deposit_txid)
        .await?;
    anyhow::ensure!(
        matches!(
            status,
            SlipstreamTxStatus::Removed {
                removal: SlipstreamRemoval::Mined { .. },
                ..
            }
        ),
        "expected the deposit to be recorded as mined, found {status:?}",
    );

    // A second treasury output that does not spend the CTIP: consensus has no
    // objection, BIP300 does, and so must slipstream.
    let utxo = node_wallet_utxo(&post_setup, min_utxo, &[utxo.0]).await?;
    let bad_deposit = deposit_hex(&post_setup, utxo, None).await?;
    let response = post_setup
        .gbt_client
        .submit_slipstream_tx(bad_deposit)
        .await?;
    anyhow::ensure!(
        !response.accepted && response.reject_reason.as_deref() == Some("rejected-by-enforcer"),
        "expected a deposit that skips the CTIP to be refused by the enforcer, got {response:?}",
    );

    // Spending the CTIP, the same deposit is accepted
    let good_deposit = deposit_hex(&post_setup, utxo, new_ctip).await?;
    let response = post_setup
        .gbt_client
        .submit_slipstream_tx(good_deposit)
        .await?;
    anyhow::ensure!(
        response.accepted,
        "second slipstream deposit refused: {response:?}"
    );
    let () = mine::wait_for_tx_in_block_template(&post_setup, &response.txid).await?;
    let () = mine::mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    let new_ctip = ctip(&post_setup).await?;
    anyhow::ensure!(
        new_ctip
            == Some((
                OutPoint {
                    txid: response.txid,
                    vout: 0
                },
                DEPOSIT_AMOUNT + DEPOSIT_AMOUNT
            )),
        "expected the second deposit as the treasury UTXO, found {new_ctip:?}",
    );
    Ok(())
}
