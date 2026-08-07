//! An M8 (BMM request) for an inactive sidechain slot is an ordinary,
//! standard, fee-paying transaction: `OP_RETURN PUSHBYTES_68 00BF00 <S> <H>
//! <P>` at vout 0 is nulldata that Bitcoin Core relays and mines. Anyone can
//! broadcast one naming a slot with no active sidechain.
//!
//! The enforcer accepts it into its mempool mirror
//! (`Validator::validate_tx` passes `accepted_bmm_requests = None`, so slot
//! activity is never consulted), and the block producer then pairs every M8 in
//! the template with an M7 BMM accept in the coinbase
//! (`BlockProducer::finalize_block_template`). So the enforcer's own block
//! template contains the M8 *and* an M7 for the inactive slot.
//!
//! This test asserts the enforcer accepts the block it just produced. It is a
//! regression test for a consensus rule that rejects a coinbase M7 naming an
//! inactive slot: with that rule, the enforcer poisons its own template and
//! `invalidateblock`s its own block, so mining stalls for as long as the M8
//! sits in the mempool. Dropping the M7 from the coinbase does not help — then
//! the M8 has no matching commitment and the block is rejected with
//! `NotAcceptedByMiners` instead.

use std::{str::FromStr as _, time::Duration};

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    messages::M8BmmRequest,
    types::{BmmCommitment, SidechainNumber},
};
use bitcoin::{
    Amount, BlockHash, OutPoint, Transaction, TxIn, TxOut, Txid, consensus::encode::serialize_hex,
    hashes::Hash as _, transaction::Version,
};
use serde::Deserialize;
use tokio::time::sleep;

use crate::{
    block_verdict::{Expect, assert_enforcer_verdict},
    mine::mine_gbt,
    setup::PostSetup,
};

/// No sidechain is ever proposed in this test, so every slot is inactive.
const INACTIVE_SLOT: SidechainNumber = SidechainNumber(99);

const FUNDING_BLOCKS: u32 = 101;

const TX_FEE: Amount = Amount::from_sat(1_000);

const TIMEOUT: Duration = Duration::from_secs(30);

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

/// Poll the enforcer's `getblocktemplate` until `txid` is included, so the test
/// mines a block that is known to contain the BMM request rather than racing
/// the mempool mirror.
///
/// `getblocktemplate` itself must keep working: `finalize_block_template`
/// dry-run-connects the candidate block, so a consensus rule that rejects the
/// coinbase it just built surfaces here as an RPC error, and the node cannot
/// mine at all for as long as the BMM request sits in the mempool.
async fn wait_for_template_tx(post_setup: &PostSetup, txid: Txid) -> anyhow::Result<()> {
    use cusf_enforcer_mempool::server::RpcClient as _;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let mut request = bitcoin_jsonrpsee::client::BlockTemplateRequest::default();
        request.capabilities.insert("coinbasetxn".to_owned());
        let template = post_setup
            .gbt_client
            .get_block_template(request)
            .await
            .map_err(|err| {
                anyhow::anyhow!(
                    "the enforcer cannot build a block template while BMM request `{txid}` \
                     is in the mempool: {err}"
                )
            })?;
        if template.transactions.iter().any(|tx| tx.txid == txid) {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for BMM request `{txid}` to enter the enforcer's block template"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

pub async fn test_inactive_slot_bmm_request(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let mining_address = post_setup.mining_address.to_string();

    // Fund the bitcoind wallet, which stands in for an arbitrary third party
    // broadcasting the BMM request. These blocks carry no coinbase messages.
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [FUNDING_BLOCKS.to_string(), mining_address],
        )
        .run_utf8()
        .await?;

    let tip = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?
        .trim()
        .parse::<BlockHash>()?;
    // An M8 is only valid against the current tip, so wait for the enforcer to
    // validate up to it before broadcasting.
    let () = assert_enforcer_verdict(&mut post_setup, tip, Expect::Accepted, TIMEOUT).await?;

    let utxos: Vec<Utxo> = {
        let json = post_setup
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "listunspent", [])
            .run_utf8()
            .await?;
        serde_json::from_str(&json)?
    };
    let (utxo, input_value) = utxos
        .into_iter()
        .find_map(|u| {
            let amount = Amount::from_btc(u.amount).ok()?;
            (amount > TX_FEE).then_some((u, amount))
        })
        .ok_or_else(|| anyhow::anyhow!("no spendable UTXO in bitcoind wallet"))?;

    let change_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?;
    let change_address = bitcoin::Address::from_str(change_address.trim())?
        .require_network(post_setup.network.into())?;

    // BIP 301 M8: output 0 is `OP_RETURN [tag] <S> <H> <P>`. `S` names a slot
    // with no active sidechain.
    let sidechain_block_hash = BmmCommitment(
        bitcoin::hashes::sha256::Hash::hash(b"inactive slot bmm request").to_byte_array(),
    );
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
                script_pubkey: M8BmmRequest::script_pubkey(
                    INACTIVE_SLOT,
                    sidechain_block_hash,
                    tip,
                )?,
                value: Amount::ZERO,
            },
            TxOut {
                script_pubkey: change_address.script_pubkey(),
                value: input_value - TX_FEE,
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

    // The M8 is standard nulldata, so plain relay is enough — no `generateblock`
    // escape hatch is needed to get it mined.
    let bmm_request_txid = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "sendrawtransaction", [signed_hex])
        .run_utf8()
        .await?
        .trim()
        .parse::<Txid>()?;
    tracing::info!(
        %bmm_request_txid,
        %INACTIVE_SLOT,
        "broadcast BMM request for an inactive sidechain slot"
    );

    let () = wait_for_template_tx(&post_setup, bmm_request_txid).await?;

    // Mine the enforcer's own template.
    let block_hash = mine_gbt(&mut post_setup).await?;
    tracing::info!(%block_hash, "mined block containing the inactive-slot BMM request");

    // The enforcer must accept the block it just produced.
    assert_enforcer_verdict(&mut post_setup, block_hash, Expect::Accepted, TIMEOUT).await
}
