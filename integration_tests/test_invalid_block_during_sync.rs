//! Regression test for the sync-path DoS from a consensus-invalid block.
//!
//! bitcoind does not enforce BIP300/301, so it accepts a block that violates
//! them. The live path (block arriving over ZMQ while the enforcer runs) is
//! covered by `test_invalid_block`. This test covers the other way of meeting
//! such a block: the enforcer is *down* when it is mined, and encounters it
//! in the catch-up sync at startup. The enforcer must report it from
//! `sync_to_tip` as `SyncToTipError::InvalidBlock`, and the
//! cusf-enforcer-mempool crate must invalidate it on the node and re-sync to
//! the resulting tip -- instead of the sync erroring out and leaving the
//! enforcer dead behind a chain it can never accept.
//!
//! The bad block is also chosen so that the rejection fires only *after*
//! part of the block has been applied to the database: its coinbase carries
//! a novel M1 and then an invalid M4. The batch sync commits its transaction
//! even when a block is rejected, so the M1 must be applied in a nested
//! transaction that is rolled back with the block, or the rejected block's
//! sidechain proposal leaks into the validator's state.

use std::str::FromStr as _;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{self, mainchain::GetSidechainProposalsRequest},
};
use bitcoin::BlockHash;
use futures::channel::mpsc;

use crate::{
    block_verdict::wait_for_enforcer_tip_hash,
    bmm_block::wait_past_mtp,
    integration_test::{activate_sidechain, fund_enforcer, propose_sidechain},
    setup::{
        DummySidechain, Mode, Network, PostSetup, PreSetup, SetupOpts, Sidechain,
        WAIT_POLL_INTERVAL_SUBPROCESS, read_enforcer_log, wait_until_every,
    },
    test_invalid_block::{M1_THEN_INVALID_M4, PHANTOM_PROPOSAL_SLOT, submit_invalid_block},
    util::BinPaths,
};

pub const TEST_NAME: &str = "invalid_block_during_sync";

async fn best_block_hash(post_setup: &PostSetup) -> anyhow::Result<BlockHash> {
    let hex = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?;
    Ok(BlockHash::from_str(hex.trim())?)
}

pub async fn test_invalid_block_during_sync(bin_paths: BinPaths) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths.clone(), Network::Regtest)?;
    let setup_opts: SetupOpts = Default::default();
    let mut post_setup = pre_setup
        .setup(Mode::Mempool, setup_opts, res_tx.clone())
        .await?;

    let (sidechain_res_tx, _sidechain_res_rx) = mpsc::unbounded();
    let _sidechain = DummySidechain::setup((), &post_setup, sidechain_res_tx).await?;
    // Activate the sidechain, so that BIP300 coinbase messages are enforced:
    // below the activation height, the M1 rules the bad block violates are
    // skipped entirely, and the block would just be accepted.
    propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    wait_past_mtp(&post_setup).await?;

    let good_tip = best_block_hash(&post_setup).await?;
    wait_for_enforcer_tip_hash(&post_setup, good_tip).await?;

    tracing::info!("killing enforcer, then mining a consensus-invalid block behind its back");
    post_setup.kill_enforcer().await?;
    let bad_block_hash = submit_invalid_block(&post_setup, &M1_THEN_INVALID_M4).await?;
    // Bury the bad block under a plain block, so that the catch-up sync meets
    // it mid-chain rather than at the tip, and the reorg must also drop a
    // descendant.
    let mining_address = post_setup.mining_address.to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", ["1".to_owned(), mining_address])
        .run_utf8()
        .await?;
    let buried_tip = best_block_hash(&post_setup).await?;
    anyhow::ensure!(
        buried_tip != bad_block_hash && buried_tip != good_tip,
        "bitcoind must have accepted the bad block and a descendant on top, \
         but its tip is `{buried_tip}`"
    );

    tracing::info!(
        %bad_block_hash,
        "restarting enforcer -- it must invalidate the bad block during sync, not halt"
    );
    post_setup
        .restart_enforcer(&bin_paths, Vec::<String>::new(), res_tx.clone())
        .await?;

    // The catch-up sync must stop at the bad block's parent and report the
    // block, and the cusf-enforcer-mempool crate must invalidate it on
    // bitcoind, reorging the node back to `good_tip`.
    wait_for_enforcer_tip_hash(&post_setup, good_tip).await?;
    wait_until_every(
        "bitcoind to reorg away from the invalid block",
        WAIT_POLL_INTERVAL_SUBPROCESS,
        || async { Ok(best_block_hash(&post_setup).await? == good_tip) },
    )
    .await?;

    let log = read_enforcer_log(&post_setup.directories.enforcer_dir)?;
    anyhow::ensure!(
        log.contains("encountered invalid block during batch sync"),
        "the bad block must be caught by the batch sync path, not the live path"
    );
    anyhow::ensure!(
        log.contains("Invalid votes: expected 1, but found 2"),
        "the sync-path rejection must carry the BIP300 reason"
    );
    anyhow::ensure!(
        log.contains("invalidating block that the enforcer reported as invalid during sync"),
        "the invalidation must be driven by the cusf-enforcer-mempool crate"
    );

    // The M1 was applied before the M4 rejected the block. It must have been
    // rolled back with the block, not committed along with the rest of the
    // sync batch.
    let phantom_proposals = post_setup
        .validator_service_client
        .get_sidechain_proposals(GetSidechainProposalsRequest::default())
        .await?
        .into_owned()
        .sidechain_proposals
        .into_iter()
        .filter(|proposal| {
            proto::unwrap_u32(proposal.sidechain_number.clone())
                == Some(u32::from(PHANTOM_PROPOSAL_SLOT.0))
        })
        .count();
    anyhow::ensure!(
        phantom_proposals == 0,
        "the rejected block's M1 for slot {} leaked into the validator's state \
         ({phantom_proposals} proposal(s) listed for it)",
        PHANTOM_PROPOSAL_SLOT.0
    );

    // The enforcer must be fully live after the recovery: a block mined on the
    // reorged chain connects. (The new block's plain coinbase differs from the
    // invalidated block's, so it gets a fresh hash rather than being rejected
    // as `duplicate-invalid`.)
    let mining_address = post_setup.mining_address.to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", ["1".to_owned(), mining_address])
        .run_utf8()
        .await?;
    let new_tip = best_block_hash(&post_setup).await?;
    anyhow::ensure!(
        new_tip != buried_tip && new_tip != bad_block_hash,
        "the new block must extend the reorged chain, but the tip is `{new_tip}`"
    );
    wait_for_enforcer_tip_hash(&post_setup, new_tip).await?;

    Ok(())
}
