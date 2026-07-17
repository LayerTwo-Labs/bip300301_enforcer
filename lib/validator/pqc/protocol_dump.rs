//! Structured dumps for P2MR script-path spends (TB-sendraw diagnostics).
//!
//! When P2MR Core rejects an enforcer-built spend with
//! `Witness program hash mismatch`, bounty hunters need the leaf script, control
//! block, tapleaf hash, and witness program bytes side-by-side — not just the RPC
//! error string. This module is pure formatting / field extraction; it does not
//! claim protocol alignment by itself (use with TB-sendraw / 3C vectors).

use bitcoin::{
    Script, Transaction, Witness,
    hashes::Hash as _,
    taproot::{LeafVersion, TapLeafHash},
};
use bitcoin_p2mr_pqc::p2mr::P2MR_LEAF_VERSION;

use super::{
    merkle::recompute_merkle_root,
    p2mr_output::{is_p2mr_script, validate_p2mr_output},
};

/// Fields extracted from a P2MR script-path witness (and optional prevout spk).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct P2mrSpendProtocolDump {
    pub witness_stack_len: usize,
    pub signature_count: usize,
    pub leaf_script_hex: Option<String>,
    pub leaf_script_len: Option<usize>,
    pub control_block_hex: Option<String>,
    pub control_block_len: Option<usize>,
    pub control_byte: Option<u8>,
    /// Leaf version used by enforcer for tapleaf / merkle hashing (`P2MR_LEAF_VERSION`).
    pub enforcer_leaf_version: u8,
    pub tapleaf_hash_hex: Option<String>,
    /// Merkle root recomputed from leaf + control block under enforcer rules.
    pub recomputed_merkle_root_hex: Option<String>,
    /// Expected P2MR scriptPubKey for the recomputed root (`OP_2 OP_PUSHBYTES_32 <root>`).
    pub recomputed_script_pubkey_hex: Option<String>,
    pub prevout_script_pubkey_hex: Option<String>,
    /// 32-byte witness program from prevout when it is a well-formed P2MR spk.
    pub prevout_witness_program_hex: Option<String>,
    /// Whether recomputed root equals prevout witness program (when both present).
    pub root_matches_prevout_program: Option<bool>,
    pub note: Option<String>,
}

fn append_note(dump: &mut P2mrSpendProtocolDump, msg: impl Into<String>) {
    let msg = msg.into();
    dump.note = Some(match dump.note.take() {
        Some(prev) => format!("{prev}; {msg}"),
        None => msg,
    });
}

/// Extract script-path dump fields from input 0 of `spend_tx`.
///
/// Witness layout (enforcer / BIP 360 script-path):
/// `[signatures...] <leaf_script> <control_block>`.
///
/// `prevout_script_pubkey` is the UTXO being spent when known (strongly preferred
/// for bounty diffs against Bob's witness-program check).
#[must_use]
pub fn extract_p2mr_spend_protocol_dump(
    spend_tx: &Transaction,
    prevout_script_pubkey: Option<&Script>,
) -> P2mrSpendProtocolDump {
    let mut dump = P2mrSpendProtocolDump {
        witness_stack_len: 0,
        signature_count: 0,
        leaf_script_hex: None,
        leaf_script_len: None,
        control_block_hex: None,
        control_block_len: None,
        control_byte: None,
        enforcer_leaf_version: P2MR_LEAF_VERSION,
        tapleaf_hash_hex: None,
        recomputed_merkle_root_hex: None,
        recomputed_script_pubkey_hex: None,
        prevout_script_pubkey_hex: None,
        prevout_witness_program_hex: None,
        root_matches_prevout_program: None,
        note: None,
    };

    if let Some(spk) = prevout_script_pubkey {
        dump.prevout_script_pubkey_hex = Some(hex::encode(spk.as_bytes()));
        if is_p2mr_script(spk) {
            if let Ok(program) = validate_p2mr_output(spk) {
                dump.prevout_witness_program_hex = Some(hex::encode(program));
            } else {
                append_note(
                    &mut dump,
                    "prevout looks P2MR-shaped but merkle-root extract failed",
                );
            }
        } else {
            append_note(
                &mut dump,
                "prevout scriptPubKey is not a 34-byte P2MR (OP_2 PUSH32) program",
            );
        }
    }

    let Some(input) = spend_tx.input.first() else {
        append_note(&mut dump, "spend_tx has no inputs");
        return dump;
    };
    fill_from_witness(&mut dump, &input.witness);
    dump
}

