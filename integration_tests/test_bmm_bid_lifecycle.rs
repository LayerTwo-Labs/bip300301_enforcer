//! BMM bid lifecycle: fee-based bidding, bid increases via standard BIP125
//! RBF, rejection of decreased/insufficient bumps, and graceful expiry with
//! UTXO reuse across several consecutive losing auctions.
//!
//! Also covers every rung of `create_bmm_request`'s replacement fallback
//! ladder (`lib/wallet/mod.rs`): the ordinary fee-bump of a canonical bid,
//! manual input reuse for a bid evicted from BDK's view, fresh builds when
//! the previous bid's inputs are truly spent or its transaction confirmed
//! while still recorded as tracked, and restoration of a settled bid's
//! tracking row when the block that settled it is disconnected by a reorg.
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
    block_producer::Db as ProducerDb,
    proto::{
        self,
        common::{ConsensusHex, ReverseHex},
        mainchain::{
            BlockHeaderInfo, CreateBmmCriticalDataTransactionRequest, GetBalanceRequest,
            GetChainTipRequest, GetInfoRequest, SendTransactionRequest, send_transaction_request,
        },
    },
    types::BmmCommitment,
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

/// The modes this runs under. `NoMempool` is not redundant with
/// `GetBlockTemplate`: it is the mode where the mempool sync task never runs,
/// so the losing-bid handling the constant-bid rounds depend on has to come
/// from block connection alone.
pub const MODES: [Mode; 2] = [Mode::GetBlockTemplate, Mode::NoMempool];

pub fn trial_name(mode: Mode) -> String {
    format!("{TEST_NAME} (mode: {mode})")
}

#[derive(Deserialize)]
struct GenerateBlockResult {
    hash: String,
}

