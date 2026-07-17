//! Per-transaction metrics for BIP 360 dual-node / mining trials.

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    validator::pqc::schemes::{self, SignatureScheme},
};
use bitcoin::{Transaction, Txid, consensus::encode::serialize};
use serde::Deserialize;
use tokio::time::sleep;

use crate::setup::PostSetup;

/// Mempool fee fields from Core `getmempoolentry`.
#[derive(Clone, Debug, Deserialize)]
pub struct MempoolFees {
    #[serde(default)]
    pub base: Option<f64>,
    #[serde(default)]
    pub modified: Option<f64>,
    #[serde(default)]
    pub ancestor: Option<f64>,
    #[serde(default)]
    pub descendant: Option<f64>,
}

/// Human-readable report for a broadcast P2MR spend.
#[derive(Clone, Debug)]
pub struct TxReport {
    pub txid: Txid,
    pub size: usize,
    pub vsize: usize,
    pub weight: usize,
    pub pqc_algos: Vec<&'static str>,
    pub mempool_fees: Option<MempoolFees>,
}

#[derive(Debug, Deserialize)]
struct MempoolEntry {
    #[serde(default)]
    fees: Option<MempoolFees>,
}

fn scheme_label(scheme: SignatureScheme) -> &'static str {
    match scheme {
        SignatureScheme::Schnorr => "schnorr",
        SignatureScheme::MlDsa44 => "mldsa",
        SignatureScheme::SlhDsaSha2128s => "slh",
    }
}

/// Classify PQC / Schnorr algorithms present in a P2MR script-path witness stack.
#[must_use]
pub fn pqc_algorithms_in_witness(tx: &Transaction) -> Vec<&'static str> {
    let Some(witness) = tx.input.first().map(|input| &input.witness) else {
        return Vec::new();
    };
    let sig_count = witness.len().saturating_sub(2);
    let mut algos = Vec::new();
    for element in witness.iter().take(sig_count) {
        if let Ok(scheme) = schemes::classify_signature(element) {
            let label = scheme_label(scheme);
            if !algos.contains(&label) {
                algos.push(label);
            }
        }
    }
    algos
}

/// Build a [`TxReport`] from a transaction and optional mempool fee data.
#[must_use]
pub fn tx_report(tx: &Transaction, mempool_fees: Option<MempoolFees>) -> TxReport {
    let txid = tx.compute_txid();
    let size = serialize(tx).len();
    let weight = tx.weight().to_wu() as usize;
    let vsize = weight.div_ceil(4);
    TxReport {
        txid,
        size,
        vsize,
        weight,
        pqc_algos: pqc_algorithms_in_witness(tx),
        mempool_fees,
    }
}

/// Poll Core `getmempoolentry` until the tx appears or the timeout elapses.
pub async fn wait_for_mempool_entry(
    post_setup: &PostSetup,
    txid: Txid,
    timeout: Duration,
) -> anyhow::Result<MempoolFees> {
    let txid_str = txid.to_string();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match post_setup
            .bitcoin_cli
            .command::<String, _, _, _, _>([], "getmempoolentry", [&txid_str])
            .run_utf8()
            .await
        {
            Ok(json) => {
                let entry: MempoolEntry = serde_json::from_str(&json)?;
                return Ok(entry.fees.unwrap_or(MempoolFees {
                    base: None,
                    modified: None,
                    ancestor: None,
                    descendant: None,
                }));
            }
            Err(err) => {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "tx {txid} not in mempool within {timeout:?}: {err:#}"
                );
                sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// Log a concise transaction report at info level.
///
/// `peer` is a free-form label (node role, path, or round context) — not only P2P mempool.
pub fn log_tx_report(round: u32, peer: &str, report: &TxReport) {
    tracing::info!(
        round,
        peer,
        txid = %report.txid,
        size = report.size,
        vsize = report.vsize,
        weight = report.weight,
        pqc_algos = ?report.pqc_algos,
        mempool_fees = ?report.mempool_fees,
        "BIP 360 tx report"
    );
}

/// Kitchen-sink product-climax shape: witness depth 5, three algos, weight > 10_000 WU.
///
/// Shared by TB-mine / TB-factory / TB-sidecar so the bar cannot drift across trials.
pub fn assert_kitchen_sink_spend_shape(spend_tx: &Transaction) -> anyhow::Result<()> {
    anyhow::ensure!(
        spend_tx.input[0].witness.len() == 5,
        "kitchen-sink witness stack depth must be 5 (3 sigs + leaf + control block; got {})",
        spend_tx.input[0].witness.len()
    );
    let report = tx_report(spend_tx, None);
    anyhow::ensure!(
        report.pqc_algos.len() == 3,
        "kitchen-sink expects three algorithms (got {:?})",
        report.pqc_algos
    );
    anyhow::ensure!(
        report.weight > 10_000,
        "kitchen-sink weight should exceed 10_000 WU (got {})",
        report.weight
    );
    Ok(())
}