fn fill_from_witness(dump: &mut P2mrSpendProtocolDump, witness: &Witness) {
    dump.witness_stack_len = witness.len();
    if witness.len() < 2 {
        append_note(
            dump,
            format!(
                "witness stack too short for script-path (len={}); need [sigs…] leaf control_block",
                witness.len()
            ),
        );
        return;
    }

    dump.signature_count = witness.len() - 2;
    let Some(leaf_bytes) = witness.nth(witness.len() - 2) else {
        append_note(dump, "missing leaf_script stack element");
        return;
    };
    let Some(control_bytes) = witness.nth(witness.len() - 1) else {
        append_note(dump, "missing control_block stack element");
        return;
    };

    dump.leaf_script_hex = Some(hex::encode(leaf_bytes));
    dump.leaf_script_len = Some(leaf_bytes.len());
    dump.control_block_hex = Some(hex::encode(control_bytes));
    dump.control_block_len = Some(control_bytes.len());
    dump.control_byte = control_bytes.first().copied();

    let leaf_script = bitcoin::Script::from_bytes(leaf_bytes);
    if let Ok(lv) = LeafVersion::from_consensus(P2MR_LEAF_VERSION) {
        let tapleaf = TapLeafHash::from_script(leaf_script, lv);
        dump.tapleaf_hash_hex = Some(hex::encode(tapleaf.to_byte_array()));
    } else {
        append_note(dump, "invalid enforcer leaf version for TapLeafHash");
    }

    match recompute_merkle_root_for_dump(leaf_bytes, control_bytes) {
        Ok(root) => {
            dump.recomputed_merkle_root_hex = Some(hex::encode(root));
            let mut spk = vec![0x52, 0x20];
            spk.extend_from_slice(&root);
            dump.recomputed_script_pubkey_hex = Some(hex::encode(spk));
            if let Some(prev_program_hex) = dump.prevout_witness_program_hex.as_ref() {
                dump.root_matches_prevout_program =
                    Some(prev_program_hex.as_str() == hex::encode(root));
            }
        }
        Err(msg) => {
            append_note(dump, format!("enforcer merkle recompute failed: {msg}"));
        }
    }
}

/// Recompute the P2MR merkle root the enforcer would commit from leaf + control block.
///
/// Uses Core wire layout (`0xc1 || 32*N`), same as [`super::merkle::verify_merkle_path`].
fn recompute_merkle_root_for_dump(
    leaf_bytes: &[u8],
    control_block_bytes: &[u8],
) -> Result<[u8; 32], String> {
    let leaf_script = bitcoin::Script::from_bytes(leaf_bytes);
    recompute_merkle_root(leaf_script, control_block_bytes).map_err(|e| e.to_string())
}

/// Human-readable multi-line dump for TB-sendraw failure messages.
#[must_use]
pub fn format_p2mr_spend_protocol_dump(
    spend_tx: &Transaction,
    prevout_script_pubkey: Option<&Script>,
) -> String {
    let d = extract_p2mr_spend_protocol_dump(spend_tx, prevout_script_pubkey);
    format_dump(&d)
}

