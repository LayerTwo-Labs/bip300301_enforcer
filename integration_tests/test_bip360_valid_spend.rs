//! BIP 360 valid-block integration trials (submitblock → retained on enforcer tip).

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::validator::quantum::signer_dev::SignAlgorithm;
use futures::channel::mpsc;

use crate::{
    bip360_block::{
        MLDSA_ENTROPY, SCHNORR_ENTROPY, SLH_ENTROPY, bip360_setup_opts, build_block_with_coinbase,
        build_valid_hybrid_ec_slh_p2mr_spend_txs, build_valid_p2mr_spend_txs, funding_prevout,
        prepare_coinbase, submit_block, submit_cross_block_p2mr_spend,
        submit_cross_block_p2mr_spend_with_txs, wait_for_enforcer_synced,
    },
    block_verdict::{Expect, assert_enforcer_verdict},
    setup::{Mode, PreSetup},
};

async fn run_valid_p2mr_spend_trial(
    pre_setup: PreSetup,
    algorithm: SignAlgorithm,
    entropy: &[u8],
) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut post_setup = pre_setup
        .setup(Mode::NoMempool, bip360_setup_opts(), res_tx)
        .await?;

    wait_for_enforcer_synced(&mut post_setup).await?;

    let prevout = funding_prevout(&post_setup).await?;
    let (template, coinbase) = prepare_coinbase(&mut post_setup).await?;
    let (funding_tx, spend_tx) =
        build_valid_p2mr_spend_txs(&post_setup, prevout, algorithm, entropy).await?;

    let block =
        build_block_with_coinbase(&post_setup, &template, coinbase, vec![funding_tx, spend_tx])
            .await?;
    let block_hash = submit_block(&mut post_setup, &block).await?;
    tracing::info!(%block_hash, algorithm = algorithm.label(), "submitted valid BIP 360 block");

    assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Accepted,
        Duration::from_secs(15),
    )
    .await
}

async fn run_valid_cross_block_p2mr_spend_trial(
    pre_setup: PreSetup,
    algorithm: SignAlgorithm,
    entropy: &[u8],
) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut post_setup = pre_setup
        .setup(Mode::NoMempool, bip360_setup_opts(), res_tx)
        .await?;

    wait_for_enforcer_synced(&mut post_setup).await?;

    let (_funding_hash, spend_hash) =
        submit_cross_block_p2mr_spend(&mut post_setup, algorithm, entropy).await?;
    tracing::info!(
        %spend_hash,
        algorithm = algorithm.label(),
        "submitted cross-block valid BIP 360 spend block"
    );

    assert_enforcer_verdict(
        &mut post_setup,
        spend_hash,
        Expect::Accepted,
        Duration::from_secs(15),
    )
    .await
}

pub async fn test_bip360_valid_schnorr_spend(pre_setup: PreSetup) -> anyhow::Result<()> {
    run_valid_p2mr_spend_trial(pre_setup, SignAlgorithm::Schnorr, &SCHNORR_ENTROPY).await
}

pub async fn test_bip360_valid_mldsa_spend(pre_setup: PreSetup) -> anyhow::Result<()> {
    run_valid_p2mr_spend_trial(pre_setup, SignAlgorithm::Mldsa, &MLDSA_ENTROPY).await
}

pub async fn test_bip360_valid_slh_spend(pre_setup: PreSetup) -> anyhow::Result<()> {
    run_valid_p2mr_spend_trial(pre_setup, SignAlgorithm::Slh, &SLH_ENTROPY).await
}

/// Valid P2MR spend where funding is confirmed in a prior block (cross-block prevout).
pub async fn test_bip360_valid_cross_block_schnorr_spend(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_valid_cross_block_p2mr_spend_trial(pre_setup, SignAlgorithm::Schnorr, &SCHNORR_ENTROPY)
        .await
}

/// Valid cross-block ML-DSA-44 P2MR spend (funding in block N, spend in block N+1).
pub async fn test_bip360_valid_cross_block_mldsa_spend(pre_setup: PreSetup) -> anyhow::Result<()> {
    run_valid_cross_block_p2mr_spend_trial(pre_setup, SignAlgorithm::Mldsa, &MLDSA_ENTROPY).await
}

/// Valid cross-block SLH-DSA P2MR spend (funding in block N, spend in block N+1).
pub async fn test_bip360_valid_cross_block_slh_spend(pre_setup: PreSetup) -> anyhow::Result<()> {
    run_valid_cross_block_p2mr_spend_trial(pre_setup, SignAlgorithm::Slh, &SLH_ENTROPY).await
}

async fn run_valid_hybrid_ec_slh_spend_trial(pre_setup: PreSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut post_setup = pre_setup
        .setup(Mode::NoMempool, bip360_setup_opts(), res_tx)
        .await?;

    wait_for_enforcer_synced(&mut post_setup).await?;

    let prevout = funding_prevout(&post_setup).await?;
    let (template, coinbase) = prepare_coinbase(&mut post_setup).await?;
    let (funding_tx, spend_tx) =
        build_valid_hybrid_ec_slh_p2mr_spend_txs(&post_setup, prevout).await?;

    let block =
        build_block_with_coinbase(&post_setup, &template, coinbase, vec![funding_tx, spend_tx])
            .await?;
    let block_hash = submit_block(&mut post_setup, &block).await?;
    tracing::info!(%block_hash, "submitted valid hybrid EC+SLH BIP 360 block");

    assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Accepted,
        Duration::from_secs(15),
    )
    .await
}

async fn run_valid_cross_block_hybrid_ec_slh_spend_trial(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut post_setup = pre_setup
        .setup(Mode::NoMempool, bip360_setup_opts(), res_tx)
        .await?;

    wait_for_enforcer_synced(&mut post_setup).await?;

    let prevout = funding_prevout(&post_setup).await?;
    let (funding_tx, spend_tx) =
        build_valid_hybrid_ec_slh_p2mr_spend_txs(&post_setup, prevout).await?;
    let (_funding_hash, spend_hash) =
        submit_cross_block_p2mr_spend_with_txs(&mut post_setup, funding_tx, spend_tx).await?;
    tracing::info!(
        %spend_hash,
        "submitted cross-block valid hybrid EC+SLH BIP 360 spend block"
    );

    assert_enforcer_verdict(
        &mut post_setup,
        spend_hash,
        Expect::Accepted,
        Duration::from_secs(15),
    )
    .await
}

pub async fn test_bip360_valid_hybrid_ec_slh_spend(pre_setup: PreSetup) -> anyhow::Result<()> {
    run_valid_hybrid_ec_slh_spend_trial(pre_setup).await
}

pub async fn test_bip360_valid_hybrid_ec_slh_cross_block_spend(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_valid_cross_block_hybrid_ec_slh_spend_trial(pre_setup).await
}
