//! BIP 360 invalid-spend integration trials (tampered sig, bad pubkey, bad merkle path).

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::validator::quantum::signer_dev::SignAlgorithm;
use futures::channel::mpsc;

use crate::{
    bip360_block::{
        MLDSA_ENTROPY, SCHNORR_ENTROPY, SLH_ENTROPY, bip360_setup_opts, build_block_with_coinbase,
        build_mldsa_sig_wrong_pubkey_spend_txs, build_valid_hybrid_ec_slh_p2mr_spend_txs,
        build_valid_p2mr_spend_txs, funding_prevout, prepare_coinbase, submit_block,
        submit_cross_block_p2mr_spend_with_txs, swap_hybrid_witness_signatures,
        tamper_hybrid_witness_ec_signature, tamper_hybrid_witness_slh_signature,
        tamper_witness_control_block, tamper_witness_signature, wait_for_enforcer_synced,
    },
    block_verdict::{Expect, assert_enforcer_verdict},
    setup::{Mode, PreSetup},
};

const REJECT_LOG_SIGNATURE_SCHNORR: &str = "invalid Schnorr signature";
const REJECT_LOG_SIGNATURE_MLDSA: &str = "invalid ML-DSA-44 signature";
const REJECT_LOG_SIGNATURE_SLH: &str = "invalid SLH-DSA-SHA2-128s signature";
const REJECT_LOG_PUBKEY_SIZE: &str = "incompatible with signature scheme";
const REJECT_LOG_MERKLE_PATH: &str = "merkle root mismatch for leaf script";

#[derive(Clone, Copy, Debug)]
enum SpendBlockMode {
    SameBlock,
    CrossBlock,
}

#[derive(Clone, Copy, Debug)]
enum InvalidSpendCase {
    TamperedSignature(SignAlgorithm, &'static [u8]),
    TamperedMerklePath(SignAlgorithm, &'static [u8]),
    WrongPubkeySize,
    TamperedHybridEcSig,
    TamperedHybridSlhSig,
    SwappedHybridSigs,
}

async fn build_invalid_spend_txs(
    post_setup: &crate::setup::PostSetup,
    funding_prevout: bitcoin::OutPoint,
    case: InvalidSpendCase,
) -> anyhow::Result<(bitcoin::Transaction, bitcoin::Transaction)> {
    match case {
        InvalidSpendCase::TamperedSignature(algorithm, entropy) => {
            build_tampered_signature_spend_txs(post_setup, funding_prevout, algorithm, entropy)
                .await
        }
        InvalidSpendCase::TamperedMerklePath(algorithm, entropy) => {
            build_tampered_merkle_path_spend_txs(post_setup, funding_prevout, algorithm, entropy)
                .await
        }
        InvalidSpendCase::WrongPubkeySize => {
            build_mldsa_sig_wrong_pubkey_spend_txs(post_setup, funding_prevout).await
        }
        InvalidSpendCase::TamperedHybridEcSig => {
            build_tampered_hybrid_ec_signature_spend_txs(post_setup, funding_prevout).await
        }
        InvalidSpendCase::TamperedHybridSlhSig => {
            build_tampered_hybrid_slh_signature_spend_txs(post_setup, funding_prevout).await
        }
        InvalidSpendCase::SwappedHybridSigs => {
            build_swapped_hybrid_signatures_spend_txs(post_setup, funding_prevout).await
        }
    }
}

async fn run_invalid_p2mr_spend_trial(
    pre_setup: PreSetup,
    reject_log: &'static str,
    mode: SpendBlockMode,
    case: InvalidSpendCase,
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
                build_invalid_spend_txs(&post_setup, prevout, case).await?;

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
                build_invalid_spend_txs(&post_setup, prevout, case).await?;
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
    tracing::info!(%block_hash, ?mode, "submitted BIP 360 invalid spend block");

    assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Rejected {
            log_contains: reject_log,
        },
        Duration::from_secs(15),
    )
    .await
}

async fn build_tampered_signature_spend_txs(
    post_setup: &crate::setup::PostSetup,
    funding_prevout: bitcoin::OutPoint,
    algorithm: SignAlgorithm,
    entropy: &[u8],
) -> anyhow::Result<(bitcoin::Transaction, bitcoin::Transaction)> {
    let (funding_tx, mut spend_tx) =
        build_valid_p2mr_spend_txs(post_setup, funding_prevout, algorithm, entropy).await?;
    tamper_witness_signature(&mut spend_tx)?;
    Ok((funding_tx, spend_tx))
}

async fn build_tampered_merkle_path_spend_txs(
    post_setup: &crate::setup::PostSetup,
    funding_prevout: bitcoin::OutPoint,
    algorithm: SignAlgorithm,
    entropy: &[u8],
) -> anyhow::Result<(bitcoin::Transaction, bitcoin::Transaction)> {
    let (funding_tx, mut spend_tx) =
        build_valid_p2mr_spend_txs(post_setup, funding_prevout, algorithm, entropy).await?;
    tamper_witness_control_block(&mut spend_tx)?;
    Ok((funding_tx, spend_tx))
}

pub async fn test_bip360_invalid_signature(pre_setup: PreSetup) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_SIGNATURE_SCHNORR,
        SpendBlockMode::SameBlock,
        InvalidSpendCase::TamperedSignature(SignAlgorithm::Schnorr, &SCHNORR_ENTROPY),
    )
    .await
}

