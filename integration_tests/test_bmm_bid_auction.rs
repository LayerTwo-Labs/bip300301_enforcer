//! Competing BMM bids must resolve to one M7 accept per slot, with the
//! highest bid winning, losers excluded from the block, stale requests kept
//! out of later blocks, and losing bids evicted from the enforcer wallet.
//!
//! The enforcer wallet places at most one bid per tip: a second unconfirmed
//! bid from the same BDK wallet either chains onto or conflicts with the
//! first, depending on coin selection. Competing bids are crafted through
//! Bitcoin Core's wallet from explicitly chosen mature UTXOs instead.

use bip300301_enforcer_lib::{
    bins::CommandExt,
    messages::{CoinbaseMessage, M7BmmAccept, M8BmmRequest},
    proto::{
        self,
        common::{ConsensusHex, ReverseHex},
        mainchain::{
            BlockHeaderInfo, CreateBmmCriticalDataTransactionRequest, GetChainTipRequest,
            GetSidechainsRequest, ListUnspentOutputsRequest,
        },
    },
    types::{BmmCommitment, SidechainNumber},
};
use bitcoin::{
    Amount, BlockHash, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, WPubkeyHash,
    Weight, Witness, absolute::LockTime, hashes::Hash as _, transaction::Version as TxVersion,
};
use buffa::MessageField;

use crate::{
    integration_test::{
        fund_enforcer, propose_sidechain, propose_sidechain_for_slot, wait_for_wallet_sync,
    },
    mine::mine_check_block_events,
    setup::{DummySidechain, Mode, PostSetup, Sidechain, wait_for_tx_in_mempool, wait_until},
};

const OTHER_SLOT: SidechainNumber = SidechainNumber(1);

const LOW_BID: u64 = 2_000;
const HIGH_BID: u64 = 25_000;
const OTHER_SLOT_BID: u64 = 7_000;
const LOSING_BID: u64 = 10_000;
const OUTSIDE_BID: u64 = 40_000;
/// Must exceed [`LOSING_BID`]: if coin selection reuses the freed UTXO while
/// the lost bid still sits in Bitcoin Core's mempool, the broadcast has to be
/// a valid RBF replacement.
const REBID: u64 = 25_000;
/// Far above Bitcoin Core's default RPC fee-rate cap (0.10 BTC/kvB, about
/// 1.5M sats on a bid-sized transaction): the bid is a deliberate fee, so it
/// must broadcast anyway.
const JUMBO_BID: u64 = 5_000_000;
/// More than the wallet holds, so bid creation must fail cleanly.
const ABSURD_BID: u64 = 21_000_000 * 100_000_000;
/// Round 5 pits fee rate against absolute fee: the small bid has the higher
/// fee rate, the padded bid the higher absolute fee. The bid is the absolute
/// fee, so the padded bid must win.
const SMALL_HIGH_FEERATE_BID: u64 = 15_000;
const BIG_LOW_FEERATE_BID: u64 = 30_000;
/// Inflates the big bid to roughly 10x the small bid's size, putting its fee
/// rate well below the small bid's.
const BIG_BID_PAD_OUTPUTS: usize = 40;
/// Round 6 replays round 5's skew against a full block: this many unrelated
/// transactions, each at a fee rate far above the winning bid's, compete with
/// it for block space. At [`FILLER_OUTPUTS`] outputs apiece they weigh about
/// 4.5M weight units between them, more than the roughly 4M a block holds.
const CONGESTION_FILLERS: usize = 20;
/// Outputs in a full-size filler. Each one costs 124 weight units, putting a
/// filler at roughly 224k -- well under the 400k standard transaction limit,
/// and small enough that the signed transaction's hex still fits in the
/// single `bitcoin-cli` argument it is passed as.
const FILLER_OUTPUTS: usize = 1_800;
/// Running out of space is not enough on its own: selection skips past what
/// does not fit and keeps going, so full-size fillers alone would leave a
/// filler's worth of room -- far more than a bid needs. A tail of fillers
/// halving from [`FILLER_OUTPUTS`] down to this takes that room out in
/// shrinking bites, and what is left over at the end is smaller than the
/// smallest of them: about 1.2k weight units, a fraction of the padded bid's.
const FILLER_MIN_OUTPUTS: usize = 7;
/// What signing adds to a filler's one P2WPKH input: a signature and a
/// pubkey. The fee has to be settled before the witness exists, so allow for
/// it up front.
const FILLER_WITNESS_WEIGHT: Weight = Weight::from_wu(110);
/// The first filler's fee rate; each subsequent one goes a satoshi lower, so
/// that ancestor-fee-rate selection takes them in the order they are built --
/// full-size ones first, then the shrinking tail. Every one of them stays far
/// above the padded bid's roughly 20 sat/vB, which is what puts the bid last.
const FILLER_FEE_RATE_SAT_VB: u64 = 500;
/// How much of a block the fillers must occupy before round 6's assertions
/// mean anything. The fillers are witness-light, so this is around 2.8M
/// weight units' worth: a template only gets there once selection has taken
/// as many of them as it has room for.
const CONGESTED_TEMPLATE_MIN_BYTES: usize = 700_000;

