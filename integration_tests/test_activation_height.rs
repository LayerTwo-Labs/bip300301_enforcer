//! Verifies that a `--network-preset` BIP300/301 activation height cleanly
//! gates enforcement: M1 sidechain proposals mined into coinbases *below*
//! the activation height are ignored by the validator, while from the
//! activation height onwards the exact same machinery works normally
//! (proposal registers, acks accumulate, sidechain activates).
//!
//! Runs with the hidden `test-activation` preset (see the trial registration
//! in `integration_test::tests`); all preset parameters are learned over
//! RPC via `GetChainInfo`, keeping the test black-box.

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        common::{ConsensusHex, Hex},
        mainchain::{
            CreateSidechainProposalRequest, GetChainInfoRequest, GetSidechainProposalsRequest,
            GetSidechainsRequest,
        },
    },
};
use tokio::time::sleep;

use crate::{
    mine::mine,
    setup::{DummySidechain, PostSetup, Sidechain as _},
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

    // The wallet will re-include the M1 in every coinbase it mines until the
    // validator registers the proposal, so a single request up front is
    // enough to have an M1 present in every block of this test.
    tracing::info!("Proposing sidechain (below the activation height)");
    let create_sidechain_proposal_request = {
        let v0 = proto::mainchain::sidechain_declaration::V0 {
            title: proto::wrap_string("sidechain"),
            description: proto::wrap_string("sidechain"),
            hash_id_1: buffa::MessageField::some(ConsensusHex::encode(&[0; 32])),
            hash_id_2: buffa::MessageField::some(Hex::encode(&[0u8; 20])),
        };
        let declaration = proto::mainchain::SidechainDeclaration {
            sidechain_declaration: Some(v0.into()),
        };
        CreateSidechainProposalRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            declaration: buffa::MessageField::some(declaration),
        }
    };
    let _resp_stream = post_setup
        .block_producer_service_client
        .create_sidechain_proposal(create_sidechain_proposal_request)
        .await?;
    sleep(std::time::Duration::from_secs(1)).await;

    // Mine up to (but not including) the activation height. Every one of
    // these coinbases carries the M1; the validator must ignore all of them.
    let height = block_count(&post_setup).await?;
    anyhow::ensure!(
        height < activation_height - 1,
        "setup mined too many blocks ({height}) for activation at {activation_height}"
    );
    let below = activation_height - 1 - height;
    tracing::info!("Mining {below} block(s), staying below activation height");
    let () = mine::<DummySidechain>(&mut post_setup, below, Some(true)).await?;
    anyhow::ensure!(block_count(&post_setup).await? == activation_height - 1);
    anyhow::ensure!(
        proposal_count(&mut post_setup).await? == 0,
        "M1s below the activation height must be ignored"
    );

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
