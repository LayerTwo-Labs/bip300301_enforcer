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
//! Also demonstrates the pre-activation header-only sync: most of the
//! pre-activation blocks are mined while the enforcer is down, so its initial
//! sync on restart finds them missing and must connect them from the headers
//! alone, without fetching any block bodies (asserted via the validator's
//! sync log). The rest of the test then runs against that header-synced
//! prefix, proving it supports the full activation lifecycle on top.
//!
//! Runs with the hidden `test-activation` preset; all preset parameters are
//! learned over RPC via `GetChainInfo`, keeping the test black-box.

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
use futures::channel::mpsc;

use crate::{
    integration_test::wait_for_validator_tip,
    mine::mine,
    setup::{
        DummySidechain, Mode, Network, PostSetup, PreSetup, SetupOpts, Sidechain as _,
        wait_for_block_templates, wait_for_enforcer_log, wait_for_pending_proposal,
    },
    util::BinPaths,
};

pub const TEST_NAME: &str = "activation_height";

/// Args for every enforcer this test spawns, restarts included: the preset
/// carries the activation height the whole test is built around.
fn enforcer_args() -> Vec<String> {
    vec!["--network-preset=test-activation".to_owned()]
}

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

pub async fn test_activation_height(bin_paths: BinPaths) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths.clone(), Network::Regtest)?;
    let setup_opts: SetupOpts = SetupOpts {
        enforcer_args: enforcer_args(),
        ..Default::default()
    };
    let mut post_setup = pre_setup
        .setup(Mode::GetBlockTemplate, setup_opts, res_tx.clone())
        .await?;

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

    // Mine most of the pre-activation blocks while the enforcer is down. On
    // restart, its initial sync must connect the gap from the stored headers
    // alone: pre-activation blocks are plain Bitcoin history, so no block
    // bodies are fetched, and the validator logs the header-only connect.
    let height = block_count(&post_setup).await?;
    // Stop at `activation_height - 2`, so the pre-activation RPC-rejection
    // checks below still have a block of room.
    let dark_blocks = (activation_height - 2).saturating_sub(height);
    anyhow::ensure!(
        dark_blocks >= 2,
        "setup mined too many blocks ({height}) to demonstrate the pre-activation \
         header sync with activation at {activation_height}"
    );
    tracing::info!("Killing enforcer, then mining {dark_blocks} block(s) behind its back");
    post_setup.kill_enforcer().await?;
    let mining_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_owned();
    let _output = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            [dark_blocks.to_string(), mining_address],
        )
        .run_utf8()
        .await?;
    tracing::info!("Restarting enforcer -- the gap must sync from headers, without block fetches");
    post_setup
        .restart_enforcer(&bin_paths, enforcer_args(), res_tx.clone())
        .await?;
    let _log = wait_for_enforcer_log(
        &post_setup.directories.enforcer_dir,
        "the pre-activation gap to be connected from stored headers",
        |log| {
            log.contains(&format!(
                "Connected {dark_blocks} pre-activation block(s) from stored headers"
            ))
        },
    )
    .await?;
    let () = wait_for_validator_tip(&post_setup).await?;
    // The test mines over `getblocktemplate` below, which stays unavailable
    // until the restarted enforcer's mempool sync also finishes.
    let () = wait_for_block_templates(&post_setup.gbt_client).await?;

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
