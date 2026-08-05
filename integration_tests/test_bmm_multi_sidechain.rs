//! Several sidechains bidding, depositing and withdrawing at the same time.
//!
//! Every other BMM test drives a single sidechain, which hides the bookkeeping
//! that is keyed globally rather than per sidechain: `tracked_bmm_txids` spans
//! every sidechain, `bid_seq` is one counter shared by all of them, and a
//! block settles some sidechains' bids while leaving others' in the mempool.
//!
//! Deposits and withdrawals run alongside the bidding, so the policy tables
//! are written for several reasons at once.

use std::time::Duration;

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
pub const BID_CHAIN_EVICTION_TEST_NAME: &str = "bmm_bid_chain_eviction";
pub const MISSING_INPUT_BLOCK_TEST_NAME: &str = "bmm_missing_input_block";

type Sc0 = DummySidechainImpl<0>;
type Sc1 = DummySidechainImpl<1>;
type Sc2 = DummySidechainImpl<2>;

/// The slots under test, in the order they are driven.
const SLOTS: [u8; 3] = [0, 1, 2];

/// Opening bid per sidechain, all against the same tip.
const OPENING_BID: u64 = 100_000;

/// Set to dominate every opening bid combined: bids from one wallet often
/// chain off each other's change, and replacing an ancestor means out-paying
/// the whole chain.
///
/// RBF bumping is left to `bmm_bid_lifecycle`. Here it would only evict the
/// other sidechains' bids, which tests the chain rather than this test's
/// subject -- see [`bid_chain_eviction_body`] for that.
const POST_SETTLEMENT_BID: u64 = 900_000;

/// The sidechain left unconfirmed by the settling block. Must be the slot bid
/// *last*, so that omitting it from the block cannot orphan an ancestor.
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
    // The sidechains bid from one wallet, so coin selection often funds each
    // bid from the previous one's change and the in-flight bids form a
    // descendant chain. A block may omit a descendant but never an ancestor,
    // so the sidechain left unconfirmed has to be the one bid *last* -- with
    // any other choice the block would be invalid on the runs that chained.
    // Everything before it is confirmed together.
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

/// Bumping one sidechain's bid must not strand another sidechain.
///
/// The wallet is funded with a single spendable output, so the second bid can
/// only come from the first's change: the two chain. Replacing the ancestor
/// then makes Core evict the descendant with it, which is correct and
/// unavoidable. What must not follow is the enforcer carrying on as though
/// the descendant were live -- its row stays tracked, so the next bid for that
/// sidechain is built against a transaction bitcoind has already discarded.
///
/// Coin selection is still free to surprise us, so the chaining is checked
/// rather than assumed: a run where it never happens fails loudly instead of
/// passing having tested nothing.
/// The two sidechains driven by [`place_chained_bids`], and how many bid pairs
/// it places before giving up on them chaining. Each round replaces the last,
/// so the attempts cost no extra UTXOs.
const CHAINED_SLOTS: [u8; 2] = [0, 1];
const CHAIN_ATTEMPTS: u8 = 6;

/// A pair of bids where the second is funded from the first's change.
struct ChainedBids {
    prev_bytes: ReverseHex,
    height: u32,
    ancestor_h_star: [u8; 32],
    ancestor_txid: String,
    descendant_txid: String,
    /// The two bids combined -- what a replacement of the ancestor must
    /// out-pay, since Core weighs it against everything it evicts.
    chain_total: u64,
}

/// Activate two sidechains on a wallet with a single spendable output, then
/// bid on both until the second is funded from the first's change.
///
/// One spendable output is the point: `fund_enforcer_with_utxos` would mature
/// a second, which the descendant could be funded from instead. Coin selection
/// is still free to surprise us, so the chaining is checked rather than
/// assumed, and never chaining fails loudly instead of passing having tested
/// nothing.
async fn place_chained_bids(post_setup: &mut PostSetup) -> anyhow::Result<ChainedBids> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let _sc0 = Sc0::setup((), post_setup, res_tx.clone()).await?;
    let _sc1 = Sc1::setup((), post_setup, res_tx).await?;
    let () = propose_sidechain::<Sc0>(post_setup).await?;
    let () = propose_sidechain::<Sc1>(post_setup).await?;
    let () = activate_sidechains::<Sc0>(post_setup, CHAINED_SLOTS.len()).await?;
    let () = fund_enforcer::<Sc0>(post_setup).await?;

    let (prev_bytes, height) = get_tip_info(post_setup).await?;
    let mut chained = None;
    for round in 1..=CHAIN_ATTEMPTS {
        // Each round replaces the previous round's bids, so every bid must
        // out-pay its predecessor.
        let ancestor_bid = OPENING_BID * round as u64;
        let descendant_bid = ancestor_bid + 10_000;
        let ancestor_h_star = sidechain_block_hash(CHAINED_SLOTS[0], round);
        let descendant_h_star = sidechain_block_hash(CHAINED_SLOTS[1], round);
        let ancestor_txid = create_bid(
            post_setup,
            CHAINED_SLOTS[0],
            height,
            &prev_bytes,
            &ancestor_h_star,
            ancestor_bid,
        )
        .await?;
        let descendant_txid = create_bid(
            post_setup,
            CHAINED_SLOTS[1],
            height,
            &prev_bytes,
            &descendant_h_star,
            descendant_bid,
        )
        .await?;

        let descendant_hex = get_raw_tx_hex(&*post_setup, &descendant_txid).await?;
        let descendant_tx: bitcoin::Transaction =
            bitcoin::consensus::encode::deserialize_hex(&descendant_hex)?;
        if descendant_tx
            .input
            .iter()
            .any(|txin| txin.previous_output.txid.to_string() == ancestor_txid)
        {
            tracing::info!(round, %ancestor_txid, %descendant_txid, "Bids chained");
            chained = Some(ChainedBids {
                prev_bytes: prev_bytes.clone(),
                height,
                ancestor_h_star,
                ancestor_txid,
                descendant_txid,
                chain_total: ancestor_bid + descendant_bid,
            });
            break;
        }
        tracing::debug!(round, "Bids funded independently, bidding again");
    }
    chained.ok_or_else(|| {
        anyhow::anyhow!(
            "no pair of bids chained in {CHAIN_ATTEMPTS} attempts, so the paths that need one \
             bid to descend from another were never reached"
        )
    })
}

