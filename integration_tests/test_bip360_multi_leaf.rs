//! BIP 360 multi-leaf P2MR integration trials (algorithm-per-leaf model).

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::validator::quantum::multi_leaf::{
    MLDSA_LEAF_INDEX, SCHNORR_LEAF_INDEX, SLH_LEAF_INDEX,
};
use futures::channel::mpsc;

use crate::{
    bip360_block::{
        bip360_setup_opts, build_block_with_coinbase,
        build_multi_leaf_tampered_signature_spend_txs,
        build_multi_leaf_wrong_control_block_spend_txs, build_three_leaf_p2mr_spend_txs,
        funding_prevout, prepare_coinbase, submit_block, submit_cross_block_p2mr_spend_with_txs,
        wait_for_enforcer_synced,
    },
    block_verdict::{Expect, assert_enforcer_verdict},
    setup::{Mode, PreSetup},
};

const REJECT_LOG_MERKLE_PATH: &str = "merkle root mismatch for leaf script";
const REJECT_LOG_SIGNATURE_MLDSA: &str = "invalid ML-DSA-44 signature";

#[derive(Clone, Copy, Debug)]
enum SpendBlockMode {
    SameBlock,
    CrossBlock,
}

async fn run_valid_multi_leaf_spend_trial(
    pre_setup: PreSetup,
    leaf_index: usize,
    mode: SpendBlockMode,
) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut post_setup = pre_setup
        .setup(Mode::NoMempool, bip360_setup_opts(), res_tx)
        .await?;

    wait_for_enforcer_synced(&mut post_setup).await?;

    let block_hash = match mode {
        SpendBlockMode::SameBlock => {
            let prevout = funding_prevout(&post_setup).await?;
            let (template, coinbase) = prepare_coinbase(&mut post_setup).await?;
            let (funding_tx, spend_tx) =
                build_three_leaf_p2mr_spend_txs(&post_setup, prevout, leaf_index).await?;

            let block = build_block_with_coinbase(
                &post_setup,
                &template,
                coinbase,
                vec![funding_tx, spend_tx],
            )
            .await?;
            submit_block(&mut post_setup, &block).await?
        }
        SpendBlockMode::CrossBlock => {
            let prevout = funding_prevout(&post_setup).await?;
            let (funding_tx, spend_tx) =
                build_three_leaf_p2mr_spend_txs(&post_setup, prevout, leaf_index).await?;
            let (funding_hash, spend_hash) =
                submit_cross_block_p2mr_spend_with_txs(&mut post_setup, funding_tx, spend_tx)
                    .await?;

            assert_enforcer_verdict(
                &mut post_setup,
                funding_hash,
                Expect::Accepted,
                Duration::from_secs(15),
            )
            .await?;

            spend_hash
        }
    };

    tracing::info!(%block_hash, leaf_index, ?mode, "submitted valid multi-leaf BIP 360 block");

    assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Accepted,
        Duration::from_secs(15),
    )
    .await
}

async fn run_invalid_multi_leaf_wrong_control_block_trial(
    pre_setup: PreSetup,
    reveal_leaf_index: usize,
    wrong_control_leaf_index: usize,
    mode: SpendBlockMode,
) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut post_setup = pre_setup
        .setup(Mode::NoMempool, bip360_setup_opts(), res_tx)
        .await?;

    wait_for_enforcer_synced(&mut post_setup).await?;

    let block_hash = match mode {
        SpendBlockMode::SameBlock => {
            let prevout = funding_prevout(&post_setup).await?;
            let (template, coinbase) = prepare_coinbase(&mut post_setup).await?;
            let (funding_tx, spend_tx) = build_multi_leaf_wrong_control_block_spend_txs(
                &post_setup,
                prevout,
                reveal_leaf_index,
                wrong_control_leaf_index,
            )
            .await?;

            let block = build_block_with_coinbase(
                &post_setup,
                &template,
                coinbase,
                vec![funding_tx, spend_tx],
            )
            .await?;
            submit_block(&mut post_setup, &block).await?
        }
        SpendBlockMode::CrossBlock => {
            let prevout = funding_prevout(&post_setup).await?;
            let (funding_tx, spend_tx) = build_multi_leaf_wrong_control_block_spend_txs(
                &post_setup,
                prevout,
                reveal_leaf_index,
                wrong_control_leaf_index,
            )
            .await?;
            let (funding_hash, spend_hash) =
                submit_cross_block_p2mr_spend_with_txs(&mut post_setup, funding_tx, spend_tx)
                    .await?;

            assert_enforcer_verdict(
                &mut post_setup,
                funding_hash,
                Expect::Accepted,
                Duration::from_secs(15),
            )
            .await?;

            spend_hash
        }
    };

    tracing::info!(
        %block_hash,
        reveal_leaf_index,
        wrong_control_leaf_index,
        ?mode,
        "submitted invalid multi-leaf wrong-control-block BIP 360 block"
    );

    assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Rejected {
            log_contains: REJECT_LOG_MERKLE_PATH,
        },
        Duration::from_secs(15),
    )
    .await
}

