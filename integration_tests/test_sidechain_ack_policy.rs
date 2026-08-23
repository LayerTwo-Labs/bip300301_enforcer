//! The block producer's sidechain ACK policy: the automatic policies, and
//! explicit ACK/NACK.
//!
//! Runs in `GetBlockTemplate` mode: templates are what read the persisted
//! policy, so the M2 acks in the coinbases mined here come from the policy
//! alone, and the `ack_policy` argument to `mine` is ignored.

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    messages::{CoinbaseMessage, M4AckBundles},
    proto::{
        self,
        mainchain::{
            AckAllProposalsPolicy, BlockHeaderInfo, BroadcastWithdrawalBundleRequest,
            GetBlockProducerStateRequest, GetChainInfoRequest, GetChainTipRequest,
            GetSidechainProposalsRequest, GetSidechainProposalsResponse, GetSidechainsRequest,
            GetWithdrawalBundleProposalsRequest, GetWithdrawalBundleProposalsResponse,
            SetAckAllProposalsRequest, SetSidechainAckRequest, SetWithdrawalBundlePolicyRequest,
            WithdrawalBundlePolicy, get_sidechain_proposals_response,
        },
    },
    types::SidechainNumber,
};
use bitcoin::{Amount, BlockHash, Txid, hashes::sha256d};
use futures::channel::mpsc;

use crate::{
    integration_test::{deposit, fund_enforcer, propose_sidechain, propose_sidechain_for_slot},
    mine::{MiningPolicy, mine},
    setup::{DummySidechain, PostSetup, Sidechain as _},
    test_blinded_m6_roundtrip::{make_blinded_m6, serialize_zero_input_legacy},
};

pub(crate) async fn set_bundle_policy(
    post_setup: &mut PostSetup,
    policy: WithdrawalBundlePolicy,
) -> anyhow::Result<()> {
    post_setup
        .block_producer_service_client
        .set_withdrawal_bundle_policy(SetWithdrawalBundlePolicyRequest {
            policy: policy.into(),
        })
        .await
        .map(|_| ())
        .map_err(Into::into)
}

async fn set_ack_policy(
    post_setup: &mut PostSetup,
    policy: AckAllProposalsPolicy,
) -> anyhow::Result<()> {
    post_setup
        .block_producer_service_client
        .set_ack_all_proposals(SetAckAllProposalsRequest {
            policy: policy.into(),
        })
        .await
        .map(|_| ())
        .map_err(Into::into)
}

/// The single pending proposal for `slot`, per the validator.
async fn proposal_for_slot(
    post_setup: &mut PostSetup,
    slot: SidechainNumber,
) -> anyhow::Result<get_sidechain_proposals_response::SidechainProposal> {
    let mut matching: Vec<_> = post_setup
        .validator_service_client
        .get_sidechain_proposals(GetSidechainProposalsRequest::default())
        .await?
        .into_owned()
        .sidechain_proposals
        .into_iter()
        .filter(|proposal| {
            proto::unwrap_u32(proposal.sidechain_number.clone()) == Some(u32::from(slot.0))
        })
        .collect();
    match matching.len() {
        1 => Ok(matching.remove(0)),
        n => anyhow::bail!("expected exactly 1 pending proposal for slot {slot}, got {n}"),
    }
}

async fn sidechains_active(post_setup: &mut PostSetup) -> anyhow::Result<usize> {
    let resp = post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest::default())
        .await?
        .into_owned();
    Ok(resp.sidechains.len())
}

/// All parseable BIP300 coinbase messages in the chain tip's coinbase.
pub(crate) async fn tip_coinbase_messages(
    post_setup: &mut PostSetup,
) -> anyhow::Result<Vec<CoinbaseMessage>> {
    let tip_hash: BlockHash = post_setup
        .validator_service_client
        .get_chain_tip(GetChainTipRequest::default())
        .await?
        .into_owned()
        .block_header_info
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("GetChainTip returned no block_header_info"))?
        .block_hash
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("GetChainTip returned no block_hash"))?
        .decode::<BlockHeaderInfo, _>("block_hash")?;
    let block_hex = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [tip_hash.to_string(), "0".to_string()])
        .run_utf8()
        .await?;
    let block: bitcoin::Block = bitcoin::consensus::deserialize(&hex::decode(block_hex.trim())?)?;
    let coinbase = block
        .txdata
        .first()
        .ok_or_else(|| anyhow::anyhow!("block {tip_hash} has no coinbase"))?;
    let messages = coinbase
        .output
        .iter()
        .filter_map(|txout| {
            CoinbaseMessage::parse(&txout.script_pubkey)
                .ok()
                .map(|(_rest, message)| message)
        })
        .collect();
    Ok(messages)
}