fn h_star(label: &str) -> [u8; 32] {
    bitcoin::hashes::sha256::Hash::hash(label.as_bytes()).to_byte_array()
}

/// The validator's chain tip: the proto hash (for `prev_bytes`), the decoded
/// hash, and the height.
async fn chain_tip(post_setup: &mut PostSetup) -> anyhow::Result<(ReverseHex, BlockHash, u32)> {
    let header_info = post_setup
        .validator_service_client
        .get_chain_tip(GetChainTipRequest::default())
        .await?
        .into_owned()
        .block_header_info
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("expected `block_header_info` in GetChainTipResponse"))?;
    let height = header_info.height;
    let proto_hash = header_info
        .block_hash
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("expected `block_hash` in BlockHeaderInfo"))?;
    let hash: BlockHash = proto_hash
        .clone()
        .decode::<BlockHeaderInfo, _>("block_hash")?;
    Ok((proto_hash, hash, height))
}

/// Create a BMM request via the enforcer wallet gRPC, returning its txid.
async fn create_wallet_bid(
    post_setup: &mut PostSetup,
    slot: SidechainNumber,
    bid_sats: u64,
    h_star: &[u8; 32],
    prev_bytes: ReverseHex,
    tip_height: u32,
) -> anyhow::Result<Txid> {
    let txid = post_setup
        .wallet_service_client
        .create_bmm_critical_data_transaction(CreateBmmCriticalDataTransactionRequest {
            sidechain_id: proto::wrap_u32(slot.0.into()),
            value_sats: proto::wrap_u64(bid_sats),
            height: proto::wrap_u32(tip_height),
            critical_hash: MessageField::some(ConsensusHex::encode(h_star)),
            prev_bytes: MessageField::some(prev_bytes),
        })
        .await?
        .into_owned()
        .txid
        .into_option()
        .and_then(|txid| proto::unwrap_string(txid.hex))
        .ok_or_else(|| anyhow::anyhow!("expected `txid` in CreateBmmCriticalDataTransaction"))?
        .parse::<Txid>()?;
    let () = wait_for_tx_in_mempool(&post_setup.bitcoin_cli, &txid).await?;
    Ok(txid)
}

/// Craft an M8 bid through Bitcoin Core's wallet: one explicitly chosen
/// mature UTXO in, the M8 OP_RETURN at output zero, change back, the bid as
/// the exact absolute fee. No coin selection, so crafted bids never chain
/// onto unconfirmed change or touch earlier bids' UTXOs. `pad_outputs` extra
/// change outputs inflate the transaction's size, lowering its fee rate
/// without touching the bid.
async fn craft_core_bid(
    post_setup: &PostSetup,
    slot: SidechainNumber,
    bid_sats: u64,
    h_star: &[u8; 32],
    prev_hash: BlockHash,
    pad_outputs: usize,
) -> anyhow::Result<Txid> {
    let unspent_json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "listunspent", ["100".to_owned()])
        .run_utf8()
        .await?;
    let unspent: serde_json::Value = serde_json::from_str(&unspent_json)?;
    let utxo = unspent
        .as_array()
        .into_iter()
        .flatten()
        .find(|utxo| {
            utxo["spendable"].as_bool() == Some(true)
                && utxo["amount"].as_f64().is_some_and(|amount| amount >= 1.0)
        })
        .ok_or_else(|| anyhow::anyhow!("no mature Core wallet UTXO available for a crafted bid"))?;
    let utxo_txid = utxo["txid"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("listunspent entry without txid"))?;
    let utxo_vout = utxo["vout"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("listunspent entry without vout"))?;
    let utxo_value = Amount::from_btc(
        utxo["amount"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("listunspent entry without amount"))?,
    )?;
    const PAD_OUTPUT_SATS: u64 = 10_000;
    let change = utxo_value - Amount::from_sat(bid_sats + pad_outputs as u64 * PAD_OUTPUT_SATS);
    // `createrawtransaction` rejects duplicated addresses across outputs, so
    // every output needs its own.
    let mut addresses = Vec::with_capacity(pad_outputs + 1);
    for _ in 0..=pad_outputs {
        addresses.push(
            post_setup
                .bitcoin_cli
                .command::<String, _, String, _, _>([], "getnewaddress", [])
                .run_utf8()
                .await?
                .trim()
                .to_owned(),
        );
    }

    let payload = M8BmmRequest::data(slot, BmmCommitment(*h_star), prev_hash)?;
    let inputs = serde_json::json!([{"txid": utxo_txid, "vout": utxo_vout}]).to_string();
    let outputs = {
        let change_output = |address: &str, sats: Amount| {
            let mut output = serde_json::Map::new();
            output.insert(
                address.to_owned(),
                serde_json::Value::String(format!("{:.8}", sats.to_btc())),
            );
            serde_json::Value::Object(output)
        };
        let mut outputs = vec![serde_json::json!({"data": hex::encode(payload.as_bytes())})];
        let (change_addr, pad_addrs) = addresses
            .split_first()
            .expect("addresses is never empty by construction");
        outputs.push(change_output(change_addr, change));
        outputs.extend(
            pad_addrs
                .iter()
                .map(|addr| change_output(addr, Amount::from_sat(PAD_OUTPUT_SATS))),
        );
        serde_json::Value::Array(outputs).to_string()
    };
    let raw = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "createrawtransaction", [inputs, outputs])
        .run_utf8()
        .await?;
    sign_and_broadcast(post_setup, raw.trim()).await
}

