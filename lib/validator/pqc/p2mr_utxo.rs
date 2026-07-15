//! P2MR UTXO diff computation for block connect / disconnect.

use std::collections::{HashMap, HashSet};

use bitcoin::{Block, OutPoint, Transaction, TxOut};
use serde::{Deserialize, Serialize};

use super::p2mr_output;

type P2mrUtxoDiffMaps<'a> = (
    &'a mut HashMap<OutPoint, TxOut>,
    &'a mut HashMap<OutPoint, TxOut>,
);

/// P2MR UTXO changes from connecting a single block.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct P2mrUtxoBlockDiff {
    /// P2MR UTXOs spent in this block (restored on disconnect).
    pub spent: HashMap<OutPoint, TxOut>,
    /// P2MR UTXOs created in this block (removed on disconnect).
    pub created: HashMap<OutPoint, TxOut>,
}

/// Compute the P2MR UTXO diff for `block` against `chain_utxos`.
///
/// Processes transactions in block order so intra-block spends are reflected
/// in the running available set (same semantics as block validation).
#[must_use]
pub fn compute_block_diff(
    block: &Block,
    chain_utxos: &HashMap<OutPoint, TxOut>,
) -> P2mrUtxoBlockDiff {
    let mut available = chain_utxos.clone();
    let mut spent = HashMap::new();
    let mut created = HashMap::new();

    for tx in &block.txdata {
        apply_tx_to_available(tx, &mut available, &mut spent, &mut created);
    }

    P2mrUtxoBlockDiff { spent, created }
}

/// Update the incremental prevout map after successfully validating a transaction.
///
/// Unlike [`apply_tx_to_available`], this inserts **all** outputs into `available`
/// (not only P2MR) so `Prevouts::All` sighash can reference same-block non-P2MR
/// outputs. When `diff` is `Some`, P2MR-only spent/created entries are recorded
/// for block-connect persistence.
pub fn apply_tx_to_validation_available(
    tx: &Transaction,
    available: &mut HashMap<OutPoint, TxOut>,
    mut diff: Option<P2mrUtxoDiffMaps<'_>>,
    spent_p2mr_in_block: &mut HashSet<OutPoint>,
) {
    for input in &tx.input {
        let outpoint = input.previous_output;
        if let Some(prevout) = available.remove(&outpoint)
            && p2mr_output::is_p2mr_script(prevout.script_pubkey.as_script())
        {
            spent_p2mr_in_block.insert(outpoint);
            if let Some((spent, created)) = diff.as_mut() {
                // Same-block create → spend cancels out: never hit the chain DB.
                if created.remove(&outpoint).is_none() {
                    spent.insert(outpoint, prevout);
                }
            }
        }
    }

    let txid = tx.compute_txid();
    for (vout, output) in tx.output.iter().enumerate() {
        let outpoint = OutPoint {
            txid,
            vout: vout as u32,
        };
        available.insert(outpoint, output.clone());
        if let Some((_, created)) = diff.as_mut()
            && p2mr_output::is_p2mr_script(output.script_pubkey.as_script())
        {
            created.insert(outpoint, output.clone());
        }
    }
}

