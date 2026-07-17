//! Three-leaf P2MR tree: one overload-model leaf per signature algorithm.
//!
//! Per BIP360 overload addendum, algorithm exclusion uses different merkle leaves;
//! the enforcer validates only the revealed leaf script and its merkle path.

use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness,
    hashes::Hash as _,
    sighash::{Prevouts, SighashCache, TapSighashType},
    taproot::{LeafVersion, TapLeafHash},
    transaction::Version,
};
use bitcoin_p2mr_pqc::{
    TapScriptBuf as P2mrScriptBuf,
    p2mr::{P2MR_LEAF_VERSION, P2mrBuilder, P2mrControlBlock},
    taproot::{LeafVersion as P2mrLeafVersion, TapTree},
};
use bitcoinpqc::{KeyPair, generate_keypair, sign};

use super::{
    merkle::control_block_bytes_for_enforcer,
    signer_dev::{
        SignAlgorithm, append_sighash, build_checksig_leaf, p2mr_script_pubkey, validate_entropy,
        validate_output_value,
    },
};

/// Schnorr overload leaf (index 0).
pub const SCHNORR_LEAF_INDEX: usize = 0;
/// ML-DSA-44 overload leaf (index 1).
pub const MLDSA_LEAF_INDEX: usize = 1;
/// SLH-DSA-SHA2-128s overload leaf (index 2).
pub const SLH_LEAF_INDEX: usize = 2;

const LEAF_COUNT: usize = 3;

/// P2MR tree with Schnorr, ML-DSA-44, and SLH-DSA leaves (algorithm-per-leaf model).
#[derive(Clone, Debug)]
pub struct ThreeAlgorithmP2mrTree {
    keypairs: [KeyPair; LEAF_COUNT],
    leaf_scripts: [ScriptBuf; LEAF_COUNT],
    control_blocks: [Vec<u8>; LEAF_COUNT],
    merkle_root: [u8; 32],
    script_pubkey: ScriptBuf,
}

struct LeafPlacement {
    depth: u8,
    script: P2mrScriptBuf,
}

fn algorithm_for_leaf(leaf_index: usize) -> Result<SignAlgorithm, String> {
    match leaf_index {
        SCHNORR_LEAF_INDEX => Ok(SignAlgorithm::Schnorr),
        MLDSA_LEAF_INDEX => Ok(SignAlgorithm::Mldsa),
        SLH_LEAF_INDEX => Ok(SignAlgorithm::Slh),
        _ => Err(format!(
            "leaf index must be 0 (Schnorr), 1 (ML-DSA), or 2 (SLH); got {leaf_index}"
        )),
    }
}

fn generate_leaf_keypair(algorithm: SignAlgorithm, entropy: &[u8]) -> Result<KeyPair, String> {
    validate_entropy(entropy, algorithm)?;
    generate_keypair(algorithm.to_bitcoinpqc(), entropy)
        .map_err(|e| format!("key generation failed: {e}"))
}

