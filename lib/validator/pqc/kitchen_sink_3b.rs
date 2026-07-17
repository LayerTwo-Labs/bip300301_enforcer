//! **3B** — fixed-entropy overload kitchen-sink protocol fixture.
//!
//! Golden sizes / hex for the full triple-algo kitchen-sink spend (Schnorr +
//! ML-DSA-44 + SLH-DSA-SHA2-128s, all `OP_CHECKSIG`, control `c1`).
//!
//! # Core barriers (historical → closed post Core 3B multi-barrier fix)
//!
//! Overload kitchen-sink witness (bottom → top):
//! `[ec_sig ~65 B, mldsa_sig ~2421 B, slh_sig ~7857 B, leaf 1387 B, control c1]`.
//!
//! Pre-fix Core (tapscript **without** `OP_SUCCESS127`) rejected as:
//!
//! 1. **Initial witness stack** — each item ≤ `MAX_SCRIPT_ELEMENT_SIZE` (**520**).
//!    ML-DSA / SLH sigs (~2420 / ~7856) failed first → `SCRIPT_ERR_PUSH_SIZE`.
//! 2. **Leaf script pushes** — `EvalScript` also capped at **520**. ML-DSA pk
//!    **1312 B** would fail next.
//! 3. **Opcode model** — `MAX_SCRIPT_ELEMENT_SIZE_SLHDSA` (**8000**) only raised
//!    the stack cap when `OP_SUCCESS127` was pre-scanned; multi-`OP_CHECKSIG`
//!    kitchen-sink never activated it.
//!
//! **Post-Core fix (cryptoquick P2MR):** TAPSCRIPT always uses element size
//! `MAX_SCRIPT_ELEMENT_SIZE_P2MR` / `_SLHDSA` (**8000**) for initial stack **and**
//! leaf pushes; `EvalChecksigTapscript` size-gates:
//! - pk 32 + sig 64/65 → Schnorr
//! - pk 32 + sig ~7856/7857 → SLH-DSA-SHA2-128s
//! - pk 1312 + sig ~2420/2421 → ML-DSA-44
//!
//! TB-sendraw shape 3 is a **hard green gate** (`just bip360-tier-b-mempool`).
//! Tip/CUSF kitchen-sink remains green (`just bip360-tier-b-cusf`). Constants
//! `CORE_MAX_SCRIPT_ELEMENT_SIZE` (520) and
//! `BOB_EXPECTED_REJECT_SUBSTRING` document the **pre-fix** reject class;
//! post-fix trials expect **accept**, not that reject string.
//!
//! Entropy matches integration harness (`bip360_block` /
//! `HYBRID_EC_ENTROPY` / `MLDSA_ENTROPY` / `SLH_ENTROPY`).

use bitcoin::{ScriptBuf, sighash::TapSighashType};

use super::{
    limits::{ML_DSA_44_PUBLIC_KEY_SIZE, SLH_DSA_PUBLIC_KEY_SIZE},
    signer_dev::{build_kitchen_sink_spend_from_prevout, p2mr_output_for_kitchen_sink},
};

/// Core consensus max script element size (Bitcoin Core `script.h`).
///
/// Applies to **both** initial witness stack items (when no OP_SUCCESS127) **and**
/// leaf script pushes during `EvalScript`.
pub const CORE_MAX_SCRIPT_ELEMENT_SIZE: usize = 520;

/// Core SLH-DSA stack-only raise (`MAX_SCRIPT_ELEMENT_SIZE_SLHDSA`).
///
/// Only initial witness stack, and only when `OP_SUCCESS127` was pre-scanned.
/// Does **not** apply to multi-`OP_CHECKSIG` kitchen-sink; does **not** lift leaf pushes.
pub const CORE_MAX_SCRIPT_ELEMENT_SIZE_SLHDSA: usize = 8000;

/// Pre-fix Bob `sendrawtransaction` reject substring (`SCRIPT_ERR_PUSH_SIZE`).
///
/// Documented for historical diagnosis only — post-Core 3B, Bob is expected to
/// **accept** kitchen-sink (shape 3 green gate). Do not use as a live reject criterion.
pub const BOB_EXPECTED_REJECT_SUBSTRING: &str = "Push value size limit exceeded";

