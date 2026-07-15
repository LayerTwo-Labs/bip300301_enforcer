//! Size-gated signature scheme selection and verification (no OP_SUBSTR).

use std::borrow::Borrow;
use std::time::Instant;

use bitcoin::hashes::Hash as _;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::{Message, schnorr::Signature};
use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
use bitcoin::taproot::TapLeafHash;
use bitcoin::{Transaction, XOnlyPublicKey};
use thiserror::Error;

use super::limits::{
    ML_DSA_44_PUBLIC_KEY_SIZE, ML_DSA_44_SIGNATURE_SIZE, PQC_SIGNATURE_SIZE_TOLERANCE,
    PqcVerifyBudget, SCHNORR_SIGNATURE_SIZE, SLH_DSA_128S_SIGNATURE_SIZE, SLH_DSA_PUBLIC_KEY_SIZE,
};

/// Signature algorithm selected by witness element size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureScheme {
    Schnorr,
    MlDsa44,
    SlhDsaSha2128s,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SchemeError {
    #[error("unrecognized signature size: {size} bytes")]
    UnrecognizedSize { size: usize },
    #[error("invalid Schnorr signature")]
    InvalidSchnorr,
    #[error("invalid ML-DSA-44 signature")]
    InvalidMlDsa44,
    #[error("invalid SLH-DSA-SHA2-128s signature")]
    InvalidSlhDsaSha2128s,
    #[error("pubkey size {pubkey_size} incompatible with signature scheme {scheme:?}")]
    PubkeySchemeMismatch {
        pubkey_size: usize,
        scheme: SignatureScheme,
    },
    #[error("failed to compute tapscript sighash: {0}")]
    Sighash(String),
    #[error("per-block PQC verification budget exceeded during PQC verify")]
    PqcVerifyBudgetExceeded,
    #[error("per-block verification budget exhausted before checking signature")]
    BlockVerifyBudgetExhausted,
}

/// Parse tapscript sighash type from a witness signature element.
///
/// When the element is a known canonical signature size with no trailing byte, defaults to
/// [`TapSighashType::All`] (e.g. 64-byte Schnorr). When a valid trailing sighash byte is
/// present after a canonical signature body, that type is returned.
#[must_use]
pub fn parse_sighash_type(signature: &[u8]) -> TapSighashType {
    let size = signature.len();
    if size <= 1 {
        return TapSighashType::All;
    }
    let without_sighash = &signature[..size - 1];
    let last = signature[size - 1];
    if (without_sighash.len() == SCHNORR_SIGNATURE_SIZE
        || without_sighash.len() == ML_DSA_44_SIGNATURE_SIZE
        || without_sighash.len() == SLH_DSA_128S_SIGNATURE_SIZE)
        && let Ok(sighash_type) = TapSighashType::from_consensus_u8(last)
    {
        return sighash_type;
    }
    TapSighashType::All
}

/// Classify a witness signature element by byte length (optionally with trailing sighash byte).
pub fn classify_signature(signature: &[u8]) -> Result<SignatureScheme, SchemeError> {
    let size = strip_sighash_byte(signature).len();
    if size == SCHNORR_SIGNATURE_SIZE {
        return Ok(SignatureScheme::Schnorr);
    }
    if (size + PQC_SIGNATURE_SIZE_TOLERANCE >= ML_DSA_44_SIGNATURE_SIZE)
        && (size <= ML_DSA_44_SIGNATURE_SIZE + PQC_SIGNATURE_SIZE_TOLERANCE)
    {
        return Ok(SignatureScheme::MlDsa44);
    }
    if (size + PQC_SIGNATURE_SIZE_TOLERANCE >= SLH_DSA_128S_SIGNATURE_SIZE)
        && (size <= SLH_DSA_128S_SIGNATURE_SIZE + PQC_SIGNATURE_SIZE_TOLERANCE)
    {
        return Ok(SignatureScheme::SlhDsaSha2128s);
    }
    Err(SchemeError::UnrecognizedSize { size })
}

/// Expected public key size for a classified signature scheme.
#[must_use]
pub fn expected_pubkey_size(scheme: SignatureScheme) -> usize {
    match scheme {
        SignatureScheme::Schnorr | SignatureScheme::SlhDsaSha2128s => SLH_DSA_PUBLIC_KEY_SIZE,
        SignatureScheme::MlDsa44 => ML_DSA_44_PUBLIC_KEY_SIZE,
    }
}

