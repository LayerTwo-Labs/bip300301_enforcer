//! Verifies that a `--network-preset` BIP300/301 activation height cleanly
//! gates enforcement: sidechain-proposal and sidechain-ACK RPCs are rejected
//! while the next block is still below the activation height, and proposals
//! become available one block before it — the earliest block whose coinbase
//! M1 the validator will process. From there the machinery works normally
//! (the M1 in the activation-height block registers, acks accumulate, the
//! sidechain activates). The validator-side half of the gate — M1s mined into
//! coinbases below the activation height are ignored — is covered by the
//! `connect_block_ignores_bip300_messages_below_activation_height` unit
//! test in `lib/validator/task`.
//!
//! Runs with the hidden `test-activation` preset (see the trial registration
//! in `integration_test::tests`); all preset parameters are learned over
//! RPC via `GetChainInfo`, keeping the test black-box.

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        common::{ConsensusHex, Hex, ReverseHex},
        mainchain::{
            CreateSidechainProposalRequest, GetChainInfoRequest, GetSidechainProposalsRequest,
            GetSidechainsRequest, SetSidechainAckRequest, SubmitSidechainProposalRequest,
        },
    },
};

use crate::{
    mine::mine,
    setup::{DummySidechain, PostSetup, Sidechain as _, wait_for_pending_proposal},
};

async fn block_count(post_setup: &PostSetup) -> anyhow::Result<u32> {
    let count: u32 = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .parse()?;
    Ok(count)
}

async fn proposal_count(post_setup: &mut PostSetup) -> anyhow::Result<usize> {
    let resp = post_setup
        .validator_service_client
        .get_sidechain_proposals(GetSidechainProposalsRequest::default())
        .await?
        .into_owned();
    Ok(resp.sidechain_proposals.len())
}

async fn active_sidechain_count(post_setup: &mut PostSetup) -> anyhow::Result<usize> {
    let resp = post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest::default())
        .await?
        .into_owned();
    Ok(resp.sidechains.len())
}

pub async fn test_activation_height(mut post_setup: PostSetup) -> anyhow::Result<()> {
    // Black box: learn the preset's parameters over RPC, like any client.
    // This also verifies GetChainInfo reports the preset rather than the
    // network defaults.
    let constants = post_setup
        .validator_service_client
        .get_chain_info(GetChainInfoRequest::default())
        .await?
        .into_owned()
        .bip300_constants
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("GetChainInfo returned no bip300_constants"))?;
    let activation_height = constants.activation_height;
    anyhow::ensure!(
        activation_height > 0,
        "test-activation preset must report a nonzero activation height"
    );

    let declaration = {
        let v0 = proto::mainchain::sidechain_declaration::V0 {
            title: proto::wrap_string("sidechain"),
            description: proto::wrap_string("sidechain"),
            hash_id_1: buffa::MessageField::some(ConsensusHex::encode(&[0; 32])),
            hash_id_2: buffa::MessageField::some(Hex::encode(&[0u8; 20])),
        };
        proto::mainchain::SidechainDeclaration {
            sidechain_declaration: Some(v0.into()),
        }
    };

    // While the next block is still below the activation height, proposal
    // RPCs must be rejected: an M1 mined before activation is ignored by the
    // validator, so the proposal could never confirm.
    let height = block_count(&post_setup).await?;
    anyhow::ensure!(
        height < activation_height - 1,
        "setup mined too many blocks ({height}) for activation at {activation_height}"
    );
    tracing::info!("Proposing sidechain (below the activation height) must fail");
    let submit_sidechain_proposal_request = SubmitSidechainProposalRequest {
        sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
        declaration: buffa::MessageField::some(declaration.clone()),
    };
    let Err(err) = post_setup
        .block_producer_service_client
        .submit_sidechain_proposal(submit_sidechain_proposal_request)
        .await
    else {
        anyhow::bail!("a sidechain proposal below the activation height must be rejected");
    };
    let err_msg = format!("{err:#}");
    anyhow::ensure!(
        err_msg.contains("BIP300 activates at block height"),
        "unexpected error for a pre-activation sidechain proposal: {err_msg}"
    );

    // An ACK below the activation height must be rejected the same way: no
    // proposal can be pending in the validator down here, so a stored ACK
    // could only ever be deleted as stale.
    tracing::info!("Setting a sidechain ACK (below the activation height) must fail");
    let set_sidechain_ack_request = SetSidechainAckRequest {
        sidechain_number: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
        description_sha256d_hash: buffa::MessageField::some(ReverseHex::encode(&[0u8; 32])),
        ack: true,
    };
    let Err(err) = post_setup
        .block_producer_service_client
        .set_sidechain_ack(set_sidechain_ack_request)
        .await
    else {
        anyhow::bail!("a sidechain ACK below the activation height must be rejected");
    };
    let err_msg = format!("{err:#}");
    anyhow::ensure!(
        err_msg.contains("BIP300 activates at block height"),
        "unexpected error for a pre-activation sidechain ACK: {err_msg}"
    );

    // Mine up to one block before the activation height. The block producer
    // skips drivechain coinbase messages entirely below the activation
    // height, so despite mining with ack-all set these are plain Bitcoin
    // coinbases.
    let below = activation_height - 1 - height;
    tracing::info!("Mining {below} block(s), staying below activation height");
    let () = mine::<DummySidechain>(&mut post_setup, below, Some(true)).await?;
    anyhow::ensure!(block_count(&post_setup).await? == activation_height - 1);
    anyhow::ensure!(
        proposal_count(&mut post_setup).await? == 0,
        "no proposal may register below the activation height"
    );

    // One block before the activation height the proposal RPC must succeed:
    // its M1 lands in the next block, which is AT the activation height.
    tracing::info!("Proposing sidechain (one block before the activation height)");
    let create_sidechain_proposal_request = CreateSidechainProposalRequest {
        sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
        declaration: buffa::MessageField::some(declaration),
    };
    let _resp_stream = post_setup
        .block_producer_service_client
        .create_sidechain_proposal(create_sidechain_proposal_request)
        .await?;
    // The proposal must be persisted before we mine, or the activation-height
    // coinbase won't carry the M1 this test needs it to.
    let () = wait_for_pending_proposal(
        &post_setup.block_producer_service_client,
        DummySidechain::SIDECHAIN_NUMBER,
    )
    .await?;

    // The next block is AT the activation height: its M1 must register.
    tracing::info!("Mining the activation-height block");
    let () = mine::<DummySidechain>(&mut post_setup, 1, Some(true)).await?;
    anyhow::ensure!(block_count(&post_setup).await? == activation_height);
    anyhow::ensure!(
        proposal_count(&mut post_setup).await? == 1,
        "the M1 at the activation height must register as a proposal"
    );

    // And the rest of the machinery runs normally from here: activation
    // needs strictly more acks than the preset's threshold.
    let acks = constants.unused_sidechain_slot_activation_threshold + 1;
    tracing::info!("Mining {acks} acked blocks to activate the sidechain");
    let () = mine::<DummySidechain>(&mut post_setup, acks, Some(true)).await?;
    anyhow::ensure!(
        active_sidechain_count(&mut post_setup).await? == 1,
        "sidechain must activate normally after the activation height"
    );
    tracing::info!("Activation-height gating works");
    Ok(())
}
