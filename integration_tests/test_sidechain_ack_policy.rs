//! The block producer's sidechain ACK policy: ack-all, and explicit ACK/NACK.
//!
//! Runs in `GetBlockTemplate` mode: templates are what read the persisted
//! policy, so the M2 acks in the coinbases mined here come from the policy
//! alone, and the `ack_all_proposals` argument to `mine` is ignored.

use bip300301_enforcer_lib::proto::{
    self,
    mainchain::{
        GetBlockProducerStateRequest, GetChainInfoRequest, GetSidechainProposalsRequest,
        GetSidechainsRequest, SetAckAllProposalsRequest, SetSidechainAckRequest,
    },
};

use crate::{
    integration_test::propose_sidechain,
    mine::mine,
    setup::{DummySidechain, PostSetup, Sidechain as _},
};

async fn sidechains_active(post_setup: &mut PostSetup) -> anyhow::Result<usize> {
    let resp = post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest::default())
        .await?
        .into_owned();
    Ok(resp.sidechains.len())
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

    drop(post_setup);
    Ok(())
}
