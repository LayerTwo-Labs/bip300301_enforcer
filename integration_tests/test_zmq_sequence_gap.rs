//! A gap in bitcoind's ZMQ sequence stream must not kill the enforcer.
//!
//! bitcoind's ZMQ publisher drops notifications once the socket's high-water
//! mark is reached. The mempool sync task detects the resulting gap
//! (`SequenceStreamError::MissingMempoolSequence`) and returns an error.
//!
//! Observed on drynet4 roughly every 1–3 hours once the chain was mining at
//! ~1 block/2.5s, each occurrence costing a process restart. Raising
//! `-zmqpubsequencehwm` to 100000 reduced the rate but did not eliminate it.
//!
//! Forced deterministically here with `-zmqpubsequencehwm=1`: a one-slot queue
//! drops on any burst, so a batch of transactions is enough.

use std::time::Duration;

use bip300301_enforcer_lib::{bins::CommandExt as _, proto::mainchain::GetChainInfoRequest};

use crate::{
    integration_test,
    setup::{DummySidechain, PostSetup},
};

/// Extra bitcoind args that make the publisher drop sequence messages.
pub const BITCOIND_ARGS: [&str; 1] = ["-zmqpubsequencehwm=1"];

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
    let address = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getnewaddress", Vec::<String>::new())
        .run_utf8()
        .await?;

    // Fund bitcoind's own wallet so it can pay for the burst below.
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", ["101".to_owned(), address.clone()])
        .run_utf8()
        .await?;

    // Build a deep mempool
    const TXS: usize = 200;
    for _ in 0..TXS {
        let _res = post_setup
            .bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "sendtoaddress",
                [address.clone(), "0.0001".to_owned()],
            )
            .run_utf8()
            .await;
    }

    // Confirm them all into one block, then orphan it. Disconnecting returns
    // every tx to the mempool at once, so bitcoind emits ~200 `A` sequence
    // messages back-to-back from inside a single RPC — faster than the
    // subscriber can drain a one-slot queue, so the publisher drops some.
    //
    // This is what drynet4 hits organically: at ~1 block/2.5s the enforcer is
    // busy applying blocks while messages keep arriving.
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", ["1".to_owned(), address.clone()])
        .run_utf8()
        .await?;

    let block_hash = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getbestblockhash", Vec::<String>::new())
        .run_utf8()
        .await?;

    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "invalidateblock", [block_hash.trim().to_owned()])
        .run_utf8()
        .await?;

    // Give the sync task a moment to process (or die on) the stream.
    tokio::time::sleep(Duration::from_secs(5)).await;

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
    // Recovery must stay narrow. Stopping bitcoind makes the next re-sync fail
    // on the ZMQ reachability check (`ZmqNotReachable`), which is NOT
    // resyncable, so the sync task must give up rather than spin forever
    // against a node that is gone. A too-broad `is_resyncable` would turn
    // every fatal condition into a silent infinite retry — worse than exiting.
    //
    // Asserted on the enforcer's own log rather than on gRPC availability:
    // `get_chain_info` is served from the validator's local database, so the
    // enforcer keeps answering for a while after bitcoind disappears. Serving
    // therefore says nothing about whether the sync task is retrying.
    let resyncs_before_stop = read_enforcer_log(&post_setup)?
        .matches("recoverably")
        .count();

    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "stop", Vec::<String>::new())
        .run_utf8()
        .await?;

    tokio::time::sleep(Duration::from_secs(15)).await;

    let log = read_enforcer_log(&post_setup)?;
    let resyncs = log.matches("recoverably").count();
    anyhow::ensure!(
        resyncs <= resyncs_before_stop + 1,
        "sync task kept re-syncing after bitcoind stopped ({resyncs} attempts, \
         was {resyncs_before_stop}): a non-recoverable error is being retried"
    );

    Ok(())
}
