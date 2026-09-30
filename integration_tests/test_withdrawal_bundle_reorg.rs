//! The block producer's own withdrawal bundles follow the chain: a bundle is
//! no longer ours to propose or vote on once it fails or pays out, and is ours
//! again if a reorg undoes that. Reorging out its M3 leaves it ours to propose
//! again. It belongs to its slot, so it stays ours when the sidechain there is
//! replaced.
//! Resubmitting a failed bundle queues it again, for good, and resubmitting a
//! paid-out one is refused. An explicit ACK outlives its bundle's settlement.
//! The reorg cases shared with sidechain proposals (above the settling block,
//! a competing block, a rebuild) are covered by `sidechain_proposal_reorg`.
//!
//! Runs in `GetBlockTemplate` mode, like the withdrawal-bundle policy test:
//! templates read the persisted policy, which is what is checked here. Only one
//! bundle is pending at a time, so KNOWN's vote for it is `[0]`.

use bip300301_enforcer_lib::{
    messages::{CoinbaseMessage, M4AckBundles},
    proto::{
        self,
        common::ConsensusHex,
        mainchain::{
            AckAllProposalsPolicy, GetSidechainsRequest, SetAckAllProposalsRequest,
            SetWithdrawalBundleAckRequest, WithdrawalBundlePolicy,
        },
    },
    types::Thresholds,
};
use bitcoin::{Amount, BlockHash, Txid, hashes::Hash as _};

use crate::{
    integration_test::propose_sidechain_for_slot,
    mine::{MiningPolicy, mine},
    setup::{
        DummySidechain, PostSetup, Sidechain as _, activate_funded_sidechain, best_block_hash,
        broadcast_bundle, lose_policy_tip, pending_bundles, reorg_out,
        restart_enforcer_with_defaults, template_coinbase_messages, try_broadcast_bundle,
        wait_until,
    },
    test_blinded_m6_roundtrip::make_blinded_m6,
    test_sidechain_ack_policy::set_bundle_policy,
};

async fn set_explicit_ack(post_setup: &PostSetup, m6id: Txid, ack: bool) -> anyhow::Result<()> {
    let _resp = post_setup
        .block_producer_service_client
        .set_withdrawal_bundle_ack(SetWithdrawalBundleAckRequest {
            sidechain_number: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            m6id: buffa::MessageField::some(ConsensusHex::encode(&m6id)),
            ack,
        })
        .await?;
    Ok(())
}

async fn mine_one(post_setup: &mut PostSetup) -> anyhow::Result<BlockHash> {
    let () = mine::<DummySidechain>(post_setup, 1, MiningPolicy::SILENT).await?;
    best_block_hash(&post_setup.bitcoind_client).await
}

/// Mine until the one pending bundle fails or pays out, and return the block
/// that settled it.
async fn mine_until_settled(post_setup: &mut PostSetup) -> anyhow::Result<BlockHash> {
    for _ in 0..=u32::from(Thresholds::SHORT.withdrawal_bundle_max_age) + 1 {
        let block = mine_one(post_setup).await?;
        if pending_bundles(post_setup).await? == 0 {
            return Ok(block);
        }
    }
    anyhow::bail!("the bundle never settled")
}

/// Whether the next template proposes `m6id` in an M3.
async fn template_proposes(post_setup: &PostSetup, m6id: Txid) -> anyhow::Result<bool> {
    Ok(template_coinbase_messages(&post_setup.gbt_client)
        .await?
        .into_iter()
        .any(|message| {
            matches!(
                message,
                CoinbaseMessage::M3ProposeBundle(m3) if m3.bundle_txid == m6id.to_byte_array()
            )
        }))
}

/// Whether the next template upvotes the one pending bundle. Under KNOWN, it
/// does exactly when the bundle is ours.
async fn template_upvotes(post_setup: &PostSetup) -> anyhow::Result<bool> {
    Ok(template_coinbase_messages(&post_setup.gbt_client)
        .await?
        .into_iter()
        .any(|message| {
            matches!(
                message,
                CoinbaseMessage::M4AckBundles(M4AckBundles::OneByte { upvotes })
                    if upvotes == [0]
            )
        }))
}

/// The policy DB catches up with the chain only when read, so poll.
async fn wait_for_ours(post_setup: &PostSetup) -> anyhow::Result<()> {
    wait_until("KNOWN to upvote the pending bundle as ours", || {
        template_upvotes(post_setup)
    })
    .await
}

async fn ensure_settled(post_setup: &PostSetup, m6id: Txid) -> anyhow::Result<()> {
    anyhow::ensure!(
        !template_proposes(post_setup, m6id).await?,
        "the settled bundle {m6id} was proposed again"
    );
    Ok(())
}