/// Update the P2MR-only incremental prevout map for diff computation.
pub fn apply_tx_to_available(
    tx: &Transaction,
    available: &mut HashMap<OutPoint, TxOut>,
    spent: &mut HashMap<OutPoint, TxOut>,
    created: &mut HashMap<OutPoint, TxOut>,
) {
    for input in &tx.input {
        let outpoint = input.previous_output;
        if let Some(prevout) = available.remove(&outpoint)
            && p2mr_output::is_p2mr_script(prevout.script_pubkey.as_script())
            && created.remove(&outpoint).is_none()
        {
            spent.insert(outpoint, prevout);
        }
    }

    let txid = tx.compute_txid();
    for (vout, output) in tx.output.iter().enumerate() {
        if p2mr_output::is_p2mr_script(output.script_pubkey.as_script()) {
            let outpoint = OutPoint {
                txid,
                vout: vout as u32,
            };
            created.insert(outpoint, output.clone());
            available.insert(outpoint, output.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{Amount, ScriptBuf, TxOut, transaction::Version};
    use miette::IntoDiagnostic;

    use crate::validator::pqc::signer_dev::{SignAlgorithm, sign_p2mr_script_path_spend};
    use crate::validator::test_utils::{create_test_dbs, test_block_header};
    use bitcoin::hashes::Hash as _;
    use bitcoin::sighash::TapSighashType;

    fn p2mr_funding_tx() -> bitcoin::Transaction {
        let mut spk = vec![0x52, 0x20];
        spk.extend_from_slice(&[0xCD; 32]);
        bitcoin::Transaction {
            version: Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: ScriptBuf::from_bytes(spk),
            }],
        }
    }

    #[test]
    fn compute_block_diff_tracks_spent_and_created() {
        let signed = sign_p2mr_script_path_spend(
            SignAlgorithm::Schnorr,
            &[0x11; 32],
            TapSighashType::All,
            50_000,
            40_000,
        )
        .expect("sign");

        let funding_block = Block {
            header: test_block_header(bitcoin::BlockHash::all_zeros()),
            txdata: vec![signed.funding_tx.clone()],
        };

        let chain_utxos = compute_block_diff(&funding_block, &HashMap::new()).created;
        let spend_block = Block {
            header: funding_block.header,
            txdata: vec![
                bitcoin::Transaction {
                    version: Version::TWO,
                    lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
                    input: vec![],
                    output: vec![],
                },
                signed.signed_spend_tx.clone(),
            ],
        };

        let diff = compute_block_diff(&spend_block, &chain_utxos);
        assert_eq!(diff.spent.len(), 1);
        assert!(diff.created.is_empty());
    }

    #[test]
    fn connect_disconnect_roundtrip_restores_utxo_set() -> miette::Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;

        let signed = sign_p2mr_script_path_spend(
            SignAlgorithm::Schnorr,
            &[0x22; 32],
            TapSighashType::All,
            50_000,
            40_000,
        )
        .map_err(|e| miette::miette!(e))?;

        let funding_block = Block {
            header: test_block_header(bitcoin::BlockHash::all_zeros()),
            txdata: vec![signed.funding_tx],
        };

        let fund_diff = compute_block_diff(&funding_block, &HashMap::new());
        dbs.p2mr_utxos
            .apply_diff(&mut rwtxn, &fund_diff.spent, &fund_diff.created)
            .into_diagnostic()?;

        let chain_utxos = dbs.p2mr_utxos.load_map(&rwtxn).into_diagnostic()?;
        assert_eq!(chain_utxos.len(), 1);

        let spend_block = Block {
            header: funding_block.header,
            txdata: vec![
                bitcoin::Transaction {
                    version: Version::TWO,
                    lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
                    input: vec![],
                    output: vec![],
                },
                signed.signed_spend_tx,
            ],
        };
        let spend_diff = compute_block_diff(&spend_block, &chain_utxos);
        dbs.p2mr_utxos
            .apply_diff(&mut rwtxn, &spend_diff.spent, &spend_diff.created)
            .into_diagnostic()?;
        assert!(
            dbs.p2mr_utxos
                .load_map(&rwtxn)
                .into_diagnostic()?
                .is_empty()
        );

        dbs.p2mr_utxos
            .undo_diff(&mut rwtxn, &spend_diff.spent, &spend_diff.created)
            .into_diagnostic()?;
        let restored = dbs.p2mr_utxos.load_map(&rwtxn).into_diagnostic()?;
        assert_eq!(restored.len(), 1);
        assert_eq!(restored, chain_utxos);
        Ok(())
    }

    #[test]
    fn p2mr_funding_only_block_adds_utxo() {
        let block = Block {
            header: test_block_header(bitcoin::BlockHash::all_zeros()),
            txdata: vec![p2mr_funding_tx()],
        };
        let diff = compute_block_diff(&block, &HashMap::new());
        assert!(diff.spent.is_empty());
        assert_eq!(diff.created.len(), 1);
    }
}
