//! BIP 360 P2MR + PQC signature validation (CUSF PQC rules).
//!
//! SHRINCs / XMSS hybrid verification is deferred — see `docs/SHRINCS_DEFERRED.md`.
//! A future `shrincs` Cargo feature may gate optional backup-signature paths when a
//! reference implementation and test vectors exist.

#[cfg(feature = "shrincs")]
compile_error!(
    "the `shrincs` feature is reserved but not implemented; see docs/SHRINCS_DEFERRED.md"
);

pub mod activation;
pub mod leaf_script;
pub mod limits;
pub mod merkle;
pub mod p2mr_output;
pub mod p2mr_utxo;
pub mod schemes;
pub mod signer_dev;
pub mod spend;

#[cfg(feature = "bip360")]
pub mod multi_leaf;

#[cfg(all(test, feature = "bip360"))]
mod overload_vectors;

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};

use bitcoin::{Block, OutPoint, Transaction, TxOut};
use thiserror::Error;

use self::activation::Bip360Activation;
use self::limits::PqcVerifyBudget;
use self::spend::SpendError;

#[derive(Debug, Error)]
pub enum PqcValidationError {
    #[error("BIP 360 validation failed for tx {txid}: {source}")]
    Transaction {
        txid: bitcoin::Txid,
        #[source]
        source: SpendError,
    },
    #[error("BIP 360 validation failed: {0}")]
    Block(String),
}

/// Validate BIP 360 rules for a single transaction.
pub fn validate_transaction(
    tx: &Transaction,
    height: u32,
    activation: Bip360Activation,
    available_prevouts: &HashMap<OutPoint, TxOut>,
    chain_p2mr_utxos: Option<&HashMap<OutPoint, TxOut>>,
    spent_p2mr_in_block: Option<&HashSet<OutPoint>>,
    pqc_budget: &mut Option<PqcVerifyBudget>,
) -> Result<(), PqcValidationError> {
    if !activation.is_active(height) {
        return Ok(());
    }
    spend::validate_transaction_spends(
        tx,
        available_prevouts,
        chain_p2mr_utxos,
        spent_p2mr_in_block,
        pqc_budget,
    )
    .map_err(|source| PqcValidationError::Transaction {
        txid: tx.compute_txid(),
        source,
    })
}

/// Validate BIP 360 rules for all non-coinbase transactions in a block.
///
/// `chain_p2mr_utxos` holds confirmed P2MR outputs from prior blocks. Outputs
/// from earlier transactions in the same block are merged incrementally so
/// intra-block and cross-block spends are both validated.
///
/// Incremental map updates use [`p2mr_utxo::apply_tx_to_validation_available`]
/// rather than [`p2mr_utxo::apply_tx_to_available`]: validation must insert
/// **all** same-block outputs into the working set for `Prevouts::All` sighash,
/// while diff computation tracks P2MR-only creates/spends.
pub fn validate_block_transactions(
    block: &Block,
    height: u32,
    activation: Bip360Activation,
    chain_p2mr_utxos: &HashMap<OutPoint, TxOut>,
    pqc_verify_budget_ms: u64,
) -> Result<(), PqcValidationError> {
    if !activation.is_active(height) {
        return Ok(());
    }
    let mut pqc_budget = Some(PqcVerifyBudget::with_budget_ms(pqc_verify_budget_ms));
    let mut available_prevouts = chain_p2mr_utxos.clone();
    let mut spent_p2mr_in_block = HashSet::new();
    seed_coinbase_outputs(block, &mut available_prevouts);
    for tx in block.txdata.iter().skip(1) {
        validate_transaction(
            tx,
            height,
            activation,
            &available_prevouts,
            Some(chain_p2mr_utxos),
            Some(&spent_p2mr_in_block),
            &mut pqc_budget,
        )?;
        p2mr_utxo::apply_tx_to_validation_available(
            tx,
            &mut available_prevouts,
            None,
            &mut spent_p2mr_in_block,
        );
    }
    Ok(())
}

