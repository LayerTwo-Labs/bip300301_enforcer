//! BIP300 M4 vote-array validation, end to end.
//!
//! The vote array `A` is indexed against `ASN`, the active sidechain slots
//! sorted ascending. `A` may cover fewer slots than are active — the trailing
//! slots it omits abstain — but never more, and it may not omit slots in a
//! coinbase where an earlier M2 has already grown `ASN`.
//!
//! Every case here mines a hand-built coinbase: the block producer only emits
//! M4s it believes are valid, so the rejected shapes are unreachable through
//! it, and the accepted ones need vote values the producer would never pick.
//!
//! <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m4-ack-bundle>

use std::time::Duration;

use bip300301_enforcer_lib::{
    messages::{M2AckSidechain, M4AckBundles},
    proto::{
        self,
        mainchain::{
            BroadcastWithdrawalBundleRequest, GetChainInfoRequest, GetSidechainProposalsRequest,
            GetSidechainProposalsResponse, GetSidechainsRequest,
            GetWithdrawalBundleProposalsRequest, GetWithdrawalBundleProposalsResponse,
            SetAckAllProposalsRequest, SetSidechainAckRequest,
        },
    },
    types::SidechainNumber,
};
use bitcoin::{Amount, ScriptBuf, TxOut, Txid, hashes::sha256d};

use crate::{
    block_verdict::{Expect, assert_enforcer_verdict},
    custom_coinbase::{submit_block_with_coinbase_outputs, zero_value},
    integration_test::{activate_sidechain, propose_sidechain, propose_sidechain_for_slot},
    mine::mine,
    setup::{DummySidechain, PostSetup},
    test_blinded_m6_roundtrip::{make_blinded_m6, serialize_zero_input_legacy},
};

/// The slot [`DummySidechain`] occupies, and the lowest of the three used here.
const SLOT_LOW: SidechainNumber = SidechainNumber(0);
/// Activated last, by an M2 in a hand-built coinbase. Sorts *between* the other
/// two, so activating it shifts `SLOT_HIGH` from index 1 to index 2 — the
/// misalignment that makes a shortened array ambiguous.
const SLOT_MID: SidechainNumber = SidechainNumber(1);
/// Holds a pending bundle, and sits mid-`ASN` once `SLOT_MID` activates.
const SLOT_HIGH: SidechainNumber = SidechainNumber(5);
/// Holds a pending bundle, and is the trailing entry of `ASN` throughout.
///
/// A fourth slot is what makes the shift `SLOT_MID`'s activation causes
/// *dangerous* rather than merely wrong: with three slots the only displaced
/// vote lands on the freshly activated slot, which holds no bundle, so the
/// pre-existing out-of-range rule rejects the block regardless. Here a
/// displaced vote can instead land on `SLOT_HIGH`, whose bundle it would
/// silently upvote.
const SLOT_EXTRA: SidechainNumber = SidechainNumber(7);

const VERDICT_TIMEOUT: Duration = Duration::from_secs(10);

async fn sidechains_active(post_setup: &mut PostSetup) -> anyhow::Result<usize> {
    let resp = post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest::default())
        .await?
        .into_owned();
    Ok(resp.sidechains.len())
}

/// The vote count of the only pending withdrawal bundle in `slot`.
async fn bundle_vote_count(
    post_setup: &mut PostSetup,
    slot: SidechainNumber,
    expected_m6id: Txid,
) -> anyhow::Result<u32> {
    let proposals = post_setup
        .validator_service_client
        .get_withdrawal_bundle_proposals(GetWithdrawalBundleProposalsRequest {
            sidechain_id: proto::wrap_u32(slot.0.into()),
        })
        .await?
        .into_owned()
        .proposals;
    let [bundle] = proposals.as_slice() else {
        anyhow::bail!(
            "expected exactly 1 pending withdrawal bundle in slot {slot}, got {}",
            proposals.len()
        )
    };
    let m6id: Txid = bundle
        .m6id
        .clone()
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("withdrawal bundle proposal missing m6id"))?
        .decode::<GetWithdrawalBundleProposalsResponse, _>("m6id")?;
    anyhow::ensure!(
        m6id == expected_m6id,
        "slot {slot} bundle m6id mismatch: {m6id} != {expected_m6id}"
    );
    proto::unwrap_u32(bundle.vote_count.clone())
        .ok_or_else(|| anyhow::anyhow!("withdrawal bundle proposal missing vote_count"))
}

