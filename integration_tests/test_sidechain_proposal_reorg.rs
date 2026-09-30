//! The block producer's queue of our sidechain proposals follows the chain: a
//! proposal leaves the queue once its M1 is mined, and returns if a reorg
//! takes that block out, however deep, but not if the reorg stays above it or
//! swaps the block for one carrying the same M1. An explicit ACK outlives its
//! proposal's M1 being reorged out. A rebuild re-marks the queue from the
//! chain, including a block lost while the enforcer was down.
//! Resubmitting sees a mined proposal as mined even before anything has read
//! the queue, and queues it again for good: neither a reorg that takes the M1
//! out and back in, a rebuild nor an upgrade from a DB without a cursor undoes
//! it, and it goes out once the first proposal has failed. Its new M1 then
//! counts like any other. Catching up across a reorg after downtime is covered
//! by `wallet_reorg_multi_block`.

use bip300301_enforcer_lib::{
    messages::CoinbaseMessage,
    proto::{
        self,
        common::{ConsensusHex, Hex, ReverseHex},
        mainchain::{
            AckAllProposalsPolicy, SetAckAllProposalsRequest, SetSidechainAckRequest,
            SidechainDeclaration, SubmitSidechainProposalRequest, sidechain_declaration,
        },
    },
    types::{SidechainNumber, Thresholds},
};
use bitcoin::BlockHash;
use futures::channel::mpsc;
use jsonrpsee::{core::client::ClientT as _, rpc_params};

use crate::{
    block_verdict::wait_for_enforcer_tip_hash,
    mine::{MiningPolicy, mine},
    setup::{
        DummySidechain, PostSetup, Sidechain as _, best_block_hash, downgrade_policy_db,
        generate_empty_block, invalidate_block, lose_policy_tip, reconsider_block, reorg_out,
        restart_enforcer_with_defaults, wait_for_no_pending_proposal, wait_for_pending_proposal,
    },
    test_sidechain_ack_policy::tip_coinbase_messages,
};

fn submit_request(slot: SidechainNumber) -> SubmitSidechainProposalRequest {
    let declaration = SidechainDeclaration {
        sidechain_declaration: Some(
            sidechain_declaration::V0 {
                title: proto::wrap_string(format!("slot {slot}")),
                description: proto::wrap_string("sidechain-proposal-reorg test"),
                hash_id_1: buffa::MessageField::some(ConsensusHex::encode(&[0; 32])),
                hash_id_2: buffa::MessageField::some(Hex::encode(&[0u8; 20])),
            }
            .into(),
        ),
    };
    SubmitSidechainProposalRequest {
        sidechain_id: proto::wrap_u32(slot.0.into()),
        declaration: buffa::MessageField::some(declaration),
    }
}

async fn submit_proposal(post_setup: &PostSetup, slot: SidechainNumber) -> anyhow::Result<()> {
    let _resp = post_setup
        .block_producer_service_client
        .submit_sidechain_proposal(submit_request(slot))
        .await?;
    wait_for_pending_proposal(&post_setup.block_producer_service_client, slot).await
}

async fn wait_for_pending(post_setup: &PostSetup, slot: SidechainNumber) -> anyhow::Result<()> {
    wait_for_pending_proposal(&post_setup.block_producer_service_client, slot).await
}

/// Also the read that records the M1 in the producer's policy DB.
async fn wait_for_mined(post_setup: &PostSetup, slot: SidechainNumber) -> anyhow::Result<()> {
    wait_for_no_pending_proposal(&post_setup.block_producer_service_client, slot).await
}

/// Mine one block with the enforcer and return the slots of its M1s, sorted.
async fn mine_one(post_setup: &mut PostSetup) -> anyhow::Result<Vec<SidechainNumber>> {
    let () = mine::<DummySidechain>(post_setup, 1, MiningPolicy::SILENT).await?;
    let mut slots: Vec<_> = tip_coinbase_messages(post_setup)
        .await?
        .into_iter()
        .filter_map(|message| match message {
            CoinbaseMessage::M1ProposeSidechain(m1) => Some(m1.sidechain_number),
            _ => None,
        })
        .collect();
    slots.sort_unstable();
    Ok(slots)
}