pub async fn test_withdrawal_bundle_reorg(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (_sidechain, _sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;

    tracing::info!("A failed bundle is settled, and resubmitting it queues it again");
    let () = set_bundle_policy(&post_setup, WithdrawalBundlePolicy::None).await?;
    let failing_tx = make_blinded_m6(1_000, Amount::from_sat(50_000));
    let failing = failing_tx.compute_txid();
    let () = broadcast_bundle(&post_setup, &failing_tx).await?;
    // Reorging out its M3 leaves it ours to propose again.
    let submitted_in = mine_one(&mut post_setup).await?;
    let () = reorg_out(&post_setup, submitted_in, || async {
        anyhow::ensure!(
            template_proposes(&post_setup, failing).await?,
            "the bundle was not proposed again after its M3 was reorged out"
        );
        Ok(())
    })
    .await?;
    let _failed_in = mine_until_settled(&mut post_setup).await?;
    let () = ensure_settled(&post_setup, failing).await?;
    let () = broadcast_bundle(&post_setup, &failing_tx).await?;
    anyhow::ensure!(
        template_proposes(&post_setup, failing).await?,
        "the resubmitted bundle was not proposed again"
    );
    let _failed_in = mine_until_settled(&mut post_setup).await?;
    let () = ensure_settled(&post_setup, failing).await?;

    tracing::info!("Reorging out a payout, however deep, makes the bundle ours again");
    let paid_tx = make_blinded_m6(1_000, Amount::from_sat(60_000));
    let paid = paid_tx.compute_txid();
    let () = broadcast_bundle(&post_setup, &paid_tx).await?;
    let _submitted_in = mine_one(&mut post_setup).await?;
    let () = set_explicit_ack(&post_setup, paid, true).await?;
    let paid_in = mine_until_settled(&mut post_setup).await?;
    // Built while the bundle is settled.
    let _above = mine_one(&mut post_setup).await?;
    let () = ensure_settled(&post_setup, paid).await?;
    // Unlike a failed one, a paid-out bundle cannot be queued again: it would
    // pay out twice.
    let resubmitted = try_broadcast_bundle(&post_setup, &paid_tx).await;
    anyhow::ensure!(
        resubmitted
            .as_ref()
            .is_err_and(|err| err.code == connectrpc::ErrorCode::AlreadyExists),
        "resubmitting a paid-out bundle must fail with AlreadyExists, got: {resubmitted:?}"
    );
    let () = reorg_out(&post_setup, paid_in, || async {
        // Under NONE, only the explicit ACK can vote.
        let () = wait_until("the explicit ACK to upvote the restored bundle", || {
            template_upvotes(&post_setup)
        })
        .await?;
        let () = set_explicit_ack(&post_setup, paid, false).await?;
        let () = set_bundle_policy(&post_setup, WithdrawalBundlePolicy::Known).await?;
        wait_for_ours(&post_setup).await
    })
    .await?;
    // Core's template holds the old M6 again, which must not be paid twice.
    let _paid_in = mine_until_settled(&mut post_setup).await?;
    let () = ensure_settled(&post_setup, paid).await?;

    tracing::info!(
        "A rebuild keeps a failed bundle's resubmission, and still refuses the paid one"
    );
    let () = broadcast_bundle(&post_setup, &failing_tx).await?;
    let _submitted_in = mine_one(&mut post_setup).await?;
    let () = post_setup.kill_enforcer().await?;
    let () = lose_policy_tip(&post_setup.directories)?;
    let () = restart_enforcer_with_defaults(&mut post_setup).await?;
    // The bundle's earlier failures are still on the chain.
    let () = wait_for_ours(&post_setup).await?;
    anyhow::ensure!(
        try_broadcast_bundle(&post_setup, &paid_tx).await.is_err(),
        "resubmitting a paid-out bundle succeeded after a rebuild"
    );

    // Bundles belong to the slot: the validator keeps a replaced sidechain's
    // pending ones, and so do we.
    tracing::info!("A replaced sidechain's pending bundle stays ours");
    let original_activation = slot_activation_height(&post_setup).await?;
    let () = set_bundle_policy(&post_setup, WithdrawalBundlePolicy::None).await?;
    let () = set_ack_policy(&post_setup, AckAllProposalsPolicy::None).await?;
    let () = propose_sidechain_for_slot::<DummySidechain>(
        &mut post_setup,
        DummySidechain::SIDECHAIN_NUMBER,
        "replacement",
    )
    .await?;
    let () = set_ack_policy(&post_setup, AckAllProposalsPolicy::All).await?;
    let mut replaced = false;
    for _ in 0..=Thresholds::SHORT.used_sidechain_slot_proposal_max_age {
        let _block = mine_one(&mut post_setup).await?;
        if slot_activation_height(&post_setup).await? != original_activation {
            replaced = true;
            break;
        }
    }
    anyhow::ensure!(replaced, "the replacement never activated");
    anyhow::ensure!(
        pending_bundles(&post_setup).await? == 1,
        "the replaced sidechain's bundle is no longer pending"
    );
    let () = set_bundle_policy(&post_setup, WithdrawalBundlePolicy::Known).await?;
    wait_for_ours(&post_setup).await
}

/// When the sidechain in [`DummySidechain`]'s slot was activated.
async fn slot_activation_height(post_setup: &PostSetup) -> anyhow::Result<Option<u32>> {
    Ok(post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest::default())
        .await?
        .into_owned()
        .sidechains
        .into_iter()
        .find(|info| {
            proto::unwrap_u32(info.sidechain_number.clone())
                == Some(u32::from(DummySidechain::SIDECHAIN_NUMBER.0))
        })
        .and_then(|info| proto::unwrap_u32(info.activation_height)))
}

async fn set_ack_policy(
    post_setup: &PostSetup,
    policy: AckAllProposalsPolicy,
) -> anyhow::Result<()> {
    let _resp = post_setup
        .block_producer_service_client
        .set_ack_all_proposals(SetAckAllProposalsRequest {
            policy: policy.into(),
        })
        .await?;
    Ok(())
}