/// Fixed EC/Schnorr entropy for the 3B kitchen-sink vector (`HYBRID_EC_ENTROPY`).
pub const KITCHEN_SINK_EC_ENTROPY: [u8; 32] = [0x33; 32];

/// Fixed ML-DSA-44 entropy (`MLDSA_ENTROPY`).
pub const KITCHEN_SINK_MLDSA_ENTROPY: [u8; 128] = [0x22; 128];

/// Fixed SLH-DSA-SHA2-128s entropy (`SLH_ENTROPY`).
pub const KITCHEN_SINK_SLH_ENTROPY: [u8; 128] = [0x88; 128];

/// Prevout value for the synthetic kitchen-sink vector template (sats).
pub const KITCHEN_SINK_VECTOR_PREVOUT_VALUE: u64 = 50_000;

/// Spend output value for the synthetic template (sats).
pub const KITCHEN_SINK_VECTOR_OUTPUT_VALUE: u64 = 40_000;

// --- Structural goldens (layout / sizes; independent of key material) ---

/// Control block Core wire after 3A (single-leaf).
pub const GOLDEN_CONTROL_HEX: &str = "c1";

/// Expected kitchen-sink leaf length (bytes):
/// `PUSH32(1+32) CHECKSIG + PUSHDATA2(1+2+1312) CHECKSIG + PUSH32(1+32) CHECKSIG
///  + BOOLAND + BOOLAND + VERIFY` = 1387.
pub const GOLDEN_LEAF_SCRIPT_LEN: usize = 1387;

/// Max push inside the leaf (ML-DSA-44 public key).
pub const GOLDEN_MAX_LEAF_PUSH_SIZE: usize = ML_DSA_44_PUBLIC_KEY_SIZE;

/// Schnorr / EC x-only pubkey size.
pub const GOLDEN_EC_PUBKEY_SIZE: usize = 32;

/// SLH-DSA x-only pubkey size.
pub const GOLDEN_SLH_PUBKEY_SIZE: usize = SLH_DSA_PUBLIC_KEY_SIZE;

/// ML-DSA-44 public key size (exceeds Core 520).
pub const GOLDEN_MLDSA_PUBKEY_SIZE: usize = ML_DSA_44_PUBLIC_KEY_SIZE;

/// Witness stack depth: `[ec_sig, mldsa_sig, slh_sig, leaf, control]`.
pub const GOLDEN_WITNESS_DEPTH: usize = 5;

// --- Deterministic hex goldens (entropy above; Keygen is fixed) ---

/// X-only EC/Schnorr public key hex (32 B).
pub const GOLDEN_EC_PUBKEY_HEX: &str =
    "3c72addb4fdf09af94f0c94d7fe92a386a7e70cf8a1d85916386bb2535c7b1b1";

/// SLH-DSA-SHA2-128s public key hex (32 B).
pub const GOLDEN_SLH_PUBKEY_HEX: &str =
    "888888888888888888888888888888889ba5ff987ecbeffe3ce3bf278b9bf4a9";

/// Single-leaf merkle root hex.
pub const GOLDEN_MERKLE_ROOT_HEX: &str =
    "26f78122d40af1759bf01514c8473676e13a984478dca2068a8fcc6877a605bd";

/// P2MR scriptPubKey hex: `OP_2 PUSH32 <root>`.
pub const GOLDEN_SPK_HEX: &str =
    "522026f78122d40af1759bf01514c8473676e13a984478dca2068a8fcc6877a605bd";

/// Leaf script hex prefix: `PUSH32 <ec_pk>…`.
pub const GOLDEN_LEAF_HEX_PREFIX: &str = "203c72addb4fdf09af94";

/// Leaf script hex suffix: `… <slh_pk> OP_CHECKSIG OP_BOOLAND OP_BOOLAND OP_VERIFY`.
pub const GOLDEN_LEAF_HEX_SUFFIX: &str = "bf278b9bf4a9ac9a9a69";