impl ThreeAlgorithmP2mrTree {
    /// Build a nested three-leaf tree from fixed entropy (deterministic keys and merkle root).
    pub fn from_entropy(
        schnorr_entropy: &[u8; 32],
        mldsa_entropy: &[u8; 128],
        slh_entropy: &[u8; 128],
    ) -> Result<Self, String> {
        let schnorr_kp = generate_leaf_keypair(SignAlgorithm::Schnorr, schnorr_entropy)?;
        let mldsa_kp = generate_leaf_keypair(SignAlgorithm::Mldsa, mldsa_entropy)?;
        let slh_kp = generate_leaf_keypair(SignAlgorithm::Slh, slh_entropy)?;

        let schnorr_leaf = build_checksig_leaf(&schnorr_kp.public_key.bytes)?;
        let mldsa_leaf = build_checksig_leaf(&mldsa_kp.public_key.bytes)?;
        let slh_leaf = build_checksig_leaf(&slh_kp.public_key.bytes)?;

        let leaf_version =
            P2mrLeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("valid P2MR leaf version");

        // Tree shape matches `overload_three_leaf_complex`: [schnorr, [mldsa, slh]]
        // (root left = Schnorr; inner left = ML-DSA, inner right = SLH).
        // `P2mrBuilder` DFS visits right subtree first: SLH depth 2, ML-DSA depth 2,
        // then Schnorr depth 1. Placement order below matches that traversal.
        let placements = [
            LeafPlacement {
                depth: 2,
                script: P2mrScriptBuf::from_bytes(slh_leaf.as_bytes().to_vec()),
            },
            LeafPlacement {
                depth: 2,
                script: P2mrScriptBuf::from_bytes(mldsa_leaf.as_bytes().to_vec()),
            },
            LeafPlacement {
                depth: 1,
                script: P2mrScriptBuf::from_bytes(schnorr_leaf.as_bytes().to_vec()),
            },
        ];

        let mut builder = P2mrBuilder::new();
        for leaf in &placements {
            builder = builder
                .add_leaf_with_ver(leaf.depth, leaf.script.clone(), leaf_version)
                .map_err(|e| format!("P2mrBuilder add_leaf failed: {e:?}"))?;
        }

        let spend_info = builder
            .clone()
            .finalize()
            .map_err(|e| format!("finalize: {e:?}"))?;
        let merkle_root = spend_info.merkle_root.expect("merkle root").to_byte_array();
        let tap_tree: TapTree = builder
            .into_inner()
            .try_into_taptree()
            .map_err(|e| format!("tap tree: {e}"))?;

        let mut leaf_scripts = [ScriptBuf::new(), ScriptBuf::new(), ScriptBuf::new()];
        let mut control_blocks = [Vec::new(), Vec::new(), Vec::new()];

        let expected_scripts = [schnorr_leaf, mldsa_leaf, slh_leaf];
        for derived_leaf in tap_tree.script_leaves() {
            let script_bytes = derived_leaf.script().to_vec();
            let script = ScriptBuf::from_bytes(script_bytes.clone());
            let leaf_index = expected_scripts
                .iter()
                .position(|expected| expected.as_bytes() == script.as_bytes())
                .ok_or_else(|| "derived leaf script not in expected set".to_string())?;

            leaf_scripts[leaf_index] = script;
            let control_block = P2mrControlBlock {
                merkle_branch: derived_leaf.merkle_branch().to_owned(),
            };
            control_blocks[leaf_index] =
                control_block_bytes_for_enforcer(&control_block.serialize());
        }

        for (idx, script) in leaf_scripts.iter().enumerate() {
            if script.is_empty() {
                return Err(format!("missing derived leaf script for index {idx}"));
            }
            if control_blocks[idx].is_empty() {
                return Err(format!("missing control block for leaf index {idx}"));
            }
        }

        Ok(Self {
            keypairs: [schnorr_kp, mldsa_kp, slh_kp],
            leaf_scripts,
            control_blocks,
            merkle_root,
            script_pubkey: p2mr_script_pubkey(merkle_root),
        })
    }

    #[must_use]
    pub const fn leaf_count() -> usize {
        LEAF_COUNT
    }

    #[must_use]
    pub fn merkle_root(&self) -> [u8; 32] {
        self.merkle_root
    }

    #[must_use]
    pub fn script_pubkey(&self) -> &ScriptBuf {
        &self.script_pubkey
    }

    pub fn leaf_script(&self, leaf_index: usize) -> Result<&ScriptBuf, String> {
        algorithm_for_leaf(leaf_index)?;
        Ok(&self.leaf_scripts[leaf_index])
    }

    pub fn control_block(&self, leaf_index: usize) -> Result<&[u8], String> {
        algorithm_for_leaf(leaf_index)?;
        Ok(&self.control_blocks[leaf_index])
    }

    /// Sign a script-path spend revealing `leaf_index` (0=Schnorr, 1=ML-DSA, 2=SLH).
    pub fn sign_spend_for_leaf(
        &self,
        leaf_index: usize,
        sighash_type: TapSighashType,
        prevout: TxOut,
        spend_previous_output: OutPoint,
        output_value: u64,
        spend_destination: ScriptBuf,
    ) -> Result<Transaction, String> {
        algorithm_for_leaf(leaf_index)?;
        validate_output_value(output_value, prevout.value.to_sat())?;

        let leaf_script = &self.leaf_scripts[leaf_index];
        let control_block = self.control_blocks[leaf_index].clone();
        let keypair = &self.keypairs[leaf_index];

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

        let leaf_hash = TapLeafHash::from_script(
            leaf_script.as_script(),
            LeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("valid leaf version"),
        );
        let prevouts = Prevouts::All(std::slice::from_ref(&prevout));
        let mut cache = SighashCache::new(&unsigned_spend_tx);
        let sighash = cache
            .taproot_script_spend_signature_hash(0, &prevouts, leaf_hash, sighash_type)
            .map_err(|e| format!("sighash computation failed: {e}"))?;

        let signature = sign(&keypair.secret_key, sighash.as_byte_array())
            .map_err(|e| format!("signing failed: {e}"))?;
        let witness_sig = append_sighash(signature.bytes, sighash_type);

        let mut witness = Witness::new();
        witness.push(&witness_sig);
        witness.push(leaf_script.as_bytes());
        witness.push(&control_block);

        let mut signed_spend_tx = unsigned_spend_tx;
        signed_spend_tx.input[0].witness = witness;
        Ok(signed_spend_tx)
    }
}