/// Per-input tapscript verification context threaded through spend validation.
pub struct TapscriptVerifyContext<'a, T: Borrow<bitcoin::TxOut> + 'a> {
    pub tx: &'a Transaction,
    pub input_index: usize,
    pub prevouts: &'a Prevouts<'a, T>,
    pub leaf_hash: TapLeafHash,
    pub pqc_budget: &'a mut Option<PqcVerifyBudget>,
}

/// Check that `pubkey_bytes` length is valid for `scheme`.
pub fn validate_pubkey_for_scheme(
    pubkey_bytes: &[u8],
    scheme: SignatureScheme,
) -> Result<(), SchemeError> {
    let size = pubkey_bytes.len();
    let expected = expected_pubkey_size(scheme);
    if size == expected {
        Ok(())
    } else {
        Err(SchemeError::PubkeySchemeMismatch {
            pubkey_size: size,
            scheme,
        })
    }
}

/// Verify an overloaded OP_CHECKSIG using pubkey-size + signature-size duck typing.
pub fn verify_overloaded_checksig<T: Borrow<bitcoin::TxOut>>(
    pubkey_bytes: &[u8],
    signature: &[u8],
    ctx: &mut TapscriptVerifyContext<'_, T>,
) -> Result<(), SchemeError> {
    if let Some(budget) = ctx.pqc_budget.as_ref()
        && budget.elapsed() > budget.budget()
    {
        tracing::warn!(
            elapsed_ms = budget.elapsed().as_millis(),
            budget_ms = budget.budget().as_millis(),
            "per-block verification budget exhausted before signature check"
        );
        return Err(SchemeError::BlockVerifyBudgetExhausted);
    }
    let scheme = classify_signature(signature)?;
    validate_pubkey_for_scheme(pubkey_bytes, scheme)?;
    verify_tapscript_signature(scheme, signature, pubkey_bytes, ctx)
}

/// Verify a tapscript-path signature for the given leaf and input.
pub fn verify_tapscript_signature<T: Borrow<bitcoin::TxOut>>(
    scheme: SignatureScheme,
    signature: &[u8],
    pubkey_bytes: &[u8],
    ctx: &mut TapscriptVerifyContext<'_, T>,
) -> Result<(), SchemeError> {
    let sighash_type = parse_sighash_type(signature);
    let mut cache = SighashCache::new(ctx.tx);
    let sighash = cache
        .taproot_script_spend_signature_hash(
            ctx.input_index,
            ctx.prevouts,
            ctx.leaf_hash,
            sighash_type,
        )
        .map_err(|err| SchemeError::Sighash(err.to_string()))?;
    let message = Message::from_digest(sighash.to_byte_array());

    match scheme {
        SignatureScheme::Schnorr => verify_schnorr(signature, &message, pubkey_bytes),
        SignatureScheme::MlDsa44 => verify_ml_dsa_44(
            signature,
            sighash.as_byte_array(),
            pubkey_bytes,
            ctx.pqc_budget,
        ),
        SignatureScheme::SlhDsaSha2128s => verify_slh_dsa_sha2_128s(
            signature,
            sighash.as_byte_array(),
            pubkey_bytes,
            ctx.pqc_budget,
        ),
    }
}

fn strip_sighash_byte(signature: &[u8]) -> &[u8] {
    let size = signature.len();
    if size <= 1 {
        return signature;
    }
    let without_sighash = &signature[..size - 1];
    let last = signature[size - 1];
    if TapSighashType::from_consensus_u8(last).is_err() {
        return signature;
    }
    // Only strip when the prefix matches a known canonical signature size.
    if without_sighash.len() == SCHNORR_SIGNATURE_SIZE
        || without_sighash.len() == ML_DSA_44_SIGNATURE_SIZE
        || without_sighash.len() == SLH_DSA_128S_SIGNATURE_SIZE
    {
        return without_sighash;
    }
    signature
}

fn verify_schnorr(
    signature: &[u8],
    message: &Message,
    pubkey_bytes: &[u8],
) -> Result<(), SchemeError> {
    let sig_bytes = strip_sighash_byte(signature);
    let sig = Signature::from_slice(sig_bytes).map_err(|_| SchemeError::InvalidSchnorr)?;
    let pubkey =
        XOnlyPublicKey::from_slice(pubkey_bytes).map_err(|_| SchemeError::InvalidSchnorr)?;
    let secp = Secp256k1::verification_only();
    secp.verify_schnorr(&sig, message, &pubkey)
        .map_err(|_| SchemeError::InvalidSchnorr)
}

