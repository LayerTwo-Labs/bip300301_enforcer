//! Several sidechains bidding, depositing and withdrawing at the same time.
//!
//! Every other BMM test drives a single sidechain, which hides the bookkeeping
//! that is keyed globally rather than per sidechain: `tracked_bmm_txids` spans
//! every sidechain, `bid_seq` is one counter shared by all of them, and a
//! block settles some sidechains' bids while leaving others' in the mempool.
//!
//! Deposits and withdrawals run alongside the bidding, so the policy tables
//! are written for several reasons at once.

use std::{str::FromStr as _, time::Duration};

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        common::{ConsensusHex, ReverseHex},
        mainchain::{BlockHeaderInfo, CreateBmmCriticalDataTransactionRequest, GetChainTipRequest},
    },
    types::SidechainNumber,
};
use buffa::MessageField;
use futures::{StreamExt as _, channel::mpsc};
use tracing::Instrument as _;

use crate::{
    block_verdict::{Expect, assert_enforcer_verdict},
    integration_test::{
        activate_sidechains, deposit, fund_enforcer, fund_enforcer_with_utxos, propose_sidechain,
        wait_for_wallet_sync,
    },
    setup::{DummySidechainImpl, Mode, Network, PostSetup, PreSetup, SetupOpts, Sidechain},
    util::{self, BinPaths, FileDumpConfig, TestFileRegistry},
};

pub const TEST_NAME: &str = "bmm_multi_sidechain";
pub const BID_ISOLATION_TEST_NAME: &str = "bmm_bid_isolation";
pub const MISSING_INPUT_BLOCK_TEST_NAME: &str = "bmm_missing_input_block";
pub const DUPLICATE_BID_BLOCK_TEST_NAME: &str = "bmm_duplicate_bid_block";

type Sc0 = DummySidechainImpl<0>;
type Sc1 = DummySidechainImpl<1>;
type Sc2 = DummySidechainImpl<2>;

/// The slots under test, in the order they are driven.
const SLOTS: [u8; 3] = [0, 1, 2];

/// Opening bid per sidechain, all against the same tip.
const OPENING_BID: u64 = 100_000;

/// Set above every opening bid, so each sidechain's second bid replaces its
/// own first rather than being refused as a decrease.
const POST_SETTLEMENT_BID: u64 = 900_000;

/// The sidechain left unconfirmed by the settling block.
const LOSER_SLOT: u8 = 2;

fn register_files(
    file_registry: &TestFileRegistry,
    test_name: &str,
    directories: &crate::setup::Directories,
) {
    for (path, label) in [
        (
            directories.bitcoin_dir.join("stdout.txt"),
            "Bitcoin Core stdout",
        ),
        (
            directories.bitcoin_dir.join("stderr.txt"),
            "Bitcoin Core stderr",
        ),
        (
            directories.enforcer_dir.join("stdout.txt"),
            "Enforcer stdout",
        ),
        (
            directories.enforcer_dir.join("stderr.txt"),
            "Enforcer stderr",
        ),
    ] {
        file_registry.register_file(test_name, path, FileDumpConfig::new().with_label(label));
    }
}

/// Distinct h* per (slot, round), so no two bids in this test ever share a
/// commitment and every assertion names an unambiguous transaction.
fn sidechain_block_hash(slot: u8, round: u8) -> [u8; 32] {
    use bitcoin::hashes::Hash;
    bitcoin::hashes::sha256::Hash::hash(format!("multi sc{slot} round{round}").as_bytes())
        .to_byte_array()
}

async fn get_tip_info(post_setup: &mut PostSetup) -> anyhow::Result<(ReverseHex, u32)> {
    let BlockHeaderInfo {
        block_hash: tip_block_hash,
        height: tip_height,
        ..
    } = post_setup
        .validator_service_client
        .get_chain_tip(GetChainTipRequest::default())
        .await?
        .into_owned()
        .block_header_info
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("Expected `block_header_info` in GetChainTipResponse"))?;
    let tip_block_hash = tip_block_hash
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("Expected `block_hash` in BlockHeaderInfo"))?;
    Ok((tip_block_hash, tip_height))
}