/// Deterministic kitchen-sink protocol artifacts for residual 3B export.
#[derive(Clone, Debug)]
pub struct KitchenSink3bVector {
    pub ec_entropy: [u8; 32],
    pub mldsa_entropy: [u8; 128],
    pub slh_entropy: [u8; 128],
    pub ec_pubkey_hex: String,
    pub mldsa_pubkey_hex: String,
    pub slh_pubkey_hex: String,
    pub ec_pubkey_len: usize,
    pub mldsa_pubkey_len: usize,
    pub slh_pubkey_len: usize,
    pub leaf_script_hex: String,
    pub leaf_script_len: usize,
    pub max_leaf_push_size: usize,
    pub control_block_hex: String,
    pub merkle_root_hex: String,
    pub script_pubkey_hex: String,
    pub sighash_type: TapSighashType,
    pub witness_depth: usize,
    pub ec_sig_len: usize,
    pub mldsa_sig_len: usize,
    pub slh_sig_len: usize,
    pub leaf_script: ScriptBuf,
    pub script_pubkey: ScriptBuf,
}

/// Scan a Bitcoin script for the largest data push (bytes).
///
/// Handles OP_PUSHBYTES_1..75, OP_PUSHDATA1/2/4. Returns 0 for empty/push-free scripts.
#[must_use]
pub fn max_script_push_size(script: &[u8]) -> usize {
    let mut i = 0;
    let mut max_push = 0usize;
    while i < script.len() {
        let op = script[i];
        i += 1;
        let push_len = if op <= 75 {
            op as usize
        } else if op == 0x4c {
            // OP_PUSHDATA1
            if i >= script.len() {
                break;
            }
            let n = script[i] as usize;
            i += 1;
            n
        } else if op == 0x4d {
            // OP_PUSHDATA2
            if i + 1 >= script.len() {
                break;
            }
            let n = u16::from_le_bytes([script[i], script[i + 1]]) as usize;
            i += 2;
            n
        } else if op == 0x4e {
            // OP_PUSHDATA4
            if i + 3 >= script.len() {
                break;
            }
            let n = u32::from_le_bytes([script[i], script[i + 1], script[i + 2], script[i + 3]])
                as usize;
            i += 4;
            n
        } else {
            continue;
        };
        if i + push_len > script.len() {
            break;
        }
        max_push = max_push.max(push_len);
        i += push_len;
    }
    max_push
}

/// Whether the kitchen-sink leaf is expected to fail Core script parse (push > 520).
///
/// This is Core barrier **(2)** — only reached if barrier **(1)** (witness stack) is fixed.
#[must_use]
pub fn leaf_exceeds_core_script_element_size(leaf: &ScriptBuf) -> bool {
    max_script_push_size(leaf.as_bytes()) > CORE_MAX_SCRIPT_ELEMENT_SIZE
}

/// Whether any kitchen-sink **signature** stack item exceeds Core 520 (barrier **1**).
///
/// Overload kitchen-sink without OP_SUCCESS127 is rejected on initial stack before
/// leaf EvalScript because ML-DSA / SLH sigs are ≫ 520.
#[must_use]
pub fn witness_sigs_exceed_core_script_element_size(v: &KitchenSink3bVector) -> bool {
    v.mldsa_sig_len > CORE_MAX_SCRIPT_ELEMENT_SIZE || v.slh_sig_len > CORE_MAX_SCRIPT_ELEMENT_SIZE
}

/// True if reject text matches Core push-size failure (required residual criterion).
#[must_use]
pub fn reject_mentions_push_size(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    msg.contains(BOB_EXPECTED_REJECT_SUBSTRING)
        || lower.contains("push value size")
        || lower.contains("script_err_push_size")
        || lower.contains("push size limit")
}