/// Sign a raw transaction with Bitcoin Core's wallet and broadcast it,
/// returning its txid.
async fn sign_and_broadcast(post_setup: &PostSetup, raw_hex: &str) -> anyhow::Result<Txid> {
    let signed_json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "signrawtransactionwithwallet", [raw_hex.to_owned()])
        .run_utf8()
        .await?;
    let signed: serde_json::Value = serde_json::from_str(&signed_json)?;
    anyhow::ensure!(
        signed["complete"].as_bool() == Some(true),
        "failed to sign crafted transaction: {signed}"
    );
    let signed_hex = signed["hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("signrawtransactionwithwallet returned no hex"))?;
    let txid = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "sendrawtransaction", [signed_hex.to_owned()])
        .run_utf8()
        .await?
        .trim()
        .parse::<Txid>()?;
    Ok(txid)
}

/// The fillers' output counts: [`CONGESTION_FILLERS`] full-size ones, then a
/// tail halving down to [`FILLER_MIN_OUTPUTS`].
fn filler_output_counts() -> Vec<usize> {
    let mut counts = vec![FILLER_OUTPUTS; CONGESTION_FILLERS];
    let mut outputs = FILLER_OUTPUTS / 2;
    while outputs >= FILLER_MIN_OUTPUTS {
        counts.push(outputs);
        outputs /= 2;
    }
    counts
}

/// Congest the mempool with high-fee-rate transactions, each spending one
/// mature Bitcoin Core UTXO into outputs that exist only to take up room.
/// Every one of them outranks the auction winner on fee rate, and together
/// they weigh more than a block holds, so packing by ancestor fee rate has
/// nowhere left to put the winner by the time it reaches it.
///
/// Built here rather than sent through `sendtoaddress`, which produces
/// transactions a few hundred weight units in size: filling a block takes
/// transactions three orders of magnitude larger than that.
async fn craft_core_fillers(post_setup: &PostSetup) -> anyhow::Result<Vec<Txid>> {
    let output_counts = filler_output_counts();
    let count = output_counts.len();
    let unspent_json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "listunspent", ["100".to_owned()])
        .run_utf8()
        .await?;
    let unspent: serde_json::Value = serde_json::from_str(&unspent_json)?;
    let utxos: Vec<(OutPoint, Amount)> = unspent
        .as_array()
        .into_iter()
        .flatten()
        .filter(|utxo| {
            utxo["spendable"].as_bool() == Some(true)
                && utxo["amount"].as_f64().is_some_and(|amount| amount >= 1.0)
        })
        .take(count)
        .map(|utxo| {
            let txid = utxo["txid"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("listunspent entry without txid"))?
                .parse::<Txid>()?;
            let vout = utxo["vout"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("listunspent entry without vout"))?;
            let value = Amount::from_btc(
                utxo["amount"]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("listunspent entry without amount"))?,
            )?;
            let outpoint = OutPoint {
                txid,
                vout: vout as u32,
            };
            Ok((outpoint, value))
        })
        .collect::<anyhow::Result<_>>()?;
    anyhow::ensure!(
        utxos.len() == count,
        "expected {count} mature Core wallet UTXOs for the fillers, got {}",
        utxos.len()
    );
    // A witness program Bitcoin Core's wallet does not own. The fillers exist
    // only to take up block space, and there is no reason to make the wallet
    // track tens of thousands of outputs it will never spend.
    let filler_spk = ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array([0x11; 20]));
    let mut txids = Vec::with_capacity(count);
    for (index, (previous_output, value)) in utxos.into_iter().enumerate() {
        let outputs = output_counts[index];
        let mut tx = Transaction {
            version: TxVersion::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: filler_spk.clone(),
                };
                outputs
            ],
        };
        // Output values do not change the transaction's size, so the fee can
        // be settled before they are filled in. Every input is at least 1 BTC,
        // far above the fee, and the change spread over the outputs stays well
        // clear of the dust limit.
        let vsize = (tx.weight() + FILLER_WITNESS_WEIGHT).to_vbytes_ceil();
        let fee_rate = FILLER_FEE_RATE_SAT_VB - index as u64;
        let output_value = (value - Amount::from_sat(fee_rate * vsize)) / outputs as u64;
        for txout in &mut tx.output {
            txout.value = output_value;
        }
        let txid =
            sign_and_broadcast(post_setup, &bitcoin::consensus::encode::serialize_hex(&tx)).await?;
        let () = wait_for_tx_in_mempool(&post_setup.bitcoin_cli, &txid).await?;
        txids.push(txid);
    }
    Ok(txids)
}