/// Build a coinbase-funded three-leaf P2MR output and signed spend for block harnesses.
pub fn build_block_three_leaf_p2mr_spend(
    tree: &ThreeAlgorithmP2mrTree,
    leaf_index: usize,
    sighash_type: TapSighashType,
    coinbase_outpoint: OutPoint,
    prevout_value: u64,
    output_value: u64,
    spend_destination: ScriptBuf,
) -> Result<(Transaction, Transaction), String> {
    validate_output_value(output_value, prevout_value)?;

    let funding_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: coinbase_outpoint,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(prevout_value),
            script_pubkey: tree.script_pubkey().clone(),
        }],
    };
    let funding_txid = funding_tx.compute_txid();
    let prevout = funding_tx.output[0].clone();

    let spend_tx = tree.sign_spend_for_leaf(
        leaf_index,
        sighash_type,
        prevout,
        OutPoint {
            txid: funding_txid,
            vout: 0,
        },
        output_value,
        spend_destination,
    )?;

    Ok((funding_tx, spend_tx))
}

/// Replace the witness control block with one from a different leaf (same tree).
pub fn swap_multi_leaf_control_block(
    spend_tx: &mut Transaction,
    tree: &ThreeAlgorithmP2mrTree,
    wrong_leaf_index: usize,
) -> Result<(), String> {
    let witness = &spend_tx.input[0].witness;
    let sig = witness
        .nth(0)
        .ok_or_else(|| "witness missing signature".to_string())?;
    let leaf_script = witness
        .nth(1)
        .ok_or_else(|| "witness missing leaf script".to_string())?;

    let wrong_control = tree.control_block(wrong_leaf_index)?.to_vec();
    let mut bad_witness = Witness::new();
    bad_witness.push(sig);
    bad_witness.push(leaf_script);
    bad_witness.push(wrong_control);
    spend_tx.input[0].witness = bad_witness;
    Ok(())
}

#[cfg(test)]
mod tests {
    use hex::ToHex;

    use super::{
        super::spend::{SpendError, validate_p2mr_input_spend},
        *,
    };

    const SCHNORR_ENTROPY: [u8; 32] = [0x11; 32];
    const MLDSA_ENTROPY: [u8; 128] = [0x22; 128];
    const SLH_ENTROPY: [u8; 128] = [0x88; 128];

    fn build_tree() -> ThreeAlgorithmP2mrTree {
        ThreeAlgorithmP2mrTree::from_entropy(&SCHNORR_ENTROPY, &MLDSA_ENTROPY, &SLH_ENTROPY)
            .expect("build three-leaf tree")
    }

