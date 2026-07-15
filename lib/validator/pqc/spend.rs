//! P2MR script-path spend validation.

use std::borrow::Borrow;
use std::collections::HashMap;

use bitcoin::sighash::{Prevouts, TapSighashType};
use bitcoin::taproot::{LeafVersion, TapLeafHash};
use bitcoin::{OutPoint, Script, Transaction, TxOut};
use bitcoin_p2mr_pqc::p2mr::P2MR_LEAF_VERSION;
use thiserror::Error;

use super::leaf_script::{self, LeafScriptError};
use super::limits::{
    MAX_P2MR_LEAF_SCRIPT_SIZE, MAX_P2MR_WITNESS_STACK, MAX_PQC_SIG_WU_PER_INPUT, PqcVerifyBudget,
    estimate_sig_wu,
};
use super::merkle::{self, MerkleError};
use super::p2mr_output::{self, P2mrOutputError};
use super::schemes::{self, SchemeError};

#[derive(Debug, Error)]
pub enum SpendError {
    #[error("P2MR witness stack is empty")]
    EmptyWitness,
    #[error("P2MR witness stack exceeds maximum ({MAX_P2MR_WITNESS_STACK} elements)")]
    WitnessStackTooLarge,
    #[error("P2MR witness stack missing leaf script or control block")]
    IncompleteWitness,
    #[error(
        "P2MR witness signature weight {sig_wu} exceeds maximum ({MAX_PQC_SIG_WU_PER_INPUT} WU)"
    )]
    SignatureWeightExceeded { sig_wu: usize },
    #[error("P2MR leaf script exceeds maximum size")]
    LeafScriptTooLarge,
    #[error("invalid P2MR leaf version")]
    InvalidP2mrLeafVersion,
    #[error(transparent)]
    LeafScript(#[from] LeafScriptError),
    #[error("prevout not available for input {input_index}")]
    MissingPrevout { input_index: usize },
    #[error(transparent)]
    Output(#[from] P2mrOutputError),
    #[error(transparent)]
    Merkle(#[from] MerkleError),
    #[error(transparent)]
    Scheme(#[from] SchemeError),
}

fn p2mr_leaf_version() -> Result<LeafVersion, SpendError> {
    LeafVersion::from_consensus(P2MR_LEAF_VERSION).map_err(|_| SpendError::InvalidP2mrLeafVersion)
}

/// Validate a P2MR script-path spend for a single input.
pub fn validate_p2mr_input_spend<T: Borrow<TxOut>>(
    tx: &Transaction,
    input_index: usize,
    prevout: &TxOut,
    prevouts: &Prevouts<'_, T>,
    pqc_budget: &mut Option<PqcVerifyBudget>,
) -> Result<(), SpendError> {
    let input = tx
        .input
        .get(input_index)
        .ok_or(SpendError::MissingPrevout { input_index })?;
    let witness = &input.witness;
    if witness.is_empty() {
        return Err(SpendError::EmptyWitness);
    }
    if witness.len() > MAX_P2MR_WITNESS_STACK {
        return Err(SpendError::WitnessStackTooLarge);
    }

    let merkle_root = p2mr_output::validate_p2mr_output(prevout.script_pubkey.as_script())?;

    // Witness layout: [signatures...] <leaf_script> <control_block>
    let witness_len = witness.len();
    if witness_len < 2 {
        return Err(SpendError::IncompleteWitness);
    }
    let control_block_bytes = &witness[witness_len - 1];
    let leaf_script_bytes = &witness[witness_len - 2];
    if leaf_script_bytes.len() > MAX_P2MR_LEAF_SCRIPT_SIZE {
        return Err(SpendError::LeafScriptTooLarge);
    }

    let signature_count = witness_len - 2;
    if signature_count == 0 {
        return Err(SpendError::EmptyWitness);
    }

    let sig_wu: usize = witness
        .iter()
        .take(signature_count)
        .map(|sig| estimate_sig_wu(sig.len()))
        .sum();
    if sig_wu > MAX_PQC_SIG_WU_PER_INPUT {
        return Err(SpendError::SignatureWeightExceeded { sig_wu });
    }

    let leaf_script = Script::from_bytes(leaf_script_bytes);
    let parsed = leaf_script::validate_witness_sig_count(leaf_script, signature_count)?;
    merkle::verify_merkle_path(leaf_script, merkle_root, control_block_bytes)?;

    let leaf_hash = TapLeafHash::from_script(leaf_script, p2mr_leaf_version()?);

    let mut verify_ctx = schemes::TapscriptVerifyContext {
        tx,
        input_index,
        prevouts,
        leaf_hash,
        pqc_budget,
    };
    for (site, signature) in parsed
        .sig_sites
        .iter()
        .zip(witness.iter().take(signature_count))
    {
        schemes::verify_overloaded_checksig(&site.pubkey, signature, &mut verify_ctx)?;
    }
    Ok(())
}

/// Validate all outputs in `tx` that are P2MR.
pub fn validate_p2mr_outputs(tx: &Transaction) -> Result<(), SpendError> {
    for output in &tx.output {
        if p2mr_output::is_p2mr_script(output.script_pubkey.as_script()) {
            p2mr_output::validate_p2mr_output(output.script_pubkey.as_script())?;
        }
    }
    Ok(())
}

/// Build a map of outpoints to outputs from the given transactions (in order).
#[must_use]
pub fn utxo_map_from_transactions(txs: &[Transaction]) -> HashMap<OutPoint, TxOut> {
    let mut map = HashMap::new();
    for tx in txs {
        let txid = tx.compute_txid();
        for (vout, output) in tx.output.iter().enumerate() {
            map.insert(
                OutPoint {
                    txid,
                    vout: vout as u32,
                },
                output.clone(),
            );
        }
    }
    map
}

fn sighash_anyonecanpay(sighash_type: TapSighashType) -> bool {
    matches!(
        sighash_type,
        TapSighashType::AllPlusAnyoneCanPay
            | TapSighashType::NonePlusAnyoneCanPay
            | TapSighashType::SinglePlusAnyoneCanPay
    )
}

/// Build [`Prevouts`] for tapscript sighash when validating a P2MR input.
///
/// `ANYONECANPAY` family sighashes use `Prevouts::One`. Otherwise multi-input txs
/// require every input prevout in the map for `Prevouts::All`. Returns `None` when
/// a non-`ANYONECANPAY` sighash needs prevouts that are not available.
fn prevouts_for_p2mr_input<'a>(
    tx: &Transaction,
    input_index: usize,
    available_prevouts: &'a HashMap<OutPoint, TxOut>,
    owned_prevouts: &'a mut Vec<TxOut>,
    sighash_type: TapSighashType,
) -> Option<Prevouts<'a, TxOut>> {
    let prevout = available_prevouts.get(&tx.input[input_index].previous_output)?;

    if sighash_anyonecanpay(sighash_type) {
        return Some(Prevouts::One(input_index, prevout.clone()));
    }

    if tx.input.len() == 1 {
        return Some(Prevouts::All(std::slice::from_ref(prevout)));
    }

    owned_prevouts.clear();
    for input in &tx.input {
        let p = available_prevouts.get(&input.previous_output)?;
        owned_prevouts.push(p.clone());
    }
    Some(Prevouts::All(owned_prevouts))
}

/// Validate all P2MR-related rules for `tx` using `available_prevouts`.
///
/// Inputs whose prevout is not present in `available_prevouts` are skipped (e.g.
/// spends of confirmed non-P2MR UTXOs from earlier blocks). When
/// `chain_p2mr_utxos` is provided (block connect), a non-empty witness spend of a
/// known chain P2MR UTXO without an available prevout is rejected. The same
/// applies to outpoints already spent as P2MR earlier in the same block
/// (`spent_p2mr_in_block`).
pub fn validate_transaction_spends(
    tx: &Transaction,
    available_prevouts: &HashMap<OutPoint, TxOut>,
    chain_p2mr_utxos: Option<&HashMap<OutPoint, TxOut>>,
    spent_p2mr_in_block: Option<&std::collections::HashSet<OutPoint>>,
    pqc_budget: &mut Option<PqcVerifyBudget>,
) -> Result<(), SpendError> {
    validate_p2mr_outputs(tx)?;

    let mut owned_prevouts = Vec::new();
    for (input_index, input) in tx.input.iter().enumerate() {
        let prevout = match available_prevouts.get(&input.previous_output) {
            Some(prevout) => prevout,
            None => {
                if !input.witness.is_empty()
                    && (chain_p2mr_utxos
                        .is_some_and(|chain| chain.contains_key(&input.previous_output))
                        || spent_p2mr_in_block
                            .is_some_and(|spent| spent.contains(&input.previous_output)))
                {
                    return Err(SpendError::MissingPrevout { input_index });
                }
                continue;
            }
        };
        if !p2mr_output::is_p2mr_script(prevout.script_pubkey.as_script()) {
            continue;
        }
        if input.witness.is_empty() {
            return Err(SpendError::EmptyWitness);
        }

        let sighash_type = schemes::parse_sighash_type(input.witness.nth(0).unwrap_or(&[]));
        let Some(prevouts) = prevouts_for_p2mr_input(
            tx,
            input_index,
            available_prevouts,
            &mut owned_prevouts,
            sighash_type,
        ) else {
            continue;
        };
        validate_p2mr_input_spend(tx, input_index, prevout, &prevouts, pqc_budget)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::TxIn;
    use bitcoin::blockdata::opcodes::all::{OP_PUSHBYTES_32, OP_SUBSTR};
    use bitcoin::hashes::Hash as _;
    use bitcoin::key::{Keypair, Secp256k1};
    use bitcoin::secp256k1::Message;
    use bitcoin::secp256k1::rand::thread_rng;
    use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
    use bitcoin::taproot::{LeafVersion, TapLeafHash};
    use bitcoin::{Amount, ScriptBuf, Sequence, Witness, XOnlyPublicKey};
    use bitcoin_p2mr_pqc::p2mr::P2MR_LEAF_VERSION;
    use bitcoin_p2mr_pqc::taproot::{LeafVersion as P2mrLeafVersion, TapNodeHash};

    use super::super::leaf_script::{
        build_2of2_multisig_leaf, build_2of2_multisigverify_leaf,
        build_hybrid_ec_slh_checksigadd_leaf, build_hybrid_ec_slh_leaf,
        build_hybrid_ec_slh_multisig_leaf, build_mldsa_checksig_leaf, build_push32_checksig_leaf,
        build_push32_checksigadd_leaf, build_push32_checksigverify_leaf, parse_leaf_script,
    };
    use super::super::limits::ML_DSA_44_PUBLIC_KEY_SIZE;
    use bitcoinpqc::{Algorithm, generate_keypair, sign};

    fn dummy_prevout() -> TxOut {
        TxOut {
            value: Amount::from_sat(1000),
            script_pubkey: ScriptBuf::new(),
        }
    }

    fn prevouts_of(prevout: &TxOut) -> Prevouts<'_, TxOut> {
        Prevouts::All(std::slice::from_ref(prevout))
    }

    fn single_leaf_merkle_root(leaf_script: &ScriptBuf) -> [u8; 32] {
        let leaf_version =
            P2mrLeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("valid P2MR leaf version");
        TapNodeHash::from_script(
            bitcoin_p2mr_pqc::Script::from_bytes(leaf_script.as_bytes()),
            leaf_version,
        )
        .to_byte_array()
    }

    fn single_leaf_control_block() -> Vec<u8> {
        // P2MR decode expects TAPROOT_CONTROL_BASE_SIZE (33) bytes before the merkle branch;
        // single-leaf script path uses 0xc1 + 32-byte padding + empty branch.
        let mut control_block = vec![0xc1];
        control_block.extend_from_slice(&[0u8; 32]);
        control_block
    }

    fn p2mr_prevout(merkle_root: [u8; 32]) -> TxOut {
        let mut spk = vec![0x52, 0x20];
        spk.extend_from_slice(&merkle_root);
        TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: ScriptBuf::from_bytes(spk),
        }
    }

    fn multi_output_tx(prevout: &TxOut) -> Transaction {
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![
                TxOut {
                    value: Amount::from_sat(prevout.value.to_sat().saturating_sub(10_000)),
                    script_pubkey: ScriptBuf::new(),
                },
                TxOut {
                    value: Amount::from_sat(20_000),
                    script_pubkey: ScriptBuf::new(),
                },
            ],
        }
    }

    fn append_sighash(mut sig: Vec<u8>, sighash_type: TapSighashType) -> Vec<u8> {
        if sighash_type != TapSighashType::All {
            sig.push(sighash_type as u8);
        }
        sig
    }

    fn tapscript_sighash(
        tx: &Transaction,
        input_index: usize,
        leaf_script: &ScriptBuf,
        prevout: &TxOut,
        sighash_type: TapSighashType,
    ) -> [u8; 32] {
        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("valid leaf version"),
        );
        let mut cache = SighashCache::new(tx);
        cache
            .taproot_script_spend_signature_hash(
                input_index,
                &prevouts_of(prevout),
                leaf_hash,
                sighash_type,
            )
            .expect("sighash")
            .to_byte_array()
    }

    fn tapscript_sighash_all(
        tx: &Transaction,
        input_index: usize,
        leaf_script: &ScriptBuf,
        prevout: &TxOut,
    ) -> [u8; 32] {
        tapscript_sighash(tx, input_index, leaf_script, prevout, TapSighashType::All)
    }

    #[test]
    fn valid_leaf_script_format() {
        let script = build_push32_checksig_leaf(&[0xCD; 32]);
        parse_leaf_script(script.as_script()).unwrap();
    }

    #[test]
    fn rejects_op_substr_leaf() {
        let mut bytes = vec![OP_PUSHBYTES_32.to_u8()];
        bytes.extend_from_slice(&[0xAB; 32]);
        bytes.push(OP_SUBSTR.to_u8());
        let script = ScriptBuf::from_bytes(bytes);
        assert!(matches!(
            parse_leaf_script(script.as_script()),
            Err(LeafScriptError::OpSubstrForbidden)
        ));
    }

    #[test]
    fn skips_inputs_without_prevout_in_map() {
        let mut p2mr_spk = vec![0x52, 0x20];
        p2mr_spk.extend_from_slice(&[0xAB; 32]);
        let funding = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(1000),
                script_pubkey: ScriptBuf::from_bytes(p2mr_spk),
            }],
        };
        let spend = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: funding.compute_txid(),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(900),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        // No prevout in map: spend of external UTXO is skipped, not rejected.
        validate_transaction_spends(&spend, &HashMap::new(), None, None, &mut None).unwrap();
    }

    #[test]
    fn slh_only_leaf_roundtrip() {
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        const ENTROPY: &[u8] = &[0x42; 128];
        let keypair = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, ENTROPY).expect("SLH keygen");
        let leaf_script = build_push32_checksig_leaf(
            keypair
                .public_key
                .bytes
                .as_slice()
                .try_into()
                .expect("32-byte SLH pk"),
        );
        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };
        let prevout = dummy_prevout();
        let sighash = tapscript_sighash_all(&tx, 0, &leaf_script, &prevout);
        let signature = sign(&keypair.secret_key, &sighash).expect("SLH sign");
        let parsed = parse_leaf_script(leaf_script.as_script()).unwrap();
        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let prevouts = prevouts_of(&prevout);
        let mut budget = None;
        let mut verify_ctx = schemes::TapscriptVerifyContext {
            tx: &tx,
            input_index: 0,
            prevouts: &prevouts,
            leaf_hash,
            pqc_budget: &mut budget,
        };
        schemes::verify_overloaded_checksig(
            &parsed.sig_sites[0].pubkey,
            &signature.bytes,
            &mut verify_ctx,
        )
        .expect("SLH overloaded checksig");
    }

    #[test]
    fn mldsa_leaf_roundtrip() {
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let keypair = generate_keypair(Algorithm::ML_DSA_44, &[0x11; 128]).expect("ML-DSA keygen");
        assert_eq!(keypair.public_key.bytes.len(), ML_DSA_44_PUBLIC_KEY_SIZE);
        let leaf_script = build_mldsa_checksig_leaf(&keypair.public_key.bytes);
        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };
        let prevout = dummy_prevout();
        let sighash = tapscript_sighash_all(&tx, 0, &leaf_script, &prevout);
        let signature = sign(&keypair.secret_key, &sighash).expect("ML-DSA sign");
        let parsed = parse_leaf_script(leaf_script.as_script()).unwrap();
        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let prevouts = prevouts_of(&prevout);
        let mut budget = None;
        let mut verify_ctx = schemes::TapscriptVerifyContext {
            tx: &tx,
            input_index: 0,
            prevouts: &prevouts,
            leaf_hash,
            pqc_budget: &mut budget,
        };
        schemes::verify_overloaded_checksig(
            &parsed.sig_sites[0].pubkey,
            &signature.bytes,
            &mut verify_ctx,
        )
        .expect("ML-DSA overloaded checksig");
    }

    #[test]
    fn hybrid_ec_slh_leaf_roundtrip() {
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());

        let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &[0x55; 128]).expect("SLH");
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let slh_pk: [u8; 32] = slh_kp
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("slh pk");
        let leaf_script = build_hybrid_ec_slh_leaf(&ec_pk, &slh_pk);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };
        let prevout = dummy_prevout();
        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let sighash = tapscript_sighash_all(&tx, 0, &leaf_script, &prevout);

        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(&prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair);
        let slh_sig = sign(&slh_kp.secret_key, &sighash).expect("slh sign");

        let parsed = parse_leaf_script(leaf_script.as_script()).unwrap();
        let prevouts = prevouts_of(&prevout);
        let mut budget = None;
        let mut verify_ctx = schemes::TapscriptVerifyContext {
            tx: &tx,
            input_index: 0,
            prevouts: &prevouts,
            leaf_hash,
            pqc_budget: &mut budget,
        };
        schemes::verify_overloaded_checksig(
            &parsed.sig_sites[0].pubkey,
            &sig.serialize(),
            &mut verify_ctx,
        )
        .expect("ec sig");
        schemes::verify_overloaded_checksig(
            &parsed.sig_sites[1].pubkey,
            &slh_sig.bytes,
            &mut verify_ctx,
        )
        .expect("slh sig");
    }

    #[test]
    fn kitchen_sink_triple_algo_leaf_roundtrip() {
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let secp = Secp256k1::new();
        let ec_keypair = Keypair::new(&secp, &mut thread_rng());
        let mldsa_kp = generate_keypair(Algorithm::ML_DSA_44, &[0x44; 128]).expect("ML-DSA");
        let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &[0x55; 128]).expect("SLH");

        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&ec_keypair).0.serialize();
        let mldsa_pk = mldsa_kp.public_key.bytes.clone();
        let slh_pk: [u8; 32] = slh_kp
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("slh pk");

        let leaf_script =
            super::super::leaf_script::build_kitchen_sink_leaf(&ec_pk, &mldsa_pk, &slh_pk);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };
        let prevout = dummy_prevout();
        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let sighash = tapscript_sighash_all(&tx, 0, &leaf_script, &prevout);

        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(&prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let ec_sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &ec_keypair);
        let mldsa_sig = sign(&mldsa_kp.secret_key, &sighash).expect("mldsa sign");
        let slh_sig = sign(&slh_kp.secret_key, &sighash).expect("slh sign");

        let parsed = parse_leaf_script(leaf_script.as_script()).unwrap();
        assert_eq!(parsed.sig_sites.len(), 3);
        assert_eq!(parsed.sig_sites[0].pubkey.len(), 32);
        assert_eq!(parsed.sig_sites[1].pubkey.len(), ML_DSA_44_PUBLIC_KEY_SIZE);
        assert_eq!(parsed.sig_sites[2].pubkey.len(), 32);

        let prevouts = prevouts_of(&prevout);
        let mut budget = None;
        let mut verify_ctx = schemes::TapscriptVerifyContext {
            tx: &tx,
            input_index: 0,
            prevouts: &prevouts,
            leaf_hash,
            pqc_budget: &mut budget,
        };
        schemes::verify_overloaded_checksig(
            &parsed.sig_sites[0].pubkey,
            &ec_sig.serialize(),
            &mut verify_ctx,
        )
        .expect("ec sig");
        schemes::verify_overloaded_checksig(
            &parsed.sig_sites[1].pubkey,
            &mldsa_sig.bytes,
            &mut verify_ctx,
        )
        .expect("mldsa sig");
        schemes::verify_overloaded_checksig(
            &parsed.sig_sites[2].pubkey,
            &slh_sig.bytes,
            &mut verify_ctx,
        )
        .expect("slh sig");
    }

    fn kitchen_sink_spend_tx(
        ec_keypair: &Keypair,
        mldsa_kp: &bitcoinpqc::KeyPair,
        slh_kp: &bitcoinpqc::KeyPair,
        leaf_script: &bitcoin::ScriptBuf,
        prevout: &TxOut,
    ) -> Transaction {
        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };
        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let sighash = tapscript_sighash_all(&tx, 0, leaf_script, prevout);
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let secp = Secp256k1::new();
        let ec_sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), ec_keypair);
        let mldsa_sig = sign(&mldsa_kp.secret_key, &sighash).expect("mldsa sign");
        let slh_sig = sign(&slh_kp.secret_key, &sighash).expect("slh sign");

        let mut witness = Witness::new();
        witness.push(ec_sig.serialize());
        witness.push(mldsa_sig.bytes);
        witness.push(slh_sig.bytes);
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;
        tx
    }

    fn kitchen_sink_fixture() -> (
        Keypair,
        bitcoinpqc::KeyPair,
        bitcoinpqc::KeyPair,
        bitcoin::ScriptBuf,
        TxOut,
    ) {
        let secp = Secp256k1::new();
        let ec_keypair = Keypair::new(&secp, &mut thread_rng());
        let mldsa_kp = generate_keypair(Algorithm::ML_DSA_44, &[0x44; 128]).expect("ML-DSA");
        let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &[0x66; 128]).expect("SLH");
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&ec_keypair).0.serialize();
        let mldsa_pk = mldsa_kp.public_key.bytes.clone();
        let slh_pk: [u8; 32] = slh_kp
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("slh pk");
        let leaf_script =
            super::super::leaf_script::build_kitchen_sink_leaf(&ec_pk, &mldsa_pk, &slh_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);
        (ec_keypair, mldsa_kp, slh_kp, leaf_script, prevout)
    }

    #[test]
    fn kitchen_sink_validate_p2mr_input_spend_roundtrip() {
        let (ec_keypair, mldsa_kp, slh_kp, leaf_script, prevout) = kitchen_sink_fixture();
        let tx = kitchen_sink_spend_tx(&ec_keypair, &mldsa_kp, &slh_kp, &leaf_script, &prevout);

        assert_eq!(tx.input[0].witness.len(), 5);
        assert_eq!(tx.input[0].witness.nth(0).expect("ec sig").len(), 64);
        assert!(
            tx.input[0].witness.nth(1).expect("mldsa sig").len() > 2400,
            "expected ML-DSA signature size"
        );
        assert!(
            tx.input[0].witness.nth(2).expect("slh sig").len() > 7800,
            "expected SLH-DSA signature size"
        );

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .expect("kitchen-sink P2MR spend");
    }

    #[test]
    fn kitchen_sink_accepts_at_wu_boundary() {
        use super::super::limits::{
            ML_DSA_44_SIGNATURE_SIZE, SCHNORR_SIGNATURE_SIZE, SLH_DSA_128S_SIGNATURE_SIZE,
            estimate_sig_wu,
        };

        let (ec_keypair, mldsa_kp, slh_kp, leaf_script, prevout) = kitchen_sink_fixture();
        let tx = kitchen_sink_spend_tx(&ec_keypair, &mldsa_kp, &slh_kp, &leaf_script, &prevout);
        let witness = &tx.input[0].witness;
        let sig_wu: usize = (0..3)
            .map(|i| estimate_sig_wu(witness.nth(i).expect("sig").len()))
            .sum();
        assert_eq!(sig_wu, 10_340);
        assert_eq!(
            sig_wu,
            estimate_sig_wu(SCHNORR_SIGNATURE_SIZE)
                + estimate_sig_wu(ML_DSA_44_SIGNATURE_SIZE)
                + estimate_sig_wu(SLH_DSA_128S_SIGNATURE_SIZE)
        );

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .expect("kitchen-sink at WU boundary");
    }

    #[test]
    fn kitchen_sink_rejects_signature_weight_over_limit() {
        let (ec_keypair, mldsa_kp, slh_kp, leaf_script, prevout) = kitchen_sink_fixture();
        let mut tx = kitchen_sink_spend_tx(&ec_keypair, &mldsa_kp, &slh_kp, &leaf_script, &prevout);
        let witness = &tx.input[0].witness;
        let mut bad_witness = Witness::new();
        bad_witness.push(witness.nth(0).expect("ec sig"));
        bad_witness.push(witness.nth(1).expect("mldsa sig"));
        bad_witness.push(witness.nth(2).expect("slh sig"));
        bad_witness.push(vec![0u8; 2_000]);
        bad_witness.push(witness.nth(3).expect("leaf script"));
        bad_witness.push(witness.nth(4).expect("control block"));
        tx.input[0].witness = bad_witness;

        let err = validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .unwrap_err();
        assert!(matches!(err, SpendError::SignatureWeightExceeded { .. }));
    }

    #[test]
    fn kitchen_sink_rejects_wrong_witness_sig_count() {
        let (ec_keypair, mldsa_kp, slh_kp, leaf_script, prevout) = kitchen_sink_fixture();
        let mut tx = kitchen_sink_spend_tx(&ec_keypair, &mldsa_kp, &slh_kp, &leaf_script, &prevout);
        let witness = &tx.input[0].witness;
        let mut bad_witness = Witness::new();
        bad_witness.push(witness.nth(0).expect("ec sig"));
        bad_witness.push(witness.nth(1).expect("mldsa sig"));
        bad_witness.push(witness.nth(3).expect("leaf script"));
        bad_witness.push(witness.nth(4).expect("control block"));
        tx.input[0].witness = bad_witness;

        let err = validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not match script signature sites"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn kitchen_sink_rejects_tampered_ec_signature() {
        let (ec_keypair, mldsa_kp, slh_kp, leaf_script, prevout) = kitchen_sink_fixture();
        let mut tx = kitchen_sink_spend_tx(&ec_keypair, &mldsa_kp, &slh_kp, &leaf_script, &prevout);
        let witness = &tx.input[0].witness;
        let mut bad_ec_sig = witness.nth(0).expect("ec sig").to_vec();
        bad_ec_sig[0] ^= 0x01;
        let mut bad_witness = Witness::new();
        bad_witness.push(bad_ec_sig);
        bad_witness.push(witness.nth(1).expect("mldsa sig"));
        bad_witness.push(witness.nth(2).expect("slh sig"));
        bad_witness.push(witness.nth(3).expect("leaf script"));
        bad_witness.push(witness.nth(4).expect("control block"));
        tx.input[0].witness = bad_witness;

        let err = validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid Schnorr signature"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn kitchen_sink_rejects_tampered_mldsa_signature() {
        let (ec_keypair, mldsa_kp, slh_kp, leaf_script, prevout) = kitchen_sink_fixture();
        let mut tx = kitchen_sink_spend_tx(&ec_keypair, &mldsa_kp, &slh_kp, &leaf_script, &prevout);
        let witness = &tx.input[0].witness;
        let mut bad_mldsa_sig = witness.nth(1).expect("mldsa sig").to_vec();
        bad_mldsa_sig[0] ^= 0x01;
        let mut bad_witness = Witness::new();
        bad_witness.push(witness.nth(0).expect("ec sig"));
        bad_witness.push(bad_mldsa_sig);
        bad_witness.push(witness.nth(2).expect("slh sig"));
        bad_witness.push(witness.nth(3).expect("leaf script"));
        bad_witness.push(witness.nth(4).expect("control block"));
        tx.input[0].witness = bad_witness;

        let err = validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid ML-DSA-44 signature"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn kitchen_sink_rejects_tampered_slh_signature() {
        let (ec_keypair, mldsa_kp, slh_kp, leaf_script, prevout) = kitchen_sink_fixture();
        let mut tx = kitchen_sink_spend_tx(&ec_keypair, &mldsa_kp, &slh_kp, &leaf_script, &prevout);
        let witness = &tx.input[0].witness;
        let mut bad_slh_sig = witness.nth(2).expect("slh sig").to_vec();
        bad_slh_sig[0] ^= 0x01;
        let mut bad_witness = Witness::new();
        bad_witness.push(witness.nth(0).expect("ec sig"));
        bad_witness.push(witness.nth(1).expect("mldsa sig"));
        bad_witness.push(bad_slh_sig);
        bad_witness.push(witness.nth(3).expect("leaf script"));
        bad_witness.push(witness.nth(4).expect("control block"));
        tx.input[0].witness = bad_witness;

        let err = validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid SLH-DSA-SHA2-128s signature"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn kitchen_sink_rejects_swapped_ec_and_mldsa_signatures() {
        let (ec_keypair, mldsa_kp, slh_kp, leaf_script, prevout) = kitchen_sink_fixture();
        let mut tx = kitchen_sink_spend_tx(&ec_keypair, &mldsa_kp, &slh_kp, &leaf_script, &prevout);
        let witness = &tx.input[0].witness;
        let mut bad_witness = Witness::new();
        bad_witness.push(witness.nth(1).expect("mldsa sig"));
        bad_witness.push(witness.nth(0).expect("ec sig"));
        bad_witness.push(witness.nth(2).expect("slh sig"));
        bad_witness.push(witness.nth(3).expect("leaf script"));
        bad_witness.push(witness.nth(4).expect("control block"));
        tx.input[0].witness = bad_witness;

        let err = validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("invalid ML-DSA-44 signature")
                || err_msg.contains("incompatible with signature scheme"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_pubkey_sig_size_mismatch_via_scheme() {
        let err =
            schemes::validate_pubkey_for_scheme(&[0u8; 32], schemes::SignatureScheme::MlDsa44)
                .unwrap_err();
        assert!(matches!(err, SchemeError::PubkeySchemeMismatch { .. }));
    }

    #[test]
    fn schnorr_only_validate_p2mr_input_spend_roundtrip() {
        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let leaf_script = build_push32_checksig_leaf(&ec_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
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
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(&prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair);

        let mut witness = Witness::new();
        witness.push(sig.serialize());
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .expect("schnorr P2MR spend");
    }

    #[test]
    fn checksigverify_validate_p2mr_input_spend_roundtrip() {
        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let leaf_script = build_push32_checksigverify_leaf(&ec_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(&prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair);

        let mut witness = Witness::new();
        witness.push(sig.serialize());
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .expect("CHECKSIGVERIFY P2MR spend");
    }

    #[test]
    fn checksigadd_validate_p2mr_input_spend_roundtrip() {
        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let leaf_script = build_push32_checksigadd_leaf(&ec_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(&prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair);

        let mut witness = Witness::new();
        witness.push(sig.serialize());
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .expect("CHECKSIGADD P2MR spend");
    }

    #[test]
    fn multisig_validate_p2mr_input_spend_roundtrip() {
        let secp = Secp256k1::new();
        let keypair1 = Keypair::new(&secp, &mut thread_rng());
        let keypair2 = Keypair::new(&secp, &mut thread_rng());
        let pk1: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair1).0.serialize();
        let pk2: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair2).0.serialize();
        let leaf_script = build_2of2_multisig_leaf(&pk1, &pk2);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(&prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let sig1 =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair1);
        let sig2 =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair2);

        let mut witness = Witness::new();
        witness.push(sig1.serialize());
        witness.push(sig2.serialize());
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .expect("CHECKMULTISIG P2MR spend");
    }

    #[test]
    fn hybrid_ec_slh_validate_p2mr_input_spend_roundtrip() {
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &[0x77; 128]).expect("SLH");
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let slh_pk: [u8; 32] = slh_kp
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("slh pk");
        let leaf_script = build_hybrid_ec_slh_leaf(&ec_pk, &slh_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let sighash = tapscript_sighash_all(&tx, 0, &leaf_script, &prevout);
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(&prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let ec_sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair);
        let slh_sig = sign(&slh_kp.secret_key, &sighash).expect("slh sign");

        let mut witness = Witness::new();
        witness.push(ec_sig.serialize());
        witness.push(slh_sig.bytes);
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .expect("hybrid P2MR spend");
    }

    fn hybrid_ec_slh_spend_tx(
        ec_keypair: &Keypair,
        slh_kp: &bitcoinpqc::KeyPair,
        leaf_script: &bitcoin::ScriptBuf,
        prevout: &TxOut,
    ) -> Transaction {
        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };
        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let sighash = tapscript_sighash_all(&tx, 0, leaf_script, prevout);
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let secp = Secp256k1::new();
        let ec_sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), ec_keypair);
        let slh_sig = sign(&slh_kp.secret_key, &sighash).expect("slh sign");

        let mut witness = Witness::new();
        witness.push(ec_sig.serialize());
        witness.push(slh_sig.bytes);
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;
        tx
    }

    #[test]
    fn hybrid_ec_slh_rejects_tampered_ec_signature() {
        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &[0x77; 128]).expect("SLH");
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let slh_pk: [u8; 32] = slh_kp
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("slh pk");
        let leaf_script = build_hybrid_ec_slh_leaf(&ec_pk, &slh_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let mut tx = hybrid_ec_slh_spend_tx(&keypair, &slh_kp, &leaf_script, &prevout);
        let witness = &tx.input[0].witness;
        let mut bad_ec_sig = witness.nth(0).expect("ec sig").to_vec();
        bad_ec_sig[0] ^= 0x01;
        let mut bad_witness = Witness::new();
        bad_witness.push(bad_ec_sig);
        bad_witness.push(witness.nth(1).expect("slh sig"));
        bad_witness.push(witness.nth(2).expect("leaf script"));
        bad_witness.push(witness.nth(3).expect("control block"));
        tx.input[0].witness = bad_witness;

        let err = validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid Schnorr signature"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hybrid_ec_slh_rejects_tampered_slh_signature() {
        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &[0x77; 128]).expect("SLH");
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let slh_pk: [u8; 32] = slh_kp
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("slh pk");
        let leaf_script = build_hybrid_ec_slh_leaf(&ec_pk, &slh_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let mut tx = hybrid_ec_slh_spend_tx(&keypair, &slh_kp, &leaf_script, &prevout);
        let witness = &tx.input[0].witness;
        let mut bad_slh_sig = witness.nth(1).expect("slh sig").to_vec();
        bad_slh_sig[0] ^= 0x01;
        let mut bad_witness = Witness::new();
        bad_witness.push(witness.nth(0).expect("ec sig"));
        bad_witness.push(bad_slh_sig);
        bad_witness.push(witness.nth(2).expect("leaf script"));
        bad_witness.push(witness.nth(3).expect("control block"));
        tx.input[0].witness = bad_witness;

        let err = validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid SLH-DSA-SHA2-128s signature"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hybrid_ec_slh_rejects_swapped_signatures() {
        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &[0x77; 128]).expect("SLH");
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let slh_pk: [u8; 32] = slh_kp
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("slh pk");
        let leaf_script = build_hybrid_ec_slh_leaf(&ec_pk, &slh_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let mut tx = hybrid_ec_slh_spend_tx(&keypair, &slh_kp, &leaf_script, &prevout);
        let witness = &tx.input[0].witness;
        let mut bad_witness = Witness::new();
        bad_witness.push(witness.nth(1).expect("slh sig"));
        bad_witness.push(witness.nth(0).expect("ec sig"));
        bad_witness.push(witness.nth(2).expect("leaf script"));
        bad_witness.push(witness.nth(3).expect("control block"));
        tx.input[0].witness = bad_witness;

        let err = validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid SLH-DSA-SHA2-128s signature"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_oversized_witness_stack() {
        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let leaf_script = build_push32_checksig_leaf(&ec_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let mut witness = Witness::new();
        for _ in 0..super::super::limits::MAX_P2MR_WITNESS_STACK - 1 {
            witness.push([0u8; 64]);
        }
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness,
            }],
            output: vec![],
        };

        let err = validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .unwrap_err();
        assert!(matches!(err, SpendError::WitnessStackTooLarge));
    }

    #[test]
    fn rejects_signature_weight_over_limit() {
        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let leaf_script = build_push32_checksig_leaf(&ec_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let mut witness = Witness::new();
        witness.push(vec![
            0u8;
            super::super::limits::MAX_PQC_SIG_WU_PER_INPUT + 1
        ]);
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness,
            }],
            output: vec![],
        };

        let err = validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .unwrap_err();
        assert!(matches!(err, SpendError::SignatureWeightExceeded { .. }));
    }

    #[test]
    fn second_input_p2mr_spend_with_anyonecanpay_sighash() {
        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let leaf_script = build_push32_checksig_leaf(&ec_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default(), TxIn::default()],
            output: vec![],
        };

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let prevouts = Prevouts::One(1, &prevout);
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                1,
                &prevouts,
                leaf_hash,
                TapSighashType::AllPlusAnyoneCanPay,
            )
            .expect("sighash");
        let sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair);

        let mut witness = Witness::new();
        witness.push(append_sighash(
            sig.serialize().to_vec(),
            TapSighashType::AllPlusAnyoneCanPay,
        ));
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[1].witness = witness;

        validate_p2mr_input_spend(&tx, 1, &prevout, &prevouts, &mut None)
            .expect("input 1 P2MR spend with ACP sighash");
    }

    #[test]
    fn validate_transaction_spends_skips_external_input_validates_intra_block_p2mr_acp() {
        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let leaf_script = build_push32_checksig_leaf(&ec_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let funding = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![prevout.clone()],
        };
        let funding_outpoint = OutPoint {
            txid: funding.compute_txid(),
            vout: 0,
        };

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![
                TxIn {
                    previous_output: OutPoint::null(),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                },
                TxIn {
                    previous_output: funding_outpoint,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                },
            ],
            output: vec![],
        };

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let prevouts = Prevouts::One(1, &prevout);
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                1,
                &prevouts,
                leaf_hash,
                TapSighashType::AllPlusAnyoneCanPay,
            )
            .expect("sighash");
        let sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair);

        let mut witness = Witness::new();
        witness.push(append_sighash(
            sig.serialize().to_vec(),
            TapSighashType::AllPlusAnyoneCanPay,
        ));
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[1].witness = witness;

        let mut available_prevouts = HashMap::new();
        available_prevouts.insert(funding_outpoint, prevout);

        validate_transaction_spends(&tx, &available_prevouts, None, None, &mut None)
            .expect("input 1 P2MR spend via validate_transaction_spends");
    }

    #[test]
    fn schnorr_none_sighash_validate_p2mr_input_spend_roundtrip() {
        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let leaf_script = build_push32_checksig_leaf(&ec_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);
        let tx = multi_output_tx(&prevout);

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let prevouts = Prevouts::All(std::slice::from_ref(&prevout));
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(0, &prevouts, leaf_hash, TapSighashType::None)
            .expect("sighash");
        let sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair);

        let mut witness = Witness::new();
        witness.push(append_sighash(
            sig.serialize().to_vec(),
            TapSighashType::None,
        ));
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts, &mut None)
            .expect("SIGHASH_NONE P2MR spend");
    }

    #[test]
    fn multisigverify_validate_p2mr_input_spend_roundtrip() {
        let secp = Secp256k1::new();
        let keypair1 = Keypair::new(&secp, &mut thread_rng());
        let keypair2 = Keypair::new(&secp, &mut thread_rng());
        let pk1: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair1).0.serialize();
        let pk2: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair2).0.serialize();
        let leaf_script = build_2of2_multisigverify_leaf(&pk1, &pk2);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(&prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let sig1 =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair1);
        let sig2 =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair2);

        let mut witness = Witness::new();
        witness.push(sig1.serialize());
        witness.push(sig2.serialize());
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .expect("CHECKMULTISIGVERIFY P2MR spend");
    }

    #[test]
    fn hybrid_ec_slh_checksigadd_validate_p2mr_input_spend_roundtrip() {
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &[0x79; 128]).expect("SLH");
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let slh_pk: [u8; 32] = slh_kp
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("slh pk");
        let leaf_script = build_hybrid_ec_slh_checksigadd_leaf(&ec_pk, &slh_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let sighash = tapscript_sighash_all(&tx, 0, &leaf_script, &prevout);
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(&prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let ec_sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair);
        let slh_sig = sign(&slh_kp.secret_key, &sighash).expect("slh sign");

        let mut witness = Witness::new();
        witness.push(ec_sig.serialize());
        witness.push(slh_sig.bytes);
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .expect("hybrid CHECKSIGADD P2MR spend");
    }

    #[test]
    fn hybrid_ec_slh_multisig_validate_p2mr_input_spend_roundtrip() {
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &[0x7A; 128]).expect("SLH");
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let slh_pk: [u8; 32] = slh_kp
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("slh pk");
        let leaf_script = build_hybrid_ec_slh_multisig_leaf(&ec_pk, &slh_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let sighash = tapscript_sighash_all(&tx, 0, &leaf_script, &prevout);
        let mut cache = SighashCache::new(&tx);
        let msg = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(&prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let ec_sig =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair);
        let slh_sig = sign(&slh_kp.secret_key, &sighash).expect("slh sign");

        let mut witness = Witness::new();
        witness.push(ec_sig.serialize());
        witness.push(slh_sig.bytes);
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .expect("hybrid multisig P2MR spend");
    }

    #[test]
    fn mldsa_multisig_validate_p2mr_input_spend_roundtrip() {
        use bitcoin::blockdata::opcodes::all::{OP_CHECKMULTISIG, OP_PUSHBYTES_0, OP_PUSHNUM_2};
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let mldsa_kp1 = generate_keypair(Algorithm::ML_DSA_44, &[0x7B; 128]).expect("ML-DSA");
        let mldsa_kp2 = generate_keypair(Algorithm::ML_DSA_44, &[0x7C; 128]).expect("ML-DSA");

        let mut leaf_bytes = Vec::new();
        leaf_bytes.push(OP_PUSHBYTES_0.to_u8());
        for pk in [&mldsa_kp1.public_key.bytes, &mldsa_kp2.public_key.bytes] {
            leaf_bytes.push(0x4d); // OP_PUSHDATA2
            leaf_bytes.extend_from_slice(&(pk.len() as u16).to_le_bytes());
            leaf_bytes.extend_from_slice(pk);
        }
        leaf_bytes.push(OP_PUSHNUM_2.to_u8());
        leaf_bytes.push(OP_CHECKMULTISIG.to_u8());
        let leaf_script = ScriptBuf::from_bytes(leaf_bytes);

        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        };

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let mut cache = SighashCache::new(&tx);
        let sighash = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts_of(&prevout),
                leaf_hash,
                TapSighashType::All,
            )
            .expect("sighash");
        let sig1 = sign(&mldsa_kp1.secret_key, sighash.as_byte_array()).expect("mldsa sign");
        let sig2 = sign(&mldsa_kp2.secret_key, sighash.as_byte_array()).expect("mldsa sign");

        let mut witness = Witness::new();
        witness.push(sig1.bytes);
        witness.push(sig2.bytes);
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts_of(&prevout), &mut None)
            .expect("ML-DSA CHECKMULTISIG P2MR spend");
    }

    #[test]
    fn slh_single_plus_anyonecanpay_validate_p2mr_input_spend_roundtrip() {
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &[0x7D; 128]).expect("SLH");
        let slh_pk: [u8; 32] = slh_kp
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("slh pk");
        let leaf_script = build_push32_checksig_leaf(&slh_pk);
        let merkle_root = single_leaf_merkle_root(&leaf_script);
        let prevout = p2mr_prevout(merkle_root);
        let tx = multi_output_tx(&prevout);

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let prevouts = Prevouts::All(std::slice::from_ref(&prevout));
        let mut cache = SighashCache::new(&tx);
        let sighash = cache
            .taproot_script_spend_signature_hash(
                0,
                &prevouts,
                leaf_hash,
                TapSighashType::SinglePlusAnyoneCanPay,
            )
            .expect("sighash");
        let signature = sign(&slh_kp.secret_key, sighash.as_byte_array()).expect("slh sign");

        let mut witness = Witness::new();
        witness.push(append_sighash(
            signature.bytes.to_vec(),
            TapSighashType::SinglePlusAnyoneCanPay,
        ));
        witness.push(leaf_script.as_bytes());
        witness.push(single_leaf_control_block());

        let mut tx = tx;
        tx.input[0].witness = witness;

        validate_p2mr_input_spend(&tx, 0, &prevout, &prevouts, &mut None)
            .expect("SLH SIGHASH_SINGLE|ACP P2MR spend");
    }

    #[test]
    fn p2mr_signer_roundtrip_schnorr() {
        use super::super::signer_dev::{SignAlgorithm, sign_p2mr_script_path_spend};
        use bitcoin::sighash::TapSighashType;

        let signed = sign_p2mr_script_path_spend(
            SignAlgorithm::Schnorr,
            &[0x11; 32],
            TapSighashType::All,
            50_000,
            40_000,
        )
        .expect("signer schnorr");

        let utxo_map = utxo_map_from_transactions(&[signed.funding_tx]);
        validate_transaction_spends(&signed.signed_spend_tx, &utxo_map, None, None, &mut None)
            .expect("signer schnorr spend validates");
    }

    #[test]
    fn p2mr_signer_roundtrip_mldsa() {
        use super::super::signer_dev::{SignAlgorithm, sign_p2mr_script_path_spend};
        use bitcoin::sighash::TapSighashType;

        let signed = sign_p2mr_script_path_spend(
            SignAlgorithm::Mldsa,
            &[0x22; 128],
            TapSighashType::All,
            50_000,
            40_000,
        )
        .expect("signer mldsa");

        let utxo_map = utxo_map_from_transactions(&[signed.funding_tx]);
        validate_transaction_spends(&signed.signed_spend_tx, &utxo_map, None, None, &mut None)
            .expect("signer mldsa spend validates");
    }

    #[test]
    fn p2mr_signer_roundtrip_slh() {
        use super::super::signer_dev::{SignAlgorithm, sign_p2mr_script_path_spend};
        use bitcoin::sighash::TapSighashType;

        let signed = sign_p2mr_script_path_spend(
            SignAlgorithm::Slh,
            &[0x42; 128],
            TapSighashType::All,
            50_000,
            40_000,
        )
        .expect("signer slh");

        let utxo_map = utxo_map_from_transactions(&[signed.funding_tx]);
        validate_transaction_spends(&signed.signed_spend_tx, &utxo_map, None, None, &mut None)
            .expect("signer slh spend validates");
    }

    #[test]
    fn p2mr_signer_roundtrip_sighash_none() {
        use super::super::signer_dev::{SignAlgorithm, sign_p2mr_script_path_spend};
        use bitcoin::sighash::TapSighashType;

        let signed = sign_p2mr_script_path_spend(
            SignAlgorithm::Slh,
            &[0x42; 128],
            TapSighashType::None,
            50_000,
            40_000,
        )
        .expect("signer slh none");

        let utxo_map = utxo_map_from_transactions(&[signed.funding_tx]);
        validate_transaction_spends(&signed.signed_spend_tx, &utxo_map, None, None, &mut None)
            .expect("signer non-ALL sighash spend validates");
    }
}