async fn run_invalid_multi_leaf_tampered_signature_trial(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut post_setup = pre_setup
        .setup(Mode::NoMempool, bip360_setup_opts(), res_tx)
        .await?;

    wait_for_enforcer_synced(&mut post_setup).await?;

    let prevout = funding_prevout(&post_setup).await?;
    let (template, coinbase) = prepare_coinbase(&mut post_setup).await?;
    let (funding_tx, spend_tx) =
        build_multi_leaf_tampered_signature_spend_txs(&post_setup, prevout).await?;

    let block =
        build_block_with_coinbase(&post_setup, &template, coinbase, vec![funding_tx, spend_tx])
            .await?;
    let block_hash = submit_block(&mut post_setup, &block).await?;

    tracing::info!(%block_hash, "submitted invalid multi-leaf tampered ML-DSA signature block");

    assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Rejected {
            log_contains: REJECT_LOG_SIGNATURE_MLDSA,
        },
        Duration::from_secs(15),
    )
    .await
}

pub async fn test_bip360_valid_multi_leaf_schnorr_spend(pre_setup: PreSetup) -> anyhow::Result<()> {
    run_valid_multi_leaf_spend_trial(pre_setup, SCHNORR_LEAF_INDEX, SpendBlockMode::SameBlock).await
}

pub async fn test_bip360_valid_multi_leaf_mldsa_spend(pre_setup: PreSetup) -> anyhow::Result<()> {
    run_valid_multi_leaf_spend_trial(pre_setup, MLDSA_LEAF_INDEX, SpendBlockMode::SameBlock).await
}

pub async fn test_bip360_valid_multi_leaf_slh_spend(pre_setup: PreSetup) -> anyhow::Result<()> {
    run_valid_multi_leaf_spend_trial(pre_setup, SLH_LEAF_INDEX, SpendBlockMode::SameBlock).await
}

pub async fn test_bip360_valid_multi_leaf_cross_block_mldsa_spend(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_valid_multi_leaf_spend_trial(pre_setup, MLDSA_LEAF_INDEX, SpendBlockMode::CrossBlock).await
}

pub async fn test_bip360_valid_multi_leaf_cross_block_schnorr_spend(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_valid_multi_leaf_spend_trial(pre_setup, SCHNORR_LEAF_INDEX, SpendBlockMode::CrossBlock)
        .await
}

pub async fn test_bip360_valid_multi_leaf_cross_block_slh_spend(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_valid_multi_leaf_spend_trial(pre_setup, SLH_LEAF_INDEX, SpendBlockMode::CrossBlock).await
}

pub async fn test_bip360_invalid_multi_leaf_wrong_control_block(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_multi_leaf_wrong_control_block_trial(
        pre_setup,
        MLDSA_LEAF_INDEX,
        SCHNORR_LEAF_INDEX,
        SpendBlockMode::SameBlock,
    )
    .await
}

pub async fn test_bip360_invalid_multi_leaf_cross_block_wrong_control_block(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_multi_leaf_wrong_control_block_trial(
        pre_setup,
        MLDSA_LEAF_INDEX,
        SCHNORR_LEAF_INDEX,
        SpendBlockMode::CrossBlock,
    )
    .await
}

pub async fn test_bip360_invalid_multi_leaf_tampered_signature_mldsa(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_multi_leaf_tampered_signature_trial(pre_setup).await
}
