//! Regression test for the BMM requests a produced block consumes surviving a
//! reorg of that block.
//!
//! Producing a block deletes the `bmm_requests` rows queued against its parent,
//! first snapshotting them into `bmm_requests_undo` keyed by the produced block
//! hash, so a disconnect can put them back instead of making the operator
//! re-queue them against the new tip. That snapshot is taken just before
//! `submitblock` (`lib/block_producer/mine.rs`), because from the moment
//! Bitcoin Core has the block the validator may connect *and* disconnect it at
//! any time: a disconnect processed against an empty undo log restores nothing,
//! and a snapshot landing after it would then move the still-live rows under a
//! block hash that is never disconnected again, stranding them there.
//!
//! This drives that whole path end to end -- the producer's own `mine()`, a
//! real `submitblock`, and a real `invalidateblock` -- rather than the
//! interleaving itself, which is a window of microseconds no external driver
//! can force. The ordering that closes the window is asserted directly against
//! `snapshot_and_delete_bmm_requests`/`restore_bmm_requests_from_undo` in
//! `lib/block_producer/db.rs`.

use std::time::Duration;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        mainchain::{
            BlockHeaderInfo, GenerateToAddressRequest, GenerateToAddressResponse,
            GetChainTipRequest,
        },
    },
};
use bitcoin::{BlockHash, hashes::Hash as _};

use crate::{
    block_verdict::wait_for_enforcer_tip_hash,
    setup::{PostSetup, wait_until},
};

pub const TEST_NAME: &str = "bmm_request_reorg";

/// Sidechain slot for the queued request. Nothing here validates it: the undo
/// log is keyed by previous block hash alone, so no sidechain has to be
/// proposed or active for a request to be queued, consumed, and restored.
const SIDECHAIN_NUMBER: u8 = 5;

/// Commitment for the queued request, distinctive enough to identify the
/// restored row as the one that was queued.
const SIDE_BLOCK_HASH: [u8; 32] = [0xab; 32];

/// The block producer's policy DB. `app/main.rs` puts it at
/// `<data-dir>/wallet/<chain>/db.sqlite`, with no datadir suffix on plain
/// regtest.
fn producer_db(post_setup: &PostSetup) -> anyhow::Result<rusqlite::Connection> {
    let path = post_setup
        .directories
        .enforcer_dir
        .join("wallet")
        .join("regtest")
        .join("db.sqlite");
    let connection = rusqlite::Connection::open(path)?;
    // The running enforcer holds the same file open and writes to it as blocks
    // connect, so wait for its lock rather than failing on `database is locked`.
    connection.busy_timeout(Duration::from_secs(10))?;
    Ok(connection)
}

/// Queue a BMM request against `prev_blockhash`, exactly as
/// `Db::insert_new_bmm_request` would.
///
/// Written into the DB directly because no RPC reaches this table any more:
/// `CreateBmmCriticalDataTransaction` broadcasts an M8 bid, and the coinbase's
/// M7 accepts are selected from the validator's own view of seen bids. The
/// rows are still consumed and restored by block production, which is what
/// this test covers. Staging `db.sqlite` this way is what `test_seed_migration`
/// does too.
fn queue_bmm_request(post_setup: &PostSetup, prev_blockhash: BlockHash) -> anyhow::Result<()> {
    producer_db(post_setup)?.execute(
        "INSERT INTO bmm_requests (sidechain_number, prev_block_hash, side_block_hash) \
         VALUES (?1, ?2, ?3);",
        (
            SIDECHAIN_NUMBER,
            prev_blockhash.to_byte_array(),
            SIDE_BLOCK_HASH,
        ),
    )?;
    Ok(())
}

/// The `prev_block_hash` of every live (i.e. not snapshotted away) request
/// carrying [`SIDE_BLOCK_HASH`].
fn live_request_prevs(post_setup: &PostSetup) -> anyhow::Result<Vec<BlockHash>> {
    let connection = producer_db(post_setup)?;
    let mut statement =
        connection.prepare("SELECT prev_block_hash FROM bmm_requests WHERE side_block_hash = ?")?;
    let prevs = statement
        .query_map([&SIDE_BLOCK_HASH], |row| {
            row.get::<_, [u8; 32]>(0).map(BlockHash::from_byte_array)
        })?
        .collect::<Result<_, _>>()?;
    Ok(prevs)
}