async fn create_bid(
    post_setup: &mut PostSetup,
    slot: u8,
    height: u32,
    prev_bytes: &ReverseHex,
    h_star: &[u8; 32],
    value_sats: u64,
) -> anyhow::Result<String> {
    let resp = post_setup
        .wallet_service_client
        .create_bmm_critical_data_transaction(CreateBmmCriticalDataTransactionRequest {
            sidechain_id: proto::wrap_u32(slot.into()),
            value_sats: proto::wrap_u64(value_sats),
            height: proto::wrap_u32(height),
            critical_hash: MessageField::some(ConsensusHex::encode(h_star)),
            prev_bytes: MessageField::some(prev_bytes.clone()),
        })
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "bid for sidechain {slot} failed: {err} (code: {:?})",
                err.code
            )
        })?
        .into_owned();
    resp.txid
        .into_option()
        .and_then(|txid| proto::unwrap_string(txid.hex))
        .ok_or_else(|| anyhow::anyhow!("bid response for sidechain {slot} had no txid"))
}

async fn mempool_contains(post_setup: &PostSetup, txid: &str) -> anyhow::Result<bool> {
    let result = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getmempoolentry", [txid.to_owned()])
        .run_utf8()
        .await;
    Ok(result.is_ok())
}

async fn get_best_block_hash(post_setup: &PostSetup) -> anyhow::Result<bitcoin::BlockHash> {
    let hex = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?;
    Ok(hex.trim().parse()?)
}

/// The txids of every transaction in `block_hash`, coinbase first.
async fn block_txids(
    post_setup: &PostSetup,
    block_hash: &bitcoin::BlockHash,
) -> anyhow::Result<Vec<String>> {
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [block_hash.to_string()])
        .run_utf8()
        .await?;
    let block: serde_json::Value = serde_json::from_str(&json)?;
    let txids = block
        .get("tx")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("getblock: missing `tx` array"))?
        .iter()
        .filter_map(|txid| txid.as_str().map(str::to_owned))
        .collect();
    Ok(txids)
}

async fn get_raw_tx_hex(post_setup: &PostSetup, txid: &str) -> anyhow::Result<String> {
    let hex = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getrawtransaction", [txid.to_owned()])
        .run_utf8()
        .await?;
    Ok(hex.trim().to_owned())
}

