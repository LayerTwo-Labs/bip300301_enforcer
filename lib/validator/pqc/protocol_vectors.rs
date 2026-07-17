//! Shared P2MR protocol vectors (lane 3C) — fixed-entropy Schnorr script-path.
//!
//! Locks the TB-sendraw green gate shape:
//! - entropy [`SCHNORR_VECTOR_ENTROPY`] (`[0x11; 32]`, same as integration harness)
//! - control block Core wire: single byte `0xc1`
//! - [`TapSighashType::Default`] → bare **64-byte** Schnorr (no trailing `0x01`)
//! - leaf: `PUSH32 <pk> OP_CHECKSIG`
//! - spk: `OP_2 PUSH32 <merkle_root>` (`52 20 <root>`)
//!
//! Hex fields are exported for Core fixture porting. Builders reuse
//! [`super::signer_dev`] so vector bytes stay bit-identical to production test paths.

use bitcoin::sighash::TapSighashType;

use super::signer_dev::{SignAlgorithm, SignedP2mrSpend, sign_p2mr_script_path_spend};

/// Fixed entropy for the shared Schnorr protocol vector (`integration_tests` `SCHNORR_ENTROPY`).
pub const SCHNORR_VECTOR_ENTROPY: [u8; 32] = [0x11; 32];

/// Prevout value used when materializing the Schnorr vector spend (sats).
pub const SCHNORR_VECTOR_PREVOUT_VALUE: u64 = 50_000;

/// Spend output value for the Schnorr vector template (sats).
pub const SCHNORR_VECTOR_OUTPUT_VALUE: u64 = 40_000;

// --- Golden fixture hex (3C Core port; entropy [0x11;32], Default bare 64) ---
// Locked from deterministic `sign_p2mr_script_path_spend` template (50k→40k, empty dest).

/// Control block Core wire (single-leaf).
pub const GOLDEN_CONTROL_HEX: &str = "c1";

/// X-only Schnorr public key hex (32 B).
pub const GOLDEN_PUBKEY_HEX: &str =
    "4f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa";

/// Leaf script hex: `PUSH32 <pk> OP_CHECKSIG`.
pub const GOLDEN_LEAF_HEX: &str =
    "204f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aaac";

/// Single-leaf merkle root hex.
pub const GOLDEN_MERKLE_ROOT_HEX: &str =
    "54461f083426f688bb72aed949de73395b4a89f2f05b438ec4401443002eeb70";

/// P2MR scriptPubKey hex: `OP_2 PUSH32 <root>`.
pub const GOLDEN_SPK_HEX: &str =
    "522054461f083426f688bb72aed949de73395b4a89f2f05b438ec4401443002eeb70";

/// TapSighash digest hex for the synthetic funding template (Default).
pub const GOLDEN_SIGHASH_HEX: &str =
    "1f761a93e11d9032101a1a7af6a4ba77053941bb043c854e93b8506076404720";

/// Bare 64-byte Schnorr signature hex (Default; no trailing type byte).
pub const GOLDEN_SIGNATURE_HEX: &str = "a2cd0d4e6950a5d71369f5c91816ed4ba99a90a6e719496fd25356a76b68b44b18de938404e4614771c0d549bd92a57c7400329b7ff25c429500193b62091c70";

/// Deterministic Schnorr P2MR script-path artifacts for protocol export / regression.
#[derive(Clone, Debug)]
pub struct SchnorrProtocolVector {
    pub entropy: [u8; 32],
    pub public_key_hex: String,
    pub leaf_script_hex: String,
    pub control_block_hex: String,
    pub merkle_root_hex: String,
    pub script_pubkey_hex: String,
    pub sighash_type: TapSighashType,
    pub signature_hex: String,
    pub sighash_hex: String,
    pub funding_tx_hex: String,
    pub signed_spend_tx_hex: String,
    pub signed: SignedP2mrSpend,
}

/// Build the fixed-entropy Schnorr protocol vector (TapSighash Default / bare 64 B).
pub fn schnorr_protocol_vector() -> Result<SchnorrProtocolVector, String> {
    let signed = sign_p2mr_script_path_spend(
        SignAlgorithm::Schnorr,
        &SCHNORR_VECTOR_ENTROPY,
        TapSighashType::Default,
        SCHNORR_VECTOR_PREVOUT_VALUE,
        SCHNORR_VECTOR_OUTPUT_VALUE,
    )?;

    Ok(SchnorrProtocolVector {
        entropy: SCHNORR_VECTOR_ENTROPY,
        public_key_hex: hex::encode(&signed.public_key),
        leaf_script_hex: hex::encode(signed.leaf_script.as_bytes()),
        control_block_hex: hex::encode(&signed.control_block),
        merkle_root_hex: hex::encode(signed.merkle_root),
        script_pubkey_hex: hex::encode(signed.script_pubkey.as_bytes()),
        sighash_type: signed.sighash_type,
        signature_hex: hex::encode(&signed.signature),
        sighash_hex: hex::encode(signed.sighash),
        funding_tx_hex: super::signer_dev::tx_to_hex(&signed.funding_tx),
        signed_spend_tx_hex: super::signer_dev::tx_to_hex(&signed.signed_spend_tx),
        signed,
    })
}