fn format_dump(d: &P2mrSpendProtocolDump) -> String {
    let mut lines = Vec::new();
    lines.push("--- TB-sendraw protocol dump (enforcer-built spend) ---".to_string());
    lines.push(format!(
        "witness_stack_len={}  signature_count={}  enforcer_leaf_version=0x{:02x}",
        d.witness_stack_len, d.signature_count, d.enforcer_leaf_version
    ));
    if let Some(v) = d.control_byte {
        lines.push(format!(
            "control_byte=0x{v:02x}  control_block_len={}",
            d.control_block_len.unwrap_or(0)
        ));
    }
    // Full hex for protocol fields (leaf/control can be multi-KB kitchen-sink leaves).
    // No middle truncation — 3C porting needs complete bytes.
    push_hex_field_full(
        &mut lines,
        "prevout_script_pubkey",
        d.prevout_script_pubkey_hex.as_deref(),
    );
    push_hex_field_full(
        &mut lines,
        "prevout_witness_program (32 B)",
        d.prevout_witness_program_hex.as_deref(),
    );
    push_hex_field_full(&mut lines, "leaf_script", d.leaf_script_hex.as_deref());
    if let Some(len) = d.leaf_script_len {
        lines.push(format!("leaf_script_len={len}"));
    }
    push_hex_field_full(&mut lines, "control_block", d.control_block_hex.as_deref());
    push_hex_field_full(&mut lines, "tapleaf_hash", d.tapleaf_hash_hex.as_deref());
    push_hex_field_full(
        &mut lines,
        "recomputed_merkle_root (enforcer)",
        d.recomputed_merkle_root_hex.as_deref(),
    );
    push_hex_field_full(
        &mut lines,
        "recomputed_script_pubkey (enforcer)",
        d.recomputed_script_pubkey_hex.as_deref(),
    );
    match d.root_matches_prevout_program {
        Some(true) => lines.push(
            "root_matches_prevout_program=true  (enforcer leaf+control recommits to prevout \
             program; if a verifier still rejects, compare control-block / leaf-version protocol)"
                .into(),
        ),
        Some(false) => lines.push(
            "root_matches_prevout_program=false  (enforcer recomputed root ≠ prevout program; \
             funding spk vs spend leaf mismatch or wrong prevout passed to dump)"
                .into(),
        ),
        None => {
            lines.push("root_matches_prevout_program=n/a  (prevout spk missing or not P2MR)".into())
        }
    }
    if let Some(note) = &d.note {
        lines.push(format!("note: {note}"));
    }
    lines.push("How to read: docs/TIER_B_P2MR_MEMPOOL.md § protocol dump; lanes 3A/3B/3C.".into());
    lines.push("-----------------------------------------------".into());
    lines.join("\n")
}