/// Runs the test body, so the caller can tear `post_setup` down on both the
/// success and the failure path. Leaving teardown to a trailing statement
/// skips it on every `?`, and the leaked bitcoind/enforcer processes then hold
/// the whole suite open past its last test.
async fn test_bmm_multi_sidechain_body(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut sc0 = Sc0::setup((), post_setup, res_tx.clone()).await?;
    let mut sc1 = Sc1::setup((), post_setup, res_tx.clone()).await?;
    let mut sc2 = Sc2::setup((), post_setup, res_tx).await?;

    // ---- Three sidechains proposed, then activated together ----
    // Activation acks every pending proposal in one coinbase, so all three
    // take effect in the same run of blocks.
    let () = propose_sidechain::<Sc0>(post_setup).await?;
    let () = propose_sidechain::<Sc1>(post_setup).await?;
    let () = propose_sidechain::<Sc2>(post_setup).await?;
    let () = activate_sidechains::<Sc0>(post_setup, SLOTS.len()).await?;
    tracing::info!("Activated {} sidechains", SLOTS.len());

    // One spendable output per sidechain, plus headroom, so concurrent bids
    // get genuinely independent inputs instead of being forced to replace one
    // another.
    let () = fund_enforcer_with_utxos::<Sc0>(post_setup, 8).await?;

    // ---- A deposit on each sidechain, before any bidding ----
    let () = deposit::<Sc0>(
        post_setup,
        &mut sc0,
        "sc0 deposit",
        bitcoin::Amount::from_sat(21_000_000),
        bitcoin::Amount::from_sat(1_000_000),
    )
    .await?;
    let () = deposit::<Sc1>(
        post_setup,
        &mut sc1,
        "sc1 deposit",
        bitcoin::Amount::from_sat(21_000_000),
        bitcoin::Amount::from_sat(1_000_000),
    )
    .await?;
    let () = deposit::<Sc2>(
        post_setup,
        &mut sc2,
        "sc2 deposit",
        bitcoin::Amount::from_sat(21_000_000),
        bitcoin::Amount::from_sat(1_000_000),
    )
    .await?;
    tracing::info!("Deposited to all three sidechains");

    // ---- Interleaved bids, then interleaved bumps ----
    // Round-robin rather than per-sidechain, so `bid_seq` values for one
    // sidechain are separated by other sidechains' writes. An ordering scheme
    // that compared across sidechains would hand back the wrong transaction
    // here.
    let (prev_bytes, height) = get_tip_info(post_setup).await?;
    let mut bids = Vec::new();
    for (i, slot) in SLOTS.iter().enumerate() {
        let h_star = sidechain_block_hash(*slot, 1);
        let txid = create_bid(
            post_setup,
            *slot,
            height,
            &prev_bytes,
            &h_star,
            OPENING_BID + (i as u64 * 10_000),
        )
        .await?;
        bids.push((*slot, h_star, txid));
    }

    let bumped = bids.clone();
    // ---- One block settles some sidechains and not others ----
    // Any slot can be the one left out: bids never spend each other's change
    // (see [`bid_isolation_body`]), so omitting one cannot orphan another.
    let winners: Vec<_> = bumped
        .iter()
        .filter(|(slot, _, _)| *slot != LOSER_SLOT)
        .cloned()
        .collect();
    let loser = bumped
        .iter()
        .find(|(slot, _, _)| *slot == LOSER_SLOT)
        .cloned()
        .expect("the last-bid sidechain must have a bid");

    let mut accepts = Vec::new();
    let mut winner_hexes = Vec::new();
    for (slot, h_star, txid) in &winners {
        accepts.push((SidechainNumber(*slot), *h_star));
        winner_hexes.push(get_raw_tx_hex(&*post_setup, txid).await?);
    }
    let winner_hex_refs: Vec<&str> = winner_hexes.iter().map(String::as_str).collect();

    let settling_block =
        crate::bmm_block::submit_block_with_bmm_accepts(&*post_setup, &accepts, &winner_hex_refs)
            .await?;
    let () = assert_enforcer_verdict(
        post_setup,
        settling_block,
        Expect::Accepted,
        Duration::from_secs(30),
    )
    .await?;
    let () = wait_for_wallet_sync(post_setup).await?;

    for (slot, _, txid) in &winners {
        anyhow::ensure!(
            !mempool_contains(&*post_setup, txid).await?,
            "sidechain {slot}: winning bid {txid} should have confirmed"
        );
    }
    anyhow::ensure!(
        mempool_contains(&*post_setup, &loser.2).await?,
        "sidechain {LOSER_SLOT}: bid {} lost its auction but is not confirmed, so it must \
         still be resident in the mempool -- losing is not a double-spend",
        loser.2
    );
    tracing::info!(%settling_block, "One block settled 2 of 3 sidechains");

    // ---- Bidding again diverges by sidechain ----
    // 0 and 2 were settled, so their tracking rows are gone and the next bid
    // is built from scratch. 1's row survived, so its next bid must *replace*
    // the stranded transaction rather than leave a second one beside it.
    let (prev_bytes, height) = get_tip_info(post_setup).await?;
    for (i, slot) in SLOTS.iter().enumerate() {
        let h_star = sidechain_block_hash(*slot, 2);
        let txid = create_bid(
            post_setup,
            *slot,
            height,
            &prev_bytes,
            &h_star,
            POST_SETTLEMENT_BID + (i as u64 * 10_000),
        )
        .await?;
        anyhow::ensure!(
            mempool_contains(&*post_setup, &txid).await?,
            "sidechain {slot}: post-settlement bid {txid} is not in the mempool"
        );
        if *slot == LOSER_SLOT {
            anyhow::ensure!(
                !mempool_contains(&*post_setup, &loser.2).await?,
                "sidechain {LOSER_SLOT}: the stranded losing bid {} is still in the mempool \
                 beside its replacement {txid} -- the tracking row survived but was not used \
                 to replace it",
                loser.2
            );
        }
    }
    tracing::info!("Post-settlement bids behaved correctly per sidechain");

    // ---- A withdrawal while bids are in flight ----
    // Exercises the policy tables being written for two unrelated reasons at
    // once: `bmm_requests` for the live bids, `bundle_proposals` for this.
    let receive_address = post_setup.receive_address.clone();
    let _m6id = sc1
        .create_withdrawal(
            post_setup,
            &receive_address,
            bitcoin::Amount::from_sat(10_000_000),
            bitcoin::Amount::from_sat(1_000_000),
        )
        .await?;
    tracing::info!("Broadcast a withdrawal bundle while BMM bids were in flight");
    Ok(())
}