    fn funding_prevout(tree: &ThreeAlgorithmP2mrTree) -> TxOut {
        TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: tree.script_pubkey().clone(),
        }
    }

    #[test]
    fn three_leaf_merkle_root_is_deterministic_with_fixed_entropy() {
        let tree_a = build_tree();
        let tree_b = build_tree();
        assert_eq!(tree_a.merkle_root(), tree_b.merkle_root());
        // TDD regression lock: fixed entropy + overload_three_leaf_complex topology
        // must always yield this merkle root.
        assert_eq!(
            tree_a.merkle_root().encode_hex::<String>(),
            "741e59741422eab4fe6dcc3bb3c73ad4d5279a613b999381b3d47d4b05e02f9c"
        );
    }

    #[test]
    fn three_leaf_placement_depths_match_overload_shape() {
        let tree = build_tree();
        // Core wire layout: 0xc1 + 32 bytes per merkle sibling (no internal-key pad).
        let schnorr_cb = tree.control_block(SCHNORR_LEAF_INDEX).expect("schnorr cb");
        let mldsa_cb = tree.control_block(MLDSA_LEAF_INDEX).expect("mldsa cb");
        let slh_cb = tree.control_block(SLH_LEAF_INDEX).expect("slh cb");
        assert_eq!(schnorr_cb.len(), 1 + 32, "Schnorr at depth 1");
        assert_eq!(mldsa_cb.len(), 1 + 64, "ML-DSA at depth 2");
        assert_eq!(slh_cb.len(), 1 + 64, "SLH at depth 2");
        assert_eq!(schnorr_cb[0], 0xc1);
        assert_eq!(mldsa_cb[0], 0xc1);
        assert_eq!(slh_cb[0], 0xc1);
    }

    #[test]
    fn rejects_invalid_leaf_index() {
        let tree = build_tree();
        let prevout = funding_prevout(&tree);
        let err = tree
            .sign_spend_for_leaf(
                3,
                TapSighashType::All,
                prevout,
                OutPoint::null(),
                40_000,
                ScriptBuf::new(),
            )
            .unwrap_err();
        assert!(err.contains("leaf index must be 0"));
    }

    #[test]
    fn build_block_three_leaf_p2mr_spend_links_coinbase_to_signed_witness() {
        use bitcoin::{blockdata::script::Builder, locktime::absolute::LockTime};

        let tree = build_tree();
        let coinbase = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: Builder::new().push_int(1).into_script(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(100_000),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let coinbase_outpoint = OutPoint {
            txid: coinbase.compute_txid(),
            vout: 0,
        };

        let (funding_tx, spend_tx) = build_block_three_leaf_p2mr_spend(
            &tree,
            MLDSA_LEAF_INDEX,
            TapSighashType::All,
            coinbase_outpoint,
            50_000,
            40_000,
            ScriptBuf::new(),
        )
        .expect("build_block_three_leaf_p2mr_spend");

        assert_eq!(funding_tx.input[0].previous_output, coinbase_outpoint);
        assert_eq!(
            spend_tx.input[0].previous_output.txid,
            funding_tx.compute_txid()
        );
        assert_eq!(funding_tx.output[0].script_pubkey, *tree.script_pubkey());
        assert_eq!(spend_tx.input[0].witness.len(), 3);
    }

    #[test]
    fn sign_and_validate_spend_for_schnorr_leaf() {
        let tree = build_tree();
        let prevout = funding_prevout(&tree);
        let spend_tx = tree
            .sign_spend_for_leaf(
                SCHNORR_LEAF_INDEX,
                TapSighashType::All,
                prevout.clone(),
                OutPoint::null(),
                40_000,
                ScriptBuf::new(),
            )
            .expect("sign schnorr leaf");

        let prevouts = Prevouts::All(std::slice::from_ref(&prevout));
        validate_p2mr_input_spend(&spend_tx, 0, &prevout, &prevouts, &mut None)
            .expect("schnorr leaf spend validates");
    }

    #[test]
    fn sign_and_validate_spend_for_mldsa_leaf() {
        let tree = build_tree();
        let prevout = funding_prevout(&tree);
        let spend_tx = tree
            .sign_spend_for_leaf(
                MLDSA_LEAF_INDEX,
                TapSighashType::All,
                prevout.clone(),
                OutPoint::null(),
                40_000,
                ScriptBuf::new(),
            )
            .expect("sign mldsa leaf");

        let prevouts = Prevouts::All(std::slice::from_ref(&prevout));
        validate_p2mr_input_spend(&spend_tx, 0, &prevout, &prevouts, &mut None)
            .expect("mldsa leaf spend validates");
    }

    #[test]
    fn sign_and_validate_spend_for_slh_leaf() {
        let tree = build_tree();
        let prevout = funding_prevout(&tree);
        let spend_tx = tree
            .sign_spend_for_leaf(
                SLH_LEAF_INDEX,
                TapSighashType::All,
                prevout.clone(),
                OutPoint::null(),
                40_000,
                ScriptBuf::new(),
            )
            .expect("sign slh leaf");

        let prevouts = Prevouts::All(std::slice::from_ref(&prevout));
        validate_p2mr_input_spend(&spend_tx, 0, &prevout, &prevouts, &mut None)
            .expect("slh leaf spend validates");
    }

    #[test]
    fn wrong_control_block_rejects_merkle_root_mismatch() {
        let tree = build_tree();
        let prevout = funding_prevout(&tree);
        let mut spend_tx = tree
            .sign_spend_for_leaf(
                MLDSA_LEAF_INDEX,
                TapSighashType::All,
                prevout.clone(),
                OutPoint::null(),
                40_000,
                ScriptBuf::new(),
            )
            .expect("sign mldsa leaf");

        swap_multi_leaf_control_block(&mut spend_tx, &tree, SCHNORR_LEAF_INDEX)
            .expect("swap control block");

        let prevouts = Prevouts::All(std::slice::from_ref(&prevout));
        let err =
            validate_p2mr_input_spend(&spend_tx, 0, &prevout, &prevouts, &mut None).unwrap_err();
        assert!(matches!(
            err,
            SpendError::Merkle(super::super::merkle::MerkleError::MerkleRootMismatch)
        ));
    }
}