async fn mine_expecting_m1s(
    post_setup: &mut PostSetup,
    expected: &[SidechainNumber],
) -> anyhow::Result<BlockHash> {
    let slots = mine_one(post_setup).await?;
    anyhow::ensure!(
        slots == expected,
        "expected M1s for slots {expected:?}, got {slots:?}"
    );
    best_block_hash(&post_setup.bitcoind_client).await
}

/// Mine one block with the enforcer, expecting M2s for exactly `expected`.
async fn mine_expecting_m2s(
    post_setup: &mut PostSetup,
    expected: &[SidechainNumber],
) -> anyhow::Result<()> {
    let () = mine::<DummySidechain>(post_setup, 1, MiningPolicy::SILENT).await?;
    let mut slots: Vec<_> = tip_coinbase_messages(post_setup)
        .await?
        .into_iter()
        .filter_map(|message| match message {
            CoinbaseMessage::M2AckSidechain(m2) => Some(m2.sidechain_number),
            _ => None,
        })
        .collect();
    slots.sort_unstable();
    anyhow::ensure!(
        slots == expected,
        "expected M2s for slots {expected:?}, got {slots:?}"
    );
    Ok(())
}

async fn m1_description_hash(
    post_setup: &PostSetup,
    block: BlockHash,
) -> anyhow::Result<bitcoin::hashes::sha256d::Hash> {
    let script = m1_script(post_setup, block).await?;
    let Ok((_rest, CoinbaseMessage::M1ProposeSidechain(m1))) = CoinbaseMessage::parse(&script)
    else {
        anyhow::bail!("block {block} carries no M1");
    };
    Ok(m1.description.sha256d_hash())
}

async fn set_sidechain_ack(
    post_setup: &PostSetup,
    slot: SidechainNumber,
    description_hash: bitcoin::hashes::sha256d::Hash,
) -> anyhow::Result<()> {
    let _resp = post_setup
        .block_producer_service_client
        .set_sidechain_ack(SetSidechainAckRequest {
            sidechain_number: proto::wrap_u32(slot.0.into()),
            description_sha256d_hash: buffa::MessageField::some(ReverseHex::encode(
                &description_hash,
            )),
            ack: true,
        })
        .await?;
    Ok(())
}

/// The output script of the M1 in `block`'s coinbase.
async fn m1_script(post_setup: &PostSetup, block: BlockHash) -> anyhow::Result<bitcoin::ScriptBuf> {
    let block_hex: String = post_setup
        .bitcoind_client
        .request("getblock", rpc_params![block, 0])
        .await?;
    let raw_block: bitcoin::Block = bitcoin::consensus::deserialize(&hex::decode(block_hex)?)?;
    raw_block
        .txdata
        .first()
        .into_iter()
        .flat_map(|coinbase| &coinbase.output)
        .find(|txout| {
            matches!(
                CoinbaseMessage::parse(&txout.script_pubkey),
                Ok((_rest, CoinbaseMessage::M1ProposeSidechain(_)))
            )
        })
        .map(|txout| txout.script_pubkey.clone())
        .ok_or_else(|| anyhow::anyhow!("block {block} carries no M1"))
}

async fn parent_of(post_setup: &PostSetup, block: BlockHash) -> anyhow::Result<BlockHash> {
    let header: serde_json::Value = post_setup
        .bitcoind_client
        .request("getblockheader", rpc_params![block])
        .await?;
    Ok(header["previousblockhash"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("block {block} has no parent"))?
        .parse()?)
}