/// The two sidechains driven by [`bid_isolation_body`].
const ISOLATED_SLOTS: [u8; 2] = [0, 1];

/// A BMM bid never spends another BMM bid's change.
///
/// Bids that chain are bids that die together: replacing one makes bitcoind
/// evict the whole descendant set, so raising one sidechain's bid would cancel
/// another's, and the raise would have to out-pay every bid it took down.
/// Keeping them apart is what makes each sidechain's auction its own.
///
/// Funded first with a single spendable output, where a second bid has nowhere
/// to draw from *but* the first bid's change -- so it must be refused rather
/// than quietly chained. Then funded properly, where both bids stand on their
/// own inputs and raising one leaves the other alone.
async fn bid_isolation_body(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let _sc0 = Sc0::setup((), post_setup, res_tx.clone()).await?;
    let _sc1 = Sc1::setup((), post_setup, res_tx).await?;
    let () = propose_sidechain::<Sc0>(post_setup).await?;
    let () = propose_sidechain::<Sc1>(post_setup).await?;
    let () = activate_sidechains::<Sc0>(post_setup, ISOLATED_SLOTS.len()).await?;

    // ---- One output between them: the second bid is refused, not chained ----
    let () = fund_enforcer::<Sc0>(post_setup).await?;
    let (prev_bytes, height) = get_tip_info(post_setup).await?;
    let thin_h_star = sidechain_block_hash(ISOLATED_SLOTS[0], 1);
    let thin_txid = create_bid(
        post_setup,
        ISOLATED_SLOTS[0],
        height,
        &prev_bytes,
        &thin_h_star,
        OPENING_BID,
    )
    .await?;
    let err = create_bid(
        post_setup,
        ISOLATED_SLOTS[1],
        height,
        &prev_bytes,
        &sidechain_block_hash(ISOLATED_SLOTS[1], 1),
        OPENING_BID,
    )
    .await
    .err()
    .ok_or_else(|| {
        anyhow::anyhow!(
            "sidechain {}'s bid succeeded on a wallet whose only spendable output funds \
             sidechain {}'s bid -- it can only have come from that bid's change",
            ISOLATED_SLOTS[1],
            ISOLATED_SLOTS[0],
        )
    })?;
    anyhow::ensure!(
        format!("{err:#}").contains("Insufficient funds"),
        "expected the second bid to be refused for want of an output of its own, got {err:#}",
    );
    tracing::info!("Second bid refused rather than funded from the first's change");

    // Settle that bid into a block before mining any more. `generatetoaddress`
    // builds from the mempool, and a bid mined without a matching M7 in the
    // coinbase is a block the enforcer rightly rejects.
    let settling_block = crate::bmm_block::submit_block_with_bmm_accepts(
        &*post_setup,
        &[(SidechainNumber(ISOLATED_SLOTS[0]), thin_h_star)],
        &[&get_raw_tx_hex(&*post_setup, &thin_txid).await?],
    )
    .await?;
    let () = assert_enforcer_verdict(
        post_setup,
        settling_block,
        Expect::Accepted,
        Duration::from_secs(30),
    )
    .await?;
    let () = wait_for_wallet_sync(post_setup).await?;

    // ---- An output apiece: both stand, and stay independent ----
    let () = fund_enforcer_with_utxos::<Sc0>(post_setup, 4).await?;
    let (prev_bytes, height) = get_tip_info(post_setup).await?;
    let first_txid = create_bid(
        post_setup,
        ISOLATED_SLOTS[0],
        height,
        &prev_bytes,
        &sidechain_block_hash(ISOLATED_SLOTS[0], 2),
        OPENING_BID,
    )
    .await?;
    let second_txid = create_bid(
        post_setup,
        ISOLATED_SLOTS[1],
        height,
        &prev_bytes,
        &sidechain_block_hash(ISOLATED_SLOTS[1], 2),
        OPENING_BID + 10_000,
    )
    .await?;
    let second_hex = get_raw_tx_hex(&*post_setup, &second_txid).await?;
    let second_tx: bitcoin::Transaction = bitcoin::consensus::encode::deserialize_hex(&second_hex)?;
    anyhow::ensure!(
        !second_tx
            .input
            .iter()
            .any(|txin| txin.previous_output.txid.to_string() == first_txid),
        "sidechain {}'s bid {second_txid} is funded from sidechain {}'s bid {first_txid}",
        ISOLATED_SLOTS[1],
        ISOLATED_SLOTS[0],
    );

    // Raising one sidechain's bid must leave the other's alone. Where they
    // chained, this is where the other one silently disappeared.
    let raised_txid = create_bid(
        post_setup,
        ISOLATED_SLOTS[0],
        height,
        &prev_bytes,
        &sidechain_block_hash(ISOLATED_SLOTS[0], 2),
        OPENING_BID * 3,
    )
    .await?;
    anyhow::ensure!(
        mempool_contains(&*post_setup, &second_txid).await?,
        "raising sidechain {}'s bid to {raised_txid} evicted sidechain {}'s bid {second_txid}",
        ISOLATED_SLOTS[0],
        ISOLATED_SLOTS[1],
    );
    tracing::info!(%raised_txid, "Raising one sidechain's bid left the other's standing");
    Ok(())
}

