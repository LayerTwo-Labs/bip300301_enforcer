//! Per-transaction metrics for BIP 360 P2P mempool E2E trials.

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    validator::pqc::schemes::{self, SignatureScheme},
};
use bitcoin::consensus::encode::serialize;
use bitcoin::{Transaction, Txid};
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
        "P2P mempool tx report"
    );
}
