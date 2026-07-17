//! P2MR merkle path verification via control blocks.
//!
//! # Control block wire layout (P2MR Core)
//!
//! Core uses `P2MR_CONTROL_BASE_SIZE = 1` (no internal key):
//!
//! ```text
//! control_block = 0xc1 || (32 * N branch nodes)
//! ```
//!
//! Single-leaf (empty branch) is exactly **1 byte** `0xc1`. Merkle path nodes start
//! immediately after byte 0. Current `bitcoin-p2mr-pqc` (`p2mr` branch) uses
//! `P2MR_CONTROL_BASE_SIZE = 1` natively. Older pins (rev 9093253) decoded with
//! Taproot's 33-byte base, which forced a zero pad and broke Core wire match.
//!
//! This module still decodes Core wire locally. Legacy padded form
//! (`0xc1 || [0u8; 32] || branch`) is accepted by detecting an all-zero 32-byte slot.

use bitcoin::{Script, hashes::Hash as _};
use bitcoin_p2mr_pqc::{
    TapScriptBuf,
    p2mr::P2MR_LEAF_VERSION,
    taproot::{LeafVersion, TapNodeHash, TapNodeHashExt as _},
};
use thiserror::Error;

use super::p2mr_output::{P2mrMerkleRoot, is_valid_p2mr_control_byte};

/// Core wire base size: control byte only (matches P2MR Core `P2MR_CONTROL_BASE_SIZE`).
const P2MR_CONTROL_BASE_SIZE_CORE: usize = 1;
/// Merkle branch node size in bytes.
const MERKLE_NODE_SIZE: usize = 32;
/// Legacy enforcer layout used a Taproot-style 33-byte header (control + zero pad).
const LEGACY_PADDED_BASE_SIZE: usize = 33;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MerkleError {
    #[error("invalid P2MR control block size")]
    InvalidControlBlockSize,
    #[error("invalid P2MR control byte (expected 0xc1)")]
    InvalidControlByte,
    #[error("failed to decode P2MR control block")]
    DecodeFailed,
    #[error("merkle root mismatch for leaf script")]
    MerkleRootMismatch,
}

/// Wire bytes for a P2MR control block from `P2mrControlBlock::serialize()`.
///
/// Serialize already emits Core layout (`0xc1 || branch nodes`, no internal-key pad).
/// Kept as a named helper so multi-leaf / vector builders share one call site.
#[must_use]
pub(crate) fn control_block_bytes_for_enforcer(serialized: &[u8]) -> Vec<u8> {
    if serialized.is_empty() {
        return vec![0xc1];
    }
    serialized.to_vec()
}

/// Slice of merkle branch node bytes from a control block (Core wire or legacy pad).
///
/// - **Core:** `bytes[0] == 0xc1`, branch at `bytes[1..]`, length multiple of 32.
/// - **Legacy pad:** `bytes[0] == 0xc1`, `bytes[1..33] == 0`, branch at `bytes[33..]`.
fn merkle_branch_bytes(control_block_bytes: &[u8]) -> Result<&[u8], MerkleError> {
    if control_block_bytes.is_empty() {
        return Err(MerkleError::InvalidControlBlockSize);
    }
    if !is_valid_p2mr_control_byte(control_block_bytes[0]) {
        return Err(MerkleError::InvalidControlByte);
    }

    // Prefer legacy-pad detection when the 32-byte slot after 0xc1 is all zeros so
    // mid-flight fixtures still verify. A real all-zero sibling hash is not expected.
    if control_block_bytes.len() >= LEGACY_PADDED_BASE_SIZE
        && control_block_bytes[P2MR_CONTROL_BASE_SIZE_CORE..LEGACY_PADDED_BASE_SIZE]
            .iter()
            .all(|&b| b == 0)
        && (control_block_bytes.len() - LEGACY_PADDED_BASE_SIZE).is_multiple_of(MERKLE_NODE_SIZE)
    {
        return Ok(&control_block_bytes[LEGACY_PADDED_BASE_SIZE..]);
    }

    let branch = &control_block_bytes[P2MR_CONTROL_BASE_SIZE_CORE..];
    if !branch.len().is_multiple_of(MERKLE_NODE_SIZE) {
        return Err(MerkleError::InvalidControlBlockSize);
    }
    Ok(branch)
}