/// A block must never be assembled around a transaction whose parent it left
/// out.
///
/// A bid that loses its auction is excluded from the block, but anything
/// spending its change is an ordinary transaction that no BMM rule touches.
/// Dropping the bid alone leaves that transaction's input in neither the block
/// nor the UTXO set, and `submitblock` rejects the result. The fee the coinbase
/// claims has to shed the dropped transactions too.
///
/// The descendant is a plain wallet send rather than another bid: bids no
/// longer spend one another's change (see [`bid_isolation_body`]), but nothing
/// stops an ordinary payment from doing so.
async fn missing_input_block_body(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    const SLOT: u8 = 0;

    let (res_tx, _res_rx) = mpsc::unbounded();
    let _sc0 = Sc0::setup((), post_setup, res_tx).await?;
    let () = propose_sidechain::<Sc0>(post_setup).await?;
    let () = activate_sidechains::<Sc0>(post_setup, 1).await?;
    let () = fund_enforcer_with_utxos::<Sc0>(post_setup, 4).await?;

    let (prev_bytes, height) = get_tip_info(post_setup).await?;
    let prev_hash: bitcoin::BlockHash = prev_bytes
        .clone()
        .decode::<BlockHeaderInfo, bitcoin::BlockHash>("block_hash")?;
    let bid_txid = create_bid(
        post_setup,
        SLOT,
        height,
        &prev_bytes,
        &sidechain_block_hash(SLOT, 1),
        OPENING_BID,
    )
    .await?;

    // Spend the bid's change, pinned so coin selection cannot pick anything
    // else. The M8 OP_RETURN is output 0 (BIP301), so the change is output 1.
    let destination = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_owned();
    let send_txid = post_setup
        .wallet_service_client
        .send_transaction(proto::mainchain::SendTransactionRequest {
            destinations: std::collections::HashMap::from([(destination, 1_000_000_u64)])
                .into_iter()
                .collect(),
            fee_rate: MessageField::some(proto::mainchain::send_transaction_request::FeeRate {
                fee: Some(proto::mainchain::send_transaction_request::fee_rate::Fee::Sats(10_000)),
            }),
            required_utxos: vec![proto::mainchain::send_transaction_request::RequiredUtxo {
                txid: MessageField::some(ReverseHex::encode(&bitcoin::Txid::from_str(&bid_txid)?)),
                vout: 1,
            }],
            ..Default::default()
        })
        .await?
        .into_owned()
        .txid
        .into_option()
        .and_then(|txid| proto::unwrap_string(txid.hex))
        .ok_or_else(|| anyhow::anyhow!("send response must include a txid"))?;
    tracing::info!(%bid_txid, %send_txid, "A plain send now descends from the bid");

    // Outbid the bid from bitcoind's own wallet, so it loses its auction while
    // staying in the mempool -- an enforcer-side bid would replace it instead.
    let rival_txid = crate::test_bmm_cross_bidder_competition::broadcast_competing_bid(
        &*post_setup,
        SidechainNumber(SLOT),
        prev_hash,
        sidechain_block_hash(SLOT, 2),
        bitcoin::Amount::from_sat(OPENING_BID * 5),
        0,
    )
    .await?;
    anyhow::ensure!(
        mempool_contains(&*post_setup, &bid_txid).await?,
        "the losing bid {bid_txid} must stay in the mempool: the block template still offers it",
    );
    tracing::info!(%rival_txid, "Rival outbid the sidechain's own bid");

    // Producing a block is the assertion: the auction drops the losing bid,
    // and the send must not be left behind pointing at it.
    let () = crate::mine::mine::<Sc0>(post_setup, 1, None)
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "mining a block while bid {bid_txid} lost its auction failed -- the send \
                 {send_txid} that descends from it was most likely kept while the bid itself \
                 was dropped: {err}"
            )
        })?;
    tracing::info!("Mined a block with a losing bid and its descendant both excluded");
    Ok(())
}

