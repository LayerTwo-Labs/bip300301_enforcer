//! Published protocol vector JSON for Core CI / external consumers (lanes 3C / hybrid / 3B).
//!
//! Static files under [`data/`](./data/):
//! - `schnorr_3c.json` — fixed-entropy Schnorr script-path (shape 1 / 3C)
//! - `core_hybrid.json` — Core-legal OP_SUBSTR hybrid (shape 2; interop only)
//! - `kitchen_sink_3b.json` — overload kitchen-sink (shape 3 / tip; post-Core-3B Bob accepts)
//!
//! Unit tests assert these files match live builders (fail if goldens drift).

use serde_json::{Value, json};

use super::{
    core_interop::{CORE_HYBRID_LEAF_LEN, p2mr_output_for_core_hybrid_ec_slh},
    kitchen_sink_3b::{
        GOLDEN_CONTROL_HEX, GOLDEN_LEAF_SCRIPT_LEN, GOLDEN_MAX_LEAF_PUSH_SIZE,
        KITCHEN_SINK_EC_ENTROPY, KITCHEN_SINK_MLDSA_ENTROPY, KITCHEN_SINK_SLH_ENTROPY,
        kitchen_sink_3b_export_fields, kitchen_sink_3b_protocol_vector,
    },
    protocol_vectors::{
        GOLDEN_CONTROL_HEX as SCHNORR_CONTROL, GOLDEN_LEAF_HEX, GOLDEN_MERKLE_ROOT_HEX,
        GOLDEN_PUBKEY_HEX, GOLDEN_SIGHASH_HEX, GOLDEN_SIGNATURE_HEX, GOLDEN_SPK_HEX,
        SCHNORR_VECTOR_ENTROPY, schnorr_protocol_vector,
    },
    signer_dev::single_leaf_merkle_root,
};

/// Entropy hex for Core hybrid (matches integration harness / core_interop tests).
const HYBRID_EC_ENTROPY: [u8; 32] = [0x33; 32];
const HYBRID_SLH_ENTROPY: [u8; 128] = [0x88; 128];

/// Build live Schnorr 3C export object (must match `data/schnorr_3c.json`).
pub fn schnorr_3c_export_json() -> Result<Value, String> {
    let v = schnorr_protocol_vector()?;
    Ok(json!({
        "id": "schnorr_3c",
        "lane": "3C",
        "shape": 1,
        "note": "TB-sendraw shape 1 green gate; TapSighash Default bare 64 B; control c1",
        "entropy_hex": hex::encode(SCHNORR_VECTOR_ENTROPY),
        "sighash_type": "Default",
        "control_block_hex": v.control_block_hex,
        "public_key_hex": v.public_key_hex,
        "leaf_script_hex": v.leaf_script_hex,
        "merkle_root_hex": v.merkle_root_hex,
        "script_pubkey_hex": v.script_pubkey_hex,
        "signature_hex": v.signature_hex,
        "sighash_hex": v.sighash_hex,
        "signature_len": v.signed.signature.len(),
        "leaf_script_len": v.signed.leaf_script.len(),
        "goldens": {
            "control_block_hex": SCHNORR_CONTROL,
            "public_key_hex": GOLDEN_PUBKEY_HEX,
            "leaf_script_hex": GOLDEN_LEAF_HEX,
            "merkle_root_hex": GOLDEN_MERKLE_ROOT_HEX,
            "script_pubkey_hex": GOLDEN_SPK_HEX,
            "signature_hex": GOLDEN_SIGNATURE_HEX,
            "sighash_hex": GOLDEN_SIGHASH_HEX,
        }
    }))
}

