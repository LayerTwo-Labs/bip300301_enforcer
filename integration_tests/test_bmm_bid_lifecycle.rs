//! BMM bid lifecycle: fee-based bidding, bid increases via standard BIP125
//! RBF, rejection of decreased/insufficient bumps, and graceful expiry with
//! UTXO reuse across several consecutive losing auctions.
//!
//! Runs against a single, deliberately *unpatched* (stock) Bitcoin Core node
//! -- unlike `test_peer_bmm_request`, none of this relies on the
//! `cusf-enforcer-mempool` patched fork's BMM-specific mempool logic. Bid
//! placement, RBF bumping, and standard `sendrawtransaction` broadcast are
//! all expected to work against vanilla Core, since the bid is now a real
//! transaction fee rather than a burned OP_RETURN value.

use std::time::Duration;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        common::{ConsensusHex, ReverseHex},
        mainchain::{BlockHeaderInfo, CreateBmmCriticalDataTransactionRequest, GetChainTipRequest},
    },
};
use buffa::MessageField;
use connectrpc::ErrorCode;
use futures::{StreamExt as _, channel::mpsc};
use serde::Deserialize;
use tracing::Instrument as _;

use crate::{
    block_verdict::{Expect, assert_enforcer_verdict},
    integration_test::{
        activate_sidechain, fund_enforcer, propose_sidechain, wait_for_wallet_sync,
    },
    setup::{
        BitcoindKind, DummySidechain, Mode, Network, PostSetup, PreSetup, SetupOpts, Sidechain,
    },
    util::{self, BinPaths, FileDumpConfig, TestFileRegistry},
};

pub const TEST_NAME: &str = "bmm_bid_lifecycle";

#[derive(Deserialize)]
struct GenerateBlockResult {
    hash: String,
}

fn register_files(file_registry: &TestFileRegistry, directories: &crate::setup::Directories) {
    file_registry.register_file(
        TEST_NAME,
        directories.bitcoin_dir.join("stdout.txt"),
        FileDumpConfig::new().with_label("Bitcoin Core stdout"),
    );
    file_registry.register_file(
        TEST_NAME,
        directories.bitcoin_dir.join("stderr.txt"),
        FileDumpConfig::new().with_label("Bitcoin Core stderr"),
    );
    file_registry.register_file(
        TEST_NAME,
        directories.enforcer_dir.join("stdout.txt"),
        FileDumpConfig::new().with_label("Enforcer stdout"),
    );
    file_registry.register_file(
        TEST_NAME,
        directories.enforcer_dir.join("stderr.txt"),
        FileDumpConfig::new().with_label("Enforcer stderr"),
    );
}

/// Distinct h* per round, so each round's bid is unambiguous in mempool /
/// log output.
fn sidechain_block_hash(round: u8) -> [u8; 32] {
    use bitcoin::hashes::Hash;
    bitcoin::hashes::sha256::Hash::hash(format!("dummy sidechain block {round}").as_bytes())
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

async fn try_create_bid(
    post_setup: &mut PostSetup,
    height: u32,
    prev_bytes: &ReverseHex,
    h_star: &[u8; 32],
    value_sats: u64,
) -> Result<String, connectrpc::ConnectError> {
    let resp = post_setup
        .wallet_service_client
        .create_bmm_critical_data_transaction(CreateBmmCriticalDataTransactionRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            value_sats: proto::wrap_u64(value_sats),
            height: proto::wrap_u32(height),
            critical_hash: MessageField::some(ConsensusHex::encode(h_star)),
            prev_bytes: MessageField::some(prev_bytes.clone()),
        })
        .await?
        .into_owned();
    let txid = resp
        .txid
        .into_option()
        .and_then(|txid| proto::unwrap_string(txid.hex))
        .expect("response must include a txid on success");
    Ok(txid)
}

async fn create_bid(
    post_setup: &mut PostSetup,
    height: u32,
    prev_bytes: &ReverseHex,
    h_star: &[u8; 32],
    value_sats: u64,
) -> anyhow::Result<String> {
    try_create_bid(post_setup, height, prev_bytes, h_star, value_sats)
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "create_bmm_critical_data_transaction failed: {err} (code: {:?})",
                err.code
            )
        })
}