/// A block must carry at most one BMM request per sidechain slot.
///
/// Two transactions can carry byte-identical M8 payloads -- same sidechain,
/// same h*, same parent -- while spending different inputs, which anyone can
/// arrange by copying the OP_RETURN and funding it themselves. They are not
/// replacements of one another, so nothing in bitcoind separates them, and its
/// template offers both.
///
/// The enforcer's own mempool treats them as conflicts, so the block-template
/// path never sees more than one. Direct generation builds on bitcoind's
/// template instead, where the only thing standing between the two of them and
/// the same block is the auction deciding which transaction won -- not merely
/// which commitment did.
async fn duplicate_bid_block_body(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    const SLOT: u8 = 0;

    let (res_tx, _res_rx) = mpsc::unbounded();
    let _sc0 = Sc0::setup((), post_setup, res_tx).await?;
    let () = propose_sidechain::<Sc0>(post_setup).await?;
    let () = activate_sidechains::<Sc0>(post_setup, 1).await?;
    let () = fund_enforcer::<Sc0>(post_setup).await?;

    let (prev_bytes, _height) = get_tip_info(post_setup).await?;
    let prev_hash: bitcoin::BlockHash =
        prev_bytes.decode::<BlockHeaderInfo, bitcoin::BlockHash>("block_hash")?;
    let h_star = sidechain_block_hash(SLOT, 1);
    let mut bids = Vec::new();
    for (round, fee) in [(1u8, OPENING_BID), (2, OPENING_BID + 10_000)] {
        let txid = crate::test_bmm_cross_bidder_competition::broadcast_competing_bid(
            &*post_setup,
            SidechainNumber(SLOT),
            prev_hash,
            h_star,
            bitcoin::Amount::from_sat(fee),
            0,
        )
        .await?;
        tracing::info!(round, %txid, fee, "Broadcast a bid for the same sidechain and h*");
        bids.push(txid.to_string());
    }
    anyhow::ensure!(
        bids[0] != bids[1],
        "the two bids must be distinct transactions, got {} twice",
        bids[0],
    );

    let () = crate::mine::mine::<Sc0>(post_setup, 1, None).await?;
    let mined = block_txids(&*post_setup, &get_best_block_hash(&*post_setup).await?).await?;
    let included: Vec<&String> = bids.iter().filter(|txid| mined.contains(txid)).collect();
    anyhow::ensure!(
        included.len() == 1,
        "a block may accept at most one BMM request per sidechain, but this one carries \
         {} of the {} bids for sidechain {SLOT} at h* -- {included:?}",
        included.len(),
        bids.len(),
    );
    tracing::info!(winner = %included[0], "Block carried exactly one bid for the slot");

    // The rule is the validator's too, not just the producer's: a miner who
    // does not run this enforcer can force both into a block, and BIP301 says
    // that block is not one we may build on.
    let (prev_bytes, _height) = get_tip_info(post_setup).await?;
    let prev_hash: bitcoin::BlockHash =
        prev_bytes.decode::<BlockHeaderInfo, bitcoin::BlockHash>("block_hash")?;
    let h_star_forced = sidechain_block_hash(SLOT, 2);
    let mut forced_hexes = Vec::new();
    for fee in [OPENING_BID, OPENING_BID + 10_000] {
        let txid = crate::test_bmm_cross_bidder_competition::broadcast_competing_bid(
            &*post_setup,
            SidechainNumber(SLOT),
            prev_hash,
            h_star_forced,
            bitcoin::Amount::from_sat(fee),
            0,
        )
        .await?;
        forced_hexes.push(get_raw_tx_hex(&*post_setup, &txid.to_string()).await?);
    }
    let forced_hex_refs: Vec<&str> = forced_hexes.iter().map(String::as_str).collect();
    let poisoned_block = crate::bmm_block::submit_block_with_bmm_accepts(
        &*post_setup,
        &[(SidechainNumber(SLOT), h_star_forced)],
        &forced_hex_refs,
    )
    .await?;
    let () = assert_enforcer_verdict(
        post_setup,
        poisoned_block,
        Expect::Rejected {
            log_contains: "Multiple BMM requests accepted in sidechain slot",
        },
        Duration::from_secs(30),
    )
    .await?;
    tracing::info!(%poisoned_block, "Enforcer rejected a block carrying two bids for one slot");
    Ok(())
}