pub async fn test_bip360_invalid_pubkey_size(pre_setup: PreSetup) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_PUBKEY_SIZE,
        SpendBlockMode::SameBlock,
        InvalidSpendCase::WrongPubkeySize,
    )
    .await
}

pub async fn test_bip360_invalid_merkle_path(pre_setup: PreSetup) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_MERKLE_PATH,
        SpendBlockMode::SameBlock,
        InvalidSpendCase::TamperedMerklePath(SignAlgorithm::Schnorr, &SCHNORR_ENTROPY),
    )
    .await
}

pub async fn test_bip360_invalid_cross_block_signature_schnorr(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_SIGNATURE_SCHNORR,
        SpendBlockMode::CrossBlock,
        InvalidSpendCase::TamperedSignature(SignAlgorithm::Schnorr, &SCHNORR_ENTROPY),
    )
    .await
}

pub async fn test_bip360_invalid_cross_block_signature_mldsa(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_SIGNATURE_MLDSA,
        SpendBlockMode::CrossBlock,
        InvalidSpendCase::TamperedSignature(SignAlgorithm::Mldsa, &MLDSA_ENTROPY),
    )
    .await
}

pub async fn test_bip360_invalid_cross_block_signature_slh(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_SIGNATURE_SLH,
        SpendBlockMode::CrossBlock,
        InvalidSpendCase::TamperedSignature(SignAlgorithm::Slh, &SLH_ENTROPY),
    )
    .await
}

pub async fn test_bip360_invalid_cross_block_merkle_path_schnorr(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_MERKLE_PATH,
        SpendBlockMode::CrossBlock,
        InvalidSpendCase::TamperedMerklePath(SignAlgorithm::Schnorr, &SCHNORR_ENTROPY),
    )
    .await
}

pub async fn test_bip360_invalid_cross_block_merkle_path_mldsa(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_MERKLE_PATH,
        SpendBlockMode::CrossBlock,
        InvalidSpendCase::TamperedMerklePath(SignAlgorithm::Mldsa, &MLDSA_ENTROPY),
    )
    .await
}

pub async fn test_bip360_invalid_cross_block_merkle_path_slh(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_MERKLE_PATH,
        SpendBlockMode::CrossBlock,
        InvalidSpendCase::TamperedMerklePath(SignAlgorithm::Slh, &SLH_ENTROPY),
    )
    .await
}

pub async fn test_bip360_invalid_cross_block_pubkey_size_mldsa(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_PUBKEY_SIZE,
        SpendBlockMode::CrossBlock,
        InvalidSpendCase::WrongPubkeySize,
    )
    .await
}

async fn build_tampered_hybrid_ec_signature_spend_txs(
    post_setup: &crate::setup::PostSetup,
    funding_prevout: bitcoin::OutPoint,
) -> anyhow::Result<(bitcoin::Transaction, bitcoin::Transaction)> {
    let (funding_tx, mut spend_tx) =
        build_valid_hybrid_ec_slh_p2mr_spend_txs(post_setup, funding_prevout).await?;
    tamper_hybrid_witness_ec_signature(&mut spend_tx)?;
    Ok((funding_tx, spend_tx))
}

async fn build_tampered_hybrid_slh_signature_spend_txs(
    post_setup: &crate::setup::PostSetup,
    funding_prevout: bitcoin::OutPoint,
) -> anyhow::Result<(bitcoin::Transaction, bitcoin::Transaction)> {
    let (funding_tx, mut spend_tx) =
        build_valid_hybrid_ec_slh_p2mr_spend_txs(post_setup, funding_prevout).await?;
    tamper_hybrid_witness_slh_signature(&mut spend_tx)?;
    Ok((funding_tx, spend_tx))
}

async fn build_swapped_hybrid_signatures_spend_txs(
    post_setup: &crate::setup::PostSetup,
    funding_prevout: bitcoin::OutPoint,
) -> anyhow::Result<(bitcoin::Transaction, bitcoin::Transaction)> {
    let (funding_tx, mut spend_tx) =
        build_valid_hybrid_ec_slh_p2mr_spend_txs(post_setup, funding_prevout).await?;
    swap_hybrid_witness_signatures(&mut spend_tx)?;
    Ok((funding_tx, spend_tx))
}

pub async fn test_bip360_invalid_hybrid_ec_slh_tamper_ec_sig(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_SIGNATURE_SCHNORR,
        SpendBlockMode::SameBlock,
        InvalidSpendCase::TamperedHybridEcSig,
    )
    .await
}

pub async fn test_bip360_invalid_hybrid_ec_slh_tamper_slh_sig(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_SIGNATURE_SLH,
        SpendBlockMode::SameBlock,
        InvalidSpendCase::TamperedHybridSlhSig,
    )
    .await
}

pub async fn test_bip360_invalid_hybrid_ec_slh_swap_sigs(
    pre_setup: PreSetup,
) -> anyhow::Result<()> {
    run_invalid_p2mr_spend_trial(
        pre_setup,
        REJECT_LOG_SIGNATURE_SLH,
        SpendBlockMode::SameBlock,
        InvalidSpendCase::SwappedHybridSigs,
    )
    .await
}