/// Fold leaf script + control block into the committed merkle root (Core wire rules).
pub fn recompute_merkle_root(
    leaf_script: &Script,
    control_block_bytes: &[u8],
) -> Result<[u8; 32], MerkleError> {
    let branch = merkle_branch_bytes(control_block_bytes)?;
    let leaf_version =
        LeafVersion::from_consensus(P2MR_LEAF_VERSION).map_err(|_| MerkleError::DecodeFailed)?;
    let p2mr_script = TapScriptBuf::from_bytes(leaf_script.as_bytes().to_vec());
    let mut curr_hash = TapNodeHash::from_script(&p2mr_script, leaf_version);
    for chunk in branch.chunks_exact(MERKLE_NODE_SIZE) {
        let mut node = [0u8; MERKLE_NODE_SIZE];
        node.copy_from_slice(chunk);
        let sibling = TapNodeHash::assume_hidden(node);
        curr_hash = TapNodeHash::from_node_hashes(curr_hash, sibling);
    }
    Ok(curr_hash.to_byte_array())
}

/// Verify that `leaf_script` is committed to `merkle_root` via `control_block_bytes`.
pub fn verify_merkle_path(
    leaf_script: &Script,
    merkle_root: P2mrMerkleRoot,
    control_block_bytes: &[u8],
) -> Result<(), MerkleError> {
    let derived = recompute_merkle_root(leaf_script, control_block_bytes)?;
    if derived != merkle_root {
        return Err(MerkleError::MerkleRootMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use bitcoin::{ScriptBuf, hashes::Hash};
    use bitcoin_p2mr_pqc::{
        TapScriptBuf,
        p2mr::P2MR_LEAF_VERSION,
        taproot::{LeafVersion, TapNodeHash, TapNodeHashExt as _},
    };

    use super::*;

    fn sample_leaf_script() -> ScriptBuf {
        // Minimal OP_CHECKSIG-shaped leaf: PUSH32 <pk> OP_CHECKSIG
        let mut b = vec![0x20];
        b.extend_from_slice(&[1u8; 32]);
        b.push(0xac);
        ScriptBuf::from_bytes(b)
    }

    fn leaf_only_root(leaf: &ScriptBuf) -> [u8; 32] {
        let lv = LeafVersion::from_consensus(P2MR_LEAF_VERSION).unwrap();
        TapNodeHash::from_script(&TapScriptBuf::from_bytes(leaf.as_bytes().to_vec()), lv)
            .to_byte_array()
    }

    #[test]
    fn rejects_empty_control_block() {
        let script = bitcoin::Script::from_bytes(&[0x20; 33]);
        let root = [0u8; 32];
        assert!(matches!(
            verify_merkle_path(script, root, &[]),
            Err(MerkleError::InvalidControlBlockSize)
        ));
    }

    #[test]
    fn rejects_invalid_control_byte() {
        let script = bitcoin::Script::from_bytes(&[0x20; 33]);
        let root = [0u8; 32];
        assert!(matches!(
            verify_merkle_path(script, root, &[0xc0]),
            Err(MerkleError::InvalidControlByte)
        ));
    }

    #[test]
    fn core_wire_single_leaf_control_is_one_byte_c1() {
        let leaf = sample_leaf_script();
        let root = leaf_only_root(&leaf);
        let core = [0xc1u8];
        assert!(
            verify_merkle_path(leaf.as_script(), root, &core).is_ok(),
            "Core wire [0xc1] must verify leaf-only root"
        );
        assert_eq!(
            recompute_merkle_root(leaf.as_script(), &core).unwrap(),
            root
        );
    }

    #[test]
    fn legacy_padded_single_leaf_still_accepted() {
        // Transition compat: old enforcer builders used 0xc1 || 32 zero pad.
        let leaf = sample_leaf_script();
        let root = leaf_only_root(&leaf);
        let mut legacy = vec![0xc1];
        legacy.extend_from_slice(&[0u8; 32]);
        assert!(
            verify_merkle_path(leaf.as_script(), root, &legacy).is_ok(),
            "legacy padded single-leaf control should still verify (pad stripped)"
        );
    }

    #[test]
    fn non_zero_sibling_is_not_stripped_as_legacy_pad() {
        // Core multi-leaf depth-1: 0xc1 || one real sibling. Must fold the sibling.
        let leaf = sample_leaf_script();
        let leaf_hash = {
            let lv = LeafVersion::from_consensus(P2MR_LEAF_VERSION).unwrap();
            TapNodeHash::from_script(&TapScriptBuf::from_bytes(leaf.as_bytes().to_vec()), lv)
        };
        let sibling = TapNodeHash::assume_hidden([0xAB; 32]);
        let expected_root = TapNodeHash::from_node_hashes(leaf_hash, sibling).to_byte_array();

        let mut control = vec![0xc1];
        control.extend_from_slice(&[0xAB; 32]);
        assert_eq!(control.len(), 1 + 32);
        assert!(
            verify_merkle_path(leaf.as_script(), expected_root, &control).is_ok(),
            "non-zero 32-byte after 0xc1 is a real branch node, not legacy pad"
        );
        // Leaf-only root must not match when a real sibling is present.
        let leaf_only = leaf_only_root(&leaf);
        assert!(matches!(
            verify_merkle_path(leaf.as_script(), leaf_only, &control),
            Err(MerkleError::MerkleRootMismatch)
        ));
    }

    #[test]
    fn multi_leaf_core_wire_has_no_internal_key_pad() {
        // Two branch nodes: control = 0xc1 || 64 bytes (no 32-zero pad after 0xc1).
        let leaf = sample_leaf_script();
        let leaf_hash = {
            let lv = LeafVersion::from_consensus(P2MR_LEAF_VERSION).unwrap();
            TapNodeHash::from_script(&TapScriptBuf::from_bytes(leaf.as_bytes().to_vec()), lv)
        };
        let s1 = TapNodeHash::assume_hidden([0x11; 32]);
        let s2 = TapNodeHash::assume_hidden([0x22; 32]);
        let mid = TapNodeHash::from_node_hashes(leaf_hash, s1);
        let expected_root = TapNodeHash::from_node_hashes(mid, s2).to_byte_array();

        let mut control = vec![0xc1];
        control.extend_from_slice(&[0x11; 32]);
        control.extend_from_slice(&[0x22; 32]);
        assert_eq!(control.len(), 1 + 64, "Core multi-leaf: 1 + 32*N, no pad");
        assert!(verify_merkle_path(leaf.as_script(), expected_root, &control).is_ok());
    }

    #[test]
    fn control_block_bytes_for_enforcer_is_identity_on_serialize() {
        // Empty branch serialize is just 0xc1 (crate encode writes control byte + branch).
        let serialized_single = [0xc1u8];
        assert_eq!(
            control_block_bytes_for_enforcer(&serialized_single),
            vec![0xc1]
        );
        let mut serialized_branch = vec![0xc1];
        serialized_branch.extend_from_slice(&[0xCD; 32]);
        assert_eq!(
            control_block_bytes_for_enforcer(&serialized_branch),
            serialized_branch
        );
        // Empty input still yields a valid single-leaf control.
        assert_eq!(control_block_bytes_for_enforcer(&[]), vec![0xc1]);
    }

    #[test]
    fn rejects_non_multiple_of_32_branch_len() {
        let leaf = sample_leaf_script();
        let root = leaf_only_root(&leaf);
        // 1 + 16 bytes — not a valid branch length.
        let mut bad = vec![0xc1];
        bad.extend_from_slice(&[0u8; 16]);
        assert!(matches!(
            verify_merkle_path(leaf.as_script(), root, &bad),
            Err(MerkleError::InvalidControlBlockSize)
        ));
    }
}
