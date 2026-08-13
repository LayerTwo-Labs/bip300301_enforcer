//! The block producer's sidechain ACK policy: ack-all, and explicit ACK/NACK.
//!
//! Runs in `GetBlockTemplate` mode: templates are what read the persisted
//! policy, so the M2 acks in the coinbases mined here come from the policy
//! alone, and the `ack_all_proposals` argument to `mine` is ignored.

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    messages::{CoinbaseMessage, M4AckBundles},
    proto::{
        self,
        mainchain::{
            BlockHeaderInfo, BroadcastWithdrawalBundleRequest, GetBlockProducerStateRequest,
            GetChainInfoRequest, GetChainTipRequest, GetSidechainProposalsRequest,
            GetSidechainsRequest, GetWithdrawalBundleProposalsRequest,
            GetWithdrawalBundleProposalsResponse, SetAckAllProposalsRequest,
            SetSidechainAckRequest,
        },
    },
    types::SidechainNumber,
};
use bitcoin::{Amount, BlockHash, Txid};

use crate::{
    integration_test::{propose_sidechain, propose_sidechain_for_slot},
    mine::mine,
    setup::{DummySidechain, PostSetup, Sidechain as _},
    test_blinded_m6_roundtrip::{make_blinded_m6, serialize_zero_input_legacy},
};

async fn sidechains_active(post_setup: &mut PostSetup) -> anyhow::Result<usize> {
    let resp = post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest::default())
        .await?
        .into_owned();
    Ok(resp.sidechains.len())
}

