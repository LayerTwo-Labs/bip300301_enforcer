//! Regression tests for the enforcer catching up, in one bulk jump, across
//! chain changes that happened entirely while it was down. A live enforcer
//! streams block connects/disconnects one at a time, so several catch-up
//! code paths only ever run after a restart with a real gap to cross.
//!
//! The rejected-block scenario must run last: it deliberately leaves the
//! enforcer's validated tip stuck behind bitcoind's tip, a terminal state
//! nothing else can sync past.

use std::{collections::HashSet, str::FromStr as _};

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    messages::M8BmmRequest,
    proto::mainchain::{
        CreateNewAddressRequest, GetBalanceRequest, GetChainTipRequest, ListTransactionsRequest,
        WalletTransaction,
    },
    types::BmmCommitment,
};
use bitcoin::{
    Amount, BlockHash, OutPoint, Transaction, TxIn, TxOut, Txid,
    consensus::encode::{deserialize_hex, serialize_hex},
    transaction::Version,
};
use futures::channel::mpsc;
use serde::Deserialize;

use crate::{
    integration_test::{fund_enforcer, wait_for_wallet_sync},
    setup::{DummySidechain, Mode, Network, PostSetup, PreSetup, SetupOpts, Sidechain, wait_until},
    util::BinPaths,
};

pub const TEST_NAME: &str = "wallet_reorg_multi_block";

const FORK_DEPTH: u32 = 15;

pub async fn test_wallet_reorg_multi_block(bin_paths: BinPaths) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths.clone(), Network::Regtest)?;
    let setup_opts: SetupOpts = Default::default();
    let mut post_setup = pre_setup
        .setup(Mode::Mempool, setup_opts, res_tx.clone())
        .await?;

    // `fund_enforcer` mines 100 blocks to a single address, so its coinbases
    // mature (100 confirmations) at a staggered rate as height advances --
    // the *last* funded block isn't mature until 100 more blocks are mined
    // on top. Mine well past that point before taking any balance snapshot,
    // so the small height change from the reorg below (net +2) can't itself
    // mature additional coinbases and mask (or mimic) the actual corruption
    // bug under test.
    fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    let throwaway_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            ["110".to_string(), throwaway_address.clone()],
        )
        .run_utf8()
        .await?;
    wait_for_wallet_sync(&mut post_setup).await?;

    tracing::info!("starting scenario: wallet_reorg");
    wallet_reorg_scenario(&mut post_setup, &bin_paths, &res_tx).await?;

    tracing::info!("starting scenario: orphaned_transactions");
    orphaned_transactions_scenario(&mut post_setup, &bin_paths, &res_tx).await?;

    tracing::info!("starting scenario: rejected_block");
    rejected_block_scenario(&mut post_setup, &bin_paths, &res_tx).await?;

    Ok(())
}