/// Build live Core hybrid interop export object (must match `data/core_hybrid.json`).
pub fn core_hybrid_export_json() -> Result<Value, String> {
    let (spk, leaf) = p2mr_output_for_core_hybrid_ec_slh(&HYBRID_EC_ENTROPY, &HYBRID_SLH_ENTROPY)?;
    let root = single_leaf_merkle_root(&leaf);
    // Re-derive pks for export.
    let ec_pk = &leaf.as_bytes()[1..33];
    let slh_pk = &leaf.as_bytes()[35..67];
    Ok(json!({
        "id": "core_hybrid_op_substr",
        "lane": "interop",
        "shape": 2,
        "note": "Core-legal hybrid OP_SUBSTR; enforcer rejects OP_SUBSTR on tip verify (Bob-only criterion)",
        "enforcer_canonical": false,
        "ec_entropy_hex": hex::encode(HYBRID_EC_ENTROPY),
        "slh_entropy_hex": hex::encode(HYBRID_SLH_ENTROPY),
        "leaf_script_len": leaf.len(),
        "expected_leaf_script_len": CORE_HYBRID_LEAF_LEN,
        "leaf_script_hex": hex::encode(leaf.as_bytes()),
        "control_block_hex": GOLDEN_CONTROL_HEX,
        "merkle_root_hex": hex::encode(root),
        "script_pubkey_hex": hex::encode(spk.as_bytes()),
        "ec_pubkey_hex": hex::encode(ec_pk),
        "slh_pubkey_hex": hex::encode(slh_pk),
        "witness_stack_sizes": {
            "ec_sig": 64,
            "slh_sig": 7856,
            "leaf": CORE_HYBRID_LEAF_LEN,
            "control": 1
        },
        "leaf_layout": "PUSH32 <ec_pk> OP_CHECKSIG PUSH32 <slh_pk> OP_SUBSTR OP_BOOLAND OP_VERIFY"
    }))
}

/// Build live kitchen-sink 3B export object (must match `data/kitchen_sink_3b.json`).
pub fn kitchen_sink_3b_export_json() -> Result<Value, String> {
    let v = kitchen_sink_3b_protocol_vector()?;
    let fields: serde_json::Map<String, Value> = kitchen_sink_3b_export_fields(&v)
        .into_iter()
        .map(|(k, val)| (k.to_string(), Value::String(val)))
        .collect();
    Ok(json!({
        "id": "kitchen_sink_3b",
        "lane": "3B",
        "shape": 3,
        "note": "Post-Core-3B Bob accepts; tip/CUSF kitchen-sink also green. multi-OP_CHECKSIG overload.",
        "bob_accepts_post_core_3b": true,
        "ec_entropy_hex": hex::encode(KITCHEN_SINK_EC_ENTROPY),
        "mldsa_entropy_hex": hex::encode(KITCHEN_SINK_MLDSA_ENTROPY),
        "slh_entropy_hex": hex::encode(KITCHEN_SINK_SLH_ENTROPY),
        "leaf_script_len": v.leaf_script_len,
        "expected_leaf_script_len": GOLDEN_LEAF_SCRIPT_LEN,
        "max_leaf_push_size": v.max_leaf_push_size,
        "expected_max_leaf_push_size": GOLDEN_MAX_LEAF_PUSH_SIZE,
        "control_block_hex": v.control_block_hex,
        "script_pubkey_hex": v.script_pubkey_hex,
        "merkle_root_hex": v.merkle_root_hex,
        "leaf_script_hex": v.leaf_script_hex,
        "ec_pubkey_hex": v.ec_pubkey_hex,
        "mldsa_pubkey_hex": v.mldsa_pubkey_hex,
        "slh_pubkey_hex": v.slh_pubkey_hex,
        "ec_pubkey_len": v.ec_pubkey_len,
        "mldsa_pubkey_len": v.mldsa_pubkey_len,
        "slh_pubkey_len": v.slh_pubkey_len,
        "witness_depth": v.witness_depth,
        "ec_sig_len": v.ec_sig_len,
        "mldsa_sig_len": v.mldsa_sig_len,
        "slh_sig_len": v.slh_sig_len,
        "size_gates": {
            "schnorr_pk_sig": [32, 64],
            "slh_pk_sig": [32, 7856],
            "ml_dsa_pk_sig": [1312, 2420]
        },
        "export_fields": fields
    }))
}