async fn mempool_contains(post_setup: &PostSetup, txid: &str) -> anyhow::Result<bool> {
    let result = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getmempoolentry", [txid.to_owned()])
        .run_utf8()
        .await;
    Ok(result.is_ok())
}

fn unix_secs() -> anyhow::Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}

/// Sleep until just after the next wall-clock second boundary, so that a
/// sequence of calls issued afterwards shares one whole second rather than
/// straddling whatever boundary happens to fall in the middle of it.
async fn wait_for_fresh_second() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let until_boundary = Duration::from_nanos(1_000_000_000 - u64::from(now.subsec_nanos()));
    tokio::time::sleep(until_boundary + Duration::from_millis(20)).await;
}

async fn get_raw_tx_hex(post_setup: &PostSetup, txid: &str) -> anyhow::Result<String> {
    let hex = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getrawtransaction", [txid.to_owned()])
        .run_utf8()
        .await?;
    Ok(hex.trim().to_owned())
}

async fn assert_in_mempool(
    post_setup: &PostSetup,
    txid: &str,
    context: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        mempool_contains(post_setup, txid).await?,
        "expected {txid} to be in the mempool ({context})",
    );
    Ok(())
}

async fn assert_not_in_mempool(
    post_setup: &PostSetup,
    txid: &str,
    context: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !mempool_contains(post_setup, txid).await?,
        "expected {txid} to have been evicted from the mempool ({context})",
    );
    Ok(())
}

async fn assert_rejected(
    post_setup: &mut PostSetup,
    height: u32,
    prev_bytes: &ReverseHex,
    h_star: &[u8; 32],
    value_sats: u64,
    context: &str,
) -> anyhow::Result<()> {
    let err = try_create_bid(post_setup, height, prev_bytes, h_star, value_sats)
        .await
        .err()
        .ok_or_else(|| {
            anyhow::anyhow!("expected bid to be rejected ({context}), but it succeeded")
        })?;
    anyhow::ensure!(
        err.code == ErrorCode::InvalidArgument,
        "expected InvalidArgument rejecting bid ({context}), got {:?}: {:?}",
        err.code,
        err.message,
    );
    Ok(())
}

