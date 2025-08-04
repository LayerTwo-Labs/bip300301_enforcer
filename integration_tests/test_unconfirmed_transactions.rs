use std::collections::HashMap;

use bip300301_enforcer_lib::proto::mainchain::{
    ListTransactionsRequest, ListUnspentOutputsRequest, SendTransactionRequest,
};
use futures::channel::mpsc;

use crate::{
    integration_test,
    setup::{DummySidechain, Mode, Network, setup},
    util::BinPaths,
};

// Verify that unconfirmed transactions are immediately available when listing wallet
// transactions.
pub async fn test_unconfirmed_transactions(
    bin_paths: BinPaths,
    network: Network,
    mode: Mode,
) -> anyhow::Result<()> {
    let (res_tx, _) = mpsc::unbounded();
    let mut post_setup = setup(&bin_paths, network, mode, res_tx).await?;

    integration_test::fund_enforcer::<DummySidechain>(&mut post_setup).await?;

    let txs_pre = post_setup
        .wallet_service_client
        .list_transactions(ListTransactionsRequest {})
        .await?
        .into_inner();

    let utxos_pre = post_setup
        .wallet_service_client
        .list_unspent_outputs(ListUnspentOutputsRequest {})
        .await?
        .into_inner();

    let destination_address = "bcrt1quzsfstj6aspah0yqzumnek9s2nmeh2ctax8tgy";

    let send_tx_res = post_setup
        .wallet_service_client
        .send_transaction(SendTransactionRequest {
            destinations: HashMap::from([(destination_address.to_string(), 123_456_u64)]),
            ..Default::default()
        })
        .await?
        .into_inner();

    let txs_post = post_setup
        .wallet_service_client
        .list_transactions(ListTransactionsRequest {})
        .await?
        .into_inner();

    assert!(txs_post.transactions.len() == txs_pre.transactions.len() + 1);

    let new_txs = txs_post
        .transactions
        .iter()
        .filter(|maybe_new_tx| {
            !txs_pre
                .transactions
                .iter()
                .any(|tx| tx.txid == maybe_new_tx.txid)
        })
        .collect::<Vec<_>>();

    assert!(new_txs.len() == 1);
    assert!(new_txs[0].txid == send_tx_res.txid);

    tracing::info!("new transaction: {:?}", new_txs);

    let unspent_post = post_setup
        .wallet_service_client
        .list_unspent_outputs(ListUnspentOutputsRequest {})
        .await?
        .into_inner();

    let new_utxos = unspent_post
        .outputs
        .iter()
        .filter(|maybe_new_utxo| {
            !utxos_pre.outputs.iter().any(|utxo_pre| {
                utxo_pre.txid == maybe_new_utxo.txid && utxo_pre.vout == maybe_new_utxo.vout
            })
        })
        .collect::<Vec<_>>();

    // This is failing. Very strange! We're applying the unconfirmed transaction to the wallet,
    // as shown by it being included when listing above. Could this be a BDK bug?
    assert!(!new_utxos.is_empty());

    Ok(())
}