async fn get_raw_transaction(
    post_setup: &PostSetup,
    txid: &Txid,
) -> anyhow::Result<bitcoin::Transaction> {
    let tx_hex = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getrawtransaction", [txid.to_string()])
        .run_utf8()
        .await?;
    let tx = bitcoin::consensus::deserialize(&hex::decode(tx_hex.trim())?)?;
    Ok(tx)
}

async fn get_block(
    post_setup: &PostSetup,
    block_hash: &BlockHash,
) -> anyhow::Result<bitcoin::Block> {
    let block_hex = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [block_hash.to_string(), "0".to_string()])
        .run_utf8()
        .await?;
    let block = bitcoin::consensus::deserialize(&hex::decode(block_hex.trim())?)?;
    Ok(block)
}

/// All M7 BMM accepts in a coinbase transaction.
fn m7_accepts(coinbase: &Transaction) -> Vec<M7BmmAccept> {
    coinbase
        .output
        .iter()
        .filter_map(|txout| match CoinbaseMessage::parse(&txout.script_pubkey) {
            Ok((_rest, CoinbaseMessage::M7BmmAccept(m7))) => Some(m7),
            _ => None,
        })
        .collect()
}

/// All M7 BMM accepts in a block's coinbase.
fn coinbase_m7_accepts(block: &bitcoin::Block) -> anyhow::Result<Vec<M7BmmAccept>> {
    let coinbase = block
        .txdata
        .first()
        .ok_or_else(|| anyhow::anyhow!("block {} has no coinbase", block.block_hash()))?;
    Ok(m7_accepts(coinbase))
}

/// All M7 BMM accepts in a block template's coinbase.
fn template_m7_accepts(
    template: &bitcoin_jsonrpsee::client::BlockTemplate,
) -> anyhow::Result<Vec<M7BmmAccept>> {
    use bitcoin_jsonrpsee::client::CoinbaseTxnOrValue;
    let CoinbaseTxnOrValue::Txn(coinbase) = &template.coinbase_txn_or_value else {
        anyhow::bail!("expected a `coinbasetxn` block template");
    };
    Ok(m7_accepts(&bitcoin::consensus::deserialize(
        &coinbase.data,
    )?))
}

/// The serialized size of everything a block template selected.
fn template_bytes(template: &bitcoin_jsonrpsee::client::BlockTemplate) -> usize {
    template.transactions.iter().map(|tx| tx.data.len()).sum()
}

/// Fetch the enforcer's block template. `Mode::GetBlockTemplate` only; that
/// is the mode serving the enforcer's own `getblocktemplate`.
async fn get_block_template(
    gbt_client: &jsonrpsee::http_client::HttpClient,
) -> anyhow::Result<bitcoin_jsonrpsee::client::BlockTemplate> {
    use cusf_enforcer_mempool::server::RpcClient as _;
    let mut gbt_request = bitcoin_jsonrpsee::client::BlockTemplateRequest::default();
    gbt_request.capabilities.insert("coinbasetxn".to_owned());
    crate::util::expect_block_template(gbt_client.get_block_template(gbt_request).await?)
}

/// Wait until the enforcer's block template selects all of `want` and none
/// of `not_want`. `Mode::GetBlockTemplate` only; the mempool mirror trails
/// bitcoind's mempool, so this must gate mining on the bids.
async fn wait_for_template_txs(
    post_setup: &PostSetup,
    want: Vec<Txid>,
    not_want: Vec<Txid>,
) -> anyhow::Result<()> {
    let gbt_client = post_setup.gbt_client.clone();
    wait_until(
        "enforcer block template to settle the BMM auction",
        move || {
            let gbt_client = gbt_client.clone();
            let want = want.clone();
            let not_want = not_want.clone();
            async move {
                let template = get_block_template(&gbt_client).await?;
                let txids: Vec<Txid> = template.transactions.iter().map(|tx| tx.txid).collect();
                Ok(want.iter().all(|txid| txids.contains(txid))
                    && not_want.iter().all(|txid| !txids.contains(txid)))
            }
        },
    )
    .await
}

/// Wait until the enforcer's block template is packed with `fillers` and has
/// had to leave at least one of them out: block space has then run out, which
/// is the state round 6 is about. The mirror trails bitcoind's mempool, and a
/// template built before the fillers reached it would carry the auction
/// winner whether or not the auction forces it in.
async fn wait_for_congested_template(
    post_setup: &PostSetup,
    fillers: Vec<Txid>,
) -> anyhow::Result<()> {
    let gbt_client = post_setup.gbt_client.clone();
    wait_until("the enforcer block template to fill up", move || {
        let gbt_client = gbt_client.clone();
        let fillers = fillers.clone();
        async move {
            let template = get_block_template(&gbt_client).await?;
            let txids: Vec<Txid> = template.transactions.iter().map(|tx| tx.txid).collect();
            let selected = fillers.iter().filter(|txid| txids.contains(txid)).count();
            Ok(selected < fillers.len()
                && template_bytes(&template) >= CONGESTED_TEMPLATE_MIN_BYTES)
        }
    })
    .await
}