/// Regression scenario for a multi-block-deep reorg corrupting the wallet's
/// local chain view.
///
/// `connect_missing_block` (`lib/wallet/mod.rs`) walks a stack of ancestor
/// heights back from BDK's reported `try_include_height` until it can connect
/// a block. Each retry re-fetches a block via `getblockhash`, but used to
/// pass the *original* `try_include_height` argument instead of the current
/// stack entry's height on every nested retry -- so on any catch-up spanning
/// more than one missing block, it fetched the same (wrong) block repeatedly
/// while applying it at progressively lower heights, corrupting BDK's
/// checkpoint history.
///
/// This only manifests when the wallet has to catch up across the reorg in
/// one jump (BDK's ancestor-walk needs an actual gap to retry over) -- an
/// enforcer that's live and streaming ZMQ block-connect/disconnect events
/// one at a time never takes this path. So this scenario kills the enforcer,
/// drives the reorg entirely at the bitcoind level while it's down, then
/// restarts it -- exactly the condition under which this was first found
/// during live multi-node testing.
async fn wallet_reorg_scenario(
    post_setup: &mut PostSetup,
    bin_paths: &BinPaths,
    res_tx: &mpsc::UnboundedSender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let balance_before = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    anyhow::ensure!(
        balance_before.confirmed_sats > 0,
        "expected a confirmed balance after funding, got {balance_before:?}"
    );
    tracing::info!(?balance_before, "wallet funded");

    // Kill the enforcer *before* touching the chain, so the reorg below
    // happens entirely while it's down -- it can only learn about the new
    // tip by catching up across the gap on restart, not by observing it live.
    post_setup.kill_enforcer().await?;

    // Roll back FORK_DEPTH blocks, then mine a longer (FORK_DEPTH + 2)
    // replacement chain from the fork point, so the reorg definitely sticks.
    // None of this touches the funding tx's block -- it's on the shared
    // common ancestor -- so the wallet's confirmed balance must be unchanged
    // afterwards.
    let tip_height: u32 = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    let fork_height = tip_height - FORK_DEPTH;
    let first_invalid_hash = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblockhash", [(fork_height + 1).to_string()])
        .run_utf8()
        .await?
        .trim()
        .to_string();
    tracing::info!(
        tip_height,
        fork_height,
        %first_invalid_hash,
        "invalidating down to the fork point (enforcer is down)"
    );
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "invalidateblock", [first_invalid_hash])
        .run_utf8()
        .await?;
    // A *different* address from the one used to build the original chain --
    // regtest block generation is otherwise deterministic enough (same
    // parent, same miner, same empty mempool, same coinbase value/height)
    // that the replacement block at the fork height can come out
    // byte-identical to the one just invalidated, which bitcoind then
    // rejects outright as `duplicate-invalid` rather than accepting a
    // harmless re-mine.
    let replacement_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [(FORK_DEPTH + 2).to_string(), replacement_address],
        )
        .run_utf8()
        .await?;

    let new_tip_height: u32 = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    anyhow::ensure!(
        new_tip_height == tip_height + 2,
        "expected the replacement chain to win by 2 blocks, got height {new_tip_height} \
         (was {tip_height})"
    );

    // electrs (v3.2.0, pinned by this repo's test harness) is known to panic
    // mid-index on some reorgs -- an unrelated, pre-existing limitation, not
    // the enforcer -- so it may need restarting from a clean index before
    // the enforcer (which depends on it for wallet sync) can come back up.
    tracing::info!("restarting electrs");
    post_setup
        .restart_electrs(bin_paths, res_tx.clone())
        .await?;

    tracing::info!("restarting enforcer -- it must now catch up across the reorg in one jump");
    post_setup
        .restart_enforcer(bin_paths, Vec::<String>::new(), res_tx.clone())
        .await?;

    wait_for_wallet_sync(post_setup).await?;

    let balance_after = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    tracing::info!(
        ?balance_after,
        "wallet balance after multi-block reorg + restart"
    );
    anyhow::ensure!(
        balance_after.confirmed_sats == balance_before.confirmed_sats,
        "wallet balance should be unaffected by a {FORK_DEPTH}-block-deep reorg that doesn't \
         touch the funding tx's block, but confirmed_sats changed: {balance_after:?} \
         (was {balance_before:?})"
    );
    anyhow::ensure!(
        balance_after.pending_sats == balance_before.pending_sats,
        "pending_sats should also be unaffected: {balance_after:?} (was {balance_before:?})"
    );

    // Not just a number: prove the wallet is actually still usable -- a
    // corrupted checkpoint history could report a plausible-looking balance
    // yet still fail real coin selection/signing.
    tracing::info!("confirming the wallet can still build and sign a real transaction");
    use bip300301_enforcer_lib::proto::mainchain::SendTransactionRequest;
    let destination = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_string();
    let _send_resp = post_setup
        .wallet_service_client
        .send_transaction(SendTransactionRequest {
            destinations: std::collections::HashMap::from([(destination, 100_000_u64)])
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .await?;

    Ok(())
}

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

#[derive(Deserialize)]
struct GenerateBlockResult {
    hash: String,
}

const RAW_BID_FEE: Amount = Amount::from_sat(5_000);

/// Craft, sign, and broadcast a raw M8 straight from bitcoind's own wallet --
/// entirely independent of the enforcer, which is down throughout.
async fn broadcast_raw_bid(
    post_setup: &PostSetup,
    prev_mainchain_block_hash: BlockHash,
    h_star: [u8; 32],
) -> anyhow::Result<(Txid, String)> {
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
            (amount > RAW_BID_FEE).then_some((u, amount))
        })
        .ok_or_else(|| anyhow::anyhow!("no spendable UTXO in bitcoind wallet"))?;

    let change_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?;
    let change_address = bitcoin::Address::from_str(change_address.trim())?
        .require_network(post_setup.network.into())?;

    let script_pubkey = M8BmmRequest::script_pubkey(
        DummySidechain::SIDECHAIN_NUMBER,
        BmmCommitment(h_star),
        prev_mainchain_block_hash,
    )?;

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
                script_pubkey,
                value: Amount::ZERO,
            },
            TxOut {
                script_pubkey: change_address.script_pubkey(),
                value: input_value - RAW_BID_FEE,
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

    let txid_str = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "sendrawtransaction",
            [signed_hex.clone(), "0".to_owned()],
        )
        .run_utf8()
        .await?;
    Ok((Txid::from_str(txid_str.trim())?, signed_hex))
}

