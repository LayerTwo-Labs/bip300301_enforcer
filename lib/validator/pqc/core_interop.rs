//! **Interop / not enforcer-canonical** — P2MR Core mempool hybrid leaf builders.
//!
//! Cryptoquick P2MR Core hybrid (combined) leaf is ~70 bytes:
//!
//! ```text
//! PUSH32 <schnorr_pk> OP_CHECKSIG PUSH32 <slh_pk> OP_SUBSTR OP_BOOLAND OP_VERIFY
//! ```
//!
//! Core treats `OP_SUBSTR` (`0x7f` / OP_SUCCESS127) as the SLH-DSA path. The enforcer
//! **rejects** `OP_SUBSTR` in its own verify path (`OpSubstrForbidden`) — that is fine
//! for **Bob-only** mempool trials (TB-sendraw shape 2 asserts Bob `sendraw` +
//! `getmempoolentry`, not Alice tip keep of that spend).
//!
//! Full triple-algo kitchen-sink (Schnorr+ML-DSA+SLH, all `OP_CHECKSIG`) remains the
//! tip/CUSF product shape. True multi-algo large-push kitchen-sink on Bob still needs
//! **3B** (Core push-size / multi-algo) — do not claim 3B done.
//!
//! # Witness (Core legacy combined stack)
//!
//! `[ec_sig (64 B bare Default), slh_sig (7856 B, All preimage bare), leaf, control c1]`
//!
//! Core's OP_SUCCESS127 pre-scan matches `stack[0]==64 && stack[1]==7856` and verifies
//! SLH with **SIGHASH_ALL** when the SLH element has no trailing type byte.

use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness, XOnlyPublicKey,
    blockdata::{
        opcodes::all::{OP_BOOLAND, OP_CHECKSIG, OP_SUBSTR, OP_VERIFY},
        script::Builder,
    },
    hashes::Hash as _,
    key::Secp256k1,
    secp256k1::{Keypair, Message, SecretKey},
    sighash::{Prevouts, SighashCache, TapSighashType},
    taproot::{LeafVersion, TapLeafHash},
    transaction::Version,
};
use bitcoin_p2mr_pqc::p2mr::P2MR_LEAF_VERSION;
use bitcoinpqc::{Algorithm, generate_keypair, sign};

use super::{
    limits::SLH_DSA_128S_SIGNATURE_SIZE,
    signer_dev::{
        p2mr_script_pubkey, single_leaf_control_block, single_leaf_merkle_root,
        validate_hybrid_entropy, validate_output_value,
    },
};

/// Core combined hybrid leaf size in bytes (`PUSH32 pk CHECKSIG PUSH32 pk SUBSTR BOOLAND VERIFY`).
pub const CORE_HYBRID_LEAF_LEN: usize = 70;

/// Build Core-legal hybrid EC+SLH leaf (OP_SUBSTR second site). **Not enforcer-canonical.**
///
/// Layout (70 B):
/// `OP_PUSHBYTES_32 <ec_pk> OP_CHECKSIG OP_PUSHBYTES_32 <slh_pk> OP_SUBSTR OP_BOOLAND OP_VERIFY`
#[must_use]
pub fn build_core_hybrid_ec_slh_leaf(ec_pk: &[u8; 32], slh_pk: &[u8; 32]) -> ScriptBuf {
    Builder::new()
        .push_slice(ec_pk)
        .push_opcode(OP_CHECKSIG)
        .push_slice(slh_pk)
        .push_opcode(OP_SUBSTR)
        .push_opcode(OP_BOOLAND)
        .push_opcode(OP_VERIFY)
        .into_script()
}

fn ec_keypair_from_entropy(ec_entropy: &[u8; 32]) -> Result<Keypair, String> {
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(ec_entropy).map_err(|e| format!("invalid EC entropy: {e}"))?;
    Ok(Keypair::from_secret_key(&secp, &sk))
}

