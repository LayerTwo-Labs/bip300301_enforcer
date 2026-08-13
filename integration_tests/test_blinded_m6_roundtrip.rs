use std::borrow::Cow;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    messages::CoinbaseMessage,
    proto::{
        common::ConsensusHex,
        mainchain::{BroadcastWithdrawalBundleRequest, GetCtipRequest},
    },
    types::BlindedM6,
};
use bitcoin::{
    Amount, Block, BlockHash, ScriptBuf, Transaction, TxOut,
    blockdata::locktime::absolute::LockTime, consensus::Encodable, hashes::Hash as _,
    opcodes::all::OP_RETURN, script::Builder, transaction::Version,
};
use either::Either;
use futures::channel::mpsc;

use crate::{
    block_verdict::wait_for_enforcer_tip_hash,
    integration_test::{
        activate_sidechain, deposit, fund_enforcer, mine_until_unpayable_bundle_expires,
        propose_sidechain,
    },
    mine::mine_generateblocks_check,
    setup::{DummySidechain, PostSetup, Sidechain as _},
};

pub(crate) fn make_blinded_m6(fee_sats: u64, payout: Amount) -> Transaction {
    let fee_txout = TxOut {
        value: Amount::ZERO,
        script_pubkey: Builder::new()
            .push_opcode(OP_RETURN)
            .push_slice(fee_sats.to_be_bytes())
            .into_script(),
    };
    // A plausible P2WPKH payout destination. BlindedM6 does not constrain the
    // payout script, only that there is non-zero payout value.
    let payout_spk = {
        let mut spk = vec![0x00, 0x14];
        spk.extend_from_slice(&[0x11u8; 20]);
        ScriptBuf::from_bytes(spk)
    };
    let payout_txout = TxOut {
        value: payout,
        script_pubkey: payout_spk,
    };
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: Vec::new(), // <-- the crux: a blinded M6 has NO inputs
        output: vec![fee_txout, payout_txout],
    }
}

pub(crate) fn serialize_zero_input_legacy(tx: &Transaction) -> Vec<u8> {
    assert!(
        tx.input.is_empty(),
        "legacy zero-input serializer requires no inputs"
    );
    assert!(
        tx.output.len() < 0xFD,
        "test helper only handles <253 outputs"
    );
    let mut buf = Vec::new();
    tx.version.consensus_encode(&mut buf).unwrap();
    buf.push(0x00); // CompactSize input count = 0
    buf.push(tx.output.len() as u8); // CompactSize output count (<253 -> 1 byte)
    for out in &tx.output {
        out.consensus_encode(&mut buf).unwrap();
    }
    tx.lock_time.consensus_encode(&mut buf).unwrap();
    buf
}

async fn chain_height(post_setup: &PostSetup) -> anyhow::Result<u32> {
    Ok(post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?)
}

async fn block_hash_at(post_setup: &PostSetup, height: u32) -> anyhow::Result<BlockHash> {
    Ok(post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblockhash", [height.to_string()])
        .run_utf8()
        .await?
        .trim()
        .parse()?)
}

/// The sidechain's current treasury UTXO, if it has one.
async fn ctip(post_setup: &mut PostSetup) -> anyhow::Result<Option<u64>> {
    let resp = post_setup
        .validator_service_client
        .get_ctip(GetCtipRequest {
            sidechain_number: bip300301_enforcer_lib::proto::wrap_u32(
                DummySidechain::SIDECHAIN_NUMBER.0.into(),
            ),
        })
        .await?
        .into_owned();
    Ok(resp.ctip.into_option().map(|ctip| ctip.value))
}