/// The pending proposal for `slot`, as `(description hash, ACK count)`.
async fn proposal_for_slot(
    post_setup: &mut PostSetup,
    slot: SidechainNumber,
) -> anyhow::Result<(sha256d::Hash, u32)> {
    let proposals = post_setup
        .validator_service_client
        .get_sidechain_proposals(GetSidechainProposalsRequest::default())
        .await?
        .into_owned()
        .sidechain_proposals;
    for proposal in proposals {
        let number = proto::unwrap_u32(proposal.sidechain_number.clone());
        if number != Some(slot.0.into()) {
            continue;
        }
        let description_hash: sha256d::Hash = proposal
            .description_sha256d_hash
            .clone()
            .into_option()
            .ok_or_else(|| anyhow::anyhow!("proposal missing description_sha256d_hash"))?
            .decode::<GetSidechainProposalsResponse, _>("description_sha256d_hash")?;
        let vote_count = proto::unwrap_u32(proposal.vote_count.clone())
            .ok_or_else(|| anyhow::anyhow!("proposal missing vote_count"))?;
        return Ok((description_hash, vote_count));
    }
    anyhow::bail!("no pending sidechain proposal for slot {slot}")
}

/// Register a withdrawal bundle for `slot` and mine the M3 that proposes it.
/// Per BIP300 M3 a freshly proposed bundle starts at an ACK score of 1.
async fn propose_bundle(
    post_setup: &mut PostSetup,
    slot: SidechainNumber,
    payout_sats: u64,
) -> anyhow::Result<Txid> {
    let bundle_tx = make_blinded_m6(payout_sats, Amount::from_sat(50_000));
    let m6id = bundle_tx.compute_txid();
    let _resp = post_setup
        .wallet_service_client
        .broadcast_withdrawal_bundle(BroadcastWithdrawalBundleRequest {
            sidechain_id: proto::wrap_u32(slot.0.into()),
            transaction: buffa::MessageField::some(buffa_types::google::protobuf::BytesValue {
                value: serialize_zero_input_legacy(&bundle_tx),
                ..Default::default()
            }),
        })
        .await?;
    let () = mine::<DummySidechain>(post_setup, 1, Some(false)).await?;
    anyhow::ensure!(
        bundle_vote_count(post_setup, slot, m6id).await? == 1,
        "expected the slot {slot} bundle to sit at its initial M3 ACK score of 1"
    );
    Ok(m6id)
}

/// Propose a sidechain for `slot` and register an explicit ACK for it, so that
/// later blocks keep ACKing it without ack-all — and so without an M4 riding
/// along and disturbing the bundle vote counts.
async fn propose_and_ack(
    post_setup: &mut PostSetup,
    slot: SidechainNumber,
) -> anyhow::Result<sha256d::Hash> {
    let () = propose_sidechain_for_slot::<DummySidechain>(post_setup, slot).await?;
    let (description_hash, _) = proposal_for_slot(post_setup, slot).await?;
    let () = post_setup
        .block_producer_service_client
        .set_sidechain_ack(SetSidechainAckRequest {
            sidechain_number: proto::wrap_u32(slot.0.into()),
            description_sha256d_hash: proto::common::ReverseHex::encode(&description_hash).into(),
            ack: true,
        })
        .await
        .map(|_| ())?;
    Ok(description_hash)
}

/// Mine `slot`'s proposal to exactly one ACK short of activation.
///
/// The count is polled rather than computed: how many ACKs the proposal already
/// carries when it lands depends on the ACK policy in force at propose time.
async fn mine_to_activation_brink(
    post_setup: &mut PostSetup,
    slot: SidechainNumber,
    threshold: u32,
) -> anyhow::Result<()> {
    // Activation needs strictly more ACKs than the threshold, so the brink is
    // an ACK count of exactly `threshold`.
    loop {
        let (_, vote_count) = proposal_for_slot(post_setup, slot).await?;
        anyhow::ensure!(
            vote_count <= threshold,
            "slot {slot} proposal overshot the activation brink: {vote_count} > {threshold}"
        );
        if vote_count == threshold {
            return Ok(());
        }
        let () = mine::<DummySidechain>(post_setup, 1, Some(false)).await?;
    }
}

/// The three bundles this test votes on, one per slot that holds one.
#[derive(Clone, Copy)]
struct Bundles {
    low: Txid,
    high: Txid,
    extra: Txid,
}

/// Vote counts for all three bundles, as `(low, high, extra)`. Read together so
/// a case can state the whole expected outcome, including the slots it left
/// alone, in one assertion.
async fn counts(post_setup: &mut PostSetup, bundles: Bundles) -> anyhow::Result<(u32, u32, u32)> {
    Ok((
        bundle_vote_count(post_setup, SLOT_LOW, bundles.low).await?,
        bundle_vote_count(post_setup, SLOT_HIGH, bundles.high).await?,
        bundle_vote_count(post_setup, SLOT_EXTRA, bundles.extra).await?,
    ))
}

fn m2_output(slot: SidechainNumber, description_hash: sha256d::Hash) -> anyhow::Result<TxOut> {
    let script: ScriptBuf = M2AckSidechain {
        sidechain_number: slot,
        description_hash,
    }
    .try_into()?;
    Ok(zero_value(script))
}