/// Pretty-print with trailing newline (stable for git / Core CI).
pub fn pretty_json(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("serialize");
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use bitcoinpqc::{Algorithm, generate_keypair};

    use super::*;

    fn parse_static(name: &str, raw: &str) -> Value {
        serde_json::from_str(raw).unwrap_or_else(|e| panic!("{name}: invalid JSON: {e}"))
    }

    #[test]
    fn published_schnorr_3c_json_matches_live() {
        let live = schnorr_3c_export_json().expect("live");
        let file = parse_static("schnorr_3c.json", include_str!("data/schnorr_3c.json"));
        assert_eq!(
            live, file,
            "data/schnorr_3c.json drifted from protocol_vectors live builder"
        );
        assert_eq!(file["control_block_hex"], "c1");
        assert_eq!(file["signature_len"], 64);
    }

    #[test]
    fn published_core_hybrid_json_matches_live() {
        let live = core_hybrid_export_json().expect("live");
        let file = parse_static("core_hybrid.json", include_str!("data/core_hybrid.json"));
        assert_eq!(
            live, file,
            "data/core_hybrid.json drifted from core_interop live builder"
        );
        assert_eq!(file["leaf_script_len"], CORE_HYBRID_LEAF_LEN);
        assert_eq!(file["control_block_hex"], "c1");
        assert_eq!(file["enforcer_canonical"], false);
        // Leaf ends with OP_SUBSTR BOOLAND VERIFY
        let leaf_hex = file["leaf_script_hex"].as_str().unwrap();
        assert!(leaf_hex.ends_with("7f9a69"), "SUBSTR BOOLAND VERIFY");
    }

    #[test]
    fn published_kitchen_sink_3b_json_matches_live() {
        let live = kitchen_sink_3b_export_json().expect("live");
        let file = parse_static(
            "kitchen_sink_3b.json",
            include_str!("data/kitchen_sink_3b.json"),
        );
        assert_eq!(
            live, file,
            "data/kitchen_sink_3b.json drifted from kitchen_sink_3b live builder"
        );
        assert_eq!(file["leaf_script_len"], GOLDEN_LEAF_SCRIPT_LEN);
        assert_eq!(file["max_leaf_push_size"], GOLDEN_MAX_LEAF_PUSH_SIZE);
        assert_eq!(file["control_block_hex"], "c1");
        assert_eq!(file["bob_accepts_post_core_3b"], true);
    }

    #[test]
    fn hybrid_leaf_matches_export_hex() {
        use super::super::core_interop::build_core_hybrid_ec_slh_leaf;
        let file = parse_static("core_hybrid.json", include_str!("data/core_hybrid.json"));
        let ec: [u8; 32] = hex::decode(file["ec_pubkey_hex"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let slh: [u8; 32] = hex::decode(file["slh_pubkey_hex"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let leaf = build_core_hybrid_ec_slh_leaf(&ec, &slh);
        assert_eq!(
            hex::encode(leaf.as_bytes()),
            file["leaf_script_hex"].as_str().unwrap()
        );
        let slh_kp =
            generate_keypair(Algorithm::SLH_DSA_SHA2_128S, &HYBRID_SLH_ENTROPY).expect("slh");
        assert_eq!(slh_kp.public_key.bytes.as_slice(), &slh);
    }

    /// When regenerating static files: `PUBLISHED_VECTORS_DUMP=1 cargo test … dump_published`
    #[test]
    fn dump_published_vectors_if_env() {
        if std::env::var_os("PUBLISHED_VECTORS_DUMP").is_none() {
            return;
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("validator/pqc/data");
        std::fs::create_dir_all(&dir).expect("mkdir");
        for (name, build) in [
            (
                "schnorr_3c.json",
                schnorr_3c_export_json as fn() -> Result<Value, String>,
            ),
            ("core_hybrid.json", core_hybrid_export_json),
            ("kitchen_sink_3b.json", kitchen_sink_3b_export_json),
        ] {
            let v = build().unwrap_or_else(|e| panic!("{name}: {e}"));
            let path = dir.join(name);
            std::fs::write(&path, pretty_json(&v)).expect("write");
            // Manual --nocapture helper; clippy denies print_* in lib tests.
            #[expect(clippy::print_stderr)]
            {
                eprintln!("wrote {}", path.display());
            }
        }
    }
}
