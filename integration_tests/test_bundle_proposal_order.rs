//! BIP300 M3: a coinbase carries at most one bundle proposal. With several
//! bundles stored, the block producer proposes one per block -- lowest slot
//! first, then lowest m6id within a slot -- and never re-proposes one that is
//! already pending.

use bip300301_enforcer_lib::{
    messages::{CoinbaseMessage, M3ProposeBundle},
    proto::{
        self,
        mainchain::{
            BroadcastWithdrawalBundleRequest, CreateDepositTransactionRequest,
            CreateDepositTransactionResponse, GetSidechainsRequest,
            GetWithdrawalBundleProposalsRequest, GetWithdrawalBundleProposalsResponse,
        },
    },
    types::SidechainNumber,
};
use bitcoin::{Amount, Txid, hashes::Hash as _};

use crate::{
    integration_test::{fund_enforcer, propose_sidechain_for_slot},
    mine::{MiningPolicy, mine},
    setup::{DummySidechain, PostSetup, Sidechain as _, wait_for_tx_in_mempool},
    test_blinded_m6_roundtrip::{make_blinded_m6, serialize_zero_input_legacy},
    test_sidechain_ack_policy::tip_coinbase_messages,
};

const LOW_SLOT: SidechainNumber = DummySidechain::SIDECHAIN_NUMBER;
const HIGH_SLOT: SidechainNumber = SidechainNumber(LOW_SLOT.0 + 1);

/// The enforcer only accepts a bundle for a sidechain with a treasury UTXO.
async fn fund_treasury(post_setup: &mut PostSetup, slot: SidechainNumber) -> anyhow::Result<()> {
    let deposit_txid: Txid = post_setup
        .wallet_service_client
        .create_deposit_transaction(CreateDepositTransactionRequest {
            sidechain_id: proto::wrap_u32(slot.0.into()),
            address: proto::wrap_string("sidechain address".to_owned()),
            value_sats: proto::wrap_u64(1_000_000),
            fee_sats: proto::wrap_u64(10_000),
        })
        .await?
        .into_owned()
        .txid
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("CreateDepositTransaction returned no txid"))?
        .decode::<CreateDepositTransactionResponse, _>("txid")?;
    let () = wait_for_tx_in_mempool(&post_setup.bitcoin_cli, &deposit_txid).await?;
    let () = mine::<DummySidechain>(post_setup, 1, MiningPolicy::SILENT).await?;
    Ok(())
}

async fn broadcast_bundle(
    post_setup: &mut PostSetup,
    slot: SidechainNumber,
    payout: Amount,
) -> anyhow::Result<Txid> {
    let bundle_tx = make_blinded_m6(1_000, payout);
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
    Ok(bundle_tx.compute_txid())
}

/// The M3s in the tip's coinbase, as `(slot, m6id)`.
async fn tip_m3s(post_setup: &mut PostSetup) -> anyhow::Result<Vec<(SidechainNumber, Txid)>> {
    let m3s = tip_coinbase_messages(post_setup)
        .await?
        .into_iter()
        .filter_map(|message| match message {
            CoinbaseMessage::M3ProposeBundle(M3ProposeBundle {
                sidechain_number,
                bundle_txid,
            }) => Some((sidechain_number, Txid::from_byte_array(bundle_txid))),
            _ => None,
        })
        .collect();
    Ok(m3s)
}

async fn pending_m6ids(
    post_setup: &mut PostSetup,
    slot: SidechainNumber,
) -> anyhow::Result<Vec<Txid>> {
    post_setup
        .validator_service_client
        .get_withdrawal_bundle_proposals(GetWithdrawalBundleProposalsRequest {
            sidechain_id: proto::wrap_u32(slot.0.into()),
        })
        .await?
        .into_owned()
        .proposals
        .into_iter()
        .map(|bundle| {
            bundle
                .m6id
                .into_option()
                .ok_or_else(|| anyhow::anyhow!("withdrawal bundle proposal missing m6id"))?
                .decode::<GetWithdrawalBundleProposalsResponse, _>("m6id")
                .map_err(Into::into)
        })
        .collect()
}

async fn mine_expecting_m3(
    post_setup: &mut PostSetup,
    expected: Option<(SidechainNumber, Txid)>,
) -> anyhow::Result<()> {
    let () = mine::<DummySidechain>(post_setup, 1, MiningPolicy::SILENT).await?;
    let m3s = tip_m3s(post_setup).await?;
    anyhow::ensure!(
        m3s == Vec::from_iter(expected),
        "expected tip M3s `{expected:?}`, got `{m3s:?}`"
    );
    Ok(())
}

pub async fn test_bundle_proposal_order(mut post_setup: PostSetup) -> anyhow::Result<()> {
    for slot in [LOW_SLOT, HIGH_SLOT] {
        let () = propose_sidechain_for_slot::<DummySidechain>(&mut post_setup, slot, "sidechain")
            .await?;
    }
    let () = mine::<DummySidechain>(&mut post_setup, 6, MiningPolicy::VOTE).await?;
    let active_sidechains = post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest::default())
        .await?
        .into_owned()
        .sidechains
        .len();
    anyhow::ensure!(
        active_sidechains == 2,
        "expected 2 active sidechains, got {active_sidechains}"
    );
    fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    for slot in [LOW_SLOT, HIGH_SLOT] {
        let () = fund_treasury(&mut post_setup, slot).await?;
    }

    // Stored before any is proposed, so all three compete for the same block.
    let high = broadcast_bundle(&mut post_setup, HIGH_SLOT, Amount::from_sat(10_000)).await?;
    let low_a = broadcast_bundle(&mut post_setup, LOW_SLOT, Amount::from_sat(20_000)).await?;
    let low_b = broadcast_bundle(&mut post_setup, LOW_SLOT, Amount::from_sat(30_000)).await?;
    let (low_first, low_second) = if low_a < low_b {
        (low_a, low_b)
    } else {
        (low_b, low_a)
    };

    tracing::info!("Mining one M3 per block: low slot by m6id, then high slot");
    let () = mine_expecting_m3(&mut post_setup, Some((LOW_SLOT, low_first))).await?;
    let () = mine_expecting_m3(&mut post_setup, Some((LOW_SLOT, low_second))).await?;
    let () = mine_expecting_m3(&mut post_setup, Some((HIGH_SLOT, high))).await?;
    let () = mine_expecting_m3(&mut post_setup, None).await?;

    let mut low_pending = pending_m6ids(&mut post_setup, LOW_SLOT).await?;
    low_pending.sort();
    anyhow::ensure!(
        low_pending == [low_first, low_second],
        "expected both low-slot bundles pending, got `{low_pending:?}`"
    );
    let high_pending = pending_m6ids(&mut post_setup, HIGH_SLOT).await?;
    anyhow::ensure!(
        high_pending == [high],
        "expected the high-slot bundle pending, got `{high_pending:?}`"
    );

    drop(post_setup);
    Ok(())
}