fn m4_output(upvotes: Vec<u8>) -> anyhow::Result<TxOut> {
    let script: ScriptBuf = M4AckBundles::OneByte { upvotes }.try_into()?;
    Ok(zero_value(script))
}

pub async fn test_m4_vote_array(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let constants = post_setup
        .validator_service_client
        .get_chain_info(GetChainInfoRequest::default())
        .await?
        .into_owned()
        .bip300_constants
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("GetChainInfo returned no bip300_constants"))?;
    let threshold = constants.unused_sidechain_slot_activation_threshold;
    // The cases below upvote each bundle twice, on top of its M3 ACK score of
    // 1. A bundle that crossed the inclusion threshold would have to be paid
    // out by an M6 in the same block, and this test never deposits, so there
    // would be no CTIP to spend.
    anyhow::ensure!(
        constants.withdrawal_bundle_inclusion_threshold > 3,
        "regtest inclusion threshold {} leaves no room to vote a bundle up twice",
        constants.withdrawal_bundle_inclusion_threshold
    );

    // Three active slots: `SLOT_LOW`, `SLOT_HIGH` and `SLOT_EXTRA`. Everything
    // that mines with ack-all on happens here, before any bundle exists: with
    // nothing to vote on, the M4s in these blocks cannot disturb a vote count.
    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    let () = activate_sidechain::<DummySidechain>(&mut post_setup).await?;
    for (slot, expected_active) in [(SLOT_HIGH, 2), (SLOT_EXTRA, 3)] {
        tracing::info!("Activating slot {slot}");
        let () = propose_sidechain_for_slot::<DummySidechain>(&mut post_setup, slot).await?;
        while sidechains_active(&mut post_setup).await? < expected_active {
            let () = mine::<DummySidechain>(&mut post_setup, 1, Some(true)).await?;
        }
    }
    // Proposed while that is still true. `propose_sidechain_for_slot` mines
    // its M1 block with ack-all on, which would otherwise cost each bundle a
    // vote.
    let mid_description_hash = propose_and_ack(&mut post_setup, SLOT_MID).await?;

    // From here on every M4 in the chain is one this test hand-builds: with
    // ack-all off the producer emits no M4 at all, so the bundle vote counts
    // move only when a case says they should. `SLOT_MID`'s explicit ACK keeps
    // its M2s coming regardless.
    let () = post_setup
        .block_producer_service_client
        .set_ack_all_proposals(SetAckAllProposalsRequest { ack_all: false })
        .await
        .map(|_| ())?;

    tracing::info!("Proposing a withdrawal bundle in each active slot");
    let low_bundle = propose_bundle(&mut post_setup, SLOT_LOW, 1_000).await?;
    let high_bundle = propose_bundle(&mut post_setup, SLOT_HIGH, 2_000).await?;
    let extra_bundle = propose_bundle(&mut post_setup, SLOT_EXTRA, 3_000).await?;

    let () = mine_to_activation_brink(&mut post_setup, SLOT_MID, threshold).await?;
    anyhow::ensure!(
        sidechains_active(&mut post_setup).await? == 3,
        "slot {SLOT_MID} activated before its final ACK"
    );

    // Counts must be untouched by all of the above: no M4 has been mined since
    // each bundle's own M3.
    let bundles = Bundles {
        low: low_bundle,
        high: high_bundle,
        extra: extra_bundle,
    };
    anyhow::ensure!(
        counts(&mut post_setup, bundles).await? == (1, 1, 1),
        "a bundle gathered votes before the first hand-built M4: {:?}",
        counts(&mut post_setup, bundles).await?
    );

    // ── Case A ────────────────────────────────────────────────────────────
    // `ASN` is [LOW, HIGH, EXTRA]; the M2 in this coinbase activates MID,
    // making it [LOW, MID, HIGH, EXTRA]. A two-element array sized against the
    // pre-activation list would put its second vote on MID rather than HIGH.
    //
    // MID holds no bundle, so this particular displacement would also be
    // caught by the out-of-range rule — Case A' is the one that would
    // otherwise pass silently.
    tracing::info!("Case A: a shortened M4 in a coinbase that activates a slot");
    let block_hash = submit_block_with_coinbase_outputs(
        &post_setup,
        vec![
            m2_output(SLOT_MID, mid_description_hash)?,
            m4_output(vec![0, 0])?,
        ],
    )
    .await?;
    let () = assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Rejected {
            log_contains: "an M2 earlier in this coinbase activated a new sidechain slot",
        },
        VERDICT_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        sidechains_active(&mut post_setup).await? == 3,
        "the rejected block's M2 activated a slot anyway"
    );
    anyhow::ensure!(
        counts(&mut post_setup, bundles).await? == (1, 1, 1),
        "the rejected block's M4 moved bundle vote counts"
    );

    // ── Case A' ───────────────────────────────────────────────────────────
    // The displacement rule 4 uniquely prevents. Read against the
    // pre-activation [LOW, HIGH, EXTRA], this array upvotes LOW, abstains on
    // HIGH and upvotes EXTRA. Read against the post-activation [LOW, MID,
    // HIGH, EXTRA], every index past MID slides by one: the abstain lands on
    // MID and the second upvote lands on HIGH — a real bundle in a sidechain
    // the miner did not vote for, and no index is out of range, so nothing
    // else in the validator would object.
    tracing::info!("Case A': a shortened M4 whose displaced vote would hit a populated slot");
    let block_hash = submit_block_with_coinbase_outputs(
        &post_setup,
        vec![
            m2_output(SLOT_MID, mid_description_hash)?,
            m4_output(vec![0, M4AckBundles::ABSTAIN_ONE_BYTE, 0])?,
        ],
    )
    .await?;
    let () = assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Rejected {
            log_contains: "an M2 earlier in this coinbase activated a new sidechain slot",
        },
        VERDICT_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        counts(&mut post_setup, bundles).await? == (1, 1, 1),
        "the rejected block's M4 moved bundle vote counts"
    );

    // ── Case B ────────────────────────────────────────────────────────────
    // The same coinbase with a vote per active slot is unambiguous, and the
    // votes must resolve against the *post*-activation list: index 2 is HIGH
    // and index 3 is EXTRA, with MID — the slot the M2 just activated —
    // abstaining at index 1.
    tracing::info!("Case B: a full-length M4 in a coinbase that activates a slot");
    let block_hash = submit_block_with_coinbase_outputs(
        &post_setup,
        vec![
            m2_output(SLOT_MID, mid_description_hash)?,
            m4_output(vec![0, M4AckBundles::ABSTAIN_ONE_BYTE, 0, 0])?,
        ],
    )
    .await?;
    let () = assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Accepted,
        VERDICT_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        sidechains_active(&mut post_setup).await? == 4,
        "the M2 did not activate slot {SLOT_MID}"
    );
    anyhow::ensure!(
        counts(&mut post_setup, bundles).await? == (2, 2, 2),
        "votes did not land one per slot against the post-activation list: {:?}",
        counts(&mut post_setup, bundles).await?
    );

    // ── Case C ────────────────────────────────────────────────────────────
    // `ASN` is now [LOW, MID, HIGH, EXTRA] and stays that way. A one-element
    // array omits three slots, two of which hold real pending bundles, so an
    // omitted slot has to *abstain* rather than merely have nothing to vote
    // on. No M2 here, so nothing has grown `ASN` within the block.
    tracing::info!("Case C: a shortened M4 omitting slots that hold pending bundles");
    let block_hash =
        submit_block_with_coinbase_outputs(&post_setup, vec![m4_output(vec![0])?]).await?;
    let () = assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Accepted,
        VERDICT_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        counts(&mut post_setup, bundles).await? == (3, 2, 2),
        "the omitted trailing slots did not abstain: {:?}",
        counts(&mut post_setup, bundles).await?
    );

    // ── Case D ────────────────────────────────────────────────────────────
    // An abstain that is not trailing still has to hold its index, or the vote
    // after it lands on the wrong slot. Only `EXTRA` is omitted here.
    tracing::info!("Case D: abstains ahead of a vote hold their slots' indices");
    let block_hash = submit_block_with_coinbase_outputs(
        &post_setup,
        vec![m4_output(vec![
            M4AckBundles::ABSTAIN_ONE_BYTE,
            M4AckBundles::ABSTAIN_ONE_BYTE,
            0,
        ])?],
    )
    .await?;
    let () = assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Accepted,
        VERDICT_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        counts(&mut post_setup, bundles).await? == (3, 3, 2),
        "the vote at index 2 did not land on slot {SLOT_HIGH} alone: {:?}",
        counts(&mut post_setup, bundles).await?
    );

    // ── Case E ────────────────────────────────────────────────────────────
    // More votes than active slots sets votes in slots that are not active.
    tracing::info!("Case E: an M4 with more votes than there are active slots");
    let block_hash =
        submit_block_with_coinbase_outputs(&post_setup, vec![m4_output(vec![0, 0, 0, 0, 0])?])
            .await?;
    let () = assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Rejected {
            log_contains: "Invalid votes: expected at most",
        },
        VERDICT_TIMEOUT,
    )
    .await?;
    anyhow::ensure!(
        counts(&mut post_setup, bundles).await? == (3, 3, 2),
        "the rejected over-long M4 moved bundle vote counts"
    );

    drop(post_setup);
    Ok(())
}