/// Bumping one sidechain's bid must not strand another sidechain.
///
/// Replacing the ancestor makes Core evict the descendant with it, which is
/// correct and unavoidable. What must not follow is the enforcer carrying on
/// as though the descendant were live -- its row stays tracked, so the next
/// bid for that sidechain is built against a transaction bitcoind has already
/// discarded.
async fn bid_chain_eviction_body(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    let ChainedBids {
        prev_bytes,
        height,
        ancestor_h_star,
        ancestor_txid,
        descendant_txid,
        chain_total,
    } = place_chained_bids(post_setup).await?;
    let ancestor_slot = CHAINED_SLOTS[0];
    let descendant_slot = CHAINED_SLOTS[1];

    // Raise the ancestor, out-paying the whole chain Core evicts with it.
    let bumped_txid = create_bid(
        post_setup,
        ancestor_slot,
        height,
        &prev_bytes,
        &ancestor_h_star,
        chain_total + OPENING_BID,
    )
    .await?;
    anyhow::ensure!(
        !mempool_contains(&*post_setup, &descendant_txid).await?,
        "sidechain {descendant_slot}'s bid {descendant_txid} should have been evicted as a \
         descendant of the replaced bid {ancestor_txid}",
    );
    tracing::info!(%bumped_txid, "Raised the ancestor bid, evicting the descendant");

    // The stranded sidechain must still be able to bid.
    let h_star_retry = sidechain_block_hash(descendant_slot, CHAIN_ATTEMPTS + 1);
    let retry_txid = create_bid(
        post_setup,
        descendant_slot,
        height,
        &prev_bytes,
        &h_star_retry,
        chain_total + OPENING_BID,
    )
    .await
    .map_err(|err| {
        anyhow::anyhow!(
            "sidechain {descendant_slot} cannot bid after sidechain {ancestor_slot} bumped \
             and evicted its bid: {err}"
        )
    })?;
    anyhow::ensure!(
        mempool_contains(&*post_setup, &retry_txid).await?,
        "sidechain {descendant_slot}'s replacement bid {retry_txid} is not in the mempool",
    );
    tracing::info!(%retry_txid, "Stranded sidechain bid again successfully");
    Ok(())
}

/// A block must never be assembled around a transaction whose parent it left
/// out.
///
/// The ancestor bid is outbid by a rival from another wallet, so the auction
/// excludes it -- but the descendant, which is funded from its change, is a
/// perfectly ordinary transaction that no BMM rule touches. Dropping the
/// ancestor alone leaves the descendant's input in neither the block nor the
/// UTXO set, and `submitblock` rejects the result. The fee the coinbase claims
/// has to shed the dropped transactions too.
async fn missing_input_block_body(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    let ChainedBids {
        prev_bytes,
        height: _,
        ancestor_h_star: _,
        ancestor_txid,
        descendant_txid,
        chain_total,
    } = place_chained_bids(post_setup).await?;
    let ancestor_slot = CHAINED_SLOTS[0];

    // Outbid the ancestor from bitcoind's own wallet, so it loses its auction
    // while staying in the mempool -- an enforcer-side bid would replace it
    // instead, taking the descendant with it and testing something else.
    let prev_hash: bitcoin::BlockHash =
        prev_bytes.decode::<BlockHeaderInfo, bitcoin::BlockHash>("block_hash")?;
    let rival_h_star = sidechain_block_hash(ancestor_slot, CHAIN_ATTEMPTS + 2);
    let rival_txid = crate::test_bmm_cross_bidder_competition::broadcast_competing_bid(
        &*post_setup,
        SidechainNumber(ancestor_slot),
        prev_hash,
        rival_h_star,
        bitcoin::Amount::from_sat(chain_total + OPENING_BID),
        0,
    )
    .await?;
    anyhow::ensure!(
        mempool_contains(&*post_setup, &ancestor_txid).await?,
        "sidechain {ancestor_slot}'s losing bid {ancestor_txid} must stay in the mempool: \
         losing an auction is not a double-spend, and the block template still offers it",
    );
    tracing::info!(%rival_txid, %ancestor_txid, "Rival outbid the ancestor");

    // Producing a block is the assertion: the auction drops the ancestor, and
    // the descendant must not be left behind pointing at it.
    let () = crate::mine::mine::<Sc0>(post_setup, 1, None)
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "mining a block while sidechain {ancestor_slot}'s bid {ancestor_txid} lost its \
                 auction failed -- its descendant {descendant_txid} was most likely kept while \
                 the bid itself was dropped: {err}"
            )
        })?;
    tracing::info!("Mined a block with a losing ancestor bid excluded");
    Ok(())
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

async fn bid_chain_eviction_task(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let res = bid_chain_eviction_body(&mut post_setup).await;
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

/// Bumping one sidechain's bid must not strand another whose bid descends
/// from it. Needs its own node: the wallet is deliberately funded with a
/// single output, which is what forces the two bids to chain.
pub async fn test_bmm_bid_chain_eviction(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths, Network::Regtest)?;
    register_files(
        &file_registry,
        BID_CHAIN_EVICTION_TEST_NAME,
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
            let res = bid_chain_eviction_task(post_setup).await;
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
