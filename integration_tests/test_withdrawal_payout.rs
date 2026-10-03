//! The block producer's withdrawal payouts, when a reorg returns one to the
//! mempool.

use bip300301_enforcer_lib::{
    messages::M4AckBundles,
    proto::{
        self,
        mainchain::{BroadcastWithdrawalBundleRequest, WithdrawalBundlePolicy},
    },
    types::Thresholds,
};
use bitcoin::{Amount, BlockHash, Transaction, Txid};
use futures::channel::mpsc;
use jsonrpsee::{core::client::ClientT as _, rpc_params};

use crate::{
    block_verdict::wait_for_enforcer_tip_hash,
    integration_test::{activate_sidechain, deposit, fund_enforcer, propose_sidechain},
    mine::{MiningPolicy, mine},
    setup::{
        DummySidechain, PostSetup, Sidechain as _, best_block_hash, generate_empty_block,
        invalidate_block, pending_bundle_count,
    },
    test_blinded_m6_roundtrip::{make_blinded_m6, serialize_zero_input_legacy},
    test_sidechain_ack_policy::{bundle_vote_count, set_bundle_policy},
};

async fn broadcast_bundle(post_setup: &PostSetup, bundle_tx: &Transaction) -> anyhow::Result<()> {
    let _resp = post_setup
        .wallet_service_client
        .broadcast_withdrawal_bundle(BroadcastWithdrawalBundleRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            transaction: buffa::MessageField::some(buffa_types::google::protobuf::BytesValue {
                value: serialize_zero_input_legacy(bundle_tx),
                ..Default::default()
            }),
        })
        .await?;
    Ok(())
}

async fn pending_bundles(post_setup: &PostSetup) -> anyhow::Result<usize> {
    pending_bundle_count(
        &post_setup.validator_service_client,
        DummySidechain::SIDECHAIN_NUMBER,
    )
    .await
}

/// Activate [`DummySidechain`] and fund its treasury. Returns the sidechain,
/// which must be kept alive, and its deposit address.
async fn activate_funded_sidechain(
    post_setup: &mut PostSetup,
) -> anyhow::Result<(DummySidechain, String)> {
    let (sidechain_res_tx, _sidechain_res_rx) = mpsc::unbounded();
    let mut sidechain = DummySidechain::setup((), post_setup, sidechain_res_tx).await?;
    let () = propose_sidechain::<DummySidechain>(post_setup).await?;
    let () = activate_sidechain::<DummySidechain>(post_setup).await?;
    fund_enforcer::<DummySidechain>(post_setup).await?;
    let sidechain_address = sidechain.get_deposit_address().await?;
    deposit(
        post_setup,
        &mut sidechain,
        &sidechain_address,
        Amount::from_sat(1_000_000),
        Amount::from_sat(10_000),
    )
    .await?;
    Ok((sidechain, sidechain_address))
}

/// Mine with `policy` until the one pending bundle fails or pays out, and
/// return the block that settled it.
async fn mine_until_settled(
    post_setup: &mut PostSetup,
    policy: MiningPolicy,
) -> anyhow::Result<BlockHash> {
    let () = set_bundle_policy(post_setup, policy.bundle).await?;
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
    let mempool = mempool_txids(&post_setup).await?;
    let returned: Vec<Txid> = paid_in_txids
        .into_iter()
        .filter(|txid| mempool.contains(txid))
        .collect();
    let [m6_txid] = returned.as_slice() else {
        anyhow::bail!("expected just the payout's M6 back in the mempool, got {returned:?}");
    };

    tracing::info!("Restarting the enforcer with the unpayable M6 in the mempool");
    let (res_tx, _res_rx) = mpsc::unbounded();
    let () = post_setup
        .restart_enforcer(&crate::util::BinPaths::new(), Vec::<String>::new(), res_tx)
        .await?;

    tracing::info!("Mining on top, without voting the bundle back up");
    let () = set_bundle_policy(&mut post_setup, WithdrawalBundlePolicy::None).await?;
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    anyhow::ensure!(
        mempool_txids(&post_setup).await?.contains(m6_txid),
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
    let paid_again_block: serde_json::Value = post_setup
        .bitcoind_client
        .request("getblock", rpc_params![paid_again_in, 1])
        .await?;
    anyhow::ensure!(
        paid_again_block["tx"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|txid| txid.as_str() == Some(m6_txid.to_string().as_str())),
        "the bundle settled without the returned M6 being mined"
    );
    Ok(())
}

/// A bundle this node holds AND an M6 for it in the mempool, in the block where
/// the bundle is payable. The template takes the mempool M6 into its prefix;
/// the suffix must not pay the same bundle again, or the block carries two M6s
/// for one bundle and the enforcer rejects its own block. The reorg is just the
/// cheapest way to get a valid M6 for a payable bundle into the mempool; anyone
/// can broadcast one.
pub async fn test_mempool_m6_for_a_held_bundle(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (_sidechain, _sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let bundle_tx = make_blinded_m6(1_000, Amount::from_sat(60_000));
    let bundle = bundle_tx.compute_txid();
    let () = broadcast_bundle(&post_setup, &bundle_tx).await?;
    let paid_in = mine_until_settled(&mut post_setup, MiningPolicy::VOTE).await?;
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

    tracing::info!("Replacing the payout's block with one that keeps the bundle payable");
    let upvote = bitcoin::ScriptBuf::try_from(M4AckBundles::OneByte { upvotes: vec![0] })?;
    let () = invalidate_block(&post_setup.bitcoind_client, paid_in).await?;
    let replacement = generate_empty_block(
        &post_setup.bitcoind_client,
        &format!("raw({})", upvote.to_hex_string()),
    )
    .await?;
    let () = wait_for_enforcer_tip_hash(&post_setup, replacement).await?;
    let votes = bundle_vote_count(&mut post_setup, bundle).await?;
    anyhow::ensure!(
        votes > u32::from(Thresholds::SHORT.withdrawal_bundle_inclusion_threshold),
        "the replacement left the bundle unpayable ({votes} votes)"
    );
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

    tracing::info!("This node holds the bundle again");
    let () = broadcast_bundle(&post_setup, &bundle_tx).await?;

    tracing::info!("Mining the block where both could pay it");
    let () = set_bundle_policy(&mut post_setup, WithdrawalBundlePolicy::Known).await?;
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::VOTE).await?;
    anyhow::ensure!(
        pending_bundles(&post_setup).await? == 0,
        "the bundle is still pending after the block that should pay it once"
    );
    // The mempool's M6 is what paid it (in the prefix). Without this check, a
    // template that left the mempool M6 out would pass by our suffix paying.
    let best = best_block_hash(&post_setup.bitcoind_client).await?;
    let block: serde_json::Value = post_setup
        .bitcoind_client
        .request("getblock", rpc_params![best, 1])
        .await?;
    anyhow::ensure!(
        block["tx"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|txid| txid.as_str() == Some(m6_txid.to_string().as_str())),
        "the mempool's M6 is not in the block: the double-payment case was not exercised"
    );
    Ok(())
}

async fn mempool_txids(post_setup: &PostSetup) -> anyhow::Result<Vec<Txid>> {
    Ok(post_setup
        .bitcoind_client
        .request("getrawmempool", rpc_params![])
        .await?)
}