/// Build the fixed-entropy overload kitchen-sink protocol vector (TapSighash All).
///
/// Uses the same builders as tip/CUSF kitchen-sink spends. Control is Core wire `c1`.
pub fn kitchen_sink_3b_protocol_vector() -> Result<KitchenSink3bVector, String> {
    let (script_pubkey, leaf_script) = p2mr_output_for_kitchen_sink(
        &KITCHEN_SINK_EC_ENTROPY,
        &KITCHEN_SINK_MLDSA_ENTROPY,
        &KITCHEN_SINK_SLH_ENTROPY,
    )?;

    let prevout = bitcoin::TxOut {
        value: bitcoin::Amount::from_sat(KITCHEN_SINK_VECTOR_PREVOUT_VALUE),
        script_pubkey: script_pubkey.clone(),
    };
    let spend = build_kitchen_sink_spend_from_prevout(
        &KITCHEN_SINK_EC_ENTROPY,
        &KITCHEN_SINK_MLDSA_ENTROPY,
        &KITCHEN_SINK_SLH_ENTROPY,
        TapSighashType::All,
        bitcoin::OutPoint::null(),
        prevout,
        KITCHEN_SINK_VECTOR_OUTPUT_VALUE,
        ScriptBuf::new(),
    )?;

    let w = &spend.input[0].witness;
    let ec_sig = w
        .nth(0)
        .ok_or_else(|| "missing ec sig".to_string())?
        .to_vec();
    let mldsa_sig = w
        .nth(1)
        .ok_or_else(|| "missing mldsa sig".to_string())?
        .to_vec();
    let slh_sig = w
        .nth(2)
        .ok_or_else(|| "missing slh sig".to_string())?
        .to_vec();
    let leaf_w = w.nth(3).ok_or_else(|| "missing leaf".to_string())?;
    let control = w.nth(4).ok_or_else(|| "missing control".to_string())?;

    if leaf_w != leaf_script.as_bytes() {
        return Err("witness leaf != output leaf".into());
    }

    // Re-derive pubkeys for export (same entropy → same keys).
    let (ec_pk, mldsa_pk, slh_pk) = kitchen_sink_pubkeys()?;

    let max_push = max_script_push_size(leaf_script.as_bytes());
    let merkle_root = super::signer_dev::single_leaf_merkle_root(&leaf_script);

    Ok(KitchenSink3bVector {
        ec_entropy: KITCHEN_SINK_EC_ENTROPY,
        mldsa_entropy: KITCHEN_SINK_MLDSA_ENTROPY,
        slh_entropy: KITCHEN_SINK_SLH_ENTROPY,
        ec_pubkey_hex: hex::encode(&ec_pk),
        mldsa_pubkey_hex: hex::encode(&mldsa_pk),
        slh_pubkey_hex: hex::encode(&slh_pk),
        ec_pubkey_len: ec_pk.len(),
        mldsa_pubkey_len: mldsa_pk.len(),
        slh_pubkey_len: slh_pk.len(),
        leaf_script_hex: hex::encode(leaf_script.as_bytes()),
        leaf_script_len: leaf_script.len(),
        max_leaf_push_size: max_push,
        control_block_hex: hex::encode(control),
        merkle_root_hex: hex::encode(merkle_root),
        script_pubkey_hex: hex::encode(script_pubkey.as_bytes()),
        sighash_type: TapSighashType::All,
        witness_depth: w.len(),
        ec_sig_len: ec_sig.len(),
        mldsa_sig_len: mldsa_sig.len(),
        slh_sig_len: slh_sig.len(),
        leaf_script,
        script_pubkey,
    })
}

/// `(ec_xonly_pk, mldsa_pk, slh_pk)` for fixed kitchen-sink entropy.
type KitchenSinkPubkeys = (Vec<u8>, Vec<u8>, Vec<u8>);

fn kitchen_sink_pubkeys() -> Result<KitchenSinkPubkeys, String> {
    use bitcoin::{
        XOnlyPublicKey,
        key::Secp256k1,
        secp256k1::{Keypair, SecretKey},
    };
    use bitcoinpqc::{Algorithm, generate_keypair};

    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(&KITCHEN_SINK_EC_ENTROPY)
        .map_err(|e| format!("invalid EC entropy: {e}"))?;
    let ec_kp = Keypair::from_secret_key(&secp, &sk);
    let ec_pk = XOnlyPublicKey::from_keypair(&ec_kp).0.serialize().to_vec();

    let mldsa_kp = generate_keypair(Algorithm::ML_DSA_44, &KITCHEN_SINK_MLDSA_ENTROPY)
        .map_err(|e| format!("ML-DSA keygen: {e}"))?;
    let slh_kp = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &KITCHEN_SINK_SLH_ENTROPY)
        .map_err(|e| format!("SLH keygen: {e}"))?;

    Ok((ec_pk, mldsa_kp.public_key.bytes, slh_kp.public_key.bytes))
}