fn slh_pk32(keypair: &bitcoinpqc::KeyPair) -> Result<[u8; 32], String> {
    keypair.public_key.bytes.as_slice().try_into().map_err(|_| {
        format!(
            "expected 32-byte SLH pubkey, got {} bytes",
            keypair.public_key.bytes.len()
        )
    })
}

/// P2MR `scriptPubKey` + leaf for Core hybrid OP_SUBSTR shape (interop only).
pub fn p2mr_output_for_core_hybrid_ec_slh(
    ec_entropy: &[u8; 32],
    slh_entropy: &[u8],
) -> Result<(ScriptBuf, ScriptBuf), String> {
    validate_hybrid_entropy(ec_entropy, slh_entropy)?;
    let ec_keypair = ec_keypair_from_entropy(ec_entropy)?;
    let slh_keypair = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, slh_entropy)
        .map_err(|e| format!("SLH key generation failed: {e}"))?;
    let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&ec_keypair).0.serialize();
    let slh_pk = slh_pk32(&slh_keypair)?;
    let leaf_script = build_core_hybrid_ec_slh_leaf(&ec_pk, &slh_pk);
    let script_pubkey = p2mr_script_pubkey(single_leaf_merkle_root(&leaf_script));
    Ok((script_pubkey, leaf_script))
}