/// Wait until every one of `outpoints` shows up in the enforcer wallet's
/// unspent set. A losing bid's inputs must be freed once the auction block
/// evicts the bid, not stay locked behind an unconfirmable transaction.
async fn wait_for_outpoints_unspent(
    post_setup: &PostSetup,
    outpoints: Vec<OutPoint>,
) -> anyhow::Result<()> {
    let wallet_service_client = post_setup.wallet_service_client.clone();
    wait_until(
        "the losing bid's inputs to be freed in the wallet",
        move || {
            let wallet_service_client = wallet_service_client.clone();
            let outpoints = outpoints.clone();
            async move {
                let unspent = wallet_service_client
                    .clone()
                    .list_unspent_outputs(ListUnspentOutputsRequest::default())
                    .await?
                    .into_owned()
                    .outputs;
                Ok(outpoints.iter().all(|outpoint| {
                    unspent.iter().any(|output| {
                        output.txid.clone().into_option()
                            == Some(ReverseHex::encode(&outpoint.txid))
                            && output.vout == outpoint.vout
                    })
                }))
            }
        },
    )
    .await
}

/// Regtest halves the 50 BTC subsidy every 150 blocks.
fn regtest_subsidy(height: u32) -> Amount {
    Amount::from_sat((50 * Amount::ONE_BTC.to_sat()) >> (height / 150))
}

/// Mine one block, asserting the slot-0 BMM commitment its block event
/// reports, and return the block with its height.
async fn mine_and_check_commitment(
    post_setup: &mut PostSetup,
    expected_commitment: Option<&[u8; 32]>,
) -> anyhow::Result<(bitcoin::Block, u32)> {
    let () = mine_check_block_events::<_, DummySidechain>(post_setup, 1, None, |_, block_info| {
        let bmm_commitment = block_info.bmm_commitment.into_option();
        match expected_commitment {
            Some(h_star) => anyhow::ensure!(
                bmm_commitment == Some(ConsensusHex::encode(h_star)),
                "expected the slot 0 BMM commitment to be the auction winner's"
            ),
            None => anyhow::ensure!(
                bmm_commitment.is_none(),
                "expected no slot 0 BMM commitment in this block"
            ),
        }
        Ok(())
    })
    .await?;
    let (_, block_hash, height) = chain_tip(post_setup).await?;
    let block = get_block(post_setup, &block_hash).await?;
    Ok((block, height))
}

fn block_txids(block: &bitcoin::Block) -> Vec<Txid> {
    block.txdata.iter().map(|tx| tx.compute_txid()).collect()
}

fn coinbase_value(block: &bitcoin::Block) -> Amount {
    block.txdata[0].output.iter().map(|txout| txout.value).sum()
}