/// All parseable BIP300 coinbase messages in the chain tip's coinbase.
async fn tip_coinbase_messages(post_setup: &mut PostSetup) -> anyhow::Result<Vec<CoinbaseMessage>> {
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
async fn bundle_vote_count(post_setup: &mut PostSetup, m6id: Txid) -> anyhow::Result<u32> {
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
        state.ack_all_proposals && state.explicit_acks.is_empty(),
        "unexpected initial ACK policy: `{state:?}`"
    );

    let () = post_setup
        .block_producer_service_client
        .set_ack_all_proposals(SetAckAllProposalsRequest { ack_all: false })
        .await
        .map(|_| ())?;

    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;

    tracing::info!("Mining without ACKing: the proposal must gather no votes");
    let () = mine::<DummySidechain>(&mut post_setup, blocks_without_ack, Some(false)).await?;
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
        "proposal gathered votes despite ack-all being off and no explicit ACK: `{proposal:?}`"
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
        !state.ack_all_proposals && state.explicit_acks.len() == 1,
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
    let () = mine::<DummySidechain>(&mut post_setup, blocks_to_activate, Some(false)).await?;
    anyhow::ensure!(
        sidechains_active(&mut post_setup).await? == 1,
        "sidechain did not activate despite an explicit ACK"
    );

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
    // bundle starts with an ACK score of 1; ack-all is still off, so this
    // block carries no M4 on top of that.
    let () = mine::<DummySidechain>(&mut post_setup, 1, Some(false)).await?;
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 1,
        "expected the bundle to sit at its initial M3 ACK score of 1"
    );

    // A proposal for a second slot: with ack-all on, the next block template
    // wants to carry an M2 for it. A single ACK cannot activate the slot
    // (activation requires strictly more ACKs than the threshold), so the M2
    // must not suppress the M4 that votes on the bundle.
    const OTHER_SLOT: SidechainNumber = SidechainNumber(1);
    let () = propose_sidechain_for_slot::<DummySidechain>(&mut post_setup, OTHER_SLOT).await?;
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 1,
        "bundle gathered a vote in a block without an M4"
    );
    let () = post_setup
        .block_producer_service_client
        .set_ack_all_proposals(SetAckAllProposalsRequest { ack_all: true })
        .await
        .map(|_| ())?;

    tracing::info!("Mining with ack-all on: expecting an M2 and an M4 in one coinbase");
    let () = mine::<DummySidechain>(&mut post_setup, 1, Some(true)).await?;

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
    let other_slot_description_hash = other_slot_proposal.description_sha256d_hash.clone();

    // The converse case: a block whose M2 *does* activate a new slot. The
    // active-slot list an M4 is indexed against grows within that block, so
    // the producer must omit the M4, and the votes in the following block must
    // land on the same sidechains the validator resolves them to.

    // Drive the other slot's proposal to one ACK short of activation with
    // ack-all off, so the M2s keep coming from the explicit ACK while no M4 is
    // emitted. Leaving the M4 off here also keeps the bundle below the
    // inclusion threshold: this test never deposits, so an approved bundle
    // would have no CTIP to spend.
    let () = post_setup
        .block_producer_service_client
        .set_ack_all_proposals(SetAckAllProposalsRequest { ack_all: false })
        .await
        .map(|_| ())?;
    let () = post_setup
        .block_producer_service_client
        .set_sidechain_ack(SetSidechainAckRequest {
            sidechain_number: proto::wrap_u32(OTHER_SLOT.0.into()),
            description_sha256d_hash: other_slot_description_hash,
            ack: true,
        })
        .await
        .map(|_| ())?;
    tracing::info!("Mining the other slot's proposal to one ACK short of activation");
    let () = mine::<DummySidechain>(&mut post_setup, blocks_to_activate - 2, Some(false)).await?;
    anyhow::ensure!(
        sidechains_active(&mut post_setup).await? == 1,
        "the other slot activated before its final ACK"
    );
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 2,
        "the bundle gathered votes from blocks that carry no M4"
    );

    let () = post_setup
        .block_producer_service_client
        .set_ack_all_proposals(SetAckAllProposalsRequest { ack_all: true })
        .await
        .map(|_| ())?;
    tracing::info!("Mining the activating block: its M2 grows the active set, so it carries no M4");
    let () = mine::<DummySidechain>(&mut post_setup, 1, Some(true)).await?;
    anyhow::ensure!(
        sidechains_active(&mut post_setup).await? == 2,
        "the other slot did not activate on its final ACK"
    );
    let coinbase_messages = tip_coinbase_messages(&mut post_setup).await?;
    anyhow::ensure!(
        coinbase_messages.iter().any(|message| matches!(
            message,
            CoinbaseMessage::M2AckSidechain(m2) if m2.sidechain_number == OTHER_SLOT
        )),
        "expected the activating M2 for slot {OTHER_SLOT} in the coinbase"
    );
    anyhow::ensure!(
        !coinbase_messages
            .iter()
            .any(|message| matches!(message, CoinbaseMessage::M4AckBundles(_))),
        "expected no M4 in a coinbase whose M2 activates a new slot"
    );
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 2,
        "the bundle gathered a vote from a block that carries no M4"
    );

    // With both slots active, the freshly activated slot sorts *after* the
    // bundle's slot and has no pending bundle of its own, so it is a trailing
    // abstain and the producer omits it. This is the shortened-array case:
    // a one-element M4 against a two-slot active list, which the validator
    // must resolve to the bundle's slot with the omitted slot abstaining.
    tracing::info!("Mining with both slots active: the M4 must omit the trailing abstain");
    let () = mine::<DummySidechain>(&mut post_setup, 1, Some(true)).await?;
    let coinbase_messages = tip_coinbase_messages(&mut post_setup).await?;
    anyhow::ensure!(
        coinbase_messages.iter().any(|message| matches!(
            message,
            CoinbaseMessage::M4AckBundles(M4AckBundles::OneByte { upvotes })
                if upvotes.as_slice() == [0u8].as_slice()
        )),
        "expected a shortened M4 covering only the bundle's slot: `{coinbase_messages:?}`"
    );
    // Had the votes been shifted by the activation, the upvote at index 0
    // would have landed on the newly activated slot instead.
    anyhow::ensure!(
        bundle_vote_count(&mut post_setup, bundle_m6id).await? == 3,
        "the bundle did not gather the vote at its own index"
    );

    drop(post_setup);
    Ok(())
}