async fn mine_block_and_collect_proposed_m6ids(
    post_setup: &mut PostSetup,
) -> anyhow::Result<Vec<[u8; 32]>> {
    let mut block_hashes = Vec::new();
    mine_generateblocks_check(post_setup, 1, None, |block_hash| {
        block_hashes.push(block_hash);
        Ok::<(), std::convert::Infallible>(())
    })
    .await
    .map_err(|err| match err {
        Either::Left(status) => anyhow::anyhow!("generate_blocks RPC failed: {status}"),
        Either::Right(never) => match never {},
    })?;
    let block_hash = block_hashes
        .first()
        .ok_or_else(|| anyhow::anyhow!("generate_blocks produced no block"))?;
    let block_hex = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [block_hash.to_string(), "0".to_string()])
        .run_utf8()
        .await?;
    let block: Block = bitcoin::consensus::deserialize(&hex::decode(block_hex.trim())?)?;
    let coinbase = block
        .txdata
        .first()
        .ok_or_else(|| anyhow::anyhow!("mined block has no coinbase"))?;
    let proposed = coinbase
        .output
        .iter()
        .filter_map(|txout| match CoinbaseMessage::parse(&txout.script_pubkey) {
            Ok((_, CoinbaseMessage::M3ProposeBundle(m3))) => Some(m3.bundle_txid),
            _ => None,
        })
        .collect();
    Ok(proposed)
}