pub async fn test_sidechain_proposal_reorg(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (sidechain_res_tx, _sidechain_res_rx) = mpsc::unbounded();
    let _sidechain = DummySidechain::setup((), &post_setup, sidechain_res_tx).await?;
    let () = stop_acking(&post_setup).await?;

    tracing::info!("A mined proposal leaves the queue");
    let slot = SidechainNumber(1);
    let () = submit_proposal(&post_setup, slot).await?;
    let duplicate = post_setup
        .block_producer_service_client
        .submit_sidechain_proposal(submit_request(slot))
        .await;
    anyhow::ensure!(
        duplicate
            .as_ref()
            .is_err_and(|err| err.code == connectrpc::ErrorCode::AlreadyExists),
        "resubmitting a pending proposal must fail with AlreadyExists, got: {duplicate:?}"
    );
    let _m1_block = mine_expecting_m1s(&mut post_setup, &[slot]).await?;
    let () = wait_for_mined(&post_setup, slot).await?;
    let _next = mine_expecting_m1s(&mut post_setup, &[]).await?;

    tracing::info!("A multi-block reorg queues every proposal it takes out, and keeps its ACKs");
    let (deeper, shallower) = (SidechainNumber(3), SidechainNumber(4));
    let () = submit_proposal(&post_setup, deeper).await?;
    let deeper_block = mine_expecting_m1s(&mut post_setup, &[deeper]).await?;
    let () = wait_for_mined(&post_setup, deeper).await?;
    let () = set_sidechain_ack(
        &post_setup,
        deeper,
        m1_description_hash(&post_setup, deeper_block).await?,
    )
    .await?;
    let () = submit_proposal(&post_setup, shallower).await?;
    let _shallower_block = mine_expecting_m1s(&mut post_setup, &[shallower]).await?;
    let () = wait_for_mined(&post_setup, shallower).await?;
    let _above = mine_expecting_m1s(&mut post_setup, &[]).await?;
    let () = reorg_out(&post_setup, deeper_block, || async {
        let () = wait_for_pending(&post_setup, deeper).await?;
        wait_for_pending(&post_setup, shallower).await
    })
    .await?;
    // Built while the ACKed proposal is off the chain.
    let _m1_block = mine_expecting_m1s(&mut post_setup, &[deeper, shallower]).await?;
    let () = wait_for_mined(&post_setup, deeper).await?;
    let () = wait_for_mined(&post_setup, shallower).await?;
    let () = mine_expecting_m2s(&mut post_setup, &[deeper]).await?;

    tracing::info!("A reorg above the M1's block leaves the proposal mined");
    let above = mine_expecting_m1s(&mut post_setup, &[]).await?;
    let () = reorg_out(&post_setup, above, || async { Ok(()) }).await?;
    let () = wait_for_mined(&post_setup, deeper).await?;
    let _next = mine_expecting_m1s(&mut post_setup, &[]).await?;

    tracing::info!("Swapping the M1's block for a competitor carrying the same M1 keeps it mined");
    let swapped = SidechainNumber(8);
    let () = submit_proposal(&post_setup, swapped).await?;
    let m1_block = mine_expecting_m1s(&mut post_setup, &[swapped]).await?;
    let () = wait_for_mined(&post_setup, swapped).await?;
    let m1_script = m1_script(&post_setup, m1_block).await?;
    // Nothing reads the queue in between, so one catch-up both disconnects
    // the M1 and connects it again.
    let () = invalidate_block(&post_setup.bitcoind_client, m1_block).await?;
    let competitor = generate_empty_block(
        &post_setup.bitcoind_client,
        &format!("raw({})", m1_script.to_hex_string()),
    )
    .await?;
    let () = wait_for_enforcer_tip_hash(&post_setup, competitor).await?;
    let () = wait_for_mined(&post_setup, swapped).await?;

    tracing::info!("Resubmitting a proposal straight after its M1 is mined queues it again");
    let requeued = SidechainNumber(5);
    let () = submit_proposal(&post_setup, requeued).await?;
    // Nothing reads the queue between mining the M1 and resubmitting, so a
    // resubmission that missed the M1 would fail with AlreadyExists.
    let requeued_m1_block = mine_expecting_m1s(&mut post_setup, &[requeued]).await?;
    let () = submit_proposal(&post_setup, requeued).await?;
    // Queued, but not proposed again while its first proposal is alive.
    let next = mine_expecting_m1s(&mut post_setup, &[]).await?;

    tracing::info!("Reorging the requeued M1's block out and back in keeps it queued");
    let parent = parent_of(&post_setup, requeued_m1_block).await?;
    let () = invalidate_block(&post_setup.bitcoind_client, requeued_m1_block).await?;
    let () = wait_for_enforcer_tip_hash(&post_setup, parent).await?;
    let () = wait_for_pending(&post_setup, requeued).await?;
    let () = reconsider_block(&post_setup.bitcoind_client, requeued_m1_block).await?;
    let () = wait_for_enforcer_tip_hash(&post_setup, next).await?;
    let () = wait_for_pending(&post_setup, requeued).await?;

    // A cursor the validator no longer knows, as after a resync that loses its
    // branch, forces a rebuild.
    tracing::info!("A rebuild re-marks the queue from the chain, keeping the requeue");
    let (kept, lost) = (SidechainNumber(6), SidechainNumber(7));
    let () = submit_proposal(&post_setup, kept).await?;
    let _kept_block = mine_expecting_m1s(&mut post_setup, &[kept]).await?;
    let () = submit_proposal(&post_setup, lost).await?;
    let lost_block = mine_expecting_m1s(&mut post_setup, &[lost]).await?;
    let () = wait_for_mined(&post_setup, kept).await?;
    let () = wait_for_mined(&post_setup, lost).await?;
    let () = post_setup.kill_enforcer().await?;
    let () = invalidate_block(&post_setup.bitcoind_client, lost_block).await?;
    let tip = generate_empty_block(
        &post_setup.bitcoind_client,
        &post_setup.mining_address.to_string(),
    )
    .await?;
    let () = lose_policy_tip(&post_setup.directories)?;
    let () = restart_enforcer_with_defaults(&mut post_setup).await?;
    let () = wait_for_enforcer_tip_hash(&post_setup, tip).await?;
    let () = wait_for_pending(&post_setup, lost).await?;
    let () = wait_for_pending(&post_setup, requeued).await?;
    for mined in [slot, deeper, shallower, swapped, kept] {
        let () = wait_for_mined(&post_setup, mined).await?;
    }
    // The requeued proposal's first one is still alive here.
    let _m1_block = mine_expecting_m1s(&mut post_setup, &[lost]).await?;
    let () = wait_for_mined(&post_setup, lost).await?;

    tracing::info!("Upgrading from a DB without a cursor keeps the requeue");
    let () = post_setup.kill_enforcer().await?;
    let () = downgrade_policy_db(&post_setup.directories)?;
    let () = restart_enforcer_with_defaults(&mut post_setup).await?;
    let () = wait_for_pending(&post_setup, requeued).await?;

    tracing::info!("Once its first proposal fails, the requeued one goes out");
    let mut second_m1_block = None;
    for _ in 0..=Thresholds::SHORT.unused_sidechain_slot_proposal_max_age {
        let slots = mine_one(&mut post_setup).await?;
        if slots == [requeued] {
            second_m1_block = Some(best_block_hash(&post_setup.bitcoind_client).await?);
            break;
        }
        anyhow::ensure!(slots.is_empty(), "expected no M1s, got {slots:?}");
    }
    let second_m1_block = second_m1_block
        .ok_or_else(|| anyhow::anyhow!("the requeued proposal never went out again"))?;
    let () = wait_for_mined(&post_setup, requeued).await?;

    tracing::info!("Reorging out the requeued proposal's new M1 queues it again");
    let () = reorg_out(&post_setup, second_m1_block, || {
        wait_for_pending(&post_setup, requeued)
    })
    .await?;
    let _m1_block = mine_expecting_m1s(&mut post_setup, &[requeued]).await?;

    tracing::info!("A rebuild re-marks it by the new M1, not the one it was queued past");
    let () = post_setup.kill_enforcer().await?;
    let () = lose_policy_tip(&post_setup.directories)?;
    let () = restart_enforcer_with_defaults(&mut post_setup).await?;
    wait_for_mined(&post_setup, requeued).await
}

/// Unacked proposals fail, whether mined through the template server or not.
async fn stop_acking(post_setup: &PostSetup) -> anyhow::Result<()> {
    let _resp = post_setup
        .block_producer_service_client
        .set_ack_all_proposals(SetAckAllProposalsRequest {
            policy: AckAllProposalsPolicy::None.into(),
        })
        .await?;
    Ok(())
}
