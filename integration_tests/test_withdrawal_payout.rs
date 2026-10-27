//! The block producer's withdrawal payouts, when a reorg returns one to the
//! mempool.

use bip300301_enforcer_lib::{
    messages::M4AckBundles, proto::mainchain::WithdrawalBundlePolicy, types::Thresholds,
};
use bitcoin::{Amount, BlockHash, Txid};
use jsonrpsee::{core::client::ClientT as _, rpc_params};

use crate::{
    block_verdict::wait_for_enforcer_tip_hash,
    mine::{MiningPolicy, mine},
    setup::{
        DummySidechain, PostSetup, activate_funded_sidechain, best_block_hash, broadcast_bundle,
        generate_empty_block, invalidate_block, pending_bundles, reorg_out,
        restart_enforcer_with_defaults,
    },
    test_blinded_m6_roundtrip::make_blinded_m6,
    test_sidechain_ack_policy::bundle_vote_count,
};

/// Mine with `policy` until the one pending bundle fails or pays out, and
/// return the block that settled it.
async fn mine_until_settled(
    post_setup: &mut PostSetup,
    policy: MiningPolicy,
) -> anyhow::Result<BlockHash> {
    for _ in 0..=u32::from(Thresholds::SHORT.withdrawal_bundle_max_age) + 1 {
        let () = mine::<DummySidechain>(post_setup, 1, policy).await?;
        if pending_bundles(post_setup).await? == 0 {
            return best_block_hash(&post_setup.bitcoind_client).await;
        }
    }
    anyhow::bail!("the bundle never settled")
}

/// A payout's M6 that a reorg returned to the mempool can stop being valid
/// before it is mined again: here, the replacement block ALARMs the bundle
/// back below the payout threshold. The enforcer must keep producing blocks
/// without it, keep it in the mempool, and mine it once the bundle is voted
/// back up, including across a restart, which rebuilds what it knows of the
/// mempool.
pub async fn test_stale_payout_after_reorg(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (_sidechain, _sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let paid_tx = make_blinded_m6(1_000, Amount::from_sat(60_000));
    let paid = paid_tx.compute_txid();
    let () = broadcast_bundle(&post_setup, &paid_tx).await?;
    let paid_in = mine_until_settled(&mut post_setup, MiningPolicy::VOTE).await?;
    let paid_in_txids = block_txids(&post_setup, paid_in).await?;

    tracing::info!("Replacing the payout's block with one that ALARMs the bundle");
    let alarm = bitcoin::ScriptBuf::try_from(M4AckBundles::OneByte {
        upvotes: vec![M4AckBundles::ALARM_ONE_BYTE],
    })?;
    let () = invalidate_block(&post_setup.bitcoind_client, paid_in).await?;
    let replacement = generate_empty_block(
        &post_setup.bitcoind_client,
        &format!("raw({})", alarm.to_hex_string()),
    )
    .await?;
    let () = wait_for_enforcer_tip_hash(&post_setup, replacement).await?;
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, paid).await?
            <= u32::from(Thresholds::SHORT.withdrawal_bundle_inclusion_threshold),
        "the ALARM did not take the bundle back below the payout threshold"
    );
    let m6_txid = returned_m6(&post_setup, paid_in_txids).await?;

    tracing::info!("Restarting the enforcer with the unpayable M6 in the mempool");
    let () = restart_enforcer_with_defaults(&mut post_setup).await?;

    tracing::info!("Mining on top, without voting the bundle back up");
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    anyhow::ensure!(
        mempool_txids(&post_setup).await?.contains(&m6_txid),
        "the unpayable M6 must stay in the mempool"
    );

    // ALL, not KNOWN: whether this node still counts the bundle as its own
    // after the reorg is not what this tests.
    tracing::info!("Voted back up, the M6 is mined again");
    let paid_again_in = mine_until_settled(
        &mut post_setup,
        MiningPolicy {
            bundle: WithdrawalBundlePolicy::All,
            ..MiningPolicy::VOTE
        },
    )
    .await?;
    anyhow::ensure!(
        block_txids(&post_setup, paid_again_in)
            .await?
            .contains(&m6_txid),
        "the bundle settled without the returned M6 being mined"
    );
    Ok(())
}

/// One block from `generateblock` carrying only an M4 with `vote` (one byte).
async fn m4_block(post_setup: &PostSetup, vote: u8) -> anyhow::Result<BlockHash> {
    let m4 = bitcoin::ScriptBuf::try_from(M4AckBundles::OneByte {
        upvotes: vec![vote],
    })?;
    let hash = generate_empty_block(
        &post_setup.bitcoind_client,
        &format!("raw({})", m4.to_hex_string()),
    )
    .await?;
    let () = wait_for_enforcer_tip_hash(post_setup, hash).await?;
    Ok(hash)
}