/// Payments the reorg below orphans. Enough that the standard library's sort,
/// which only checks its comparator opportunistically, panicked on the old
/// one in nearly every run rather than in one of six.
const ORPHANED_PAYMENTS: usize = 24;

async fn bitcoin_cli<Arg: AsRef<std::ffi::OsStr>>(
    post_setup: &PostSetup,
    method: &str,
    args: impl IntoIterator<Item = Arg>,
) -> anyhow::Result<String> {
    let output = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], method, args)
        .run_utf8()
        .await?;
    Ok(output.trim().to_owned())
}

#[derive(Deserialize)]
struct FundResult {
    hex: String,
}

/// The height the wallet reports a transaction confirmed at, if any.
fn listed_height(tx: &WalletTransaction) -> Option<u32> {
    tx.confirmation_info
        .clone()
        .into_option()
        .filter(|info| info.block_hash.clone().into_option().is_some())
        .map(|info| info.height)
}

/// The time the wallet has for a listed transaction: its confirmation time,
/// else when the wallet last saw it in the mempool, else nothing.
fn listed_timestamp(tx: &WalletTransaction) -> Option<i64> {
    tx.confirmation_info
        .clone()
        .into_option()
        .and_then(|info| info.timestamp.into_option())
        .map(|timestamp| timestamp.seconds)
}

/// The TXIDs `ListTransactions` reports at `height` (`None` for unconfirmed).
fn listed_txids_at(listed: &[WalletTransaction], height: Option<u32>) -> HashSet<Txid> {
    listed
        .iter()
        .filter(|tx| listed_height(tx) == height)
        .filter_map(|tx| {
            tx.txid
                .clone()
                .into_option()?
                .decode::<WalletTransaction, _>("txid")
                .ok()
        })
        .collect()
}

