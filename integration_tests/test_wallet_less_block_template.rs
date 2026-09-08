//! Block producer without a wallet, against a node without `txindex`: block
//! templates and withdrawal bundle ingestion.

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::mainchain::{GetBalanceRequest, ProposeWithdrawalBundleRequest},
};
use bitcoin::Amount;
use futures::channel::mpsc;

use crate::{
    integration_test::{activate_sidechain, propose_sidechain},
    setup::{DummySidechain, EnforcerWallet, Mode, PreSetup, SetupOpts},
    test_blinded_m6_roundtrip::{make_blinded_m6, serialize_zero_input_legacy},
};

pub async fn test_wallet_less_block_template(setup: PreSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let setup_opts: SetupOpts = SetupOpts {
        enforcer_wallet: EnforcerWallet::Disabled,
        ..Default::default()
    };

    let mut post_setup = setup
        .setup(Mode::GetBlockTemplate, setup_opts, res_tx)
        .await?;

    // Verify there's really no wallet here
    let balance = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await;
    let status = balance.err().ok_or_else(|| {
        anyhow::anyhow!("GetBalance succeeded, but the enforcer was started without a wallet")
    })?;
    anyhow::ensure!(
        status.code == connectrpc::ErrorCode::Unimplemented,
        "expected WalletService to be unserved without a wallet, got: {status}"
    );

    // Guard the premise. A wallet-enabled enforcer hard-fails unless the node
    // has `txindex`; the block producer must not need it. If something later
    // switches the node back to `-txindex`, this test would still pass while
    // silently no longer covering that, so assert the node really lacks it.
    let index_info = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getindexinfo", [])
        .run_utf8()
        .await?;
    anyhow::ensure!(
        !index_info.contains("txindex"),
        "this test must run against a node without `txindex`, but the node reports: {index_info}"
    );

    let () = propose_sidechain::<DummySidechain>(&mut post_setup).await?;

    // Activation needs the templates to carry an M2 ack for the proposal in
    // each of the next blocks. If the coinbase were wrong, the
    // sidechain would never activate.
    let () = activate_sidechain::<DummySidechain>(&mut post_setup).await?;

    // Withdrawal bundle ingestion must not need the wallet either. Nothing
    // is deposited yet, so slot 0 has no CTIP and the handler rejects the
    // bundle on its own terms. That rejection is the point: the
    // BlockProducerService serves the RPC and runs the handler with no
    // wallet. An unserved RPC answers `Unimplemented` instead. The full M3
    // round trip is covered through the same shared handler by the
    // wallet-backed bundle tests; a CTIP needs a deposit, and a deposit needs
    // a wallet.
    let blinded_tx = make_blinded_m6(1_000, Amount::from_sat(10_000));
    let transaction_bytes = serialize_zero_input_legacy(&blinded_tx);
    let result = post_setup
        .block_producer_service_client
        .propose_withdrawal_bundle(ProposeWithdrawalBundleRequest {
            sidechain_id: bip300301_enforcer_lib::proto::wrap_u32(0),
            transaction: buffa::MessageField::some(buffa_types::google::protobuf::BytesValue {
                value: transaction_bytes,
                ..Default::default()
            }),
        })
        .await;
    let status = result.err().ok_or_else(|| {
        anyhow::anyhow!("ProposeWithdrawalBundle accepted a bundle for a sidechain without a CTIP")
    })?;
    anyhow::ensure!(
        status.code == connectrpc::ErrorCode::FailedPrecondition,
        "expected FailedPrecondition for a sidechain without a CTIP, got: {status}"
    );
    anyhow::ensure!(
        status.to_string().contains("no treasury UTXO"),
        "expected the rejection to identify the missing treasury UTXO, got: {status}"
    );

    drop(post_setup);
    Ok(())
}