/// A mempool M6 whose bundle is payable at the tip but expires in the block
/// being built. `connect_block` fails the bundle before it reads the block's
/// txs, so if the M6 goes into the template, the template is invalid and no
/// block can be produced at that height.
pub async fn test_mempool_m6_for_an_expiring_bundle(
    mut post_setup: PostSetup,
) -> anyhow::Result<()> {
    let (_sidechain, _sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let thresholds = Thresholds::SHORT;
    let bundle_tx = make_blinded_m6(1_000, Amount::from_sat(60_000));
    let bundle = bundle_tx.compute_txid();
    let () = broadcast_bundle(&post_setup, &bundle_tx).await?;
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::VOTE).await?;
    anyhow::ensure!(
        pending_bundles(&post_setup).await? == 1,
        "the bundle was not proposed"
    );
    let start_votes = bundle_vote_count(&mut post_setup, bundle).await?;

    // Age the bundle to max_age - 1 with blocks we make by hand: upvote it
    // past the threshold, then abstain, so it is payable but still unpaid.
    let max_age = u32::from(thresholds.withdrawal_bundle_max_age);
    let needed = (u32::from(thresholds.withdrawal_bundle_inclusion_threshold) + 1)
        .saturating_sub(start_votes);
    for age in 1..max_age {
        let vote = if age <= needed {
            0
        } else {
            M4AckBundles::ABSTAIN_ONE_BYTE
        };
        let _ = m4_block(&post_setup, vote).await?;
    }
    let votes = bundle_vote_count(&mut post_setup, bundle).await?;
    anyhow::ensure!(
        votes > u32::from(thresholds.withdrawal_bundle_inclusion_threshold),
        "not payable before its last block ({votes} votes)"
    );

    tracing::info!("Our block pays it at age max_age, its last valid block");
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::VOTE).await?;
    anyhow::ensure!(
        pending_bundles(&post_setup).await? == 0,
        "not paid at age max_age"
    );
    let paid_in = best_block_hash(&post_setup.bitcoind_client).await?;
    let paid_block: serde_json::Value = post_setup
        .bitcoind_client
        .request("getblock", rpc_params![paid_in, 1])
        .await?;
    let paid_in_txids: Vec<Txid> = paid_block["tx"]
        .as_array()
        .into_iter()
        .flatten()
        .skip(1)
        .filter_map(|txid| txid.as_str()?.parse().ok())
        .collect();

    tracing::info!("Replacing that block: the M6 returns to the mempool at max_age");
    let () = invalidate_block(&post_setup.bitcoind_client, paid_in).await?;
    let _ = m4_block(&post_setup, M4AckBundles::ABSTAIN_ONE_BYTE).await?;
    anyhow::ensure!(
        pending_bundles(&post_setup).await? == 1,
        "the bundle is not pending again"
    );
    let mempool = mempool_txids(&post_setup).await?;
    let returned: Vec<Txid> = paid_in_txids
        .into_iter()
        .filter(|txid| mempool.contains(txid))
        .collect();
    let [m6_txid] = returned.as_slice() else {
        anyhow::bail!("expected just the payout's M6 back in the mempool, got {returned:?}");
    };

    tracing::info!("The next block expires the bundle: its mempool M6 must stay out");
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::VOTE).await?;
    anyhow::ensure!(
        pending_bundles(&post_setup).await? == 0,
        "the bundle should have expired in this block"
    );
    anyhow::ensure!(
        mempool_txids(&post_setup).await?.contains(m6_txid),
        "the expired bundle's M6 left the mempool: was it mined?"
    );
    Ok(())
}

async fn mempool_txids(post_setup: &PostSetup) -> anyhow::Result<Vec<Txid>> {
    Ok(post_setup
        .bitcoind_client
        .request("getrawmempool", rpc_params![])
        .await?)
}

/// The txs in `block`, other than its coinbase.
async fn block_txids(post_setup: &PostSetup, block: BlockHash) -> anyhow::Result<Vec<Txid>> {
    let block: serde_json::Value = post_setup
        .bitcoind_client
        .request("getblock", rpc_params![block, 1])
        .await?;
    Ok(block["tx"]
        .as_array()
        .into_iter()
        .flatten()
        .skip(1)
        .filter_map(|txid| txid.as_str()?.parse().ok())
        .collect())
}

/// The one tx of a reorged-out payout block that is back in the mempool: the
/// payout's M6.
async fn returned_m6(post_setup: &PostSetup, paid_in_txids: Vec<Txid>) -> anyhow::Result<Txid> {
    let mempool = mempool_txids(post_setup).await?;
    let returned: Vec<Txid> = paid_in_txids
        .into_iter()
        .filter(|txid| mempool.contains(txid))
        .collect();
    let [m6_txid] = returned.as_slice() else {
        anyhow::bail!("expected just the payout's M6 back in the mempool, got {returned:?}");
    };
    Ok(*m6_txid)
}

/// Reorging out a payout returns its M6 to the mempool, and the bundle to
/// pending. The template carries the M6 again, so the producer must not also
/// pay the bundle out itself: that block would pay it twice.
pub async fn test_returned_payout_paid_once(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (_sidechain, _sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let paid_tx = make_blinded_m6(1_000, Amount::from_sat(60_000));
    let () = broadcast_bundle(&post_setup, &paid_tx).await?;
    let paid_in = mine_until_settled(&mut post_setup, MiningPolicy::VOTE).await?;
    let paid_in_txids = block_txids(&post_setup, paid_in).await?;

    tracing::info!("Reorging out the payout, the template offers its M6 again");
    let () = reorg_out(&post_setup, paid_in, || async { Ok(()) }).await?;
    let m6_txid = returned_m6(&post_setup, paid_in_txids).await?;
    let paid_again_in = mine_until_settled(&mut post_setup, MiningPolicy::VOTE).await?;
    anyhow::ensure!(
        block_txids(&post_setup, paid_again_in)
            .await?
            .contains(&m6_txid),
        "the bundle settled without the returned M6 being mined"
    );
    Ok(())
}