/// Sign a Core-legal hybrid EC+SLH spend from a confirmed prevout (**Bob interop only**).
///
/// - EC: [`TapSighashType::Default`] → bare 64 B (Core size-2 stack pattern)
/// - SLH: signed under [`TapSighashType::All`] preimage, emitted bare 7856 B (Core
///   combined-script path forces `SIGHASH_ALL` when no trailing type byte)
pub fn build_core_hybrid_ec_slh_spend_from_prevout(
    ec_entropy: &[u8; 32],
    slh_entropy: &[u8],
    spend_previous_output: OutPoint,
    prevout: TxOut,
    output_value: u64,
    spend_destination: ScriptBuf,
) -> Result<Transaction, String> {
    validate_hybrid_entropy(ec_entropy, slh_entropy)?;
    validate_output_value(output_value, prevout.value.to_sat())?;

    let ec_keypair = ec_keypair_from_entropy(ec_entropy)?;
    let slh_keypair = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, slh_entropy)
        .map_err(|e| format!("SLH key generation failed: {e}"))?;

    let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&ec_keypair).0.serialize();
    let slh_pk = slh_pk32(&slh_keypair)?;
    let leaf_script = build_core_hybrid_ec_slh_leaf(&ec_pk, &slh_pk);
    debug_assert_eq!(leaf_script.len(), CORE_HYBRID_LEAF_LEN);

    let leaf_hash = TapLeafHash::from_script(
        leaf_script.as_script(),
        LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("valid leaf version"),
    );

    let unsigned_spend_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: spend_previous_output,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(output_value),
            script_pubkey: spend_destination,
        }],
    };

    let prevouts = Prevouts::All(std::slice::from_ref(&prevout));
    let mut cache = SighashCache::new(&unsigned_spend_tx);

    // EC: Default → bare 64 (matches Core size-2 stack[0]==64).
    let ec_sighash = cache
        .taproot_script_spend_signature_hash(0, &prevouts, leaf_hash, TapSighashType::Default)
        .map_err(|e| format!("EC sighash failed: {e}"))?;
    let secp = Secp256k1::new();
    let ec_sig = secp.sign_schnorr_no_aux_rand(
        &Message::from_digest(ec_sighash.to_byte_array()),
        &ec_keypair,
    );

    // SLH: All preimage, bare 7856 (Core combined path uses SIGHASH_ALL for bare SLH).
    let slh_sighash = cache
        .taproot_script_spend_signature_hash(0, &prevouts, leaf_hash, TapSighashType::All)
        .map_err(|e| format!("SLH sighash failed: {e}"))?;
    let slh_sig = sign(&slh_keypair.secret_key, slh_sighash.as_byte_array())
        .map_err(|e| format!("SLH signing failed: {e}"))?;
    if slh_sig.bytes.len() != SLH_DSA_128S_SIGNATURE_SIZE {
        return Err(format!(
            "expected SLH sig {SLH_DSA_128S_SIGNATURE_SIZE} bytes, got {}",
            slh_sig.bytes.len()
        ));
    }

    let mut witness = Witness::new();
    // Do not append sighash bytes: Core size-2 path requires exactly 64 and 7856.
    witness.push(ec_sig.serialize());
    witness.push(slh_sig.bytes);
    witness.push(leaf_script.as_bytes());
    witness.push(single_leaf_control_block());

    let mut signed_spend_tx = unsigned_spend_tx;
    signed_spend_tx.input[0].witness = witness;
    Ok(signed_spend_tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator::pqc::leaf_script::{LeafScriptError, parse_leaf_script};

    /// Fixed entropy matching integration harness hybrid EC / SLH constants.
    const EC_ENTROPY: [u8; 32] = [0x33; 32];
    const SLH_ENTROPY: [u8; 128] = [0x88; 128];

    #[test]
    fn core_hybrid_leaf_is_70_bytes_with_op_substr() {
        let leaf = build_core_hybrid_ec_slh_leaf(&[0x11; 32], &[0x22; 32]);
        assert_eq!(leaf.len(), CORE_HYBRID_LEAF_LEN);
        let bytes = leaf.as_bytes();
        // PUSH32 ec_pk
        assert_eq!(bytes[0], 0x20);
        assert_eq!(&bytes[1..33], &[0x11; 32]);
        // OP_CHECKSIG
        assert_eq!(bytes[33], OP_CHECKSIG.to_u8());
        // PUSH32 slh_pk
        assert_eq!(bytes[34], 0x20);
        assert_eq!(&bytes[35..67], &[0x22; 32]);
        // OP_SUBSTR OP_BOOLAND OP_VERIFY
        assert_eq!(bytes[67], OP_SUBSTR.to_u8());
        assert_eq!(bytes[67], 0x7f);
        assert_eq!(bytes[68], OP_BOOLAND.to_u8());
        assert_eq!(bytes[69], OP_VERIFY.to_u8());
    }

    #[test]
    fn core_hybrid_leaf_rejected_by_enforcer_parse() {
        let leaf = build_core_hybrid_ec_slh_leaf(&[0xAA; 32], &[0xBB; 32]);
        assert_eq!(
            parse_leaf_script(leaf.as_script()),
            Err(LeafScriptError::OpSubstrForbidden),
            "enforcer must still reject OP_SUBSTR; Core interop is Bob-only"
        );
    }

    #[test]
    fn core_hybrid_output_deterministic_spk() {
        let (spk_a, leaf_a) =
            p2mr_output_for_core_hybrid_ec_slh(&EC_ENTROPY, &SLH_ENTROPY).expect("a");
        let (spk_b, leaf_b) =
            p2mr_output_for_core_hybrid_ec_slh(&EC_ENTROPY, &SLH_ENTROPY).expect("b");
        assert_eq!(spk_a, spk_b);
        assert_eq!(leaf_a, leaf_b);
        assert_eq!(leaf_a.len(), CORE_HYBRID_LEAF_LEN);
        assert_eq!(spk_a.as_bytes()[0..2], [0x52, 0x20]);
        assert_eq!(single_leaf_merkle_root(&leaf_a), {
            let mut root = [0u8; 32];
            root.copy_from_slice(&spk_a.as_bytes()[2..34]);
            root
        });
    }

    #[test]
    fn core_hybrid_spend_witness_matches_core_size_pattern() {
        let (spk, leaf_script) =
            p2mr_output_for_core_hybrid_ec_slh(&EC_ENTROPY, &SLH_ENTROPY).expect("spk");
        let funding = Transaction {
            version: Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: spk,
            }],
        };
        let prevout = funding.output[0].clone();
        let outpoint = OutPoint {
            txid: funding.compute_txid(),
            vout: 0,
        };
        let spend = build_core_hybrid_ec_slh_spend_from_prevout(
            &EC_ENTROPY,
            &SLH_ENTROPY,
            outpoint,
            prevout.clone(),
            40_000,
            ScriptBuf::new(),
        )
        .expect("spend");

        let w = &spend.input[0].witness;
        assert_eq!(w.len(), 4, "ec_sig, slh_sig, leaf, control");
        assert_eq!(w.nth(0).expect("ec").len(), 64, "bare Default Schnorr");
        assert_eq!(
            w.nth(1).expect("slh").len(),
            SLH_DSA_128S_SIGNATURE_SIZE,
            "bare SLH (All preimage, no trailing type byte)"
        );
        assert_eq!(w.nth(2).expect("leaf").len(), CORE_HYBRID_LEAF_LEN);
        assert_eq!(w.nth(3).expect("control"), &[0xc1]);
        // Leaf ends with SUBSTR BOOLAND VERIFY
        let leaf = w.nth(2).unwrap();
        assert_eq!(
            &leaf[leaf.len() - 3..],
            &[0x7f, OP_BOOLAND.to_u8(), OP_VERIFY.to_u8()]
        );

        // Pin EC Default vs SLH All preimage split (not merely sizes).
        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("leaf version"),
        );
        let prevouts = Prevouts::All(std::slice::from_ref(&prevout));
        let mut cache = SighashCache::new(&spend);
        let ec_msg = cache
            .taproot_script_spend_signature_hash(0, &prevouts, leaf_hash, TapSighashType::Default)
            .expect("ec Default sighash");
        let slh_msg = cache
            .taproot_script_spend_signature_hash(0, &prevouts, leaf_hash, TapSighashType::All)
            .expect("slh All sighash");
        assert_ne!(
            ec_msg.to_byte_array(),
            slh_msg.to_byte_array(),
            "EC Default and SLH All must use different TapSighash digests"
        );

        let ec_kp = ec_keypair_from_entropy(&EC_ENTROPY).expect("ec key");
        let secp = Secp256k1::new();
        let expected_ec =
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(ec_msg.to_byte_array()), &ec_kp);
        assert_eq!(
            w.nth(0).expect("ec"),
            expected_ec.serialize().as_slice(),
            "EC witness must be signed under TapSighashType::Default"
        );

        let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &SLH_ENTROPY).expect("slh");
        let expected_slh = sign(&slh_kp.secret_key, slh_msg.as_byte_array()).expect("slh sign");
        assert_eq!(
            w.nth(1).expect("slh"),
            expected_slh.bytes.as_slice(),
            "SLH witness must be signed under TapSighashType::All (bare 7856)"
        );
        // Wrong preimage must not match SLH element (guards against both using Default).
        let wrong_slh = sign(&slh_kp.secret_key, ec_msg.as_byte_array()).expect("slh wrong");
        assert_ne!(
            w.nth(1).expect("slh"),
            wrong_slh.bytes.as_slice(),
            "SLH must not be signed under Default preimage"
        );
    }

    #[test]
    fn core_hybrid_spend_is_deterministic() {
        let (spk, _) = p2mr_output_for_core_hybrid_ec_slh(&EC_ENTROPY, &SLH_ENTROPY).unwrap();
        let funding = Transaction {
            version: Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: spk,
            }],
        };
        let outpoint = OutPoint {
            txid: funding.compute_txid(),
            vout: 0,
        };
        let prevout = funding.output[0].clone();
        let a = build_core_hybrid_ec_slh_spend_from_prevout(
            &EC_ENTROPY,
            &SLH_ENTROPY,
            outpoint,
            prevout.clone(),
            40_000,
            ScriptBuf::new(),
        )
        .unwrap();
        let b = build_core_hybrid_ec_slh_spend_from_prevout(
            &EC_ENTROPY,
            &SLH_ENTROPY,
            outpoint,
            prevout,
            40_000,
            ScriptBuf::new(),
        )
        .unwrap();
        assert_eq!(a.input[0].witness, b.input[0].witness);
    }
}