async fn duplicate_bid_block_task(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let res = duplicate_bid_block_body(&mut post_setup).await;
    drop(post_setup.tasks);
    res
}

async fn test_bmm_multi_sidechain_task(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let res = test_bmm_multi_sidechain_body(&mut post_setup).await;
    drop(post_setup.tasks);
    res
}

async fn missing_input_block_task(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let res = missing_input_block_body(&mut post_setup).await;
    drop(post_setup.tasks);
    res
}

async fn bid_isolation_task(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let res = bid_isolation_body(&mut post_setup).await;
    drop(post_setup.tasks);
    res
}

/// Several sidechains bidding, depositing and withdrawing concurrently.
pub async fn test_bmm_multi_sidechain(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths, Network::Regtest)?;
    register_files(&file_registry, TEST_NAME, &pre_setup.directories);
    let post_setup = pre_setup
        .setup(
            Mode::Mempool,
            SetupOpts {
                bitcoind_args: Vec::<String>::new(),
                bitcoind_kind: Default::default(),
                enforcer_args: Vec::<String>::new(),
                enforcer_wallet: Default::default(),
            },
            res_tx.clone(),
        )
        .await?;
    let _test_task: util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = test_bmm_multi_sidechain_task(post_setup).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .in_current_span()
    })
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("Unexpected end of test task result stream"))?
}

/// A block must carry at most one BMM request per sidechain slot.
pub async fn test_bmm_duplicate_bid_block(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths, Network::Regtest)?;
    register_files(
        &file_registry,
        DUPLICATE_BID_BLOCK_TEST_NAME,
        &pre_setup.directories,
    );
    let post_setup = pre_setup
        .setup(
            Mode::Mempool,
            SetupOpts {
                bitcoind_args: Vec::<String>::new(),
                bitcoind_kind: Default::default(),
                enforcer_args: Vec::<String>::new(),
                enforcer_wallet: Default::default(),
            },
            res_tx.clone(),
        )
        .await?;
    let _test_task: util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = duplicate_bid_block_task(post_setup).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .in_current_span()
    })
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("Unexpected end of test task result stream"))?
}

/// A block must never be built around a transaction whose parent it dropped.
pub async fn test_bmm_missing_input_block(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths, Network::Regtest)?;
    register_files(
        &file_registry,
        MISSING_INPUT_BLOCK_TEST_NAME,
        &pre_setup.directories,
    );
    let post_setup = pre_setup
        .setup(
            Mode::Mempool,
            SetupOpts {
                bitcoind_args: Vec::<String>::new(),
                bitcoind_kind: Default::default(),
                enforcer_args: Vec::<String>::new(),
                enforcer_wallet: Default::default(),
            },
            res_tx.clone(),
        )
        .await?;
    let _test_task: util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = missing_input_block_task(post_setup).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .in_current_span()
    })
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("Unexpected end of test task result stream"))?
}

/// A BMM bid never spends another BMM bid's change. Needs its own node: it
/// funds the wallet twice over, first with a single output and then with
/// several.
pub async fn test_bmm_bid_isolation(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths, Network::Regtest)?;
    register_files(
        &file_registry,
        BID_ISOLATION_TEST_NAME,
        &pre_setup.directories,
    );
    let post_setup = pre_setup
        .setup(
            Mode::Mempool,
            SetupOpts {
                bitcoind_args: Vec::<String>::new(),
                bitcoind_kind: Default::default(),
                enforcer_args: Vec::<String>::new(),
                enforcer_wallet: Default::default(),
            },
            res_tx.clone(),
        )
        .await?;
    let _test_task: util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = bid_isolation_task(post_setup).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .in_current_span()
    })
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("Unexpected end of test task result stream"))?
}
