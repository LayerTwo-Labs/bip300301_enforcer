//! The block producer's withdrawal-bundle policy: what the M4 in each coinbase
//! votes, and the explicit per-bundle ACK that overrides it.
//!
//! Runs in `GetBlockTemplate` mode, like the sidechain ACK policy test:
//! templates read the persisted policy, so the M4s mined here come from the
//! policy alone rather than from anything the `mine` helper passes -- its
//! sidechain-ACK argument is ignored here.

use bip300301_enforcer_lib::{
    messages::{CoinbaseMessage, M4AckBundles},
    proto::{
        self,
        common::ConsensusHex,
        mainchain::{
            BroadcastWithdrawalBundleRequest, GetBlockProducerStateRequest,
            SetWithdrawalBundleAckRequest, WithdrawalBundlePolicy,
        },
    },
};
use bitcoin::{Amount, Txid};
use futures::channel::mpsc;

use crate::{
    integration_test::{activate_sidechain, deposit, fund_enforcer, propose_sidechain},
    mine::{MiningPolicy, mine},
    setup::{DummySidechain, PostSetup, Sidechain as _},
    test_blinded_m6_roundtrip::{make_blinded_m6, serialize_zero_input_legacy},
    test_sidechain_ack_policy::{bundle_vote_count, set_bundle_policy, tip_coinbase_messages},
};

/// The chain tip's M4, or `None` if its coinbase carries none.
async fn tip_m4(post_setup: &mut PostSetup) -> anyhow::Result<Option<M4AckBundles>> {
    let m4 = tip_coinbase_messages(post_setup)
        .await?
        .into_iter()
        .find_map(|message| match message {
            CoinbaseMessage::M4AckBundles(m4) => Some(m4),
            _ => None,
        });
    Ok(m4)
}

/// The single one-byte vote the tip's M4 casts. Exactly one sidechain is
/// active throughout this test, so an M4 that is present carries exactly one.
async fn tip_m4_vote(post_setup: &mut PostSetup) -> anyhow::Result<Option<u8>> {
    let Some(m4) = tip_m4(post_setup).await? else {
        return Ok(None);
    };
    let M4AckBundles::OneByte { upvotes } = m4 else {
        anyhow::bail!("expected a OneByte M4 with one active sidechain, got: `{m4:?}`")
    };
    let [vote] = upvotes.as_slice() else {
        anyhow::bail!("expected exactly 1 vote for the 1 active sidechain, got: `{upvotes:?}`")
    };
    Ok(Some(*vote))
}

async fn explicit_bundle_ack_count(post_setup: &mut PostSetup) -> anyhow::Result<usize> {
    let state = post_setup
        .block_producer_service_client
        .get_block_producer_state(GetBlockProducerStateRequest::default())
        .await?
        .into_owned();
    Ok(state.explicit_bundle_acks.len())
}

pub async fn test_withdrawal_bundle_policy(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (sidechain_res_tx, _sidechain_res_rx) = mpsc::unbounded();
    let mut sidechain = DummySidechain::setup((), &post_setup, sidechain_res_tx).await?;

    // A bundle can only be voted on once its sidechain is active and holds a
    // treasury UTXO to pay out of.
    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    let () = activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    let sidechain_address = sidechain.get_deposit_address().await?;
    deposit(
        &mut post_setup,
        &mut sidechain,
        &sidechain_address,
        Amount::from_sat(1_000_000),
        Amount::from_sat(10_000),
    )
    .await?;

    let () = set_bundle_policy(&mut post_setup, WithdrawalBundlePolicy::None).await?;

    tracing::info!("Proposing a withdrawal bundle");
    let bundle_tx = make_blinded_m6(1_000, Amount::from_sat(50_000));
    let bundle_m6id: Txid = bundle_tx.compute_txid();
    let _resp = post_setup
        .wallet_service_client
        .broadcast_withdrawal_bundle(BroadcastWithdrawalBundleRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            transaction: buffa::MessageField::some(buffa_types::google::protobuf::BytesValue {
                value: serialize_zero_input_legacy(&bundle_tx),
                ..Default::default()
            }),
        })
        .await?;
    // The M3 that registers the bundle. BIP300 M3 starts it at an ACK score of
    // 1 on its own, so the score below is the M3's, not a vote.
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 1,
        "expected the bundle to sit at its initial M3 ACK score of 1"
    );

    tracing::info!("Mining under NONE: no M4 at all");
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    anyhow::ensure!(
        tip_m4(&mut post_setup).await?.is_none(),
        "NONE emitted an M4 with nothing to say"
    );
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 1,
        "the bundle gathered a vote under NONE"
    );

    // The bundle was broadcast through this node, so it is one of ours.
    tracing::info!("Mining under KNOWN: the bundle must be upvoted");
    let () = set_bundle_policy(&mut post_setup, WithdrawalBundlePolicy::Known).await?;
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    anyhow::ensure!(
        tip_m4_vote(&mut post_setup).await? == Some(0),
        "expected KNOWN to upvote the sidechain's only pending bundle"
    );
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 2,
        "the bundle did not gather a vote under KNOWN"
    );

    // ALARM is a stance no other policy can express: it actively takes votes
    // back off every bundle that has them.
    tracing::info!("Mining under ALARM: the bundle must be downvoted");
    let () = set_bundle_policy(&mut post_setup, WithdrawalBundlePolicy::Alarm).await?;
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    anyhow::ensure!(
        tip_m4_vote(&mut post_setup).await? == Some(M4AckBundles::ALARM_ONE_BYTE),
        "expected ALARM to alarm the active sidechain"
    );
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 1,
        "ALARM did not take the bundle's vote back off"
    );

    // An explicit ACK is the only way to back a bundle under NONE, and the
    // point of keeping per-bundle ACKs at all.
    tracing::info!("Mining under NONE with an explicit ACK: the bundle must be upvoted anyway");
    let () = set_bundle_policy(&mut post_setup, WithdrawalBundlePolicy::None).await?;
    let set_bundle_ack = |ack: bool| SetWithdrawalBundleAckRequest {
        sidechain_number: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
        m6id: buffa::MessageField::some(ConsensusHex::encode(&bundle_m6id)),
        ack,
    };
    let () = post_setup
        .block_producer_service_client
        .set_withdrawal_bundle_ack(set_bundle_ack(true))
        .await
        .map(|_| ())?;
    anyhow::ensure!(
        explicit_bundle_ack_count(&mut post_setup).await? == 1,
        "the explicit bundle ACK was not recorded"
    );
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    anyhow::ensure!(
        tip_m4_vote(&mut post_setup).await? == Some(0),
        "an explicit ACK did not outrank the NONE policy"
    );
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 2,
        "the explicitly ACKed bundle did not gather a vote"
    );

    // NACK, so ACKs are not permanent.
    let () = post_setup
        .block_producer_service_client
        .set_withdrawal_bundle_ack(set_bundle_ack(false))
        .await
        .map(|_| ())?;
    anyhow::ensure!(
        explicit_bundle_ack_count(&mut post_setup).await? == 0,
        "NACK did not remove the explicit bundle ACK"
    );
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 2,
        "the bundle kept gathering votes after its ACK was withdrawn"
    );

    drop(post_setup);
    Ok(())
}