/// Regression scenario for `ListTransactions` panicking on a wallet holding
/// transactions a reorg has orphaned.
///
/// `list_wallet_transactions` (`lib/wallet/mod.rs`) sorts the listing
/// chronologically. A transaction the wallet only ever saw inside a block has
/// no mempool `last_seen`, and once a reorg drops that block BDK reports it as
/// unconfirmed with no timestamp at all. The old comparator ordered those by
/// TXID and everything else by time. Mixing the two relations is not a total
/// order, and the standard library's sort panics when it catches that,
/// taking the RPC down with "user-provided comparison function does not
/// correctly implement a total order".
///
/// Every route back to the wallet would give the payments a time, so all are
/// cut: they are mined straight into a block, never via the mempool; they
/// are time-locked to be final at that block and no lower, so that after
/// invalidating from one block below, bitcoind drops them instead of
/// returning them to its mempool (from where the enforcer's mempool mirror
/// would feed them to the wallet as freshly seen; standardness is no lever,
/// since the harness runs stock Bitcoin Core with `-acceptnonstdtxn`); and
/// electrs stays down until after the listing, since a sync evicts
/// transactions the chain source no longer knows.
async fn orphaned_transactions_scenario(
    post_setup: &mut PostSetup,
    bin_paths: &BinPaths,
    res_tx: &mpsc::UnboundedSender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    // Mature enough of bitcoind's coinbases to fund one payment each.
    let miner_address = bitcoin_cli(post_setup, "getnewaddress", [""; 0]).await?;
    bitcoin_cli(
        post_setup,
        "generatetoaddress",
        [(ORPHANED_PAYMENTS + 5).to_string(), miner_address.clone()],
    )
    .await?;
    wait_for_wallet_sync(post_setup).await?;

    let enforcer_address = post_setup
        .wallet_service_client
        .create_new_address(CreateNewAddressRequest::default())
        .await?
        .into_owned()
        .address;
    let payment_height: u32 = bitcoin_cli(post_setup, "getblockcount", [""; 0])
        .await?
        .parse::<u32>()?
        + 1;
    let mut payments = Vec::with_capacity(ORPHANED_PAYMENTS);
    for _ in 0..ORPHANED_PAYMENTS {
        // A height lock is final only in blocks above it; bitcoind's wallet
        // sets the input sequence that makes it count.
        let unfunded = bitcoin_cli(
            post_setup,
            "createrawtransaction",
            [
                "[]".to_owned(),
                format!("{{\"{enforcer_address}\":0.1}}"),
                (payment_height - 1).to_string(),
            ],
        )
        .await?;
        let funded: FundResult = serde_json::from_str(
            &bitcoin_cli(
                post_setup,
                "fundrawtransaction",
                [
                    unfunded,
                    r#"{"lockUnspents":true,"fee_rate":10}"#.to_owned(),
                ],
            )
            .await?,
        )?;
        let signed: SignResult = serde_json::from_str(
            &bitcoin_cli(post_setup, "signrawtransactionwithwallet", [funded.hex]).await?,
        )?;
        anyhow::ensure!(signed.complete, "signrawtransactionwithwallet incomplete");
        payments.push(signed.hex);
    }
    let payment_txids: HashSet<Txid> = payments
        .iter()
        .map(|hex| Ok(deserialize_hex::<Transaction>(hex)?.compute_txid()))
        .collect::<anyhow::Result<_>>()?;

    // Straight into a block, bypassing the mempool.
    bitcoin_cli(
        post_setup,
        "generateblock",
        [miner_address, serde_json::to_string(&payments)?],
    )
    .await?;
    wait_for_wallet_sync(post_setup).await?;
    let listed = list_transactions(post_setup).await?;
    anyhow::ensure!(
        listed_txids_at(&listed, Some(payment_height)) == payment_txids,
        "expected every payment listed as confirmed at height {payment_height} before the reorg"
    );

    tracing::info!("orphaning the payments with electrs and the enforcer down");
    post_setup.kill_enforcer().await?;
    post_setup.kill_electrs().await?;
    let fork_block_hash = bitcoin_cli(
        post_setup,
        "getblockhash",
        [(payment_height - 1).to_string()],
    )
    .await?;
    bitcoin_cli(post_setup, "invalidateblock", [fork_block_hash]).await?;
    // Empty replacement blocks, to a fresh address: the first would otherwise
    // come out byte-identical to the (also empty) block it replaces, which
    // bitcoind rejects as already invalidated.
    let replacement_address = bitcoin_cli(post_setup, "getnewaddress", [""; 0]).await?;
    for _ in 0..3 {
        bitcoin_cli(
            post_setup,
            "generateblock",
            [replacement_address.clone(), "[]".to_owned()],
        )
        .await?;
    }
    post_setup
        .restart_enforcer(bin_paths, Vec::<String>::new(), res_tx.clone())
        .await?;
    wait_for_wallet_sync(post_setup).await?;

    // Pre-fix this call is where the RPC panicked (in most runs; in the rest
    // the listing came back in an order that depended on which pairs the
    // sort happened to compare).
    let listed = list_transactions(post_setup).await?;
    anyhow::ensure!(
        listed_txids_at(&listed, None) == payment_txids,
        "expected exactly the payments listed as unconfirmed after the reorg"
    );
    // The precondition of the bug: no time at all, not a mempool sighting.
    anyhow::ensure!(
        listed
            .iter()
            .all(|tx| listed_height(tx).is_some() || listed_timestamp(tx).is_none()),
        "the wallet should have no timestamp for the orphaned payments"
    );
    let timestamps = listed.iter().map(listed_timestamp).collect::<Vec<_>>();
    anyhow::ensure!(
        timestamps.is_sorted(),
        "ListTransactions must be in chronological order, got {timestamps:?}"
    );

    post_setup.restart_electrs(bin_paths, res_tx.clone()).await
}