/// Compact export map for Core / bounty fixture porting (hex + sizes).
#[must_use]
pub fn kitchen_sink_3b_export_fields(v: &KitchenSink3bVector) -> Vec<(&'static str, String)> {
    vec![
        ("control_block", v.control_block_hex.clone()),
        ("script_pubkey", v.script_pubkey_hex.clone()),
        ("merkle_root", v.merkle_root_hex.clone()),
        ("leaf_script", v.leaf_script_hex.clone()),
        ("leaf_script_len", v.leaf_script_len.to_string()),
        ("max_leaf_push_size", v.max_leaf_push_size.to_string()),
        ("ec_pubkey_len", v.ec_pubkey_len.to_string()),
        ("mldsa_pubkey_len", v.mldsa_pubkey_len.to_string()),
        ("slh_pubkey_len", v.slh_pubkey_len.to_string()),
        ("ec_pubkey", v.ec_pubkey_hex.clone()),
        ("mldsa_pubkey", v.mldsa_pubkey_hex.clone()),
        ("slh_pubkey", v.slh_pubkey_hex.clone()),
        (
            "core_max_script_element_size",
            CORE_MAX_SCRIPT_ELEMENT_SIZE.to_string(),
        ),
        (
            "core_max_script_element_size_slhdsa_stack_only",
            CORE_MAX_SCRIPT_ELEMENT_SIZE_SLHDSA.to_string(),
        ),
        (
            "bob_expected_reject_substring",
            BOB_EXPECTED_REJECT_SUBSTRING.to_string(),
        ),
        ("witness_depth", v.witness_depth.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use bitcoinpqc::{Algorithm, generate_keypair};

    use super::*;
    use crate::validator::pqc::{
        leaf_script::build_kitchen_sink_leaf,
        limits::{ML_DSA_44_SIGNATURE_SIZE, SCHNORR_SIGNATURE_SIZE, SLH_DSA_128S_SIGNATURE_SIZE},
        signer_dev::single_leaf_control_block,
    };

    #[test]
    fn kitchen_sink_3b_control_is_core_wire_c1() {
        let v = kitchen_sink_3b_protocol_vector().expect("vector");
        assert_eq!(v.control_block_hex, GOLDEN_CONTROL_HEX);
        assert_eq!(v.control_block_hex, "c1");
        assert_eq!(
            hex::decode(&v.control_block_hex).unwrap(),
            single_leaf_control_block()
        );
    }

    #[test]
    fn kitchen_sink_3b_matches_golden_fixture_hex() {
        let v = kitchen_sink_3b_protocol_vector().expect("vector");
        assert_eq!(v.control_block_hex, GOLDEN_CONTROL_HEX);
        assert_eq!(v.ec_pubkey_hex, GOLDEN_EC_PUBKEY_HEX);
        assert_eq!(v.slh_pubkey_hex, GOLDEN_SLH_PUBKEY_HEX);
        assert_eq!(v.merkle_root_hex, GOLDEN_MERKLE_ROOT_HEX);
        assert_eq!(v.script_pubkey_hex, GOLDEN_SPK_HEX);
        assert_eq!(v.leaf_script_len, GOLDEN_LEAF_SCRIPT_LEN);
        assert!(
            v.leaf_script_hex.starts_with(GOLDEN_LEAF_HEX_PREFIX),
            "leaf prefix drift"
        );
        assert!(
            v.leaf_script_hex.ends_with(GOLDEN_LEAF_HEX_SUFFIX),
            "leaf suffix drift"
        );
        // Full leaf hex is deterministic; pin length only here (2774 hex chars).
        assert_eq!(v.leaf_script_hex.len(), GOLDEN_LEAF_SCRIPT_LEN * 2);
        assert_eq!(v.mldsa_pubkey_hex.len(), ML_DSA_44_PUBLIC_KEY_SIZE * 2);
    }

    #[test]
    fn kitchen_sink_3b_ordered_barriers_stack_then_leaf() {
        let v = kitchen_sink_3b_protocol_vector().expect("vector");
        // Barrier (1): initial witness stack — ML-DSA / SLH sigs ≫ 520.
        assert!(
            witness_sigs_exceed_core_script_element_size(&v),
            "mldsa_sig={} slh_sig={} must exceed 520 (first Core reject)",
            v.mldsa_sig_len,
            v.slh_sig_len
        );
        assert!(v.mldsa_sig_len > CORE_MAX_SCRIPT_ELEMENT_SIZE);
        assert!(v.slh_sig_len > CORE_MAX_SCRIPT_ELEMENT_SIZE);
        // Barrier (2): leaf ML-DSA pk push 1312 > 520 (second barrier if stack fixed).
        assert_eq!(v.mldsa_pubkey_len, GOLDEN_MLDSA_PUBKEY_SIZE);
        assert_eq!(v.max_leaf_push_size, GOLDEN_MAX_LEAF_PUSH_SIZE);
        assert!(
            v.max_leaf_push_size > CORE_MAX_SCRIPT_ELEMENT_SIZE,
            "ML-DSA push {} must exceed Core MAX_SCRIPT_ELEMENT_SIZE {}",
            v.max_leaf_push_size,
            CORE_MAX_SCRIPT_ELEMENT_SIZE
        );
        assert!(leaf_exceeds_core_script_element_size(&v.leaf_script));
        // Barrier (3): SLHDSA 8000 never activates without OP_SUBSTR; kitchen-sink
        // has none. Even if it did, leaf pushes stay at 520.
        assert!(
            v.max_leaf_push_size < CORE_MAX_SCRIPT_ELEMENT_SIZE_SLHDSA,
            "ML-DSA leaf push under stack-only SLHDSA cap — leaf path still needs 520 raise"
        );
        assert!(v.slh_sig_len < CORE_MAX_SCRIPT_ELEMENT_SIZE_SLHDSA);
        assert!(v.slh_sig_len > CORE_MAX_SCRIPT_ELEMENT_SIZE);
    }

    #[test]
    fn kitchen_sink_3b_leaf_contains_mldsa_push_over_520() {
        let v = kitchen_sink_3b_protocol_vector().expect("vector");
        assert_eq!(v.mldsa_pubkey_len, GOLDEN_MLDSA_PUBKEY_SIZE);
        assert_eq!(v.max_leaf_push_size, GOLDEN_MAX_LEAF_PUSH_SIZE);
        assert!(leaf_exceeds_core_script_element_size(&v.leaf_script));
    }

    #[test]
    fn reject_mentions_push_size_accepts_core_rpc_string() {
        assert!(reject_mentions_push_size(
            "mandatory-script-verify-flag-failed (Push value size limit exceeded)"
        ));
        assert!(reject_mentions_push_size("SCRIPT_ERR_PUSH_SIZE"));
        assert!(!reject_mentions_push_size(
            "TB-sendraw protocol dump\nleaf_script: abcd\ncontrol_block: c1"
        ));
        assert!(!reject_mentions_push_size("Witness program hash mismatch"));
    }

    #[test]
    fn kitchen_sink_3b_structural_sizes_match_goldens() {
        let v = kitchen_sink_3b_protocol_vector().expect("vector");
        assert_eq!(v.leaf_script_len, GOLDEN_LEAF_SCRIPT_LEN);
        assert_eq!(v.ec_pubkey_len, GOLDEN_EC_PUBKEY_SIZE);
        assert_eq!(v.slh_pubkey_len, GOLDEN_SLH_PUBKEY_SIZE);
        assert_eq!(v.mldsa_pubkey_len, GOLDEN_MLDSA_PUBKEY_SIZE);
        assert_eq!(v.witness_depth, GOLDEN_WITNESS_DEPTH);
        // All sighash → trailing 0x01 on each sig.
        assert_eq!(v.ec_sig_len, SCHNORR_SIGNATURE_SIZE + 1);
        assert!(
            (ML_DSA_44_SIGNATURE_SIZE..=ML_DSA_44_SIGNATURE_SIZE + 1).contains(&v.mldsa_sig_len),
            "mldsa sig len {}",
            v.mldsa_sig_len
        );
        assert!(
            (SLH_DSA_128S_SIGNATURE_SIZE..=SLH_DSA_128S_SIGNATURE_SIZE + 1)
                .contains(&v.slh_sig_len),
            "slh sig len {}",
            v.slh_sig_len
        );
        // SPK is OP_2 PUSH32 <root> (34 B).
        assert_eq!(v.script_pubkey.len(), 34);
        assert_eq!(&v.script_pubkey.as_bytes()[0..2], &[0x52, 0x20]);
    }

    #[test]
    fn kitchen_sink_3b_leaf_layout_has_pushdata2_mldsa() {
        let v = kitchen_sink_3b_protocol_vector().expect("vector");
        let b = v.leaf_script.as_bytes();
        // First site: PUSH32
        assert_eq!(b[0], 0x20);
        // After 32-byte pk + CHECKSIG: OP_PUSHDATA2
        assert_eq!(b[1 + 32 + 1], 0x4d);
        let mldsa_len = u16::from_le_bytes([b[1 + 32 + 1 + 1], b[1 + 32 + 1 + 2]]) as usize;
        assert_eq!(mldsa_len, ML_DSA_44_PUBLIC_KEY_SIZE);
        // Leaf ends with BOOLAND BOOLAND VERIFY
        assert_eq!(&b[b.len() - 3..], &[0x9a, 0x9a, 0x69]);
    }

    #[test]
    fn kitchen_sink_3b_bob_reject_reason_documented() {
        assert!(BOB_EXPECTED_REJECT_SUBSTRING.contains("Push value size limit exceeded"));
        // Residual is about leaf pushes, not stack.
        assert_eq!(CORE_MAX_SCRIPT_ELEMENT_SIZE, 520);
        assert_eq!(CORE_MAX_SCRIPT_ELEMENT_SIZE_SLHDSA, 8000);
        const {
            assert!(GOLDEN_MAX_LEAF_PUSH_SIZE > CORE_MAX_SCRIPT_ELEMENT_SIZE);
        }
    }

    #[test]
    fn kitchen_sink_3b_is_bit_identical_across_calls() {
        let a = kitchen_sink_3b_protocol_vector().expect("a");
        let b = kitchen_sink_3b_protocol_vector().expect("b");
        assert_eq!(a.leaf_script_hex, b.leaf_script_hex);
        assert_eq!(a.control_block_hex, b.control_block_hex);
        assert_eq!(a.script_pubkey_hex, b.script_pubkey_hex);
        assert_eq!(a.merkle_root_hex, b.merkle_root_hex);
        assert_eq!(a.mldsa_pubkey_hex, b.mldsa_pubkey_hex);
        assert_eq!(a.ec_pubkey_hex, b.ec_pubkey_hex);
        assert_eq!(a.slh_pubkey_hex, b.slh_pubkey_hex);
    }

    #[test]
    fn kitchen_sink_3b_matches_builder_leaf() {
        let v = kitchen_sink_3b_protocol_vector().expect("vector");
        let mldsa =
            generate_keypair(Algorithm::ML_DSA_44, &KITCHEN_SINK_MLDSA_ENTROPY).expect("mldsa");
        let slh =
            generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &KITCHEN_SINK_SLH_ENTROPY).expect("slh");
        let ec_pk: [u8; 32] = hex::decode(&v.ec_pubkey_hex).unwrap().try_into().unwrap();
        let slh_pk: [u8; 32] = slh.public_key.bytes.as_slice().try_into().unwrap();
        let leaf = build_kitchen_sink_leaf(&ec_pk, &mldsa.public_key.bytes, &slh_pk);
        assert_eq!(leaf.as_bytes(), v.leaf_script.as_bytes());
        assert_eq!(max_script_push_size(leaf.as_bytes()), 1312);
    }

    #[test]
    fn kitchen_sink_3b_export_fields_include_core_port_keys() {
        let v = kitchen_sink_3b_protocol_vector().expect("vector");
        let fields = kitchen_sink_3b_export_fields(&v);
        let map: std::collections::HashMap<_, _> = fields.into_iter().collect();
        assert_eq!(map.get("control_block").map(String::as_str), Some("c1"));
        assert_eq!(map.get("leaf_script_len").map(String::as_str), Some("1387"));
        assert_eq!(
            map.get("max_leaf_push_size").map(String::as_str),
            Some("1312")
        );
        assert_eq!(
            map.get("mldsa_pubkey_len").map(String::as_str),
            Some("1312")
        );
        assert!(map.get("leaf_script").unwrap().len() == 1387 * 2);
        assert!(map.get("script_pubkey").unwrap().len() == 68);
        assert_eq!(
            map.get("bob_expected_reject_substring").map(String::as_str),
            Some(BOB_EXPECTED_REJECT_SUBSTRING)
        );
    }

    #[test]
    fn max_script_push_size_handles_empty_and_small() {
        assert_eq!(max_script_push_size(&[]), 0);
        assert_eq!(max_script_push_size(&[0xac]), 0); // bare CHECKSIG
        let mut small = vec![0x20];
        small.extend_from_slice(&[0xab; 32]);
        small.push(0xac);
        assert_eq!(max_script_push_size(&small), 32);
    }
}
