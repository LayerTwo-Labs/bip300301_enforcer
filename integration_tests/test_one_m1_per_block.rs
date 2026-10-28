//! A block carries at most one M1, so the block producer emits queued
//! sidechain proposals one per block, lowest (sidechain number, proposal hash)
//! first. The one-per-block limit is M1-only: M2s for distinct slots still
//! share a coinbase.

use bip300301_enforcer_lib::{
    messages::{CoinbaseMessage, create_sidechain_proposal},
    proto::{
        self,
        mainchain::{GetSidechainProposalsRequest, SubmitSidechainProposalRequest},
    },
    types::{self, SidechainNumber, SidechainProposalId},
};
use futures::channel::mpsc;

use crate::{
    mine::{MiningPolicy, mine},
    setup::{DummySidechain, PostSetup, Sidechain as _, wait_for_pending_proposal},
    test_sidechain_ack_policy::tip_coinbase_messages,
};

pub const TEST_NAME: &str = "one_m1_per_block";

const LOWER_SLOT: SidechainNumber = SidechainNumber(2);
const HIGHER_SLOT: SidechainNumber = SidechainNumber(3);
/// Holds two proposals, so their order comes down to the proposal hash.
const TIED_SLOT: SidechainNumber = SidechainNumber(4);

fn declaration(title: String) -> types::SidechainDeclaration {
    types::SidechainDeclaration {
        title,
        description: "one-m1-per-block test".to_owned(),
        hash_id_1: [0; 32],
        hash_id_2: [0; 20],
    }
}

fn proposal_id(
    slot: SidechainNumber,
    declaration: &types::SidechainDeclaration,
) -> anyhow::Result<SidechainProposalId> {
    let (_, description) = create_sidechain_proposal(slot, declaration)?;
    Ok(SidechainProposalId {
        sidechain_number: slot,
        description_hash: description.sha256d_hash(),
    })
}

async fn submit_proposal(
    post_setup: &mut PostSetup,
    slot: SidechainNumber,
    declaration: types::SidechainDeclaration,
) -> anyhow::Result<SidechainProposalId> {
    let id = proposal_id(slot, &declaration)?;
    let _resp = post_setup
        .block_producer_service_client
        .submit_sidechain_proposal(SubmitSidechainProposalRequest {
            sidechain_id: proto::wrap_u32(slot.0.into()),
            declaration: buffa::MessageField::some(declaration.into()),
        })
        .await?;
    let () = wait_for_pending_proposal(&post_setup.block_producer_service_client, slot).await?;
    Ok(id)
}

/// Mine one block and return the M1s' proposal IDs and the M2s' slots in its
/// coinbase.
async fn mine_one(
    post_setup: &mut PostSetup,
    policy: MiningPolicy,
) -> anyhow::Result<(Vec<SidechainProposalId>, Vec<SidechainNumber>)> {
    let () = mine::<DummySidechain>(post_setup, 1, policy).await?;
    let mut m1_ids = Vec::new();
    let mut m2_slots = Vec::new();
    for message in tip_coinbase_messages(post_setup).await? {
        match message {
            CoinbaseMessage::M1ProposeSidechain(m1) => m1_ids.push(SidechainProposalId {
                sidechain_number: m1.sidechain_number,
                description_hash: m1.description.sha256d_hash(),
            }),
            CoinbaseMessage::M2AckSidechain(m2) => m2_slots.push(m2.sidechain_number),
            _ => (),
        }
    }
    Ok((m1_ids, m2_slots))
}

/// Mine one block per expected M1, then one more to check the queue drained.
/// Silent, so no M2s: the M1s are all this looks at.
async fn expect_m1_emission_order(
    post_setup: &mut PostSetup,
    expected: &[SidechainProposalId],
) -> anyhow::Result<()> {
    for expected_id in expected {
        let (m1_ids, _) = mine_one(post_setup, MiningPolicy::SILENT).await?;
        anyhow::ensure!(
            m1_ids == [*expected_id],
            "expected exactly one M1, for {expected_id:?}, got {m1_ids:?}"
        );
    }
    let (m1_ids, _) = mine_one(post_setup, MiningPolicy::SILENT).await?;
    anyhow::ensure!(
        m1_ids.is_empty(),
        "the proposal queue should have drained, got M1s for {m1_ids:?}"
    );
    Ok(())
}

pub async fn test_one_m1_per_block(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (sidechain_res_tx, _sidechain_res_rx) = mpsc::unbounded();
    let _sidechain = DummySidechain::setup((), &post_setup, sidechain_res_tx).await?;

    // Queued in the opposite of emission order, so the order observed below
    // comes from the producer's selection rather than from the queue.
    let higher = submit_proposal(
        &mut post_setup,
        HIGHER_SLOT,
        declaration(format!("slot {HIGHER_SLOT}")),
    )
    .await?;
    let lower = submit_proposal(
        &mut post_setup,
        LOWER_SLOT,
        declaration(format!("slot {LOWER_SLOT}")),
    )
    .await?;
    let () = expect_m1_emission_order(&mut post_setup, &[lower, higher]).await?;

    // Both proposals are for empty slots, so `VOTE` auto-ACKs them in one
    // coinbase, and the enforcer must accept that block.
    let (_, mut m2_slots) = mine_one(&mut post_setup, MiningPolicy::VOTE).await?;
    m2_slots.sort_unstable();
    anyhow::ensure!(
        m2_slots == [LOWER_SLOT, HIGHER_SLOT],
        "expected one M2 per proposed slot, got {m2_slots:?}"
    );
    let proposals = post_setup
        .validator_service_client
        .get_sidechain_proposals(GetSidechainProposalsRequest::default())
        .await?
        .into_owned()
        .sidechain_proposals;
    anyhow::ensure!(
        proposals.len() == 2
            && proposals
                .iter()
                .all(|proposal| proto::unwrap_u32(proposal.vote_count.clone()) == Some(1)),
        "expected both proposals to have gathered the parallel M2s' votes: {proposals:?}"
    );

    // Two proposals for one slot tie on the sidechain number, so the lower
    // proposal hash goes first. Queued higher hash first, for the same reason
    // as above.
    let mut tied = ["tie a", "tie b"]
        .into_iter()
        .map(|title| {
            let declaration = declaration(title.to_owned());
            Ok((proposal_id(TIED_SLOT, &declaration)?, declaration))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    tied.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
    let mut expected = Vec::new();
    for (_, declaration) in tied {
        expected.push(submit_proposal(&mut post_setup, TIED_SLOT, declaration).await?);
    }
    expected.reverse();
    expect_m1_emission_order(&mut post_setup, &expected).await
}