/// The produced block hashes that requests carrying [`SIDE_BLOCK_HASH`] are
/// currently snapshotted under.
fn undo_block_hashes(post_setup: &PostSetup) -> anyhow::Result<Vec<BlockHash>> {
    let connection = producer_db(post_setup)?;
    let mut statement =
        connection.prepare("SELECT block_hash FROM bmm_requests_undo WHERE side_block_hash = ?")?;
    let block_hashes = statement
        .query_map([&SIDE_BLOCK_HASH], |row| {
            row.get::<_, [u8; 32]>(0).map(BlockHash::from_byte_array)
        })?
        .collect::<Result<_, _>>()?;
    Ok(block_hashes)
}

pub async fn test_bmm_request_reorg(post_setup: PostSetup) -> anyhow::Result<()> {
    // The tip the producer will build on. Read from the validator rather than
    // bitcoind: `generate_block` looks the requests up against its own
    // validated tip.
    let tip_before = post_setup
        .validator_service_client
        .get_chain_tip(GetChainTipRequest::default())
        .await?
        .into_owned()
        .block_header_info
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("get_chain_tip: missing block_header_info"))?
        .block_hash
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("get_chain_tip: missing block_hash"))?
        .decode::<BlockHeaderInfo, BlockHash>("block_hash")?;

    let () = queue_bmm_request(&post_setup, tip_before)?;
    anyhow::ensure!(
        live_request_prevs(&post_setup)? == [tip_before],
        "the queued BMM request must be live before the block that consumes it is produced"
    );

    // Produce a block through the enforcer's own block producer, which is what
    // consumes the request.
    let response: GenerateToAddressResponse = post_setup
        .mining_service_client
        .generate_to_address(GenerateToAddressRequest {
            blocks: proto::wrap_u32(1),
            address: post_setup.mining_address.to_string(),
        })
        .await?
        .into_owned();
    let block_hashes = response
        .block_hashes
        .into_iter()
        .map(|hash| hash.decode::<GenerateToAddressResponse, BlockHash>("block_hashes"))
        .collect::<Result<Vec<_>, _>>()?;
    let &[produced_block_hash] = block_hashes.as_slice() else {
        anyhow::bail!(
            "expected exactly one block hash from GenerateToAddress, got {}",
            block_hashes.len()
        )
    };
    tracing::info!(%produced_block_hash, "produced a block consuming the queued BMM request");

    // The request has been consumed into the undo log, keyed by the block that
    // consumed it -- the snapshot a disconnect of that block restores from.
    anyhow::ensure!(
        live_request_prevs(&post_setup)?.is_empty(),
        "producing a block must consume the BMM requests queued against its parent"
    );
    anyhow::ensure!(
        undo_block_hashes(&post_setup)? == [produced_block_hash],
        "the consumed BMM request must be snapshotted under the block that consumed it"
    );

    // Reorg the produced block back out.
    tracing::info!(%produced_block_hash, "invalidating the block that consumed the BMM request");
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "invalidateblock", [produced_block_hash.to_string()])
        .run_utf8()
        .await?;
    let () = wait_for_enforcer_tip_hash(&post_setup, tip_before).await?;

    // The request is queued again against the block it was queued against
    // originally -- which is the tip once more -- so the next block produced
    // can carry it. The restore runs just after the validator's own disconnect,
    // so it is not necessarily done the moment the tip has rolled back.
    let () = wait_until(
        "the disconnected block's BMM request to be restored",
        || async { Ok(live_request_prevs(&post_setup)? == [tip_before]) },
    )
    .await?;
    anyhow::ensure!(
        undo_block_hashes(&post_setup)?.is_empty(),
        "restoring a BMM request must consume its undo snapshot, not leave it behind"
    );

    drop(post_setup);
    Ok(())
}