/// The vote count of the only pending withdrawal bundle for
/// [`DummySidechain`]'s slot, according to the validator.
pub(crate) async fn bundle_vote_count(
    post_setup: &mut PostSetup,
    m6id: Txid,
) -> anyhow::Result<u32> {
    let proposals = post_setup
        .validator_service_client
        .get_withdrawal_bundle_proposals(GetWithdrawalBundleProposalsRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
        })
        .await?
        .into_owned()
        .proposals;
    let [bundle] = proposals.as_slice() else {
        anyhow::bail!(
            "expected exactly 1 pending withdrawal bundle, got {}",
            proposals.len()
        )
    };
    let bundle_m6id: Txid = bundle
        .m6id
        .clone()
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("withdrawal bundle proposal missing m6id"))?
        .decode::<GetWithdrawalBundleProposalsResponse, _>("m6id")?;
    anyhow::ensure!(
        bundle_m6id == m6id,
        "pending withdrawal bundle m6id mismatch: {bundle_m6id} != {m6id}"
    );
    proto::unwrap_u32(bundle.vote_count.clone())
        .ok_or_else(|| anyhow::anyhow!("withdrawal bundle proposal missing vote_count"))
}

pub async fn test_sidechain_ack_policy(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (sidechain_res_tx, _sidechain_res_rx) = mpsc::unbounded();
    let mut sidechain = DummySidechain::setup((), &post_setup, sidechain_res_tx).await?;

    // Black box: learn the network's BIP300 constants over RPC, like any
    // client, rather than hardcoding the enforcer's regtest thresholds.
    let constants = post_setup
        .validator_service_client
        .get_chain_info(GetChainInfoRequest::default())
        .await?
        .into_owned()
        .bip300_constants
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("GetChainInfo returned no bip300_constants"))?;

    // Enough blocks for a proposal to reach the activation threshold, if ACKed
    // in each of them: activation requires strictly more ACKs than the
    // threshold.
    let blocks_to_activate = constants.unused_sidechain_slot_activation_threshold + 1;
    // Blocks mined while the proposal is un-ACKed: half the slack left in the
    // proposal's age budget after the activation blocks are accounted for, so
    // the proposal never expires.
    let blocks_without_ack =
        (constants.unused_sidechain_slot_proposal_max_age - blocks_to_activate) / 2;

    let state = post_setup
        .block_producer_service_client
        .get_block_producer_state(GetBlockProducerStateRequest::default())
        .await?
        .into_owned();
    anyhow::ensure!(
        state.ack_policy == AckAllProposalsPolicy::NewSlots && state.explicit_acks.is_empty(),
        "unexpected initial ACK policy: `{state:?}`"
    );

    let () = set_ack_policy(&mut post_setup, AckAllProposalsPolicy::None).await?;
    // The M4 has its own policy now; this test drives the M2 side, so hold
    // bundle voting off until it explicitly wants an M4 below.
    let () = set_bundle_policy(&mut post_setup, WithdrawalBundlePolicy::None).await?;

    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;

    tracing::info!("Mining without ACKing: the proposal must gather no votes");
    let () =
        mine::<DummySidechain>(&mut post_setup, blocks_without_ack, MiningPolicy::SILENT).await?;
    let proposals = post_setup
        .validator_service_client
        .get_sidechain_proposals(GetSidechainProposalsRequest::default())
        .await?
        .into_owned()
        .sidechain_proposals;
    let [proposal] = proposals.as_slice() else {
        anyhow::bail!(
            "expected exactly 1 sidechain proposal, got {}",
            proposals.len()
        )
    };
    anyhow::ensure!(
        proto::unwrap_u32(proposal.vote_count.clone()) == Some(0),
        "proposal gathered votes despite auto-ACK being off and no explicit ACK: `{proposal:?}`"
    );
    let description_sha256d_hash = proposal.description_sha256d_hash.clone();

    let set_ack = |ack: bool| SetSidechainAckRequest {
        sidechain_number: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
        description_sha256d_hash: description_sha256d_hash.clone(),
        ack,
    };

    let () = post_setup
        .block_producer_service_client
        .set_sidechain_ack(set_ack(true))
        .await
        .map(|_| ())?;
    let state = post_setup
        .block_producer_service_client
        .get_block_producer_state(GetBlockProducerStateRequest::default())
        .await?
        .into_owned();
    anyhow::ensure!(
        state.ack_policy == AckAllProposalsPolicy::None && state.explicit_acks.len() == 1,
        "expected exactly 1 explicit ACK, got: `{state:?}`"
    );

    // NACK, then ACK again: the NACK must remove the ACK from the policy, or
    // ACKs would be permanent.
    let () = post_setup
        .block_producer_service_client
        .set_sidechain_ack(set_ack(false))
        .await
        .map(|_| ())?;
    let state = post_setup
        .block_producer_service_client
        .get_block_producer_state(GetBlockProducerStateRequest::default())
        .await?
        .into_owned();
    anyhow::ensure!(
        state.explicit_acks.is_empty(),
        "NACK did not remove the ACK: `{state:?}`"
    );
    let () = post_setup
        .block_producer_service_client
        .set_sidechain_ack(set_ack(true))
        .await
        .map(|_| ())?;

    tracing::info!("Mining with an explicit ACK: the sidechain must activate");
    let () =
        mine::<DummySidechain>(&mut post_setup, blocks_to_activate, MiningPolicy::SILENT).await?;
    anyhow::ensure!(
        sidechains_active(&mut post_setup).await? == 1,
        "sidechain did not activate despite an explicit ACK"
    );

    // Withdrawal-bundle policy is meaningful only after the active sidechain
    // has a treasury UTXO. Establish one before testing its M3/M4 votes.
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

    // The case enabled by suppressing the M4 only for M2s that can activate a
    // new slot, rather than for any M2: a withdrawal bundle gathers votes in
    // a block whose coinbase also ACKs a proposal for another slot.

    tracing::info!("Proposing a withdrawal bundle for the active sidechain");
    let bundle_tx = make_blinded_m6(1_000, Amount::from_sat(50_000));
    let bundle_m6id = bundle_tx.compute_txid();
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
    // Mine the bundle's M3 into the chain. Per BIP300 M3, a newly proposed
    // bundle starts with an ACK score of 1; bundle voting is still off, so
    // this block carries no M4 on top of that.
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 1,
        "expected the bundle to sit at its initial M3 ACK score of 1"
    );

    // A proposal for a second slot: with auto-ACK on, the next block template
    // wants to carry an M2 for it. A single ACK cannot activate the slot
    // (activation requires strictly more ACKs than the threshold), so the M2
    // must not suppress the M4 that votes on the bundle.
    const OTHER_SLOT: SidechainNumber = SidechainNumber(1);
    let () = propose_sidechain_for_slot::<DummySidechain>(&mut post_setup, OTHER_SLOT, "sidechain")
        .await?;
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 1,
        "bundle gathered a vote in a block without an M4"
    );
    let () = set_ack_policy(&mut post_setup, AckAllProposalsPolicy::NewSlots).await?;
    // The bundle was broadcast through this node, so `Known` backs it.
    let () = set_bundle_policy(&mut post_setup, WithdrawalBundlePolicy::Known).await?;

    tracing::info!("Mining with auto-ACK on: expecting an M2 and an M4 in one coinbase");
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::VOTE).await?;

    // The coinbase must ACK the other slot's proposal...
    let coinbase_messages = tip_coinbase_messages(&mut post_setup).await?;
    anyhow::ensure!(
        coinbase_messages.iter().any(|message| matches!(
            message,
            CoinbaseMessage::M2AckSidechain(m2) if m2.sidechain_number == OTHER_SLOT
        )),
        "expected an M2 ACK for slot {OTHER_SLOT} in the coinbase"
    );
    // ...and still carry an M4 upvoting the bundle: one active sidechain,
    // upvoting its first (and only) pending bundle.
    anyhow::ensure!(
        coinbase_messages.iter().any(|message| matches!(
            message,
            CoinbaseMessage::M4AckBundles(M4AckBundles::OneByte { upvotes })
                if upvotes.as_slice() == [0u8].as_slice()
        )),
        "expected an M4 upvoting the bundle in the same coinbase"
    );
    // The validator must have counted the bundle's vote from that block...
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 2,
        "bundle vote count did not increment in a block with an M2 ACK"
    );
    // ...as well as the M2 ACK itself.
    let proposals = post_setup
        .validator_service_client
        .get_sidechain_proposals(GetSidechainProposalsRequest::default())
        .await?
        .into_owned()
        .sidechain_proposals;
    let [other_slot_proposal] = proposals.as_slice() else {
        anyhow::bail!(
            "expected exactly 1 sidechain proposal, got {}",
            proposals.len()
        )
    };
    anyhow::ensure!(
        proto::unwrap_u32(other_slot_proposal.vote_count.clone()) == Some(1),
        "the other slot's proposal was not ACKed: `{other_slot_proposal:?}`"
    );

    // A proposal for the slot the active sidechain occupies. Activating it
    // would evict that sidechain (BIP300 M2, used-slot threshold), which is
    // what `NEW_SLOTS` withholds its automatic vote from.
    //
    // Proposed under `NONE` so that the M1's own block casts no votes: it
    // keeps the other slot's proposal, which is still gathering ACKs on an
    // unused slot, well clear of activating and changing the active set under
    // the assertions below.
    let () = set_ack_policy(&mut post_setup, AckAllProposalsPolicy::None).await?;
    let occupied_slot = DummySidechain::SIDECHAIN_NUMBER;
    let () = propose_sidechain_for_slot::<DummySidechain>(
        &mut post_setup,
        occupied_slot,
        "replacement sidechain",
    )
    .await?;
    let replacement_hash: sha256d::Hash = proposal_for_slot(&mut post_setup, occupied_slot)
        .await?
        .description_sha256d_hash
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("sidechain proposal missing description_sha256d_hash"))?
        .decode::<GetSidechainProposalsResponse, _>("description_sha256d_hash")?;

    tracing::info!("Mining under NEW_SLOTS: the replacement must gather no votes");
    let () = set_ack_policy(&mut post_setup, AckAllProposalsPolicy::NewSlots).await?;
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::VOTE).await?;

    let coinbase_messages = tip_coinbase_messages(&mut post_setup).await?;
    anyhow::ensure!(
        !coinbase_messages.iter().any(|message| matches!(
            message,
            CoinbaseMessage::M2AckSidechain(m2) if m2.sidechain_number == occupied_slot
        )),
        "NEW_SLOTS ACKed a proposal that would replace the active sidechain in \
         slot {occupied_slot}"
    );
    // The same coinbase still ACKs the proposal on the unused slot, so the
    // absence above is the replacement rule, not auto-ACKing having stopped.
    anyhow::ensure!(
        coinbase_messages.iter().any(|message| matches!(
            message,
            CoinbaseMessage::M2AckSidechain(m2) if m2.sidechain_number == OTHER_SLOT
        )),
        "expected NEW_SLOTS to keep ACKing the unused slot {OTHER_SLOT}"
    );
    let replacement = proposal_for_slot(&mut post_setup, occupied_slot).await?;
    anyhow::ensure!(
        proto::unwrap_u32(replacement.vote_count.clone()) == Some(0),
        "the replacement proposal gathered a vote under NEW_SLOTS: `{replacement:?}`"
    );

    tracing::info!("Mining under ALL: the replacement must be ACKed");
    let () = set_ack_policy(&mut post_setup, AckAllProposalsPolicy::All).await?;
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::VOTE).await?;

    let coinbase_messages = tip_coinbase_messages(&mut post_setup).await?;
    anyhow::ensure!(
        coinbase_messages.iter().any(|message| matches!(
            message,
            CoinbaseMessage::M2AckSidechain(m2)
                if m2.sidechain_number == occupied_slot
                    && m2.description_hash == replacement_hash
        )),
        "expected ALL to ACK the replacement proposal for slot {occupied_slot}"
    );
    // An M2 for a *used* slot cannot grow the active set, so unlike an M2 that
    // can activate an unused slot it must not suppress the M4.
    anyhow::ensure!(
        coinbase_messages.iter().any(|message| matches!(
            message,
            CoinbaseMessage::M4AckBundles(M4AckBundles::OneByte { upvotes })
                if upvotes.len() == 1
        )),
        "a replacement ACK suppressed the M4 in the same coinbase"
    );
    let replacement = proposal_for_slot(&mut post_setup, occupied_slot).await?;
    anyhow::ensure!(
        proto::unwrap_u32(replacement.vote_count.clone()) == Some(1),
        "the replacement proposal was not ACKed under ALL: `{replacement:?}`"
    );

    drop(post_setup);
    Ok(())
}
