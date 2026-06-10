use std::borrow::Cow;

use bip300301_enforcer_lib::{
    bins::CommandExt as _, messages::CoinbaseMessage,
    proto::mainchain::BroadcastWithdrawalBundleRequest, types::BlindedM6,
};
use bitcoin::{
    Amount, Block, ScriptBuf, Transaction, TxOut, blockdata::locktime::absolute::LockTime,
    consensus::Encodable, hashes::Hash as _, opcodes::all::OP_RETURN, script::Builder,
    transaction::Version,
};
use either::Either;
use futures::channel::mpsc;

use crate::{
    integration_test::{activate_sidechain, propose_sidechain},
    mine::mine_generateblocks_check,
    setup::{DummySidechain, PostSetup, Sidechain as _},
};

fn make_blinded_m6(fee_sats: u64, payout: Amount) -> Transaction {
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

fn serialize_zero_input_legacy(tx: &Transaction) -> Vec<u8> {
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
    let mut _sidechain = DummySidechain::setup((), &post_setup, sidechain_res_tx).await?;
    propose_sidechain::<DummySidechain>(&mut post_setup).await?;
    activate_sidechain::<DummySidechain>(&mut post_setup).await?;

    let blinded_tx = make_blinded_m6(1_000, Amount::from_sat(50_000));

    // Sanity: this really is a valid blinded M6, so a fixed enforcer must accept it.
    let _checked: BlindedM6 = Cow::<Transaction>::Owned(blinded_tx.clone())
        .try_into()
        .map_err(|err| anyhow::anyhow!("test constructed an invalid blinded M6: {err}"))?;

    // Serialize the way Bitcoin Core / a sidechain does (legacy, NOT segwit).
    let transaction_bytes = serialize_zero_input_legacy(&blinded_tx);

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

    // Verify the *storage* round-trip: mining a block through the wallet's
    // own `generate_blocks` makes the enforcer read the bundle back out of its DB
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
    Ok(())
}