async fn list_transactions(post_setup: &mut PostSetup) -> anyhow::Result<Vec<WalletTransaction>> {
    Ok(post_setup
        .wallet_service_client
        .list_transactions(ListTransactionsRequest::default())
        .await?
        .into_owned()
        .transactions)
}

/// Regression scenario for the enforcer fatally crashing while catching up,
/// in one bulk jump, across a block its own validator rejects.
///
/// `handle_block_batch` (`lib/validator/task/mod.rs`) used to propagate
/// *any* error from `connect_block` verbatim, including a benign, non-fatal
/// rejection (here `HandleM8::NotAcceptedByMiners`: an M8 bid mined into a
/// block whose coinbase carries no matching M7 accept) that the live,
/// per-block streaming path (`connect_block_no_commit` in
/// `validator/cusf_enforcer.rs`) already knows how to shrug off via
/// `error_fatality`'s `Split`. A rejected block can really only be
/// encountered this way -- in a *bulk* multi-block catch-up -- because the
/// live path calls straight into the enforcer's mempool-sync task
/// (`cusf-enforcer-mempool`), which reactively `invalidateblock`s a
/// rejected block before the enforcer's own bitcoind ever offers a
/// *further* block built on top of it. A restart that has to catch up in
/// one jump has no such luxury: bitcoind may already have kept extending a
/// chain the enforcer would have rejected block-by-block, live.
///
/// This kills the enforcer, hand-crafts (and force-mines, bypassing normal
/// mempool policy) a block containing a stale M8 bid while it's down, mines
/// a couple more blocks on top of that same bad chain, then restarts --
/// forcing `sync_blocks` to walk across the rejected block in a single
/// batch. Before the fix this crashed the process outright; after the fix,
/// the enforcer must survive, stay responsive, and correctly stop its own
/// validated tip just short of the rejected block.
///
/// No sidechain needs to be proposed or activated for any of this: M8
/// parsing and rejection never consult activation state -- acceptance is
/// judged purely against M7 accepts in the containing block's own coinbase.
async fn rejected_block_scenario(
    post_setup: &mut PostSetup,
    bin_paths: &BinPaths,
    res_tx: &mpsc::UnboundedSender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    // Kill the enforcer *before* touching the chain: everything below must
    // happen while it's down, so the only way it can learn about any of it
    // is by catching up across the whole gap in one bulk jump on restart.
    post_setup.kill_enforcer().await?;

    let tip_before: BlockHash = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    let h_star = {
        use bitcoin::hashes::Hash as _;
        bitcoin::hashes::sha256::Hash::hash(b"catchup-across-rejected-block").to_byte_array()
    };

    // A real, fee-paying M8 bid targeting the current tip.
    let (stale_txid, stale_tx_hex) = broadcast_raw_bid(post_setup, tip_before, h_star).await?;
    tracing::info!(%stale_txid, %tip_before, "broadcast a bid (enforcer is down)");

    // Advance the tip past it with an *empty* block, staling the bid -- its
    // `prev_mainchain_block_hash` no longer matches the actual parent of
    // any block mined from here on. Deliberately `generateblock` with an
    // explicit empty tx list rather than `generatetoaddress`: nothing here
    // deprioritizes the bid still sitting in the mempool (no enforcer is
    // running), so ordinary mempool-based mining would just confirm it
    // in this block instead -- leaving nothing stale to force-include
    // below, and a second inclusion attempt would collide with itself
    // (BIP30).
    let miner_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generateblock",
            [miner_address.clone(), "[]".to_owned()],
        )
        .run_utf8()
        .await?;

    // The last block the enforcer can accept: everything from the poisoned
    // block onwards builds on a chain it rejects, so this is where its own
    // validated tip must come to rest once it has caught up.
    let last_valid_height: u32 = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;

    // Force-mine a block containing the now-stale bid directly, bypassing
    // ordinary mempool/fee-based template selection entirely (which would
    // never pick a bid this stale on its own) -- exactly what an operator
    // running a miner without this enforcer's mempool-sync policy could do
    // by accident, or what any block from a peer not running it at all
    // could look like.
    let poisoned_block_hash: BlockHash = {
        let json = post_setup
            .bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "generateblock",
                [miner_address.clone(), format!("[\"{stale_tx_hex}\"]")],
            )
            .run_utf8()
            .await?;
        let result: GenerateBlockResult = serde_json::from_str(&json)?;
        result.hash.parse()?
    };
    tracing::info!(%poisoned_block_hash, "force-mined a block containing the stale bid");

    // A couple more ordinary blocks on top of the same (BIP300-invalid, but
    // bitcoind itself has no opinion on that) chain, so the batch the
    // enforcer must walk on restart spans blocks both before *and* after
    // the rejected one.
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", ["2".to_string(), miner_address])
        .run_utf8()
        .await?;

    let tip_after_height: u32 = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    tracing::info!(
        tip_after_height,
        "chain extended entirely while the enforcer was down"
    );

    tracing::info!(
        "restarting enforcer -- it must catch up across the rejected block in one batch \
         without crashing"
    );
    post_setup
        .restart_enforcer(bin_paths, Vec::<String>::new(), res_tx.clone())
        .await?;

    // Wait for the batch sync to actually land on the last block the enforcer
    // accepts. Polling `GetChainTip` doubles as the liveness check this
    // scenario exists for -- pre-fix the enforcer crashed almost immediately
    // on restart, so these calls would fail with a connection error rather
    // than ever reaching the expected height.
    let () = wait_until(
        &format!("enforcer's validated tip to settle at height {last_valid_height}"),
        || async {
            let synced_height = post_setup
                .validator_service_client
                .get_chain_tip(GetChainTipRequest::default())
                .await?
                .into_owned()
                .block_header_info
                .into_option()
                .ok_or_else(|| anyhow::anyhow!("GetChainTipResponse missing block_header_info"))?
                .height;
            anyhow::ensure!(
                synced_height <= last_valid_height,
                "expected the enforcer's own validated tip ({synced_height}) to stop at the last \
                 block it accepts ({last_valid_height}), strictly before bitcoind's tip \
                 ({tip_after_height}), since everything from the rejected block onward builds on \
                 a chain it doesn't accept"
            );
            Ok(synced_height == last_valid_height)
        },
    )
    .await?;
    tracing::info!(
        last_valid_height,
        "enforcer survived the restart and caught up to its last accepted block"
    );

    Ok(())
}