async fn test_bmm_bid_lifecycle_task(mut post_setup: PostSetup) -> anyhow::Result<()> {
    tracing::info!("Setup successfully");
    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    let () = activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    // `fund_enforcer` mines exactly 100 blocks to a fresh address, so exactly
    // one coinbase output has matured (100 confirmations) by the time it
    // returns -- a deliberately tight wallet, so the expiry loop below can't
    // pass by accident just because plenty of spare UTXOs happen to exist.
    let () = fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    tracing::info!("Funded enforcer successfully");

    // ---- Basic bid: real fee, accepted by a stock node's own mempool ----
    let (prev_bytes, height) = get_tip_info(&mut post_setup).await?;
    let h_star_1 = sidechain_block_hash(1);
    let initial_bid = 5_000u64;
    let txid_1 = create_bid(&mut post_setup, height, &prev_bytes, &h_star_1, initial_bid).await?;
    tracing::info!(%txid_1, "Created initial BMM bid");
    let () = assert_in_mempool(&post_setup, &txid_1, "initial bid").await?;

    // ---- Increase the bid: same slot, same h*, higher fee -- real RBF ----
    // Deliberately well above bitcoind's default 0.10 BTC/kvB (~10,000
    // sat/vB) "absurdly-high-fee" ceiling for a tx this size (~150 vB here),
    // to prove `max_fee_rate=0.0` actually suppresses that guard rather than
    // just moving where it's enforced.
    let bumped_bid = 2_000_000u64;
    let txid_2 = create_bid(&mut post_setup, height, &prev_bytes, &h_star_1, bumped_bid).await?;
    tracing::info!(%txid_2, "Increased BMM bid");
    anyhow::ensure!(
        txid_2 != txid_1,
        "increasing the bid should produce a new transaction"
    );
    // Stock Core's own mempool evicting the old txid (with no custom BMM
    // logic involved) is the proof that this was accepted as a genuine
    // BIP125 replacement -- an unrelated second transaction would leave the
    // first one untouched, since they don't share inputs.
    let () = assert_not_in_mempool(&post_setup, &txid_1, "after bid increase").await?;
    let () = assert_in_mempool(&post_setup, &txid_2, "after bid increase").await?;

    // ---- Idempotent resubmission at the same amount is a no-op ----
    let txid_3 = create_bid(&mut post_setup, height, &prev_bytes, &h_star_1, bumped_bid).await?;
    anyhow::ensure!(
        txid_3 == txid_2,
        "resubmitting the same amount should return the existing transaction, \
         not build a new one (got {txid_3}, expected {txid_2})"
    );

    // ---- A lower bid is rejected outright, current bid left untouched ----
    let () = assert_rejected(
        &mut post_setup,
        height,
        &prev_bytes,
        &h_star_1,
        bumped_bid - 1,
        "lower bid",
    )
    .await?;
    let () = assert_in_mempool(&post_setup, &txid_2, "after rejected lower bid").await?;

    // ---- A 1-sat bump is below BIP125's minimum increment, and rejected cleanly ----
    let () = assert_rejected(
        &mut post_setup,
        height,
        &prev_bytes,
        &h_star_1,
        bumped_bid + 1,
        "insufficient RBF bump",
    )
    .await?;
    let () = assert_in_mempool(&post_setup, &txid_2, "after rejected insufficient bump").await?;
    tracing::info!("Bid increase, idempotency, and rejection behavior verified");

    // ---- Expire several bids in a row, proving UTXOs get reused rather than exhausted ----
    // Each round bids, then a block is mined *without* including that bid
    // (via `generateblock` with an explicit empty tx list, bypassing the
    // mempool entirely) -- simulating a mainchain block someone else found
    // that doesn't include our request. The bid is now consensus-dead for
    // the superseded tip, but its transaction is *not* magically gone from
    // bitcoind's mempool -- nothing ever double-spends it away. With only
    // one spendable UTXO funded above, the next round's bid can only
    // succeed by replacing that same still-unconfirmed transaction, which
    // requires a strictly higher fee each time (BIP125) -- a real sidechain
    // client re-bidding after losing would naturally do this anyway.
    const EXPIRY_ROUNDS: u8 = 5;
    let mut round_bid = bumped_bid;
    for round in 2..=(1 + EXPIRY_ROUNDS) {
        round_bid += 20_000;
        let (prev_bytes, height) = get_tip_info(&mut post_setup).await?;
        let h_star = sidechain_block_hash(round);
        let txid = create_bid(&mut post_setup, height, &prev_bytes, &h_star, round_bid).await?;
        tracing::info!(%txid, round, round_bid, "Created bid due to expire");
        let () = assert_in_mempool(&post_setup, &txid, "bid about to expire").await?;

        let mining_address = post_setup.mining_address.to_string();
        let json = post_setup
            .bitcoin_cli
            .command::<String, _, _, _, _>([], "generateblock", [mining_address, "[]".to_owned()])
            .run_utf8()
            .await?;
        let result: GenerateBlockResult = serde_json::from_str(&json)?;
        let block_hash: bitcoin::BlockHash = result.hash.parse()?;
        let () = assert_enforcer_verdict(
            &mut post_setup,
            block_hash,
            Expect::Accepted,
            Duration::from_secs(30),
        )
        .await?;
        // Wait for the *wallet* (not just the validator) to catch up, so its
        // UTXO eviction from `connect_block` has actually run before the
        // next round tries to spend the freed input.
        let () = wait_for_wallet_sync(&mut post_setup).await?;
        tracing::info!(round, "Bid expired without being mined");
    }
    tracing::info!(
        EXPIRY_ROUNDS,
        "Wallet created a fresh bid after every one of {EXPIRY_ROUNDS} consecutive losing \
         auctions with only one spendable UTXO -- no UTXO exhaustion"
    );

    // ---- A bid that WINS must not keep being reported as in-flight ----
    // The counterpart to the expiry rounds above: here the bid is actually
    // mined, by a block this enforcer did not produce. Nothing clears the
    // tracking row in that case -- `delete_bmm_requests` only runs for blocks
    // we generate ourselves (`lib/block_producer/mine.rs`) -- so the winning
    // bid stays recorded as the sidechain's in-flight bid forever.
    //
    // A client that times out on its original call and retries (the exact
    // case the same-amount idempotency path exists for) then gets handed back
    // a transaction that has already been confirmed, reported as a live bid.
    let (prev_bytes_win, height_win) = get_tip_info(&mut post_setup).await?;
    let h_star_win = sidechain_block_hash(0xA1);
    let winning_bid = round_bid + 20_000;
    let winning_txid = create_bid(
        &mut post_setup,
        height_win,
        &prev_bytes_win,
        &h_star_win,
        winning_bid,
    )
    .await?;
    let () = assert_in_mempool(&post_setup, &winning_txid, "bid about to win").await?;

    // Mine a block that *includes* the bid. It has to be hand-crafted: the
    // coinbase needs a matching M7 accept (an unaccepted M8 is rejected
    // outright), and producing the block through this enforcer would consume
    // the tracking row via `delete_bmm_requests` — the very thing whose
    // absence is under test here.
    let winning_tx_hex = get_raw_tx_hex(&post_setup, &winning_txid).await?;
    let winning_block = crate::bmm_block::submit_block_with_bmm_accepts(
        &post_setup,
        &[(DummySidechain::SIDECHAIN_NUMBER, h_star_win)],
        &[&winning_tx_hex],
    )
    .await?;
    let () = assert_enforcer_verdict(
        &mut post_setup,
        winning_block,
        Expect::Accepted,
        Duration::from_secs(30),
    )
    .await?;
    let () = wait_for_wallet_sync(&mut post_setup).await?;
    anyhow::ensure!(
        !mempool_contains(&post_setup, &winning_txid).await?,
        "expected the winning bid {winning_txid} to have left the mempool by confirming"
    );
    tracing::info!(%winning_txid, %winning_block, "Bid won and confirmed");

    // Replay the identical request, as a timed-out client would.
    let replay = try_create_bid(
        &mut post_setup,
        height_win,
        &prev_bytes_win,
        &h_star_win,
        winning_bid,
    )
    .await;
    match replay {
        // Any error is acceptable here: the auction is over, and refusing is
        // a coherent answer. What must not happen is a *success* carrying a
        // transaction that is already mined.
        Err(err) => {
            tracing::info!(
                "Replay after the bid won was rejected: {err} (code: {:?})",
                err.code
            );
        }
        Ok(replayed_txid) => {
            anyhow::ensure!(
                mempool_contains(&post_setup, &replayed_txid).await?,
                "replaying a request whose bid already won returned {replayed_txid}, which is \
                 not in the mempool -- the caller is being handed a confirmed transaction and \
                 told it is a live in-flight bid (same txid as the winner: {}). Nothing clears \
                 the tracking row when a block we did not produce confirms our bid.",
                replayed_txid == winning_txid,
            );
            tracing::info!(%replayed_txid, "Replay produced a genuinely new in-flight bid");
        }
    }

    // ---- A whole replacement chain inside one wall-clock second ----
    // BDK records unconfirmed transactions with a `last_seen` timestamp in
    // whole seconds. A bid, an RBF bump of it, and a further replacement all
    // happen within milliseconds of each other, so all three routinely share
    // one `last_seen` value. Directly-conflicting transactions with equal
    // `last_seen` leave BDK's canonicalization nothing to order them by, and
    // it could go on treating a *replaced* bid as canonical -- then offer its
    // change output, which bitcoind dropped along with it, to coin selection.
    // The next broadcast died with `bad-txns-inputs-missingorspent`.
    //
    // Waiting for a second boundary first gives the sequence a full second of
    // headroom, so it reliably lands inside one second rather than depending
    // on where the boundary happens to fall.
    const SAME_SECOND_ATTEMPTS: u8 = 3;
    let mut chain_bid = winning_bid;
    let mut landed_in_one_second = false;
    for attempt in 1..=SAME_SECOND_ATTEMPTS {
        wait_for_fresh_second().await;
        let started_at = unix_secs()?;
        let (prev_bytes, height) = get_tip_info(&mut post_setup).await?;
        let h_star_chain = sidechain_block_hash(0xB0 + attempt);

        chain_bid += 20_000;
        let chain_a = create_bid(
            &mut post_setup,
            height,
            &prev_bytes,
            &h_star_chain,
            chain_bid,
        )
        .await?;
        // Same slot, same h*, higher bid: a plain RBF fee bump of `chain_a`.
        chain_bid += 20_000;
        let chain_b = create_bid(
            &mut post_setup,
            height,
            &prev_bytes,
            &h_star_chain,
            chain_bid,
        )
        .await?;
        // Same slot, *different* h*: a content replacement of `chain_b`.
        // This is the call that used to fail — it would try to reuse
        // `chain_b`'s inputs, be told they were unavailable, silently fall
        // back to fresh coin selection, and spend `chain_a`'s change.
        chain_bid += 20_000;
        let h_star_chain_alt = sidechain_block_hash(0xC0 + attempt);
        let chain_c = create_bid(
            &mut post_setup,
            height,
            &prev_bytes,
            &h_star_chain_alt,
            chain_bid,
        )
        .await?;

        if unix_secs()? != started_at {
            tracing::warn!(
                attempt,
                "replacement chain spilled across a second boundary; retrying"
            );
            continue;
        }
        landed_in_one_second = true;

        // Exactly one live bid: each link genuinely replaced the last, rather
        // than spawning independent transactions.
        anyhow::ensure!(
            mempool_contains(&post_setup, &chain_c).await?,
            "the final bid {chain_c} of a same-second replacement chain is not in the mempool"
        );
        for (label, superseded) in [("first", &chain_a), ("bumped", &chain_b)] {
            anyhow::ensure!(
                !mempool_contains(&post_setup, superseded).await?,
                "the {label} bid {superseded} is still in the mempool alongside its \
                 replacement {chain_c} -- the chain produced independent transactions \
                 instead of BIP125 replacements"
            );
        }
        tracing::info!(
            %chain_a, %chain_b, %chain_c,
            "Replacement chain within a single second resolved to one live bid"
        );
        break;
    }
    anyhow::ensure!(
        landed_in_one_second,
        "could not land a bid/bump/replace sequence inside one wall-clock second in \
         {SAME_SECOND_ATTEMPTS} attempts; the same-second regression is not being covered"
    );

    // ---- A bid the wallet can't afford is rejected cleanly, enforcer stays alive ----
    let (prev_bytes, height) = get_tip_info(&mut post_setup).await?;
    let h_star_overspend = sidechain_block_hash(0xFF);
    let err = try_create_bid(
        &mut post_setup,
        height,
        &prev_bytes,
        &h_star_overspend,
        // Far beyond a single regtest coinbase reward.
        10_000 * bitcoin::Amount::ONE_BTC.to_sat(),
    )
    .await
    .err()
    .ok_or_else(|| anyhow::anyhow!("expected an unaffordable bid to be rejected"))?;
    anyhow::ensure!(
        err.code == ErrorCode::InvalidArgument,
        "expected InvalidArgument for an unaffordable bid, got {:?}: {:?}",
        err.code,
        err.message,
    );
    // The caller asked about a bid, so the answer has to be about the bid and
    // its balance -- not about whichever builder happened to fail first.
    let message = err.message.clone().unwrap_or_default();
    anyhow::ensure!(
        message.contains("exceeds the wallet's spendable balance"),
        "an unaffordable bid must say so plainly; got {message:?}",
    );
    tracing::info!(message = %message, "Unaffordable bid rejected clearly");
    let _tip = post_setup
        .validator_service_client
        .get_chain_tip(GetChainTipRequest::default())
        .await
        .map_err(|err| {
            anyhow::anyhow!("enforcer unresponsive after unaffordable bid (possible crash): {err}")
        })?;
    tracing::info!("Unaffordable bid rejected gracefully, enforcer still serving");

    drop(post_setup.tasks);
    Ok(())
}

/// Test the full BMM bid lifecycle against a single, unpatched bitcoind node.
pub async fn test_bmm_bid_lifecycle(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths, Network::Regtest)?;
    register_files(&file_registry, &pre_setup.directories);
    let setup_opts: SetupOpts = SetupOpts {
        bitcoind_args: Vec::new(),
        bitcoind_kind: BitcoindKind::Unpatched,
        enforcer_args: Vec::new(),
        enforcer_wallet: Default::default(),
    };
    let post_setup = pre_setup
        .setup(Mode::GetBlockTemplate, setup_opts, res_tx.clone())
        .await?;
    let _test_task: util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = test_bmm_bid_lifecycle_task(post_setup).await;
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