fn register_files(
    file_registry: &TestFileRegistry,
    test_name: &str,
    directories: &crate::setup::Directories,
) {
    file_registry.register_file(
        test_name,
        directories.bitcoin_dir.join("stdout.txt"),
        FileDumpConfig::new().with_label("Bitcoin Core stdout"),
    );
    file_registry.register_file(
        test_name,
        directories.bitcoin_dir.join("stderr.txt"),
        FileDumpConfig::new().with_label("Bitcoin Core stderr"),
    );
    file_registry.register_file(
        test_name,
        directories.enforcer_dir.join("stdout.txt"),
        FileDumpConfig::new().with_label("Enforcer stdout"),
    );
    file_registry.register_file(
        test_name,
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

/// The sole previous output the given mempool transaction spends.
async fn sole_input_outpoint(
    post_setup: &PostSetup,
    txid: &str,
) -> anyhow::Result<(bitcoin::Txid, u32)> {
    #[derive(Deserialize)]
    struct Vin {
        txid: String,
        vout: u32,
    }
    #[derive(Deserialize)]
    struct RawTx {
        vin: Vec<Vin>,
    }
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "getrawtransaction",
            [txid.to_owned(), "true".to_owned()],
        )
        .run_utf8()
        .await?;
    let mut raw: RawTx = serde_json::from_str(&json)?;
    anyhow::ensure!(
        raw.vin.len() == 1,
        "expected {txid} to have exactly one input, got {}",
        raw.vin.len()
    );
    let vin = raw.vin.remove(0);
    Ok((vin.txid.parse()?, vin.vout))
}

/// Wait until the enforcer wallet's tip is exactly `expected`. Needed after
/// a reorg: `wait_for_wallet_sync` compares heights with `>=`, which a
/// wallet still on the (higher) disconnected tip satisfies vacuously.
async fn wait_for_wallet_tip(
    post_setup: &mut PostSetup,
    expected: bitcoin::BlockHash,
    timeout: Duration,
) -> anyhow::Result<()> {
    let expected = expected.to_string();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let tip_hash = post_setup
            .wallet_service_client
            .get_info(GetInfoRequest::default())
            .await?
            .into_owned()
            .tip
            .into_option()
            .and_then(|tip| tip.hash.into_option())
            .and_then(|hash| proto::unwrap_string(hash.hex));
        if tip_hash.as_deref() == Some(expected.as_str()) {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "wallet tip did not reach {expected} within {timeout:?} (at {tip_hash:?})"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
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

/// The producer's policy DB, which the tests reach into directly to construct
/// states a live enforcer cannot be raced into.
fn producer_db_dir(post_setup: &PostSetup) -> std::path::PathBuf {
    post_setup
        .directories
        .enforcer_dir
        .join("wallet")
        .join("regtest")
}

/// A bid Core accepted but whose tracking row was never written must be
/// adopted on the next startup.
///
/// `create_bmm_request` broadcasts (`lib/wallet/mod.rs`) before it upserts the
/// tracking row, and no commit can be made atomic with Core's mempool. A crash
/// or cancellation in between -- or an upsert that simply fails, which returns
/// an error to a caller whose money is already committed -- leaves an
/// expensive transaction live and untracked: the next bid builds fresh instead
/// of replacing it, leaving two live M8s for the slot, and nothing can
/// deprioritize the loser.
///
/// The window cannot be hit deterministically from outside, so the state it
/// leaves is constructed directly: bid normally, then delete the row.
async fn untracked_bid_scenario(
    post_setup: &mut PostSetup,
    bin_paths: &BinPaths,
    res_tx: &mpsc::UnboundedSender<anyhow::Result<()>>,
    bid: &mut u64,
) -> anyhow::Result<()> {
    *bid += 20_000;
    let h_star = sidechain_block_hash(0xA1);
    let (prev_bytes, height) = get_tip_info(post_setup).await?;
    let bid_txid = create_bid(post_setup, height, &prev_bytes, &h_star, *bid).await?;
    let () = assert_in_mempool(post_setup, &bid_txid, "bid about to lose its tracking row").await?;

    let () = post_setup.kill_enforcer().await?;
    let deleted = {
        use bitcoin::hashes::Hash as _;
        let connection = rusqlite::Connection::open(producer_db_dir(post_setup).join("db.sqlite"))?;
        connection.execute(
            "DELETE FROM bmm_requests WHERE txid = ?1",
            [bid_txid.parse::<bitcoin::Txid>()?.to_byte_array()],
        )?
    };
    anyhow::ensure!(
        deleted == 1,
        "expected to delete the tracking row of {bid_txid}, deleted {deleted} row(s) -- the \
         post-broadcast crash state is not being constructed"
    );

    let () = post_setup
        .restart_enforcer(bin_paths, Vec::<String>::new(), res_tx.clone())
        .await?;
    let () = wait_for_wallet_sync(post_setup).await?;
    // The transaction Core accepted is untouched by any of this: it is the
    // thing the restarted enforcer has to notice it owns.
    let () = assert_in_mempool(post_setup, &bid_txid, "bid left live by the crash").await?;

    let tracked = ProducerDb::new(&producer_db_dir(post_setup))?
        .get_tracked_bmm_request(DummySidechain::SIDECHAIN_NUMBER)
        .await?
        .map(|tracked| tracked.txid.to_string());
    anyhow::ensure!(
        tracked.as_deref() == Some(bid_txid.as_str()),
        "{bid_txid} is live in Core's mempool and was funded by this wallet, but after a restart \
         the producer tracks {tracked:?} -- a bid broadcast without its row ever being written is \
         never adopted, so nothing can replace or deprioritize it"
    );

    *bid += 20_000;
    let (prev_bytes, height) = get_tip_info(post_setup).await?;
    let next_txid = create_bid(
        post_setup,
        height,
        &prev_bytes,
        &sidechain_block_hash(0xA2),
        *bid,
    )
    .await?;
    let () = assert_in_mempool(post_setup, &next_txid, "bid placed after the adoption").await?;
    let () =
        assert_not_in_mempool(post_setup, &bid_txid, "adopted bid, after being replaced").await?;
    tracing::info!(
        %bid_txid, %next_txid,
        "Untracked live bid was adopted on startup; the next bid replaced it"
    );
    Ok(())
}

/// A settling block reorged out while the enforcer is down must still restore
/// the bid's tracking row.
///
/// The producer's policy undo (`restore_bmm_requests`) runs only from
/// `BlockProducer::disconnect_block`, which a live enforcer reaches through
/// its streamed block events. A reorg resolved during a restart never gets
/// there: the validator walks back to the last common ancestor inside its own
/// sync, and the wallet's `sync_to_tip` delegates straight to that. The row
/// stays stranded in `bmm_requests_undo` while bitcoind puts the bid's
/// transaction back into its mempool -- untracked, so nothing can replace or
/// deprioritize it, and the sidechain's next bid becomes a second live M8.
async fn offline_reorg_scenario(
    post_setup: &mut PostSetup,
    bin_paths: &BinPaths,
    res_tx: &mpsc::UnboundedSender<anyhow::Result<()>>,
    bid: &mut u64,
) -> anyhow::Result<()> {
    *bid += 20_000;
    let h_star = sidechain_block_hash(0xF1);
    let (prev_bytes, height) = get_tip_info(post_setup).await?;
    let bid_txid = create_bid(post_setup, height, &prev_bytes, &h_star, *bid).await?;
    let bid_hex = get_raw_tx_hex(post_setup, &bid_txid).await?;
    let settling_block = crate::bmm_block::submit_block_with_bmm_accepts(
        post_setup,
        &[(DummySidechain::SIDECHAIN_NUMBER, h_star)],
        &[&bid_hex],
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
    let () = assert_not_in_mempool(
        post_setup,
        &bid_txid,
        "settled by the block about to be reorged out",
    )
    .await?;

    // From here to the restart the enforcer is down, so the disconnect is
    // something it can only learn by catching up across it.
    let () = post_setup.kill_enforcer().await?;
    let () = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "invalidateblock", [settling_block.to_string()])
        .run_utf8()
        .await
        .map(drop)?;
    let () = assert_in_mempool(
        post_setup,
        &bid_txid,
        "returned to the mempool by the offline disconnect",
    )
    .await?;

    // Two empty blocks, so the branch replacing the settling block is strictly
    // longer: electrs, and through it the wallet, won't follow a chain that
    // merely got shorter. Empty on purpose -- mempool-based generation would
    // just confirm the restored bid again.
    let mining_address = post_setup.mining_address.to_string();
    let mut replacement_block = None;
    for _ in 0..2 {
        let json = post_setup
            .bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "generateblock",
                [mining_address.clone(), "[]".to_owned()],
            )
            .run_utf8()
            .await?;
        let result: GenerateBlockResult = serde_json::from_str(&json)?;
        replacement_block = Some(result.hash.parse::<bitcoin::BlockHash>()?);
    }
    let replacement_block =
        replacement_block.ok_or_else(|| anyhow::anyhow!("no replacement block was mined"))?;

    let () = post_setup
        .restart_enforcer(bin_paths, Vec::<String>::new(), res_tx.clone())
        .await?;
    let () = wait_for_wallet_tip(post_setup, replacement_block, Duration::from_secs(60)).await?;
    // Without the transaction actually back in the mempool there is nothing
    // for the next bid to replace, and everything below would pass vacuously.
    let () = assert_in_mempool(
        post_setup,
        &bid_txid,
        "restored to the mempool by the offline reorg",
    )
    .await?;

    let tracked = ProducerDb::new(&producer_db_dir(post_setup))?
        .get_tracked_bmm_request(DummySidechain::SIDECHAIN_NUMBER)
        .await?
        .map(|tracked| tracked.txid.to_string());
    anyhow::ensure!(
        tracked.as_deref() == Some(bid_txid.as_str()),
        "the settling block was disconnected while the enforcer was down, putting {bid_txid} \
         back in bitcoind's mempool, but the producer tracks {tracked:?} as the in-flight bid -- \
         the row stayed stranded in `bmm_requests_undo`, so nothing can replace or deprioritize \
         the restored transaction"
    );

    *bid += 20_000;
    let (prev_bytes, height) = get_tip_info(post_setup).await?;
    let next_txid = create_bid(
        post_setup,
        height,
        &prev_bytes,
        &sidechain_block_hash(0xF2),
        *bid,
    )
    .await?;
    let () =
        assert_in_mempool(post_setup, &next_txid, "bid placed after the offline reorg").await?;
    let () = assert_not_in_mempool(
        post_setup,
        &bid_txid,
        "reorg-restored bid, after being replaced",
    )
    .await?;
    tracing::info!(
        %bid_txid, %next_txid,
        "Offline reorg restored the bid's tracking row; the next bid replaced it"
    );
    Ok(())
}

async fn test_bmm_bid_lifecycle_task(
    mut post_setup: PostSetup,
    bin_paths: BinPaths,
    res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
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

    // ---- Spent-inputs fallback: the tracked bid's inputs are gone for real ----
    // Stale the bid with an empty block, then spend its (now freed) input
    // with an ordinary wallet send, and confirm that send. The tracked row
    // still points at `txid_1`, but every replacement route is dead:
    // `build_fee_bump` can't see the transaction (evicted from BDK's
    // canonical view by `connect_block`), and reusing its inputs by hand
    // fails too, because a *confirmed* transaction spent them. The wallet
    // must fall through to a fresh transaction rather than error out and
    // strand the sidechain.
    let mining_address = post_setup.mining_address.to_string();
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generateblock",
            [mining_address.clone(), "[]".to_owned()],
        )
        .run_utf8()
        .await?;
    let result: GenerateBlockResult = serde_json::from_str(&json)?;
    let staling_block: bitcoin::BlockHash = result.hash.parse()?;
    let () = assert_enforcer_verdict(
        &mut post_setup,
        staling_block,
        Expect::Accepted,
        Duration::from_secs(30),
    )
    .await?;
    let () = wait_for_wallet_sync(&mut post_setup).await?;

    // The send must actually conflict with the staled bid, so pin it to the
    // bid's input via `required_utxos` -- by now more coinbases from the
    // funding run have matured, and free coin selection could pick one of
    // those instead. Spending the same input makes the send itself a genuine
    // BIP125 replacement of the bid, so its absolute fee must beat the
    // bid's.
    let (bid_input_txid, bid_input_vout) = sole_input_outpoint(&post_setup, &txid_1).await?;
    let send_destination = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_string();
    let send_resp = post_setup
        .wallet_service_client
        .send_transaction(SendTransactionRequest {
            destinations: std::collections::HashMap::from([(send_destination, 100_000_u64)])
                .into_iter()
                .collect(),
            fee_rate: MessageField::some(send_transaction_request::FeeRate {
                fee: Some(send_transaction_request::fee_rate::Fee::Sats(
                    initial_bid + 7_000,
                )),
            }),
            required_utxos: vec![send_transaction_request::RequiredUtxo {
                txid: MessageField::some(ReverseHex::encode(&bid_input_txid)),
                vout: bid_input_vout,
            }],
            ..Default::default()
        })
        .await?
        .into_owned();
    let send_txid = send_resp
        .txid
        .into_option()
        .and_then(|txid| proto::unwrap_string(txid.hex))
        .ok_or_else(|| anyhow::anyhow!("send_transaction response must include a txid"))?;
    let () = assert_not_in_mempool(&post_setup, &txid_1, "bid replaced by the wallet send").await?;
    let () = assert_in_mempool(&post_setup, &send_txid, "wallet send").await?;

    // ---- Idempotent resubmission must reflect reality, not the record ----
    // The tracked transaction was just replaced out of the mempool. A client
    // retrying the identical request (the timeout-retry case the
    // equal-amount path exists for) must not be told that transaction is a
    // live bid: the resubmission *rebroadcasts* it, and here that broadcast
    // fails, since the wallet send out-fees it. Before the rebroadcast was
    // added, this silently returned the dead transaction as if it were
    // still in flight.
    let err = try_create_bid(&mut post_setup, height, &prev_bytes, &h_star_1, initial_bid)
        .await
        .err()
        .ok_or_else(|| {
            anyhow::anyhow!("expected the resubmission of a replaced bid to be rejected")
        })?;
    anyhow::ensure!(
        err.code == ErrorCode::InvalidArgument,
        "expected InvalidArgument resubmitting a replaced bid, got {:?}: {:?}",
        err.code,
        err.message,
    );
    tracing::info!("Resubmission of a replaced bid was rejected instead of reported as live");

    // Confirm the send, so the old bid's input is gone at the consensus
    // level, not merely re-allocated within the mempool. Mined with an
    // explicit tx list: `generatetoaddress` would sweep the mempool, and any
    // M8 mined without a matching M7 accept poisons its block.
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generateblock",
            [mining_address, format!("[\"{send_txid}\"]")],
        )
        .run_utf8()
        .await?;
    let result: GenerateBlockResult = serde_json::from_str(&json)?;
    let send_block: bitcoin::BlockHash = result.hash.parse()?;
    let () = assert_enforcer_verdict(
        &mut post_setup,
        send_block,
        Expect::Accepted,
        Duration::from_secs(30),
    )
    .await?;
    let () = wait_for_wallet_sync(&mut post_setup).await?;

    // A fresh auction against the new tip. Internally this walks the whole
    // fallback ladder: fee-bump (`TransactionNotFound`), manual input reuse
    // (`UnknownUtxo`), fresh build. Externally it must simply succeed --
    // mempool acceptance is itself the proof of the fresh build, since any
    // replacement route would have produced a transaction spending the dead
    // input, which no mempool can accept.
    //
    // The lifecycle continues against this bid from here on: `txid_1`,
    // `h_star_1`, and the slot variables deliberately shadow the originals.
    let (prev_bytes, height) = get_tip_info(&mut post_setup).await?;
    let h_star_1 = sidechain_block_hash(0x11);
    let txid_1 = create_bid(&mut post_setup, height, &prev_bytes, &h_star_1, initial_bid).await?;
    let () = assert_in_mempool(&post_setup, &txid_1, "bid rebuilt after inputs were spent").await?;
    tracing::info!(%txid_1, "Bid rebuilt fresh after its predecessor's inputs were spent for real");

    // ---- Increase the bid: same slot, same h*, higher fee -- real RBF ----
    // Deliberately above *both* absurd-fee guards standing between a large
    // bid and the network, for a tx this size (~190 vB): rust-bitcoin's
    // `Psbt::extract_tx` refuses fee rates over 25,000 sat/vB (~4.7M sats
    // here), which `extract_tx_unchecked_fee_rate` must bypass, and
    // bitcoind's default 0.10 BTC/kvB (10,000 sat/vB) ceiling, which
    // `sendrawtransaction`'s `maxfeerate=0` must suppress. If either guard
    // were still in the path, this bump would fail.
    let bumped_bid = 6_000_000u64;
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

    // ---- Lose several auctions in a row, at an unchanging bid ----
    // Each round bids, then a block is mined *without* including that bid
    // (via `generateblock` with an explicit empty tx list, bypassing the
    // mempool entirely) -- simulating a mainchain block someone else found
    // that doesn't include our request. The bid is now consensus-dead for
    // the superseded tip, but its transaction is *not* magically gone from
    // bitcoind's mempool -- nothing ever double-spends it away. With only
    // one spendable UTXO funded above, the next round's bid can only succeed
    // by replacing that same still-unconfirmed transaction. Two things are
    // under test at once: that inputs get reused rather than exhausted, and
    // that losing an auction does not force the next bid upwards.
    //
    // The constant bid is the point. BIP125 makes a replacement pay strictly
    // more than what it replaces, so a client bidding a fixed amount -- what
    // real ones do -- would be locked out from its first loss onwards, its
    // funds stuck behind an unmineable transaction until bitcoind's 336-hour
    // mempool expiry. What saves it is `apply_connected_block_policy` sinking
    // every bid the new block superseded to a modified fee of about
    // -21,000,000 BTC: Core's replacement rules compare *modified* fees, so a
    // dead bid stops being a floor.
    //
    // Round 2 is the exception, and pays more once: it replaces a bid still
    // live for the current tip, which genuinely must be outbid.
    const EXPIRY_ROUNDS: u8 = 5;
    let round_bid = bumped_bid + 20_000;
    for round in 2..=(1 + EXPIRY_ROUNDS) {
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
        round_bid,
        "Wallet created a fresh bid after every one of {EXPIRY_ROUNDS} consecutive losing \
         auctions, at an unchanging bid and with only one spendable UTXO -- no UTXO \
         exhaustion, no forced escalation"
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
    // The final live bid of the successful attempt, carried into the reorg
    // scenario below.
    let mut final_chain: Option<(String, [u8; 32])> = None;
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
        final_chain = Some((chain_c.clone(), h_star_chain_alt));
        break;
    }
    anyhow::ensure!(
        landed_in_one_second,
        "could not land a bid/bump/replace sequence inside one wall-clock second in \
         {SAME_SECOND_ATTEMPTS} attempts; the same-second regression is not being covered"
    );
    let (live_bid_txid, live_bid_h_star) =
        final_chain.expect("landed_in_one_second implies a recorded final bid");

    // ---- A reorg restores a settled bid's tracking along with its tx ----
    // Settling a bid snapshots its consumed row into `bmm_requests_undo`,
    // keyed by the settling block; disconnecting that block must restore the
    // row (`restore_bmm_requests`), because the disconnect also puts the
    // bid's transaction back into bitcoind's mempool. The proof that the
    // *row* came back is behavioral: the next bid must evict the restored
    // transaction from stock Core's mempool, which only a genuine BIP125
    // replacement built from the restored row can be. Had the row stayed
    // deleted, the wallet would have built the new bid fresh from other
    // coins and left the restored transaction sitting in the mempool.
    let live_bid_hex = get_raw_tx_hex(&post_setup, &live_bid_txid).await?;
    let settling_block = crate::bmm_block::submit_block_with_bmm_accepts(
        &post_setup,
        &[(DummySidechain::SIDECHAIN_NUMBER, live_bid_h_star)],
        &[&live_bid_hex],
    )
    .await?;
    let () = assert_enforcer_verdict(
        &mut post_setup,
        settling_block,
        Expect::Accepted,
        Duration::from_secs(30),
    )
    .await?;
    let () = wait_for_wallet_sync(&mut post_setup).await?;
    let () = assert_not_in_mempool(
        &post_setup,
        &live_bid_txid,
        "settled by the block about to be disconnected",
    )
    .await?;

    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "invalidateblock", [settling_block.to_string()])
        .run_utf8()
        .await?;
    let () = assert_in_mempool(
        &post_setup,
        &live_bid_txid,
        "returned to the mempool by the disconnect",
    )
    .await?;
    // Mine an empty replacement block on the reverted branch. electrs (and
    // therefore the wallet) won't follow a chain that merely got *shorter*,
    // so the reorg only propagates once the new branch progresses. Waiting
    // for the wallet to reach the replacement block also guarantees it has
    // processed the disconnect -- a wallet still seeing the settled bid as
    // confirmed would take the fresh-build path instead of replacing it.
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generateblock",
            [post_setup.mining_address.to_string(), "[]".to_owned()],
        )
        .run_utf8()
        .await?;
    let result: GenerateBlockResult = serde_json::from_str(&json)?;
    let replacement_block: bitcoin::BlockHash = result.hash.parse()?;
    let () = assert_enforcer_verdict(
        &mut post_setup,
        replacement_block,
        Expect::Accepted,
        Duration::from_secs(30),
    )
    .await?;
    let () =
        wait_for_wallet_tip(&mut post_setup, replacement_block, Duration::from_secs(60)).await?;
    tracing::info!(
        %settling_block,
        %replacement_block,
        "Reorged out the settling block; bid is back in the mempool"
    );

    chain_bid += 20_000;
    let h_star_post_reorg = sidechain_block_hash(0xD1);
    let (prev_bytes, height) = get_tip_info(&mut post_setup).await?;
    let post_reorg_txid = create_bid(
        &mut post_setup,
        height,
        &prev_bytes,
        &h_star_post_reorg,
        chain_bid,
    )
    .await?;
    let () = assert_in_mempool(
        &post_setup,
        &post_reorg_txid,
        "replacement of the reorg-restored bid",
    )
    .await?;
    let () = assert_not_in_mempool(
        &post_setup,
        &live_bid_txid,
        "reorg-restored bid, after being replaced",
    )
    .await?;
    tracing::info!(
        %post_reorg_txid,
        "Reorg-restored bid was superseded by a genuine replacement, not a second live bid"
    );

    // ---- A confirmed bid still recorded as in-flight -> fresh build ----
    // The winning-bid scenario above proved the *normal* outcome: the
    // connect_block policy consumed the tracked row before the retry
    // arrived. What remains is the window where the row survives while the
    // wallet already sees its transaction confirmed -- e.g. a crash between
    // broadcast and the settling block being processed. A live enforcer
    // can't be raced into that state deterministically, so construct it
    // directly: settle a bid normally, then write its row back into the
    // producer's policy DB, exactly as if the policy hook had never run.
    chain_bid += 20_000;
    let h_star_settled = sidechain_block_hash(0xE1);
    let settled_slot = get_best_block_hash(&post_setup).await?;
    let (prev_bytes, height) = get_tip_info(&mut post_setup).await?;
    let settled_txid = create_bid(
        &mut post_setup,
        height,
        &prev_bytes,
        &h_star_settled,
        chain_bid,
    )
    .await?;
    let settled_hex = get_raw_tx_hex(&post_setup, &settled_txid).await?;
    let settled_block = crate::bmm_block::submit_block_with_bmm_accepts(
        &post_setup,
        &[(DummySidechain::SIDECHAIN_NUMBER, h_star_settled)],
        &[&settled_hex],
    )
    .await?;
    let () = assert_enforcer_verdict(
        &mut post_setup,
        settled_block,
        Expect::Accepted,
        Duration::from_secs(30),
    )
    .await?;
    let () = wait_for_wallet_sync(&mut post_setup).await?;
    let () = assert_not_in_mempool(&post_setup, &settled_txid, "settled bid").await?;

    let settled_bid_amount = chain_bid;
    {
        let producer_db = ProducerDb::new(&producer_db_dir(&post_setup))?;
        producer_db
            .upsert_bmm_request(
                DummySidechain::SIDECHAIN_NUMBER,
                &settled_slot,
                BmmCommitment(h_star_settled),
                settled_txid.parse()?,
                settled_bid_amount,
                &hex::decode(settled_hex.trim())?,
            )
            .await?;
    }
    tracing::info!(%settled_txid, "Re-injected the settled bid's tracking row");

    // Bid again. The wallet reads the re-injected row, `build_fee_bump`
    // reports the transaction confirmed, and the only acceptable outcome is
    // a *fresh* transaction in the mempool -- erroring would strand the
    // sidechain, and handing back the recorded (already confirmed)
    // transaction would repeat the won-bid replay bug covered above.
    chain_bid += 20_000;
    let h_star_fresh = sidechain_block_hash(0xE2);
    let (prev_bytes, height) = get_tip_info(&mut post_setup).await?;
    let fresh_txid = create_bid(
        &mut post_setup,
        height,
        &prev_bytes,
        &h_star_fresh,
        chain_bid,
    )
    .await?;
    anyhow::ensure!(
        fresh_txid != settled_txid,
        "the wallet handed back the confirmed transaction as if it were a live bid"
    );
    let () = assert_in_mempool(
        &post_setup,
        &fresh_txid,
        "fresh bid despite a confirmed tracked row",
    )
    .await?;
    tracing::info!(%fresh_txid, "Confirmed-but-tracked bid fell back to a fresh transaction");

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
    // its funds -- not about whichever builder happened to fail first.
    //
    // Which of the two phrasings comes back depends on whether a tracked bid
    // exists to replace. Without one the wallet answers from its spendable
    // balance; with one that balance understates what the replacement can
    // spend, so the question goes to BDK, which weighs the recovered inputs
    // along with everything else and names the shortfall itself.
    let message = err.message.clone().unwrap_or_default();
    anyhow::ensure!(
        message.contains("exceeds the wallet's spendable balance")
            || message.contains("Insufficient funds"),
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

    // ---- Self-mined win: the block must carry the bid it commits to ----
    // Every winning bid above was mined by a hand-crafted block. This one
    // goes through the enforcer itself -- what a single-process deployment
    // runs, where the node holding the tracked bid also builds the block.
    // Only then does `extend_coinbase_txouts` preload that bid's own M7, so
    // that by the time transactions are chosen an M7 for this sidechain
    // already exists. Reading that as "an M8 is already in" throws away the
    // very transaction the M7 commits to, and the block goes out committing
    // to h* with the fee left unpaid in the mempool. `fresh_txid` is the live
    // tracked bid from the phase above, still targeting the current tip.
    let () = assert_in_mempool(&post_setup, &fresh_txid, "bid about to be self-mined").await?;
    let () = crate::mine::mine::<DummySidechain>(&mut post_setup, 1, None).await?;
    let self_mined_block = get_best_block_hash(&post_setup).await?;
    let mined_txids = block_txids(&post_setup, &self_mined_block).await?;
    anyhow::ensure!(
        mined_txids.contains(&fresh_txid),
        "the enforcer mined a block committing to this sidechain's h*, but left the bid \
         transaction {fresh_txid} out of it -- the coinbase accepts the commitment while the \
         fee stays unpaid in the mempool (block {self_mined_block} carries {} tx(s))",
        mined_txids.len(),
    );
    let () = assert_not_in_mempool(&post_setup, &fresh_txid, "self-mined winning bid").await?;
    tracing::info!(
        %fresh_txid,
        %self_mined_block,
        "Self-mined block carried the bid its coinbase commits to"
    );

    let () = offline_reorg_scenario(&mut post_setup, &bin_paths, &res_tx, &mut chain_bid).await?;
    let () = untracked_bid_scenario(&mut post_setup, &bin_paths, &res_tx, &mut chain_bid).await?;

    // ---- Raising a bid must be judged on what the raise can spend ----
    // A replacement recovers the inputs of the bid it replaces, so the
    // wallet's *spendable* balance is the wrong yardstick: the money funding
    // the previous bid has already left it. Bid well over half of what is
    // spendable and the remaining change alone cannot cover a raise -- but
    // the raise reuses the original input, so it is affordable, and a balance
    // check that does not know that rejects a bid the wallet can pay.
    let balance = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    // Confirmed only: `pending_sats` counts immature coinbases, which cannot
    // fund anything, and the wallet's spendable balance is at least this.
    let spendable = balance.confirmed_sats;
    anyhow::ensure!(
        spendable > 0,
        "expected a confirmed balance before the large-bid phase, got {balance:?}"
    );
    let (prev_bytes, height) = get_tip_info(&mut post_setup).await?;
    let h_star_large = sidechain_block_hash(0xC1);
    let large_bid = spendable / 100 * 60;
    let large_txid = create_bid(
        &mut post_setup,
        height,
        &prev_bytes,
        &h_star_large,
        large_bid,
    )
    .await?;
    let () = assert_in_mempool(&post_setup, &large_txid, "large opening bid").await?;
    let raised_bid = spendable / 100 * 85;
    let raised_txid = create_bid(
        &mut post_setup,
        height,
        &prev_bytes,
        &h_star_large,
        raised_bid,
    )
    .await
    .map_err(|err| {
        anyhow::anyhow!(
            "raising a bid from {large_bid} to {raised_bid} sats was rejected, though the \
             replacement recovers the {large_bid}-sat bid's own input to pay it: {err}"
        )
    })?;
    let () = assert_in_mempool(&post_setup, &raised_txid, "raised large bid").await?;
    let () = assert_not_in_mempool(&post_setup, &large_txid, "replaced large bid").await?;
    tracing::info!(%raised_txid, large_bid, raised_bid, "Raise judged on what it can spend");

    let () = crate::test_bmm_stale_bid_rejected::stale_bid_rejected_body(&mut post_setup).await?;

    drop(post_setup.tasks);
    Ok(())
}

/// Test the full BMM bid lifecycle against a single, unpatched bitcoind node.
pub async fn test_bmm_bid_lifecycle(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
    mode: Mode,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths.clone(), Network::Regtest)?;
    register_files(&file_registry, &trial_name(mode), &pre_setup.directories);
    let setup_opts: SetupOpts = SetupOpts {
        bitcoind_args: Vec::new(),
        bitcoind_kind: BitcoindKind::Unpatched,
        enforcer_args: Vec::new(),
        enforcer_wallet: Default::default(),
    };
    let post_setup = pre_setup.setup(mode, setup_opts, res_tx.clone()).await?;
    let _test_task: util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = test_bmm_bid_lifecycle_task(post_setup, bin_paths, res_tx.clone()).await;
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
