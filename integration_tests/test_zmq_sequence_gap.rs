//! A gap in bitcoind's ZMQ sequence stream must not kill the enforcer.
//!
//! bitcoind's ZMQ publisher drops notifications once the socket's high-water
//! mark is reached. Both sync tasks consume that one stream, and the gap check
//! runs before the per-task message filtering, so both see the error:
//! `Mode::Mempool` drives the mempool sync task, `Mode::NoMempool` the
//! tip-chasing task that is the default mode.
//!
//! Observed on a mainnet fork roughly every 1–3 hours once the chain was
//! mining at ~1 block/2.5s, each occurrence costing a process restart. Raising
//! `-zmqpubsequencehwm` to 100000 reduced the rate but did not eliminate it.
//!
//! Forced deterministically here with `-zmqpubsequencehwm=1`: a one-slot queue
//! drops on any burst, so a batch of transactions is enough.

use std::time::Duration;

use bip300301_enforcer_lib::proto::mainchain::GetChainInfoRequest;
use jsonrpsee::{core::client::ClientT as _, rpc_params};

use crate::{
    block_verdict::wait_for_enforcer_tip_hash,
    integration_test,
    setup::{DummySidechain, Mode, PostSetup, WAIT_POLL_INTERVAL_SLOW, wait_until_every},
};

/// Extra bitcoind args that make the publisher drop sequence messages.
pub const BITCOIND_ARGS: [&str; 1] = ["-zmqpubsequencehwm=1"];

/// Mirrors `MAX_CONSECUTIVE_RESYNCS` in `run_with_resync`. That const is local
/// to the enforcer binary, so it cannot be imported here; keep the two in
/// step.
const MAX_CONSECUTIVE_RESYNCS: usize = 5;

/// The enforcer writes to `<enforcer_dir>/logs/bip300301_enforcer.log.<date>.N`.
fn read_enforcer_log(post_setup: &PostSetup) -> anyhow::Result<String> {
    let logs_dir = post_setup.directories.enforcer_dir.join("logs");
    let mut out = String::new();
    for entry in std::fs::read_dir(&logs_dir)? {
        out.push_str(&std::fs::read_to_string(entry?.path())?);
    }
    Ok(out)
}

pub async fn test_zmq_sequence_gap(mut post_setup: PostSetup) -> anyhow::Result<()> {
    integration_test::fund_enforcer::<DummySidechain>(&mut post_setup).await?;

    // Churn the MEMPOOL, not just the chain: the mempool sequence counter only
    // advances on tx add/remove, so mining alone leaves it idle and the
    // one-slot queue never overflows.
    let address: String = post_setup
        .bitcoind_client
        .request("getnewaddress", rpc_params![])
        .await?;

    // Fund bitcoind's own wallet so it can pay for the burst below.
    let _block_hashes: Vec<bitcoin::BlockHash> = post_setup
        .bitcoind_client
        .request("generatetoaddress", rpc_params![101, &address])
        .await?;

    // Build a deep mempool
    const TXS: usize = 200;
    for _ in 0..TXS {
        let _res = post_setup
            .bitcoind_client
            .request::<bitcoin::Txid, _>("sendtoaddress", rpc_params![&address, 0.0001])
            .await;
    }

    // Confirm them all into one block, then orphan it. Disconnecting returns
    // every tx to the mempool at once, so bitcoind emits ~200 `A` sequence
    // messages back-to-back from inside a single RPC — faster than the
    // subscriber can drain a one-slot queue, so the publisher drops some.
    //
    // This is what a fast-mining fork hits organically: at ~1 block/2.5s the
    // enforcer is busy applying blocks while messages keep arriving.
    let _block_hashes: Vec<bitcoin::BlockHash> = post_setup
        .bitcoind_client
        .request("generatetoaddress", rpc_params![1, &address])
        .await?;

    let block_hash: bitcoin::BlockHash = post_setup
        .bitcoind_client
        .request("getbestblockhash", rpc_params![])
        .await?;

    let () = post_setup
        .bitcoind_client
        .request("invalidateblock", rpc_params![block_hash])
        .await?;

    // The gRPC check below cannot stand on its own: it is served from the
    // validator's local database, so it answers whether or not the gap was
    // recovered — or even reached. Without a logged recovery, a run where the
    // publisher never dropped a message would go green having never entered
    // the recovery path at all.
    wait_until_every(
        "the enforcer to recover from the ZMQ sequence gap",
        WAIT_POLL_INTERVAL_SLOW,
        || async { Ok(read_enforcer_log(&post_setup)?.contains("recoverably")) },
    )
    .await?;
    // Settle before counting recoveries below: once the enforcer has followed
    // the invalidation, the burst is behind it.
    let tip_hash: bitcoin::BlockHash = post_setup
        .bitcoind_client
        .request("getbestblockhash", rpc_params![])
        .await?;
    wait_for_enforcer_tip_hash(&post_setup, tip_hash).await?;

    let chain_info = post_setup
        .validator_service_client
        .get_chain_info(GetChainInfoRequest::default())
        .await;

    anyhow::ensure!(
        chain_info.is_ok(),
        "enforcer stopped serving after a ZMQ sequence gap: {:?}",
        chain_info.err()
    );

    // ---- negative case ----
    //
    // Recovery must stay narrow. Stopping bitcoind must not leave the sync
    // task spinning forever against a node that is gone. A too-broad
    // `is_resyncable` would turn every fatal condition into a silent infinite
    // retry — worse than exiting.
    //
    // The two modes reach that outcome differently, so the bound differs:
    //
    // - `Mempool` re-syncs through a ZMQ reachability pre-check, which fails
    //   with `ZmqNotReachable`. That is not resyncable, so the task gives up
    //   on the first attempt.
    // - `NoMempool` has no such pre-check. A stopped node refuses the RPC
    //   connection, which surfaces as a transport error and IS resyncable, so
    //   the task legitimately retries until its consecutive-resync budget runs
    //   out. Bounded rather than unbounded is the property under test here.
    //
    // Asserted on the enforcer's own log rather than on gRPC availability:
    // `get_chain_info` is served from the validator's local database, so the
    // enforcer keeps answering for a while after bitcoind disappears. Serving
    // therefore says nothing about whether the sync task is retrying.
    let resyncs_before_stop = read_enforcer_log(&post_setup)?
        .matches("recoverably")
        .count();

    let _stop_output: String = post_setup
        .bitcoind_client
        .request("stop", rpc_params![])
        .await?;

    // A fixed wait, not a poll: nothing marks the retries having stopped, and
    // the enforcer need not exit (its ZMQ socket reconnects indefinitely).
    tokio::time::sleep(Duration::from_secs(15)).await;

    // The extra 1 covers the attempt already in flight when bitcoind stopped.
    let budget = match post_setup.mode {
        Mode::NoMempool => MAX_CONSECUTIVE_RESYNCS + 1,
        Mode::Mempool | Mode::GetBlockTemplate => 1,
    };
    let log = read_enforcer_log(&post_setup)?;
    let resyncs = log.matches("recoverably").count();
    anyhow::ensure!(
        resyncs <= resyncs_before_stop + budget,
        "sync task kept re-syncing after bitcoind stopped ({resyncs} attempts, \
         was {resyncs_before_stop}, budget {budget}): a non-recoverable error \
         is being retried"
    );

    Ok(())
}