/// Validate BIP 360 rules for a mempool transaction with explicit parent txs.
pub fn validate_mempool_transaction<TxRef>(
    tx: &Transaction,
    height: u32,
    activation: Bip360Activation,
    parent_txs: &HashMap<bitcoin::Txid, TxRef>,
) -> Result<(), PqcValidationError>
where
    TxRef: Borrow<Transaction>,
{
    if !activation.is_active(height) {
        return Ok(());
    }
    let mut available_prevouts = HashMap::new();
    for input in &tx.input {
        if let Some(parent) = parent_txs.get(&input.previous_output.txid) {
            let parent_tx = parent.borrow();
            if let Some(output) = parent_tx.output.get(input.previous_output.vout as usize) {
                available_prevouts.insert(input.previous_output, output.clone());
            }
        }
    }
    validate_transaction(
        tx,
        height,
        activation,
        &available_prevouts,
        None,
        None,
        &mut None,
    )
}

fn seed_coinbase_outputs(block: &Block, available: &mut HashMap<OutPoint, TxOut>) {
    let Some(coinbase) = block.txdata.first() else {
        return;
    };
    let txid = coinbase.compute_txid();
    for (vout, output) in coinbase.output.iter().enumerate() {
        available.insert(
            OutPoint {
                txid,
                vout: vout as u32,
            },
            output.clone(),
        );
    }
}

fn seed_coinbase_p2mr_diff(
    block: &Block,
    available: &mut HashMap<OutPoint, TxOut>,
    created: &mut HashMap<OutPoint, TxOut>,
) {
    let Some(coinbase) = block.txdata.first() else {
        return;
    };
    let txid = coinbase.compute_txid();
    for (vout, output) in coinbase.output.iter().enumerate() {
        let outpoint = OutPoint {
            txid,
            vout: vout as u32,
        };
        available.insert(outpoint, output.clone());
        if p2mr_output::is_p2mr_script(output.script_pubkey.as_script()) {
            created.insert(outpoint, output.clone());
        }
    }
}