pub async fn test_blinded_m6_zero_input_roundtrip(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (sidechain_res_tx, _sidechain_res_rx) = mpsc::unbounded();
    let mut sidechain = DummySidechain::setup((), &post_setup, sidechain_res_tx).await?;
    propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    activate_sidechain::<DummySidechain>(&mut post_setup).await?;

    // The height the sidechain is active but unfunded at. The final section of
    // this test rolls the chain back here to strand the stored bundle without a
    // treasury; everything the funding and deposit below add is undone.
    let active_unfunded_height = chain_height(&post_setup).await?;

    let blinded_tx = make_blinded_m6(1_000, Amount::from_sat(50_000));

    // Sanity: this really is a valid blinded M6, so a fixed enforcer must accept it.
    let _checked: BlindedM6 = Cow::<Transaction>::Owned(blinded_tx.clone())
        .try_into()
        .map_err(|err| anyhow::anyhow!("test constructed an invalid blinded M6: {err}"))?;

    // Serialize the way Bitcoin Core / a sidechain does (legacy, NOT segwit).
    let transaction_bytes = serialize_zero_input_legacy(&blinded_tx);

    // An active slot does not necessarily have a treasury yet: nothing has been
    // deposited at this point, so slot 0 has no CTIP and an M6 built from this
    // bundle would have no treasury output to spend. Reject at ingestion rather
    // than store a proposal that can never be acted on. These are the exact
    // bytes the enforcer accepts once the treasury exists below, so the
    // rejection can only come from the missing CTIP.
    let no_ctip_result = post_setup
        .wallet_service_client
        .broadcast_withdrawal_bundle(BroadcastWithdrawalBundleRequest {
            sidechain_id: bip300301_enforcer_lib::proto::wrap_u32(0),
            transaction: buffa::MessageField::some(buffa_types::google::protobuf::BytesValue {
                value: transaction_bytes.clone(),
                ..Default::default()
            }),
        })
        .await;
    let status = no_ctip_result.err().ok_or_else(|| {
        anyhow::anyhow!(
            "BroadcastWithdrawalBundle accepted a bundle for an active sidechain without a CTIP"
        )
    })?;
    anyhow::ensure!(
        status.code == connectrpc::ErrorCode::FailedPrecondition,
        "expected FailedPrecondition for a sidechain without a CTIP, got: {status}"
    );
    anyhow::ensure!(
        status.to_string().contains("no treasury UTXO"),
        "expected the rejection to identify the missing treasury UTXO, got: {status}"
    );

    // A withdrawal bundle is meaningful only after the active sidechain has a
    // treasury UTXO. Establish one before exercising the positive storage and
    // M3 round-trip below.
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

    // Verify the fixture against Bitcoin Core itself: Core must parse these bytes
    // in legacy form (`iswitness=false`) and must agree with rust-bitcoin on the txid,
    // which per BIP300 `m6_to_id` IS the M6ID.
    //
    // NB: parseability is all Core can attest here — a
    // zero-input tx always fails mempool checks (`bad-txns-vin-empty`), since a
    // blinded M6 is an ID-computation template, not a relayable transaction.
    let decoded = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "decoderawtransaction",
            [hex::encode(&transaction_bytes), "false".to_string()],
        )
        .run_utf8()
        .await?;
    let decoded: serde_json::Value = serde_json::from_str(&decoded)?;
    anyhow::ensure!(decoded["txid"] == blinded_tx.compute_txid().to_string().as_str(),);

    post_setup
        .wallet_service_client
        .broadcast_withdrawal_bundle(BroadcastWithdrawalBundleRequest {
            sidechain_id: bip300301_enforcer_lib::proto::wrap_u32(0),
            transaction: buffa::MessageField::some(buffa_types::google::protobuf::BytesValue {
                value: transaction_bytes,
                ..Default::default()
            }),
        })
        .await?;

    // Verify the *storage* round-trip: mining a block through the enforcer's
    // own `GenerateToAddress` makes the enforcer read the bundle back out of its DB
    // (`get_bundle_proposals` -> `BlindedM6::deserialize`, the second site
    // that parses a stored bundle) and propose it as an M3 in the
    // coinbase. If read-back deserialization failed, block building would error.
    // If it returned a different tx, the proposed M6ID would not match. So we
    // mine one block and assert its coinbase proposes exactly our M6ID.
    let m6id = blinded_tx.compute_txid();
    let proposed = mine_block_and_collect_proposed_m6ids(&mut post_setup).await?;
    anyhow::ensure!(
        proposed.contains(&m6id.to_byte_array()),
        "stored blinded M6 did not round-trip: mining read it back via \
         get_bundle_proposals/BlindedM6::deserialize but the coinbase did not propose our \
         M6ID ({m6id}) as an M3"
    );

    // A bundle for a sidechain slot that is NOT active must be
    // rejected at submission, not stored. Only slot 0 was activated above, so a
    // submission for slot 1 must fail with FailedPrecondition.
    //
    const INACTIVE_SIDECHAIN_ID: u32 = 1;
    let inactive_tx = make_blinded_m6(2_000, Amount::from_sat(60_000));
    let inactive_result = post_setup
        .wallet_service_client
        .broadcast_withdrawal_bundle(BroadcastWithdrawalBundleRequest {
            sidechain_id: bip300301_enforcer_lib::proto::wrap_u32(INACTIVE_SIDECHAIN_ID),
            transaction: buffa::MessageField::some(buffa_types::google::protobuf::BytesValue {
                value: serialize_zero_input_legacy(&inactive_tx),
                ..Default::default()
            }),
        })
        .await;
    let status = inactive_result.err().ok_or_else(|| {
        anyhow::anyhow!(
            "BroadcastWithdrawalBundle accepted a bundle for inactive sidechain slot \
             {INACTIVE_SIDECHAIN_ID}; it must be rejected at ingestion"
        )
    })?;
    anyhow::ensure!(
        status.code == connectrpc::ErrorCode::FailedPrecondition,
        "expected FailedPrecondition rejecting an inactive-slot bundle, got: {status}"
    );

    // A stored bundle whose sidechain loses its treasury must not stop the
    // block producer.
    //
    // Rolling the chain back to the active-but-unfunded height strands exactly
    // that state: the enforcer's funding coinbases go with it, and a
    // disconnected coinbase can never be re-mined, so the deposit that created
    // the treasury is evicted rather than re-confirmed. The sidechain stays
    // active (it activated before the funding) and the bundle stays in the block
    // producer's own DB, which is not chain state. No ingestion-time check can
    // cover this: the treasury existed when the bundle was accepted.
    let rollback_to = block_hash_at(&post_setup, active_unfunded_height).await?;
    let first_invalid = block_hash_at(&post_setup, active_unfunded_height + 1).await?;
    tracing::info!(
        %first_invalid,
        %rollback_to,
        "invalidating the funding and deposit under the stored bundle"
    );
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "invalidateblock", [first_invalid.to_string()])
        .run_utf8()
        .await?;
    wait_for_enforcer_tip_hash(&post_setup, rollback_to).await?;
    anyhow::ensure!(
        ctip(&mut post_setup).await?.is_none(),
        "sidechain still has a treasury UTXO after its deposit was reorged out"
    );

    // The bundle crosses the inclusion threshold with no treasury to pay it
    // from. Before the fix the first template built after the crossing failed
    // with a missing-CTIP error, and since only a connected block can retire
    // the bundle, nothing could ever clear it.
    let m6id_hex = ConsensusHex::encode(&m6id);
    mine_until_unpayable_bundle_expires::<DummySidechain>(&mut post_setup, &m6id_hex).await?;
    Ok(())
}