fn verify_ml_dsa_44(
    signature: &[u8],
    message: &[u8],
    pubkey_bytes: &[u8],
    pqc_budget: &mut Option<PqcVerifyBudget>,
) -> Result<(), SchemeError> {
    let sig_bytes = strip_sighash_byte(signature);
    verify_pqc(
        bitcoinpqc::Algorithm::ML_DSA_44,
        sig_bytes,
        message,
        pubkey_bytes,
        SchemeError::InvalidMlDsa44,
        pqc_budget,
    )
}

fn verify_slh_dsa_sha2_128s(
    signature: &[u8],
    message: &[u8],
    pubkey_bytes: &[u8],
    pqc_budget: &mut Option<PqcVerifyBudget>,
) -> Result<(), SchemeError> {
    let sig_bytes = strip_sighash_byte(signature);
    verify_pqc(
        bitcoinpqc::Algorithm::SLH_DSA_SHA2_128S,
        sig_bytes,
        message,
        pubkey_bytes,
        SchemeError::InvalidSlhDsaSha2128s,
        pqc_budget,
    )
}

fn verify_pqc(
    algorithm: bitcoinpqc::Algorithm,
    sig_bytes: &[u8],
    message: &[u8],
    pubkey_bytes: &[u8],
    invalid: SchemeError,
    pqc_budget: &mut Option<PqcVerifyBudget>,
) -> Result<(), SchemeError> {
    use bitcoinpqc::{PublicKey, Signature as PqcSignature, verify};

    let start = Instant::now();
    let result = (|| {
        let Ok(pqc_sig) = PqcSignature::try_from_slice(algorithm, sig_bytes) else {
            return Err(invalid);
        };
        let Ok(pqc_pk) = PublicKey::try_from_slice(algorithm, pubkey_bytes) else {
            return Err(invalid);
        };
        verify(&pqc_pk, message, &pqc_sig).map_err(|_| invalid)
    })();
    if let Some(budget) = pqc_budget.as_mut()
        && budget.record(start.elapsed())
    {
        tracing::warn!(
            elapsed_ms = budget.elapsed().as_millis(),
            budget_ms = budget.budget().as_millis(),
            "per-block PQC verification budget exceeded"
        );
        return Err(SchemeError::PqcVerifyBudgetExceeded);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Amount;
    use bitcoin::key::{Keypair, Secp256k1};
    use bitcoin::secp256k1::rand::thread_rng;
    use bitcoin::taproot::{LeafVersion, TapLeafHash};
    use bitcoin::{ScriptBuf, Sequence, TxIn, TxOut, Witness};
    use bitcoin_p2mr_pqc::p2mr::P2MR_LEAF_VERSION;

    #[test]
    fn classify_schnorr_size() {
        let sig = vec![0u8; 64];
        assert_eq!(classify_signature(&sig).unwrap(), SignatureScheme::Schnorr);
    }

    #[test]
    fn classify_schnorr_with_sighash_byte() {
        let mut sig = vec![0u8; 64];
        sig.push(TapSighashType::All as u8);
        assert_eq!(classify_signature(&sig).unwrap(), SignatureScheme::Schnorr);
    }

    #[test]
    fn parse_sighash_defaults_to_all_for_bare_schnorr() {
        let sig = vec![0u8; 64];
        assert_eq!(parse_sighash_type(&sig), TapSighashType::All);
    }

    #[test]
    fn parse_sighash_reads_trailing_byte() {
        let mut sig = vec![0u8; 64];
        sig.push(TapSighashType::Single as u8);
        assert_eq!(parse_sighash_type(&sig), TapSighashType::Single);
    }

    #[test]
    fn parse_sighash_matrix() {
        let cases = [
            (TapSighashType::None, TapSighashType::None as u8),
            (TapSighashType::Single, TapSighashType::Single as u8),
            (
                TapSighashType::SinglePlusAnyoneCanPay,
                TapSighashType::SinglePlusAnyoneCanPay as u8,
            ),
            (
                TapSighashType::NonePlusAnyoneCanPay,
                TapSighashType::NonePlusAnyoneCanPay as u8,
            ),
            (
                TapSighashType::AllPlusAnyoneCanPay,
                TapSighashType::AllPlusAnyoneCanPay as u8,
            ),
        ];
        for (expected, byte) in cases {
            let mut sig = vec![0u8; SCHNORR_SIGNATURE_SIZE];
            sig.push(byte);
            assert_eq!(parse_sighash_type(&sig), expected);
        }
    }

    #[test]
    fn classify_ml_dsa_size() {
        let sig = vec![0u8; ML_DSA_44_SIGNATURE_SIZE];
        assert_eq!(classify_signature(&sig).unwrap(), SignatureScheme::MlDsa44);
    }

    #[test]
    fn classify_slh_dsa_size() {
        let sig = vec![0u8; SLH_DSA_128S_SIGNATURE_SIZE];
        assert_eq!(
            classify_signature(&sig).unwrap(),
            SignatureScheme::SlhDsaSha2128s
        );
    }

    #[test]
    fn unrecognized_size_errors() {
        let sig = vec![0u8; 100];
        assert!(matches!(
            classify_signature(&sig),
            Err(SchemeError::UnrecognizedSize { size: 100 })
        ));
    }

    #[test]
    fn ml_dsa_wrong_pubkey_size_is_invalid() {
        let sig = vec![0u8; ML_DSA_44_SIGNATURE_SIZE];
        let mut budget = None;
        let err = verify_ml_dsa_44(&sig, b"msg", &[0u8; 32], &mut budget).unwrap_err();
        assert_eq!(err, SchemeError::InvalidMlDsa44);
    }

    #[test]
    fn slh_dsa_wrong_signature_is_invalid() {
        let sig = vec![0u8; SLH_DSA_128S_SIGNATURE_SIZE];
        let mut budget = None;
        let err =
            verify_slh_dsa_sha2_128s(&sig, b"msg", &[0u8; SLH_DSA_PUBLIC_KEY_SIZE], &mut budget)
                .unwrap_err();
        assert_eq!(err, SchemeError::InvalidSlhDsaSha2128s);
        assert!(
            err.to_string()
                .contains("invalid SLH-DSA-SHA2-128s signature"),
            "Display output must match integration trial reject log substring"
        );
    }

    #[test]
    fn overloaded_checksig_rejects_32byte_pk_with_mldsa_sig() {
        let err = validate_pubkey_for_scheme(&[0u8; 32], SignatureScheme::MlDsa44).unwrap_err();
        assert_eq!(
            err,
            SchemeError::PubkeySchemeMismatch {
                pubkey_size: 32,
                scheme: SignatureScheme::MlDsa44,
            }
        );
    }

    #[test]
    fn overloaded_checksig_accepts_matching_sizes() {
        assert!(validate_pubkey_for_scheme(&[0u8; 32], SignatureScheme::Schnorr).is_ok());
        assert!(validate_pubkey_for_scheme(&[0u8; 32], SignatureScheme::SlhDsaSha2128s).is_ok());
        assert!(
            validate_pubkey_for_scheme(&[0u8; ML_DSA_44_PUBLIC_KEY_SIZE], SignatureScheme::MlDsa44)
                .is_ok()
        );
    }

    #[test]
    fn ml_dsa_sign_verify_roundtrip() {
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let random_data = [0x42u8; 128];
        let keypair = generate_keypair(Algorithm::ML_DSA_44, &random_data).expect("ML-DSA keygen");
        let message = b"bip360 ml-dsa tapscript sighash";
        let signature = sign(&keypair.secret_key, message).expect("ML-DSA sign");
        let mut budget = None;
        verify_ml_dsa_44(
            &signature.bytes,
            message,
            &keypair.public_key.bytes,
            &mut budget,
        )
        .expect("ML-DSA verify");
    }

    #[test]
    fn slh_dsa_sha2_golden_vector_verifies() {
        // Vectors from libbitcoinpqc/tests/vectors/slh_dsa_sha2_128s_vectors.h
        const ENTROPY: &[u8] = &[
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29,
            0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37,
            0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45,
            0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x52, 0x53,
            0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x60, 0x61,
            0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f,
            0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d,
            0x7e, 0x7f,
        ];
        const MESSAGE: &[u8] = b"libbitcoinpqc SLH-DSA-SHA2-128s test message";
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let keypair = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, ENTROPY).expect("SLH keygen");
        let signature = sign(&keypair.secret_key, MESSAGE).expect("SLH sign");
        let mut budget = None;
        verify_slh_dsa_sha2_128s(
            &signature.bytes,
            MESSAGE,
            &keypair.public_key.bytes,
            &mut budget,
        )
        .expect("SLH-DSA-SHA2-128s verify");
    }

    fn multi_io_tx() -> (Transaction, TxOut, TxOut) {
        let prevout0 = TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: ScriptBuf::new(),
        };
        let prevout1 = TxOut {
            value: Amount::from_sat(40_000),
            script_pubkey: ScriptBuf::new(),
        };
        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![
                TxIn {
                    previous_output: bitcoin::OutPoint::null(),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                },
                TxIn {
                    previous_output: bitcoin::OutPoint::null(),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                },
            ],
            output: vec![
                TxOut {
                    value: Amount::from_sat(30_000),
                    script_pubkey: ScriptBuf::new(),
                },
                TxOut {
                    value: Amount::from_sat(20_000),
                    script_pubkey: ScriptBuf::new(),
                },
            ],
        };
        (tx, prevout0, prevout1)
    }

    fn append_sighash(mut sig: Vec<u8>, sighash_type: TapSighashType) -> Vec<u8> {
        if sighash_type != TapSighashType::All {
            sig.push(sighash_type as u8);
        }
        sig
    }

    fn verify_ctx<'a, T: Borrow<TxOut> + 'a>(
        tx: &'a Transaction,
        input_index: usize,
        prevouts: &'a Prevouts<'a, T>,
        leaf_hash: TapLeafHash,
        budget: &'a mut Option<PqcVerifyBudget>,
    ) -> TapscriptVerifyContext<'a, T> {
        TapscriptVerifyContext {
            tx,
            input_index,
            prevouts,
            leaf_hash,
            pqc_budget: budget,
        }
    }

    #[test]
    fn schnorr_sighash_matrix_overloaded_checksig() {
        use super::super::leaf_script::build_push32_checksig_leaf;
        use bitcoin::XOnlyPublicKey;

        let secp = Secp256k1::new();
        let keypair = Keypair::new(&secp, &mut thread_rng());
        let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&keypair).0.serialize();
        let leaf_script = build_push32_checksig_leaf(&ec_pk);
        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let (tx, prevout0, prevout1) = multi_io_tx();
        let prevouts = Prevouts::All(&[prevout0.clone(), prevout1]);

        let sighash_types = [
            TapSighashType::All,
            TapSighashType::None,
            TapSighashType::Single,
            TapSighashType::NonePlusAnyoneCanPay,
            TapSighashType::SinglePlusAnyoneCanPay,
            TapSighashType::AllPlusAnyoneCanPay,
        ];

        let mut budget = None;
        for sighash_type in sighash_types {
            let mut cache = SighashCache::new(&tx);
            let msg = cache
                .taproot_script_spend_signature_hash(0, &prevouts, leaf_hash, sighash_type)
                .expect("sighash");
            let sig =
                secp.sign_schnorr_no_aux_rand(&Message::from_digest(msg.to_byte_array()), &keypair);
            let witness_sig = append_sighash(sig.serialize().to_vec(), sighash_type);
            let mut ctx = verify_ctx(&tx, 0, &prevouts, leaf_hash, &mut budget);
            verify_overloaded_checksig(&ec_pk, &witness_sig, &mut ctx)
                .unwrap_or_else(|_| panic!("Schnorr verify failed for {sighash_type:?}"));
        }
    }

    #[test]
    fn ml_dsa_sighash_matrix_overloaded_checksig() {
        use super::super::leaf_script::build_mldsa_checksig_leaf;
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let keypair = generate_keypair(Algorithm::ML_DSA_44, &[0x33; 128]).expect("ML-DSA keygen");
        let leaf_script = build_mldsa_checksig_leaf(&keypair.public_key.bytes);
        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let (tx, prevout0, prevout1) = multi_io_tx();
        let prevouts = Prevouts::All(&[prevout0.clone(), prevout1]);

        let sighash_types = [
            TapSighashType::All,
            TapSighashType::None,
            TapSighashType::Single,
            TapSighashType::NonePlusAnyoneCanPay,
            TapSighashType::SinglePlusAnyoneCanPay,
            TapSighashType::AllPlusAnyoneCanPay,
        ];
        let mut budget = None;
        for sighash_type in sighash_types {
            let mut cache = SighashCache::new(&tx);
            let sighash = cache
                .taproot_script_spend_signature_hash(0, &prevouts, leaf_hash, sighash_type)
                .expect("sighash");
            let signature = sign(&keypair.secret_key, sighash.as_byte_array()).expect("sign");
            let witness_sig = append_sighash(signature.bytes.to_vec(), sighash_type);
            let mut ctx = verify_ctx(&tx, 0, &prevouts, leaf_hash, &mut budget);
            verify_overloaded_checksig(&keypair.public_key.bytes, &witness_sig, &mut ctx)
                .unwrap_or_else(|_| panic!("ML-DSA verify failed for {sighash_type:?}"));
        }
    }

    #[test]
    fn slh_dsa_sighash_matrix_overloaded_checksig() {
        use super::super::leaf_script::build_push32_checksig_leaf;
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let keypair =
            generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &[0x44; 128]).expect("SLH keygen");
        let slh_pk: [u8; 32] = keypair
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("32-byte pk");
        let leaf_script = build_push32_checksig_leaf(&slh_pk);
        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let (tx, prevout0, prevout1) = multi_io_tx();
        let prevouts = Prevouts::All(&[prevout0.clone(), prevout1]);

        let sighash_types = [
            TapSighashType::All,
            TapSighashType::None,
            TapSighashType::Single,
            TapSighashType::NonePlusAnyoneCanPay,
            TapSighashType::SinglePlusAnyoneCanPay,
            TapSighashType::AllPlusAnyoneCanPay,
        ];
        let mut budget = None;
        for sighash_type in sighash_types {
            let mut cache = SighashCache::new(&tx);
            let sighash = cache
                .taproot_script_spend_signature_hash(0, &prevouts, leaf_hash, sighash_type)
                .expect("sighash");
            let signature = sign(&keypair.secret_key, sighash.as_byte_array()).expect("sign");
            let witness_sig = append_sighash(signature.bytes.to_vec(), sighash_type);
            let mut ctx = verify_ctx(&tx, 0, &prevouts, leaf_hash, &mut budget);
            verify_overloaded_checksig(&slh_pk, &witness_sig, &mut ctx)
                .unwrap_or_else(|_| panic!("SLH verify failed for {sighash_type:?}"));
        }
    }

    #[test]
    fn bitcoinpqc_schnorr_signs_native_verify_overloaded_checksig() {
        use super::super::leaf_script::build_push32_checksig_leaf;
        use bitcoinpqc::{Algorithm, generate_keypair, sign};

        let keypair =
            generate_keypair(Algorithm::SECP256K1_SCHNORR, &[0x11; 32]).expect("Schnorr keygen");
        let ec_pk: [u8; 32] = keypair
            .public_key
            .bytes
            .as_slice()
            .try_into()
            .expect("32-byte x-only pk");
        let leaf_script = build_push32_checksig_leaf(&ec_pk);
        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let (tx, prevout0, prevout1) = multi_io_tx();
        let prevouts = Prevouts::All(&[prevout0.clone(), prevout1]);

        let sighash_type = TapSighashType::All;
        let mut cache = SighashCache::new(&tx);
        let sighash = cache
            .taproot_script_spend_signature_hash(0, &prevouts, leaf_hash, sighash_type)
            .expect("sighash");
        let signature =
            sign(&keypair.secret_key, sighash.as_byte_array()).expect("bitcoinpqc sign");
        let witness_sig = append_sighash(signature.bytes.to_vec(), sighash_type);

        let mut budget = None;
        let mut ctx = verify_ctx(&tx, 0, &prevouts, leaf_hash, &mut budget);
        verify_overloaded_checksig(&ec_pk, &witness_sig, &mut ctx)
            .expect("bitcoinpqc Schnorr signature verifies via native secp256k1");
    }

    #[test]
    fn pqc_verify_budget_exceeded_when_already_exhausted() {
        let mut budget = Some(PqcVerifyBudget::with_budget_ms(1));
        budget
            .as_mut()
            .unwrap()
            .record(std::time::Duration::from_millis(2));

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![],
        };
        let prevouts: Prevouts<'_, TxOut> = Prevouts::All(&[]);
        let mut ctx = verify_ctx(&tx, 0, &prevouts, TapLeafHash::all_zeros(), &mut budget);
        let err = verify_overloaded_checksig(&[0u8; 32], &[0u8; 64], &mut ctx).unwrap_err();
        assert_eq!(err, SchemeError::BlockVerifyBudgetExhausted);
    }
}