/// Validate a block's P2MR rules and return the UTXO diff to persist on connect.
///
/// P2MR UTXO diff indexing runs even when BIP 360 validation is inactive at
/// `height`, so pre-activation blocks still populate the persistent set and
/// cross-block spends are available once activation height is reached.
#[cfg(feature = "bip360")]
pub fn validate_and_diff_block_transactions(
    block: &Block,
    height: u32,
    activation: Bip360Activation,
    chain_p2mr_utxos: &HashMap<OutPoint, TxOut>,
    pqc_verify_budget_ms: u64,
) -> Result<p2mr_utxo::P2mrUtxoBlockDiff, PqcValidationError> {
    if !activation.is_active(height) {
        return Ok(p2mr_utxo::compute_block_diff(block, chain_p2mr_utxos));
    }

    let mut pqc_budget = Some(PqcVerifyBudget::with_budget_ms(pqc_verify_budget_ms));
    let mut available_prevouts = chain_p2mr_utxos.clone();
    let mut spent = HashMap::new();
    let mut created = HashMap::new();
    let mut spent_p2mr_in_block = HashSet::new();
    seed_coinbase_p2mr_diff(block, &mut available_prevouts, &mut created);

    for tx in block.txdata.iter().skip(1) {
        validate_transaction(
            tx,
            height,
            activation,
            &available_prevouts,
            Some(chain_p2mr_utxos),
            Some(&spent_p2mr_in_block),
            &mut pqc_budget,
        )?;
        p2mr_utxo::apply_tx_to_validation_available(
            tx,
            &mut available_prevouts,
            Some((&mut spent, &mut created)),
            &mut spent_p2mr_in_block,
        );
    }

    Ok(p2mr_utxo::P2mrUtxoBlockDiff { spent, created })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{Amount, TxOut, transaction::Version};
    use std::time::Duration;

    fn p2mr_output_script() -> bitcoin::ScriptBuf {
        // 0x52 OP_2, 0x20 push 32, then 32 zero bytes
        let mut bytes = vec![0x52, 0x20];
        bytes.extend_from_slice(&[0u8; 32]);
        bitcoin::ScriptBuf::from_bytes(bytes)
    }

    #[test]
    fn inactive_height_skips_validation() {
        let tx = Transaction {
            version: Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(1000),
                script_pubkey: p2mr_output_script(),
            }],
        };
        let result = validate_transaction(
            &tx,
            0,
            Bip360Activation(1),
            &HashMap::new(),
            None,
            None,
            &mut None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn active_height_validates_p2mr_outputs() {
        let tx = Transaction {
            version: Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(1000),
                script_pubkey: p2mr_output_script(),
            }],
        };
        let result = validate_transaction(
            &tx,
            1,
            Bip360Activation(1),
            &HashMap::new(),
            None,
            None,
            &mut None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn truncated_v2_program_is_not_treated_as_p2mr() {
        let mut truncated_spk = vec![0x52, 0x20];
        truncated_spk.extend_from_slice(&[0x01; 31]); // 33 bytes total, not 34
        let tx = Transaction {
            version: Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(1000),
                script_pubkey: bitcoin::ScriptBuf::from_bytes(truncated_spk),
            }],
        };
        let result = validate_transaction(
            &tx,
            1,
            Bip360Activation(0),
            &HashMap::new(),
            None,
            None,
            &mut None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn block_validation_rejects_exhausted_pqc_budget() {
        use bitcoin::blockdata::opcodes::all::OP_CHECKSIG;
        use bitcoin::hashes::Hash as _;
        use bitcoin::key::{Keypair, Secp256k1};
        use bitcoin::secp256k1::rand::thread_rng;
        use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
        use bitcoin::taproot::{LeafVersion, TapLeafHash};
        use bitcoin::{ScriptBuf, Sequence, TxIn, Witness, XOnlyPublicKey};
        use bitcoin_p2mr_pqc::p2mr::P2MR_LEAF_VERSION;
        use bitcoin_p2mr_pqc::taproot::{LeafVersion as P2mrLeafVersion, TapNodeHash};

        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let leaf_script = bitcoin::blockdata::script::Builder::new()
            .push_slice(ec_pk)
            .push_opcode(OP_CHECKSIG)
            .into_script();
        let leaf_version =
            P2mrLeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("valid P2MR leaf version");
        let merkle_root = TapNodeHash::from_script(
            bitcoin_p2mr_pqc::Script::from_bytes(leaf_script.as_bytes()),
            leaf_version,
        )
        .to_byte_array();

        let mut spk = vec![0x52, 0x20];
        spk.extend_from_slice(&merkle_root);
        let prevout = TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: ScriptBuf::from_bytes(spk),
        };

        let tx = Transaction {
            version: Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![],
        };

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let prevouts_for_sighash = Prevouts::All(std::slice::from_ref(&prevout));
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_for_sighash,
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let sig = secp.sign_schnorr_no_aux_rand(
            &bitcoin::secp256k1::Message::from_digest(msg.to_byte_array()),
            &keypair,
        );
        let mut control_block = vec![0xc1];
        control_block.extend_from_slice(&[0u8; 32]);
        let mut witness = Witness::new();
        witness.push(sig.serialize());
        witness.push(leaf_script.as_bytes());
        witness.push(control_block);

        let mut tx = tx;
        tx.input[0].witness = witness;

        let mut budget = Some(PqcVerifyBudget::with_budget_ms(1));
        budget.as_mut().unwrap().record(Duration::from_millis(2));

        let prevouts = Prevouts::All(std::slice::from_ref(&prevout));
        let err =
            spend::validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts, &mut budget).unwrap_err();

        assert!(matches!(
            err,
            SpendError::Scheme(schemes::SchemeError::BlockVerifyBudgetExhausted)
        ));
    }

    #[test]
    fn block_validation_natural_pqc_budget_exceeded_on_slh_verify() {
        use bitcoin::blockdata::opcodes::all::OP_CHECKSIG;
        use bitcoin::hashes::Hash as _;
        use bitcoin::sighash::Prevouts;
        use bitcoin::taproot::{LeafVersion, TapLeafHash};
        use bitcoin::{ScriptBuf, Sequence, TxIn, Witness};
        use bitcoin_p2mr_pqc::p2mr::P2MR_LEAF_VERSION;
        use bitcoin_p2mr_pqc::taproot::{LeafVersion as P2mrLeafVersion, TapNodeHash};
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &[0x88; 128]).expect("SLH");
        let slh_pk: [u8; 32] = slh_kp
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("slh pk");
        let leaf_script = bitcoin::blockdata::script::Builder::new()
            .push_slice(slh_pk)
            .push_opcode(OP_CHECKSIG)
            .into_script();
        let leaf_version =
            P2mrLeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("valid P2MR leaf version");
        let merkle_root = TapNodeHash::from_script(
            bitcoin_p2mr_pqc::Script::from_bytes(leaf_script.as_bytes()),
            leaf_version,
        )
        .to_byte_array();

        let mut spk = vec![0x52, 0x20];
        spk.extend_from_slice(&merkle_root);
        let funding = Transaction {
            version: Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: ScriptBuf::from_bytes(spk),
            }],
        };
        let funding_txid = funding.compute_txid();
        let prevout = funding.output[0].clone();

        let spend_tx = Transaction {
            version: Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: funding_txid,
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![],
        };

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let prevouts_for_sighash = Prevouts::All(std::slice::from_ref(&prevout));
        let mut cache = bitcoin::sighash::SighashCache::new(&spend_tx);
        let sighash = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_for_sighash,
                leaf_hash,
                bitcoin::sighash::TapSighashType::All,
            )
            .expect("sighash");
        let slh_sig = sign(&slh_kp.secret_key, sighash.as_byte_array()).expect("slh sign");

        let mut control_block = vec![0xc1];
        control_block.extend_from_slice(&[0u8; 32]);
        let mut witness = Witness::new();
        witness.push(slh_sig.bytes);
        witness.push(leaf_script.as_bytes());
        witness.push(control_block);

        let mut spend_tx = spend_tx;
        spend_tx.input[0].witness = witness;

        let block = Block {
            header: bitcoin::block::Header {
                version: bitcoin::block::Version::ONE,
                prev_blockhash: bitcoin::BlockHash::from_byte_array([0u8; 32]),
                merkle_root: bitcoin::TxMerkleNode::from_byte_array([0u8; 32]),
                time: 0,
                bits: bitcoin::CompactTarget::from_consensus(0),
                nonce: 0,
            },
            txdata: vec![funding, spend_tx],
        };

        let err = validate_block_transactions(&block, 0, Bip360Activation(0), &HashMap::new(), 0)
            .unwrap_err();
        assert!(matches!(
            err,
            PqcValidationError::Transaction {
                source: SpendError::Scheme(schemes::SchemeError::PqcVerifyBudgetExceeded),
                ..
            }
        ));
    }

    #[test]
    fn cross_block_p2mr_spend_validates_with_chain_utxos() {
        use super::signer_dev::{SignAlgorithm, sign_p2mr_script_path_spend};
        use crate::validator::test_utils::test_block_header;
        use bitcoin::hashes::Hash as _;
        use bitcoin::sighash::TapSighashType;

        let signed = sign_p2mr_script_path_spend(
            SignAlgorithm::Schnorr,
            &[0x33; 32],
            TapSighashType::All,
            50_000,
            40_000,
        )
        .expect("sign");

        let funding_outpoint = OutPoint {
            txid: signed.funding_tx.compute_txid(),
            vout: 0,
        };
        let chain_utxos = HashMap::from([(funding_outpoint, signed.funding_tx.output[0].clone())]);

        let spend_block = Block {
            header: test_block_header(bitcoin::BlockHash::all_zeros()),
            txdata: vec![
                Transaction {
                    version: Version::TWO,
                    lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
                    input: vec![],
                    output: vec![],
                },
                signed.signed_spend_tx,
            ],
        };

        validate_block_transactions(&spend_block, 0, Bip360Activation(0), &chain_utxos, 0)
            .expect("cross-block P2MR spend should validate");
    }

    #[test]
    fn cross_block_p2mr_spend_rejects_bad_signature() {
        use super::signer_dev::{SignAlgorithm, sign_p2mr_script_path_spend};
        use crate::validator::test_utils::test_block_header;
        use bitcoin::hashes::Hash as _;
        use bitcoin::sighash::TapSighashType;

        let signed = sign_p2mr_script_path_spend(
            SignAlgorithm::Schnorr,
            &[0x44; 32],
            TapSighashType::All,
            50_000,
            40_000,
        )
        .expect("sign");

        let funding_outpoint = OutPoint {
            txid: signed.funding_tx.compute_txid(),
            vout: 0,
        };
        let chain_utxos = HashMap::from([(funding_outpoint, signed.funding_tx.output[0].clone())]);

        let mut bad_spend = signed.signed_spend_tx.clone();
        let witness = &bad_spend.input[0].witness;
        let mut bad_sig = witness.nth(0).unwrap().to_vec();
        if let Some(byte) = bad_sig.first_mut() {
            *byte ^= 0x01;
        }
        let mut bad_witness = bitcoin::Witness::new();
        bad_witness.push(bad_sig);
        bad_witness.push(witness.nth(1).unwrap());
        bad_witness.push(witness.nth(2).unwrap());
        bad_spend.input[0].witness = bad_witness;

        let spend_block = Block {
            header: test_block_header(bitcoin::BlockHash::all_zeros()),
            txdata: vec![
                Transaction {
                    version: Version::TWO,
                    lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
                    input: vec![],
                    output: vec![],
                },
                bad_spend,
            ],
        };

        let err =
            validate_block_transactions(&spend_block, 0, Bip360Activation(0), &chain_utxos, 0)
                .unwrap_err();
        assert!(matches!(
            err,
            PqcValidationError::Transaction {
                source: SpendError::Scheme(_),
                ..
            }
        ));
    }

    #[test]
    fn cross_block_p2mr_spend_no_longer_skipped_without_chain_utxos() {
        use super::signer_dev::{SignAlgorithm, sign_p2mr_script_path_spend};
        use crate::validator::test_utils::test_block_header;
        use bitcoin::hashes::Hash as _;
        use bitcoin::sighash::TapSighashType;

        let signed = sign_p2mr_script_path_spend(
            SignAlgorithm::Schnorr,
            &[0x55; 32],
            TapSighashType::All,
            50_000,
            40_000,
        )
        .expect("sign");

        let spend_block = Block {
            header: test_block_header(bitcoin::BlockHash::all_zeros()),
            txdata: vec![
                Transaction {
                    version: Version::TWO,
                    lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
                    input: vec![],
                    output: vec![],
                },
                signed.signed_spend_tx,
            ],
        };

        let funding_outpoint = OutPoint {
            txid: signed.funding_tx.compute_txid(),
            vout: 0,
        };
        let chain_utxos = HashMap::from([(funding_outpoint, signed.funding_tx.output[0].clone())]);

        // Without chain UTXOs the spend is skipped (legacy behavior).
        validate_block_transactions(&spend_block, 0, Bip360Activation(0), &HashMap::new(), 0)
            .expect("skipped without chain map");

        // With chain UTXOs the same spend is fully validated.
        validate_block_transactions(&spend_block, 0, Bip360Activation(0), &chain_utxos, 0)
            .expect("validated with chain map");
    }

    #[test]
    fn cross_block_p2mr_double_spend_in_block_rejects_missing_prevout() {
        use super::signer_dev::{SignAlgorithm, sign_p2mr_script_path_spend};
        use crate::validator::test_utils::test_block_header;
        use bitcoin::hashes::Hash as _;
        use bitcoin::sighash::TapSighashType;

        let signed = sign_p2mr_script_path_spend(
            SignAlgorithm::Schnorr,
            &[0x66; 32],
            TapSighashType::All,
            50_000,
            40_000,
        )
        .expect("sign");

        let funding_outpoint = OutPoint {
            txid: signed.funding_tx.compute_txid(),
            vout: 0,
        };
        let chain_utxos = HashMap::from([(funding_outpoint, signed.funding_tx.output[0].clone())]);

        let double_spend_block = Block {
            header: test_block_header(bitcoin::BlockHash::all_zeros()),
            txdata: vec![
                Transaction {
                    version: Version::TWO,
                    lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
                    input: vec![],
                    output: vec![],
                },
                signed.signed_spend_tx.clone(),
                signed.signed_spend_tx,
            ],
        };

        let err = validate_block_transactions(
            &double_spend_block,
            0,
            Bip360Activation(0),
            &chain_utxos,
            0,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            PqcValidationError::Transaction {
                source: SpendError::MissingPrevout { input_index: 0 },
                ..
            }
        ));
    }

    #[test]
    fn intra_block_p2mr_double_spend_rejects_with_missing_prevout() {
        use super::signer_dev::{SignAlgorithm, sign_p2mr_script_path_spend};
        use crate::validator::test_utils::test_block_header;
        use bitcoin::hashes::Hash as _;
        use bitcoin::sighash::TapSighashType;

        let signed = sign_p2mr_script_path_spend(
            SignAlgorithm::Schnorr,
            &[0x77; 32],
            TapSighashType::All,
            50_000,
            40_000,
        )
        .expect("sign");

        let intra_block_block = Block {
            header: test_block_header(bitcoin::BlockHash::all_zeros()),
            txdata: vec![
                Transaction {
                    version: Version::TWO,
                    lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
                    input: vec![],
                    output: vec![],
                },
                signed.funding_tx,
                signed.signed_spend_tx.clone(),
                signed.signed_spend_tx,
            ],
        };

        let err = validate_block_transactions(
            &intra_block_block,
            0,
            Bip360Activation(0),
            &HashMap::new(),
            0,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            PqcValidationError::Transaction {
                source: SpendError::MissingPrevout { input_index: 0 },
                ..
            }
        ));
    }
}