#[cfg(test)]
mod tests {
    use bitcoinpqc::{Algorithm, generate_keypair};

    use super::*;
    use crate::validator::pqc::signer_dev::{
        append_sighash, build_checksig_leaf, p2mr_script_pubkey, single_leaf_control_block,
        single_leaf_merkle_root,
    };

    #[test]
    fn schnorr_vector_matches_golden_fixture_hex() {
        let v = schnorr_protocol_vector().expect("vector");
        assert_eq!(v.control_block_hex, GOLDEN_CONTROL_HEX);
        assert_eq!(v.public_key_hex, GOLDEN_PUBKEY_HEX);
        assert_eq!(v.leaf_script_hex, GOLDEN_LEAF_HEX);
        assert_eq!(v.merkle_root_hex, GOLDEN_MERKLE_ROOT_HEX);
        assert_eq!(v.script_pubkey_hex, GOLDEN_SPK_HEX);
        assert_eq!(v.sighash_hex, GOLDEN_SIGHASH_HEX);
        assert_eq!(v.signature_hex, GOLDEN_SIGNATURE_HEX);
        assert_eq!(v.signed.signature.len(), 64);
        assert_eq!(
            append_sighash(v.signed.signature.clone(), TapSighashType::Default).len(),
            64,
            "Default must not append a trailing sighash type byte"
        );
    }

    #[test]
    fn schnorr_vector_control_is_core_wire_c1() {
        let v = schnorr_protocol_vector().expect("vector");
        assert_eq!(v.control_block_hex, "c1");
        assert_eq!(v.signed.control_block, single_leaf_control_block());
        assert_eq!(v.signed.control_block.len(), 1);
    }

    #[test]
    fn schnorr_vector_bare_64_default_sighash() {
        let v = schnorr_protocol_vector().expect("vector");
        assert_eq!(v.sighash_type, TapSighashType::Default);
        assert_eq!(v.signed.signature.len(), 64);
        assert_eq!(v.signature_hex.len(), 128);
        // Explicit: Default path never grows the wire sig past 64 B.
        assert_eq!(
            append_sighash(v.signed.signature.clone(), TapSighashType::Default).len(),
            64
        );
        // Contrast: All would add a trailing 0x01 if applied to the same raw bytes.
        assert_eq!(
            append_sighash(v.signed.signature.clone(), TapSighashType::All).len(),
            65
        );
    }

    #[test]
    fn schnorr_vector_leaf_spk_deterministic_from_entropy() {
        let v = schnorr_protocol_vector().expect("vector");

        let kp = generate_keypair(Algorithm::SECP256K1_SCHNORR, &SCHNORR_VECTOR_ENTROPY)
            .expect("keygen");
        let leaf = build_checksig_leaf(&kp.public_key.bytes).expect("leaf");
        let root = single_leaf_merkle_root(&leaf);
        let spk = p2mr_script_pubkey(root);

        assert_eq!(v.public_key_hex, hex::encode(&kp.public_key.bytes));
        assert_eq!(v.leaf_script_hex, hex::encode(leaf.as_bytes()));
        assert_eq!(v.merkle_root_hex, hex::encode(root));
        assert_eq!(v.script_pubkey_hex, hex::encode(spk.as_bytes()));

        // Leaf is PUSH32 <pk> OP_CHECKSIG (34 bytes).
        assert_eq!(v.signed.leaf_script.len(), 34);
        assert_eq!(v.signed.leaf_script.as_bytes()[0], 0x20);
        assert_eq!(
            v.signed.leaf_script.as_bytes()[33],
            bitcoin::opcodes::all::OP_CHECKSIG.to_u8()
        );

        // SPK is OP_2 PUSH32 <root> (34 bytes).
        assert_eq!(v.signed.script_pubkey.len(), 34);
        assert_eq!(&v.signed.script_pubkey.as_bytes()[0..2], &[0x52, 0x20]);
        assert_eq!(&v.signed.script_pubkey.as_bytes()[2..], &root);
    }

    #[test]
    fn schnorr_vector_is_bit_identical_across_calls() {
        let a = schnorr_protocol_vector().expect("a");
        let b = schnorr_protocol_vector().expect("b");
        assert_eq!(a.public_key_hex, b.public_key_hex);
        assert_eq!(a.leaf_script_hex, b.leaf_script_hex);
        assert_eq!(a.control_block_hex, b.control_block_hex);
        assert_eq!(a.merkle_root_hex, b.merkle_root_hex);
        assert_eq!(a.script_pubkey_hex, b.script_pubkey_hex);
        assert_eq!(a.signature_hex, b.signature_hex);
        assert_eq!(a.sighash_hex, b.sighash_hex);
        assert_eq!(a.funding_tx_hex, b.funding_tx_hex);
        assert_eq!(a.signed_spend_tx_hex, b.signed_spend_tx_hex);
    }
}
