//! BIP 360 invalid-block harness trial (submitblock → invalidateblock).

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::validator::pqc::signer_dev::p2mr_script_pubkey;
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness, transaction::Version,
};
use futures::channel::mpsc;

use crate::{
    bip360_block::{
        P2MR_PREVOUT_VALUE, P2MR_SPEND_OUTPUT_VALUE, bip360_setup_opts, build_block_with_coinbase,
        funding_prevout, prepare_coinbase, submit_block, wait_for_enforcer_synced,
        wallet_sign_transaction,
    },
    block_verdict::{Expect, assert_enforcer_verdict},
    setup::{Mode, PreSetup},
};

const REJECT_LOG_EMPTY_WITNESS: &str = "P2MR witness stack is empty";

/// Submits a block whose second tx spends a same-block P2MR output with an empty
/// witness stack. The enforcer must reject via `invalidateblock`.
pub async fn test_bip360_invalid_block(pre_setup: PreSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut post_setup = pre_setup
        .setup(Mode::NoMempool, bip360_setup_opts(), res_tx)
        .await?;

    wait_for_enforcer_synced(&mut post_setup).await?;

    let bad_block_hash = submit_p2mr_empty_witness_block(&mut post_setup).await?;
    tracing::info!(%bad_block_hash, "submitted BIP 360 invalid block");

    assert_enforcer_verdict(
        &mut post_setup,
        bad_block_hash,
        Expect::Rejected {
            log_contains: REJECT_LOG_EMPTY_WITNESS,
        },
        Duration::from_secs(15),
    )
    .await
}

async fn submit_p2mr_empty_witness_block(
    post_setup: &mut crate::setup::PostSetup,
) -> anyhow::Result<bitcoin::BlockHash> {
    let prevout = funding_prevout(post_setup).await?;
    let (template, coinbase) = prepare_coinbase(post_setup).await?;

    let funding_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: prevout,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(P2MR_PREVOUT_VALUE),
            script_pubkey: p2mr_script_pubkey([0xCD; 32]),
        }],
    };

    let spend_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: funding_tx.compute_txid(),
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(P2MR_SPEND_OUTPUT_VALUE),
            script_pubkey: post_setup.mining_address.script_pubkey(),
        }],
    };

    let funding_tx = wallet_sign_transaction(post_setup, funding_tx).await?;
    let block =
        build_block_with_coinbase(post_setup, &template, coinbase, vec![funding_tx, spend_tx])
            .await?;
    submit_block(post_setup, &block).await
}