fn push_hex_field_full(lines: &mut Vec<String>, label: &str, hex: Option<&str>) {
    match hex {
        Some(h) => lines.push(format!("{label}: {h}")),
        None => lines.push(format!("{label}: <missing>")),
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
        sighash::TapSighashType, transaction::Version,
    };

    use super::*;
    use crate::validator::pqc::signer_dev::{
        SignAlgorithm, build_kitchen_sink_spend_from_prevout, build_p2mr_spend_from_prevout,
        p2mr_output_for_algorithm, p2mr_output_for_kitchen_sink,
    };

    const EC_ENTROPY: [u8; 32] = [0x33; 32];
    /// ML-DSA / SLH require ≥128 bytes of entropy.
    const MLDSA_ENTROPY: [u8; 128] = [0x22; 128];
    const SLH_ENTROPY: [u8; 128] = [0x88; 128];

    #[test]
    fn dump_formats_schnorr_single_leaf_spend() {
        let entropy = [0x42u8; 32];
        let (spk, _) = p2mr_output_for_algorithm(SignAlgorithm::Schnorr, &entropy).unwrap();
        let prevout = TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: spk.clone(),
        };
        let spend = build_p2mr_spend_from_prevout(
            SignAlgorithm::Schnorr,
            &entropy,
            TapSighashType::All,
            OutPoint::null(),
            prevout.clone(),
            40_000,
            ScriptBuf::new(),
        )
        .unwrap();

        let dump = extract_p2mr_spend_protocol_dump(&spend, Some(spk.as_script()));
        assert_eq!(dump.witness_stack_len, 3);
        assert_eq!(dump.signature_count, 1);
        assert_eq!(dump.enforcer_leaf_version, P2MR_LEAF_VERSION);
        assert_eq!(dump.control_byte, Some(0xc1));
        assert_eq!(
            dump.control_block_len,
            Some(1),
            "Core wire single-leaf control is exactly 1 byte"
        );
        assert_eq!(dump.control_block_hex.as_deref(), Some("c1"));
        assert!(dump.leaf_script_hex.is_some());
        assert_eq!(dump.tapleaf_hash_hex.as_ref().unwrap().len(), 64);
        assert_eq!(dump.root_matches_prevout_program, Some(true));
        assert_eq!(
            dump.recomputed_script_pubkey_hex.as_deref(),
            Some(hex::encode(spk.as_bytes()).as_str())
        );

        let text = format_dump(&dump);
        assert!(text.contains("TB-sendraw protocol dump"));
        assert!(text.contains("leaf_script:"));
        assert!(text.contains("control_block:"));
        assert!(text.contains("tapleaf_hash:"));
        assert!(text.contains("prevout_witness_program"));
        assert!(text.contains("recomputed_merkle_root (enforcer)"));
        assert!(text.contains("recomputed_script_pubkey (enforcer)"));
        assert!(text.contains("root_matches_prevout_program=true"));
        assert!(text.contains("if a verifier still rejects"));
        assert!(!text.contains("Bob still rejected"));
        assert!(text.contains("3A/3B/3C"));
    }

    /// 3C-style unit vector: fixed-entropy Schnorr single-leaf with Core control wire.
    ///
    /// Documents leaf_hex, control_hex=`c1`, spk, and root_matches for shared protocol
    /// porting (lane 3C). Control length must stay 1 under P2MR Core layout.
    #[test]
    fn protocol_vector_schnorr_single_leaf_core_control_wire() {
        const ENTROPY: [u8; 32] = [0x42; 32];
        let (spk, leaf) =
            p2mr_output_for_algorithm(SignAlgorithm::Schnorr, &ENTROPY).expect("p2mr output");
        let prevout = TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: spk.clone(),
        };
        let spend = build_p2mr_spend_from_prevout(
            SignAlgorithm::Schnorr,
            &ENTROPY,
            TapSighashType::All,
            OutPoint::null(),
            prevout,
            40_000,
            ScriptBuf::new(),
        )
        .expect("spend");

        let dump = extract_p2mr_spend_protocol_dump(&spend, Some(spk.as_script()));
        let leaf_hex = dump.leaf_script_hex.as_deref().expect("leaf_hex");
        let control_hex = dump.control_block_hex.as_deref().expect("control_hex");
        let spk_hex = dump
            .prevout_script_pubkey_hex
            .as_deref()
            .expect("prevout spk");

        assert_eq!(
            control_hex, "c1",
            "control_hex must be Core single-leaf wire"
        );
        assert_eq!(dump.control_block_len, Some(1));
        assert_eq!(dump.control_byte, Some(0xc1));
        assert_eq!(leaf_hex, &hex::encode(leaf.as_bytes()));
        assert_eq!(spk_hex, hex::encode(spk.as_bytes()));
        assert_eq!(
            dump.root_matches_prevout_program,
            Some(true),
            "leaf+control must recommit to funded spk"
        );
        // Leaf is PUSH32 <pk> OP_CHECKSIG (34 bytes → 68 hex chars).
        assert_eq!(leaf_hex.len(), 68, "documented Schnorr leaf length");
        assert!(
            leaf_hex.starts_with("20") && leaf_hex.ends_with("ac"),
            "leaf_hex={leaf_hex}"
        );
    }

    #[test]
    fn dump_without_prevout_still_emits_leaf_and_control() {
        let entropy = [0x11u8; 32];
        let (spk, _) = p2mr_output_for_algorithm(SignAlgorithm::Schnorr, &entropy).unwrap();
        let prevout = TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: spk,
        };
        let spend = build_p2mr_spend_from_prevout(
            SignAlgorithm::Schnorr,
            &entropy,
            TapSighashType::All,
            OutPoint::null(),
            prevout,
            40_000,
            ScriptBuf::new(),
        )
        .unwrap();

        let text = format_p2mr_spend_protocol_dump(&spend, None);
        assert!(text.contains("leaf_script:"));
        assert!(text.contains("control_block:"));
        assert!(text.contains("root_matches_prevout_program=n/a"));
    }

    #[test]
    fn dump_kitchen_sink_full_leaf_hex_and_root_match() {
        let (spk, leaf) =
            p2mr_output_for_kitchen_sink(&EC_ENTROPY, &MLDSA_ENTROPY, &SLH_ENTROPY).unwrap();
        assert!(
            leaf.as_bytes().len() > 80,
            "kitchen-sink leaf should be large enough to stress full-hex dump"
        );
        let prevout = TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: spk.clone(),
        };
        let spend = build_kitchen_sink_spend_from_prevout(
            &EC_ENTROPY,
            &MLDSA_ENTROPY,
            &SLH_ENTROPY,
            TapSighashType::All,
            OutPoint::null(),
            prevout,
            40_000,
            ScriptBuf::new(),
        )
        .unwrap();

        let dump = extract_p2mr_spend_protocol_dump(&spend, Some(spk.as_script()));
        assert_eq!(dump.witness_stack_len, 5);
        assert_eq!(dump.signature_count, 3);
        assert_eq!(dump.root_matches_prevout_program, Some(true));
        let leaf_hex = dump.leaf_script_hex.as_ref().expect("leaf");
        assert_eq!(leaf_hex.len(), leaf.as_bytes().len() * 2);
        assert_eq!(leaf_hex, &hex::encode(leaf.as_bytes()));

        let text = format_dump(&dump);
        // Full leaf present (no middle truncation).
        assert!(text.contains(leaf_hex));
        assert!(!text.contains("(truncated"));
        assert!(text.contains("root_matches_prevout_program=true"));
    }

    #[test]
    fn dump_root_matches_false_when_prevout_wrong_tree() {
        let (kitchen_spk, _) =
            p2mr_output_for_kitchen_sink(&EC_ENTROPY, &MLDSA_ENTROPY, &SLH_ENTROPY).unwrap();
        // Spend kitchen-sink leaf against a Schnorr-funded prevout → mismatch.
        let (schnorr_spk, _) =
            p2mr_output_for_algorithm(SignAlgorithm::Schnorr, &[0x42u8; 32]).unwrap();
        let wrong_prevout = TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: schnorr_spk,
        };
        let spend = build_kitchen_sink_spend_from_prevout(
            &EC_ENTROPY,
            &MLDSA_ENTROPY,
            &SLH_ENTROPY,
            TapSighashType::All,
            OutPoint::null(),
            // Prevout value only used for amount check; spk is what dump compares.
            TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: kitchen_spk,
            },
            40_000,
            ScriptBuf::new(),
        )
        .unwrap();

        let dump =
            extract_p2mr_spend_protocol_dump(&spend, Some(wrong_prevout.script_pubkey.as_script()));
        assert_eq!(dump.root_matches_prevout_program, Some(false));
        let text = format_dump(&dump);
        assert!(text.contains("root_matches_prevout_program=false"));
    }

    #[test]
    fn dump_short_witness_appends_note_without_panic() {
        let mut spend = Transaction {
            version: Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::from_slice(&[&[0u8; 8]]),
            }],
            output: vec![],
        };
        // Non-P2MR prevout note + short witness note should both appear (merged).
        let non_p2mr = ScriptBuf::from_bytes(vec![0x00, 0x14]);
        let dump = extract_p2mr_spend_protocol_dump(&spend, Some(non_p2mr.as_script()));
        let note = dump.note.as_deref().expect("notes");
        assert!(note.contains("not a 34-byte P2MR"));
        assert!(note.contains("witness stack too short"));
        assert!(note.contains(';'), "notes should be merged with semicolon");

        spend.input[0].witness = Witness::new();
        let dump2 = extract_p2mr_spend_protocol_dump(&spend, None);
        assert!(
            dump2
                .note
                .as_deref()
                .unwrap_or("")
                .contains("witness stack too short")
        );
        let text = format_dump(&dump2);
        assert!(text.contains("note:"));
        assert!(!text.contains("panic"));
    }
}
