//! Overload-model P2MR construction test vectors (converted from ref-impl `OP_SUBSTR` tags).
//!
//! JSON source: `test_vectors/p2mr_overload_construction.json`

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use bitcoin::ScriptBuf as BitcoinScriptBuf;
    use bitcoin::hashes::Hash;
    use bitcoin_p2mr_pqc::ScriptBuf;
    use bitcoin_p2mr_pqc::p2mr::{P2mrBuilder, P2mrControlBlock, P2mrError};
    use bitcoin_p2mr_pqc::taproot::{LeafVersion, TapLeafHash, TapTree};
    use hex::{FromHex, ToHex};
    use serde::Deserialize;

    use super::super::leaf_script::parse_leaf_script;
    use super::super::merkle::{control_block_bytes_for_enforcer, verify_merkle_path};
    use super::super::p2mr_output::validate_p2mr_output;

    const VECTORS_JSON: &str =
        include_str!("../../../test_vectors/p2mr_overload_construction.json");

    #[derive(Debug, Deserialize)]
    struct TestVectors {
        version: u32,
        model: String,
        #[serde(default)]
        given_script_encoding: Option<String>,
        #[serde(default)]
        expected_script_encoding: Option<String>,
        #[serde(default)]
        encoding_note: Option<String>,
        test_vectors: Vec<TestVector>,
    }

    #[derive(Debug, Deserialize)]
    struct TestVector {
        id: String,
        #[serde(default)]
        ref_impl_id: Option<String>,
        objective: String,
        given: TestVectorGiven,
        intermediary: TestVectorIntermediary,
        expected: TestVectorExpected,
    }

    #[derive(Debug, Deserialize)]
    struct TestVectorGiven {
        #[serde(rename = "scriptTree", default)]
        script_tree: Option<TVScriptTree>,
    }

    #[derive(Debug, Default, Deserialize)]
    struct TestVectorIntermediary {
        #[serde(default, rename = "leafHashes")]
        leaf_hashes: Vec<String>,
        #[serde(rename = "merkleRoot", default)]
        merkle_root: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    struct TestVectorExpected {
        #[serde(rename = "scriptPubKey", default)]
        script_pubkey: Option<String>,
        #[serde(default, rename = "scriptPathControlBlocks")]
        script_path_control_blocks: Option<Vec<String>>,
        #[serde(default)]
        error: Option<String>,
    }

    #[derive(Debug, Deserialize, Clone)]
    struct TVScriptLeaf {
        id: u8,
        script: String,
        #[serde(rename = "leafVersion")]
        leaf_version: u8,
        #[serde(default)]
        priv_key: Option<String>,
    }

    #[derive(Debug, Deserialize, Clone)]
    #[serde(untagged)]
    enum TVScriptTree {
        Leaf(TVScriptLeaf),
        Branch(Vec<TVScriptTree>),
    }

    #[derive(Debug, Copy, Clone, Eq, PartialEq)]
    enum Direction {
        Left,
        Right,
        Root,
    }

    impl TVScriptTree {
        fn traverse_right_subtree_first<F: FnMut(&TVScriptTree, u8, Direction)>(
            &self,
            depth: u8,
            direction: Direction,
            f: &mut F,
        ) {
            match self {
                TVScriptTree::Branch(children) => {
                    assert_eq!(children.len(), 2, "branch must have exactly two children");
                    let next_depth = depth + 1;
                    children[1].traverse_right_subtree_first(next_depth, Direction::Right, f);
                    children[0].traverse_right_subtree_first(next_depth, Direction::Left, f);
                }
                TVScriptTree::Leaf(_) => f(self, depth, direction),
            }
        }

        fn leaf_scripts_hex(&self) -> Vec<String> {
            let mut out = Vec::new();
            self.traverse_right_subtree_first(0, Direction::Root, &mut |node, _, _| {
                if let TVScriptTree::Leaf(leaf) = node {
                    out.push(leaf.script.clone());
                }
            });
            out
        }
    }

    struct DerivedP2mr {
        merkle_root_hex: String,
        leaf_hashes: Vec<String>,
        control_blocks: Vec<String>,
        script_pubkey_hex: String,
        leaf_scripts: Vec<BitcoinScriptBuf>,
    }

    /// True when `script_hex` is exactly `PUSH32 <pk> OP_SUBSTR`.
    fn is_simple_push32_substr_leaf(script_hex: &str) -> bool {
        let bytes = match Vec::from_hex(script_hex) {
            Ok(b) => b,
            Err(_) => return false,
        };
        bytes.len() == 34 && bytes[0] == 0x20 && bytes[33] == 0x7f
    }

    /// True when `script_hex` ends with a standalone `OP_SUBSTR` (0x7f) opcode.
    fn ends_with_substr_opcode(script_hex: &str) -> bool {
        let bytes = match Vec::from_hex(script_hex) {
            Ok(b) => b,
            Err(_) => return false,
        };
        bytes.last() == Some(&0x7f)
    }

    /// Convert ref-impl `OP_SUBSTR` (0x7f) tag leaves to overloaded `OP_CHECKSIG` (0xac).
    ///
    /// Only replaces a trailing standalone `0x7f` opcode; bytes inside push data are untouched.
    fn convert_substr_tag_to_checksig(script_hex: &str) -> String {
        let mut bytes = Vec::from_hex(script_hex).expect("valid script hex");
        if bytes.last() == Some(&0x7f) {
            bytes.pop();
            bytes.push(0xac);
        }
        bytes.encode_hex()
    }

    struct LeafPlacement {
        depth: u8,
        script: ScriptBuf,
        version: LeafVersion,
    }

    fn collect_leaf_placements(tree: &TVScriptTree) -> Vec<LeafPlacement> {
        let mut placements = Vec::new();
        tree.traverse_right_subtree_first(0, Direction::Root, &mut |node, depth, direction| {
            if let TVScriptTree::Leaf(leaf) = node {
                let script_hex = convert_substr_tag_to_checksig(&leaf.script);
                let script_bytes = Vec::from_hex(&script_hex).expect("valid converted script hex");
                let script = ScriptBuf::from_bytes(script_bytes);
                let version =
                    LeafVersion::from_consensus(leaf.leaf_version).expect("valid leaf version");
                // Match ref-impl `p2mr_pqc_construction.rs`: pass traversal `depth` (not depth+1).
                let _ = direction;
                placements.push(LeafPlacement {
                    depth,
                    script,
                    version,
                });
            }
        });
        placements
    }

    fn build_p2mr_from_script_tree(tree: &TVScriptTree) -> Result<DerivedP2mr, P2mrError> {
        let placements = collect_leaf_placements(tree);
        let mut builder = P2mrBuilder::new();
        for leaf in placements {
            builder = builder.add_leaf_with_ver(leaf.depth, leaf.script, leaf.version)?;
        }

        let spend_info = builder.clone().finalize()?;
        let merkle_root = spend_info.merkle_root.expect("merkle root");
        let tap_tree: TapTree = builder.into_inner().try_into_taptree().expect("tap tree");

        let mut leaf_hashes = Vec::new();
        let mut control_blocks = Vec::new();
        let mut leaf_scripts = Vec::new();

        for derived_leaf in tap_tree.script_leaves() {
            let script = derived_leaf.script().to_bytes();
            leaf_scripts.push(BitcoinScriptBuf::from_bytes(script.clone()));
            let leaf_hash = TapLeafHash::from_script(derived_leaf.script(), derived_leaf.version());
            leaf_hashes.push(leaf_hash.as_raw_hash().to_byte_array().encode_hex());

            let control_block = P2mrControlBlock {
                merkle_branch: derived_leaf.merkle_branch().clone(),
            };
            control_blocks.push(control_block.serialize().encode_hex());
        }

        let mut script_pubkey = vec![0x52, 0x20];
        script_pubkey.extend_from_slice(&merkle_root.to_byte_array());

        Ok(DerivedP2mr {
            merkle_root_hex: merkle_root.to_string(),
            leaf_hashes,
            control_blocks,
            script_pubkey_hex: script_pubkey.encode_hex(),
            leaf_scripts,
        })
    }

    fn load_vectors() -> TestVectors {
        let vectors: TestVectors = serde_json::from_str(VECTORS_JSON).expect("parse vectors JSON");
        assert_eq!(vectors.version, 1);
        assert_eq!(vectors.model, "overload_checksig");
        assert_eq!(
            vectors.given_script_encoding.as_deref(),
            Some("ref_impl_op_substr_tags")
        );
        assert_eq!(
            vectors.expected_script_encoding.as_deref(),
            Some("converted_op_checksig_at_test_load")
        );
        assert!(
            vectors
                .encoding_note
                .as_deref()
                .is_some_and(|note| note.contains("given.scriptTree") && note.contains("0x7f")),
            "encoding_note should clarify given vs converted scripts"
        );
        vectors
    }

    fn process_construction_vector(vector: &TestVector) {
        let tree = vector
            .given
            .script_tree
            .as_ref()
            .expect("construction vector requires scriptTree");

        let derived = build_p2mr_from_script_tree(tree).expect("build P2MR tree");

        let expected_root = vector
            .intermediary
            .merkle_root
            .as_ref()
            .expect("merkleRoot");
        assert_eq!(
            derived.merkle_root_hex, *expected_root,
            "merkle root mismatch for {}",
            vector.id
        );

        let expected_leaf_hashes: HashSet<_> =
            vector.intermediary.leaf_hashes.iter().cloned().collect();
        assert_eq!(
            derived.leaf_hashes.len(),
            expected_leaf_hashes.len(),
            "leaf count mismatch for {}",
            vector.id
        );
        for leaf_hash in &derived.leaf_hashes {
            assert!(
                expected_leaf_hashes.contains(leaf_hash),
                "unexpected leaf hash {leaf_hash} for {}",
                vector.id
            );
        }

        let expected_spk = vector
            .expected
            .script_pubkey
            .as_ref()
            .expect("scriptPubKey");
        assert_eq!(
            derived.script_pubkey_hex, *expected_spk,
            "scriptPubKey mismatch for {}",
            vector.id
        );

        let spk = BitcoinScriptBuf::from_bytes(Vec::from_hex(expected_spk).expect("spk hex"));
        let merkle_root = validate_p2mr_output(spk.as_script()).expect("valid p2mr output");
        assert_eq!(hex::encode(merkle_root), *expected_root);

        let expected_control_blocks: HashSet<_> = vector
            .expected
            .script_path_control_blocks
            .as_ref()
            .expect("scriptPathControlBlocks")
            .iter()
            .cloned()
            .collect();
        assert_eq!(
            derived.control_blocks.len(),
            expected_control_blocks.len(),
            "control block count mismatch for {}",
            vector.id
        );
        for (leaf_script, control_block_hex) in
            derived.leaf_scripts.iter().zip(&derived.control_blocks)
        {
            assert!(
                expected_control_blocks.contains(control_block_hex),
                "unexpected control block {control_block_hex} for {}",
                vector.id
            );
            let parse_result = parse_leaf_script(leaf_script.as_script());
            match &parse_result {
                Err(super::super::leaf_script::LeafScriptError::OpSubstrForbidden) => {
                    panic!("converted leaf still contains OP_SUBSTR for {}", vector.id);
                }
                Ok(parsed) => {
                    assert!(!parsed.sig_sites.is_empty());
                }
                Err(_) => {
                    // Construction-only leaves (CSV/SHA256/plain push) are not spendable under
                    // enforcer leaf rules; merkle commitment is still verified below.
                }
            }
            let control_block = control_block_bytes_for_enforcer(
                &Vec::from_hex(control_block_hex).expect("cb hex"),
            );
            verify_merkle_path(leaf_script.as_script(), merkle_root, &control_block)
                .unwrap_or_else(|e| {
                    panic!("verify_merkle_path failed for {}: {e}", vector.id);
                });
        }
    }

    #[test]
    fn overload_vectors_metadata() {
        let vectors = load_vectors();
        assert!(vectors.test_vectors.len() >= 5);
        for vector in &vectors.test_vectors {
            assert!(!vector.id.is_empty());
            assert!(!vector.objective.is_empty());
        }
    }

    #[test]
    fn overload_missing_leaf_script_tree_error() {
        let vectors = load_vectors();
        let vector = vectors
            .test_vectors
            .iter()
            .find(|v| v.id == "overload_missing_leaf_script_tree_error")
            .expect("missing-tree error vector");

        assert!(vector.given.script_tree.is_none());
        assert!(vector.intermediary.merkle_root.is_none());
        let expected_error = vector.expected.error.as_ref().expect("expected error");
        assert!(expected_error.contains("script tree"));

        // Runtime: empty P2mrBuilder cannot finalize to a merkle root.
        let empty_builder = P2mrBuilder::new();
        assert!(
            empty_builder.into_inner().try_into_node_info().is_err(),
            "P2mrBuilder without leaves must not finalize"
        );
    }

    #[test]
    fn overload_single_slh_leaf_script_tree() {
        let vectors = load_vectors();
        let vector = vectors
            .test_vectors
            .iter()
            .find(|v| v.id == "overload_single_slh_leaf")
            .expect("single SLH vector");
        process_construction_vector(vector);
    }

    #[test]
    fn overload_two_leaf_same_version() {
        let vectors = load_vectors();
        let vector = vectors
            .test_vectors
            .iter()
            .find(|v| v.id == "overload_two_leaf_same_version")
            .expect("two-leaf vector");
        process_construction_vector(vector);
    }

    #[test]
    fn overload_three_leaf_complex() {
        let vectors = load_vectors();
        let vector = vectors
            .test_vectors
            .iter()
            .find(|v| v.id == "overload_three_leaf_complex")
            .expect("three-leaf vector");
        process_construction_vector(vector);
    }

    #[test]
    fn overload_simple_lightning_contract() {
        let vectors = load_vectors();
        let vector = vectors
            .test_vectors
            .iter()
            .find(|v| v.id == "overload_simple_lightning_contract")
            .expect("lightning vector");
        process_construction_vector(vector);
    }

    #[test]
    fn overload_single_mldsa_leaf() {
        let vectors = load_vectors();
        let vector = vectors
            .test_vectors
            .iter()
            .find(|v| v.id == "overload_single_mldsa_leaf")
            .expect("ML-DSA vector");
        process_construction_vector(vector);
    }

    #[test]
    fn ref_impl_substr_leaves_rejected_by_parser() {
        let vectors = load_vectors();
        for vector in &vectors.test_vectors {
            let Some(tree) = &vector.given.script_tree else {
                continue;
            };
            for script_hex in tree.leaf_scripts_hex() {
                if !ends_with_substr_opcode(&script_hex) {
                    continue;
                }
                let raw = BitcoinScriptBuf::from_bytes(Vec::from_hex(&script_hex).unwrap());
                let converted = BitcoinScriptBuf::from_bytes(
                    Vec::from_hex(&convert_substr_tag_to_checksig(&script_hex)).unwrap(),
                );
                let raw_result = parse_leaf_script(raw.as_script());
                match raw_result {
                    Err(super::super::leaf_script::LeafScriptError::OpSubstrForbidden) => {}
                    Err(_) => {
                        // Complex leaves (e.g. lightning Alice: OP_CSV before OP_SUBSTR) may
                        // fail earlier; trailing 0x7f still marks ref-impl SUBSTR placement.
                    }
                    Ok(_) => {
                        panic!(
                            "raw ref-impl script with trailing OP_SUBSTR must not parse ({})",
                            vector.id
                        );
                    }
                }
                assert_ne!(
                    convert_substr_tag_to_checksig(&script_hex),
                    script_hex,
                    "trailing OP_SUBSTR must be converted for {}",
                    vector.id
                );
                match parse_leaf_script(converted.as_script()) {
                    Ok(_) => {}
                    Err(super::super::leaf_script::LeafScriptError::OpSubstrForbidden) => {
                        panic!(
                            "converted script still contains OP_SUBSTR for {}",
                            vector.id
                        );
                    }
                    Err(_) if is_simple_push32_substr_leaf(&script_hex) => {
                        panic!("simple PUSH32 OP_SUBSTR leaf must parse after conversion");
                    }
                    Err(_) => {
                        // Complex leaves (e.g. lightning Alice: CSV/DROP) may still fail
                        // enforcer leaf rules after conversion; merkle tests cover those.
                    }
                }
            }
        }
    }

    /// Helper to regenerate intermediary/expected fields after editing `given.scriptTree`.
    #[test]
    #[ignore = "run manually to refresh golden values: cargo test dump_overload_golden -- --ignored --nocapture"]
    fn dump_overload_golden() {
        let vectors = load_vectors();
        for vector in &vectors.test_vectors {
            if vector.expected.error.is_some() {
                println!("{}: error vector", vector.id);
                continue;
            }
            let tree = vector.given.script_tree.as_ref().expect("scriptTree");
            let derived = build_p2mr_from_script_tree(tree).expect("build");
            println!("=== {} ===", vector.id);
            println!("merkleRoot: {}", derived.merkle_root_hex);
            println!("leafHashes: {:?}", derived.leaf_hashes);
            println!("scriptPubKey: {}", derived.script_pubkey_hex);
            println!("scriptPathControlBlocks: {:?}", derived.control_blocks);
        }
    }
}