/// * Round 1: the enforcer outbids a crafted bid on slot 0 while a lone
///   crafted bid takes slot 1; one block settles both slots, excludes the
///   loser, and (self-mining mode) collects the winning bids as fees
/// * The stale losing bid stays out of the next block
/// * Round 2: a crafted outside bid outbids the enforcer's own bid, and the
///   losing bid's inputs are freed in the wallet
/// * Round 3: the enforcer re-bids at the next tip and wins
/// * Round 4: a bid above the wallet balance fails cleanly, then a bid above
///   Bitcoin Core's default RPC fee-rate cap broadcasts and wins
/// * Round 5: a large bid with the higher absolute fee beats a small bid
///   with the higher fee rate
/// * Round 6 (`GetBlockTemplate` only): the same skew against a block that
///   higher-fee-rate transactions have already filled: the enforcer's
///   template carries the winner, and its M7, regardless
pub async fn test_bmm_bid_auction(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let mode = post_setup.mode;

    // Both proposals are pending before the acking blocks are mined, so both
    // slots cross the activation threshold together.
    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    let () = propose_sidechain_for_slot::<DummySidechain>(&mut post_setup, OTHER_SLOT).await?;
    let () =
        mine_check_block_events::<_, DummySidechain>(&mut post_setup, 6, Some(true), |_, _| Ok(()))
            .await?;
    let sidechains = post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest::default())
        .await?
        .into_owned()
        .sidechains;
    anyhow::ensure!(
        sidechains.len() == 2,
        "expected both sidechain slots active, got {}",
        sidechains.len()
    );
    let () = fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    // The crafted bids and the round 6 fillers spend from Core's wallet,
    // which the served coinbases do not pay in GetBlockTemplate mode. Only
    // the ones 100 blocks back are mature by the time round 6 wants a UTXO per
    // filler, so mine well past what the bids alone would need.
    let core_addr = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_owned();
    let _mined: String = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", ["140".to_owned(), core_addr])
        .run_utf8()
        .await?;
    let () = wait_for_wallet_sync(&mut post_setup).await?;
    tracing::info!("Activated both sidechains and funded both wallets");

    let h_high = h_star("round 1 winning bid");
    let h_low = h_star("round 1 losing bid");
    let h_other = h_star("round 1 other slot bid");
    let (prev_bytes, prev_hash, tip_height) = chain_tip(&mut post_setup).await?;
    let high_txid = create_wallet_bid(
        &mut post_setup,
        DummySidechain::SIDECHAIN_NUMBER,
        HIGH_BID,
        &h_high,
        prev_bytes,
        tip_height,
    )
    .await?;
    let low_txid = craft_core_bid(
        &post_setup,
        DummySidechain::SIDECHAIN_NUMBER,
        LOW_BID,
        &h_low,
        prev_hash,
        0,
    )
    .await?;
    let other_txid = craft_core_bid(
        &post_setup,
        OTHER_SLOT,
        OTHER_SLOT_BID,
        &h_other,
        prev_hash,
        0,
    )
    .await?;
    tracing::info!(%high_txid, %low_txid, %other_txid, "Placed round 1 bids");
    if let Mode::GetBlockTemplate = mode {
        let () =
            wait_for_template_txs(&post_setup, vec![high_txid, other_txid], vec![low_txid]).await?;
    }

    let (auction_block, auction_height) =
        mine_and_check_commitment(&mut post_setup, Some(&h_high)).await?;
    let auction_txids = block_txids(&auction_block);
    anyhow::ensure!(
        auction_txids.contains(&high_txid) && auction_txids.contains(&other_txid),
        "the winning bids must be included in the block"
    );
    anyhow::ensure!(
        !auction_txids.contains(&low_txid),
        "the losing slot 0 bid must not be included in the block"
    );
    let m7s = coinbase_m7_accepts(&auction_block)?;
    anyhow::ensure!(
        m7s.len() == 2,
        "expected exactly one M7 accept per bidding slot, got {}",
        m7s.len()
    );
    for (slot, expected_h_star) in [
        (DummySidechain::SIDECHAIN_NUMBER, h_high),
        (OTHER_SLOT, h_other),
    ] {
        anyhow::ensure!(
            m7s.iter()
                .any(|m7| m7.sidechain_number == slot
                    && m7.sidechain_block_hash.0 == expected_h_star),
            "missing the expected M7 accept for slot {}",
            slot.0
        );
    }
    if let Mode::Mempool = mode {
        let expected =
            regtest_subsidy(auction_height) + Amount::from_sat(HIGH_BID + OTHER_SLOT_BID);
        anyhow::ensure!(
            coinbase_value(&auction_block) == expected,
            "expected the coinbase to collect the winning bids as fees: \
             got {}, expected {expected}",
            coinbase_value(&auction_block)
        );
    }
    tracing::info!("Round 1: auction block settled both slots correctly");

    // The losing bid is now stale, but still sits in Bitcoin Core's mempool.
    let (empty_block, _) = mine_and_check_commitment(&mut post_setup, None).await?;
    anyhow::ensure!(
        !block_txids(&empty_block).contains(&low_txid),
        "the stale losing bid must not be included in a later block"
    );
    anyhow::ensure!(
        coinbase_m7_accepts(&empty_block)?.is_empty(),
        "expected no M7 accepts in a block mined without fresh bids"
    );
    tracing::info!("Stale losing bid stayed out of the next block");

    let () = wait_for_wallet_sync(&mut post_setup).await?;
    let h_losing = h_star("round 2 losing bid");
    let h_outside = h_star("round 2 outside bid");
    let (prev_bytes, prev_hash, tip_height) = chain_tip(&mut post_setup).await?;
    let losing_txid = create_wallet_bid(
        &mut post_setup,
        DummySidechain::SIDECHAIN_NUMBER,
        LOSING_BID,
        &h_losing,
        prev_bytes,
        tip_height,
    )
    .await?;
    let losing_bid_inputs: Vec<OutPoint> = get_raw_transaction(&post_setup, &losing_txid)
        .await?
        .input
        .iter()
        .map(|input| input.previous_output)
        .collect();
    let outside_txid = craft_core_bid(
        &post_setup,
        DummySidechain::SIDECHAIN_NUMBER,
        OUTSIDE_BID,
        &h_outside,
        prev_hash,
        0,
    )
    .await?;
    tracing::info!(%losing_txid, %outside_txid, "Placed round 2 bids");
    if let Mode::GetBlockTemplate = mode {
        let () = wait_for_template_txs(&post_setup, vec![outside_txid], vec![losing_txid]).await?;
    }

    let (outside_block, outside_height) =
        mine_and_check_commitment(&mut post_setup, Some(&h_outside)).await?;
    let outside_txids = block_txids(&outside_block);
    anyhow::ensure!(
        outside_txids.contains(&outside_txid) && !outside_txids.contains(&losing_txid),
        "the outside bid must win over the miner's own wallet's bid"
    );
    anyhow::ensure!(
        coinbase_m7_accepts(&outside_block)?.len() == 1,
        "expected exactly one M7 accept in the round 2 block"
    );
    if let Mode::Mempool = mode {
        let expected = regtest_subsidy(outside_height) + Amount::from_sat(OUTSIDE_BID);
        anyhow::ensure!(
            coinbase_value(&outside_block) == expected,
            "expected the coinbase to collect the outside bid as fee: \
             got {}, expected {expected}",
            coinbase_value(&outside_block)
        );
    }
    let () = wait_for_outpoints_unspent(&post_setup, losing_bid_inputs.clone()).await?;
    // The stale bid still sits in Bitcoin Core's mempool, so a chain-source
    // sync can re-adopt it into the wallet; the eviction must survive the
    // next sync cycle rather than flip back to locking the inputs.
    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    let () = wait_for_outpoints_unspent(&post_setup, losing_bid_inputs.clone()).await?;
    // The eviction must also survive a restart: the bid is still in the
    // wallet DB and in Bitcoin Core's mempool, and the block whose connect
    // evicted it will not be connected again.
    let bin_paths = crate::util::BinPaths::new();
    let (restart_res_tx, _restart_res_rx) = futures::channel::mpsc::unbounded();
    let () = post_setup
        .restart_enforcer(&bin_paths, Vec::<String>::new(), restart_res_tx)
        .await?;
    let () = wait_for_wallet_sync(&mut post_setup).await?;
    let () = wait_for_outpoints_unspent(&post_setup, losing_bid_inputs).await?;
    tracing::info!("Round 2: enforcer's losing bid was evicted, inputs freed");

    let () = wait_for_wallet_sync(&mut post_setup).await?;
    let h_rebid = h_star("round 3 re-bid");
    let (prev_bytes, _, tip_height) = chain_tip(&mut post_setup).await?;
    let rebid_txid = create_wallet_bid(
        &mut post_setup,
        DummySidechain::SIDECHAIN_NUMBER,
        REBID,
        &h_rebid,
        prev_bytes,
        tip_height,
    )
    .await?;
    tracing::info!(%rebid_txid, "Placed round 3 re-bid");
    if let Mode::GetBlockTemplate = mode {
        let () = wait_for_template_txs(&post_setup, vec![rebid_txid], vec![]).await?;
    }
    let (rebid_block, _) = mine_and_check_commitment(&mut post_setup, Some(&h_rebid)).await?;
    anyhow::ensure!(
        block_txids(&rebid_block).contains(&rebid_txid),
        "the re-bid must be included in the block"
    );
    tracing::info!("Round 3: re-bid after losing was committed");

    let () = wait_for_wallet_sync(&mut post_setup).await?;
    let h_jumbo = h_star("round 4 jumbo bid");
    let (prev_bytes, _, tip_height) = chain_tip(&mut post_setup).await?;
    let absurd_bid_result = post_setup
        .wallet_service_client
        .create_bmm_critical_data_transaction(CreateBmmCriticalDataTransactionRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            value_sats: proto::wrap_u64(ABSURD_BID),
            height: proto::wrap_u32(tip_height),
            critical_hash: MessageField::some(ConsensusHex::encode(&h_jumbo)),
            prev_bytes: MessageField::some(prev_bytes.clone()),
        })
        .await;
    anyhow::ensure!(
        absurd_bid_result.is_err(),
        "a bid exceeding the wallet balance must fail"
    );
    let jumbo_txid = create_wallet_bid(
        &mut post_setup,
        DummySidechain::SIDECHAIN_NUMBER,
        JUMBO_BID,
        &h_jumbo,
        prev_bytes,
        tip_height,
    )
    .await?;
    tracing::info!(%jumbo_txid, "Placed round 4 jumbo bid");
    if let Mode::GetBlockTemplate = mode {
        let () = wait_for_template_txs(&post_setup, vec![jumbo_txid], vec![]).await?;
    }
    let (jumbo_block, jumbo_height) =
        mine_and_check_commitment(&mut post_setup, Some(&h_jumbo)).await?;
    anyhow::ensure!(
        block_txids(&jumbo_block).contains(&jumbo_txid),
        "the jumbo bid must be included in the block"
    );
    if let Mode::Mempool = mode {
        let expected = regtest_subsidy(jumbo_height) + Amount::from_sat(JUMBO_BID);
        anyhow::ensure!(
            coinbase_value(&jumbo_block) == expected,
            "expected the coinbase to collect the jumbo bid as fee: \
             got {}, expected {expected}",
            coinbase_value(&jumbo_block)
        );
    }
    tracing::info!("Round 4: jumbo bid above the default RPC fee cap was committed");

    let h_small = h_star("round 5 small high-feerate bid");
    let h_big = h_star("round 5 big high-fee bid");
    let (_, prev_hash, _) = chain_tip(&mut post_setup).await?;
    let small_txid = craft_core_bid(
        &post_setup,
        DummySidechain::SIDECHAIN_NUMBER,
        SMALL_HIGH_FEERATE_BID,
        &h_small,
        prev_hash,
        0,
    )
    .await?;
    let big_txid = craft_core_bid(
        &post_setup,
        DummySidechain::SIDECHAIN_NUMBER,
        BIG_LOW_FEERATE_BID,
        &h_big,
        prev_hash,
        BIG_BID_PAD_OUTPUTS,
    )
    .await?;
    tracing::info!(%small_txid, %big_txid, "Placed round 5 bids");
    for txid in [&small_txid, &big_txid] {
        let () = wait_for_tx_in_mempool(&post_setup.bitcoin_cli, txid).await?;
    }
    if let Mode::GetBlockTemplate = mode {
        let () = wait_for_template_txs(&post_setup, vec![big_txid], vec![small_txid]).await?;
    }
    let (skew_block, skew_height) =
        mine_and_check_commitment(&mut post_setup, Some(&h_big)).await?;
    let skew_txids = block_txids(&skew_block);
    anyhow::ensure!(
        skew_txids.contains(&big_txid) && !skew_txids.contains(&small_txid),
        "the higher absolute fee must win the slot, not the higher fee rate"
    );
    if let Mode::Mempool = mode {
        let expected = regtest_subsidy(skew_height) + Amount::from_sat(BIG_LOW_FEERATE_BID);
        anyhow::ensure!(
            coinbase_value(&skew_block) == expected,
            "expected the coinbase to collect the big bid as fee: \
             got {}, expected {expected}",
            coinbase_value(&skew_block)
        );
    }
    tracing::info!("Round 5: absolute fee beat fee rate for the slot");

    // Round 6 replays that skew with the block already full. Settling the
    // slot by excluding the losing bid only works if the winner itself makes
    // the block, and the winner is the padded bid: every filler outranks it on
    // ancestor fee rate, and the fillers' shrinking tail leaves less room
    // behind than the bid needs. The auction's winner must be in the template
    // anyway, or the slot is settled with no bid at all.
    //
    // Only in the mode that serves the enforcer's own templates: `Mempool`
    // mode builds on Bitcoin Core's `getblocktemplate`, which knows nothing
    // of the auction, so there is no enforcer-side selection to congest.
    if let Mode::GetBlockTemplate = mode {
        // Before the bids, so the congestion is already established when they
        // arrive: a template that still had room would carry the winner on
        // merit, and the round would prove nothing. Fillers and bids alike
        // spend a mature Core UTXO apiece, and the chain was mined long enough
        // to leave more of those than the two of them together need.
        let filler_txids = craft_core_fillers(&post_setup).await?;
        let () = wait_for_congested_template(&post_setup, filler_txids.clone()).await?;
        tracing::info!(
            fillers = filler_txids.len(),
            "Filled the block template ahead of the round 6 bids"
        );

        let h_loser = h_star("round 6 small high-feerate bid");
        let h_winner = h_star("round 6 big high-fee bid");
        let (_, prev_hash, _) = chain_tip(&mut post_setup).await?;
        let loser_txid = craft_core_bid(
            &post_setup,
            DummySidechain::SIDECHAIN_NUMBER,
            SMALL_HIGH_FEERATE_BID,
            &h_loser,
            prev_hash,
            0,
        )
        .await?;
        let winner_txid = craft_core_bid(
            &post_setup,
            DummySidechain::SIDECHAIN_NUMBER,
            BIG_LOW_FEERATE_BID,
            &h_winner,
            prev_hash,
            BIG_BID_PAD_OUTPUTS,
        )
        .await?;
        for txid in [&loser_txid, &winner_txid] {
            let () = wait_for_tx_in_mempool(&post_setup.bitcoin_cli, txid).await?;
        }
        tracing::info!(%loser_txid, %winner_txid, "Placed round 6 bids into a congested mempool");

        let () = wait_for_template_txs(&post_setup, vec![winner_txid], vec![loser_txid]).await?;
        let template = get_block_template(&post_setup.gbt_client).await?;
        anyhow::ensure!(
            template_bytes(&template) >= CONGESTED_TEMPLATE_MIN_BYTES,
            "the template must still be full where the round reads it, got {} bytes",
            template_bytes(&template)
        );
        let template_txids: Vec<Txid> = template.transactions.iter().map(|tx| tx.txid).collect();
        anyhow::ensure!(
            template_txids.contains(&winner_txid),
            "the auction winner must be in the template even when higher-fee-rate \
             transactions have already taken the block's space"
        );
        anyhow::ensure!(
            !template_txids.contains(&loser_txid),
            "the losing bid must stay out of the congested template"
        );
        // The M7 follows the M8s the template actually carries, so a winner
        // squeezed out of it would leave the slot with no accept at all.
        let template_m7s = template_m7_accepts(&template)?;
        anyhow::ensure!(
            template_m7s.len() == 1
                && template_m7s[0].sidechain_number == DummySidechain::SIDECHAIN_NUMBER
                && template_m7s[0].sidechain_block_hash.0 == h_winner,
            "expected exactly one M7 accept, committing the congested round's winner"
        );
        tracing::info!("Round 6: the auction winner survived a full block template");
    }

    Ok(())
}
