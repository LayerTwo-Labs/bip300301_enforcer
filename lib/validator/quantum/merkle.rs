//! P2MR merkle path verification via control blocks.

use bitcoin::Script;
use bitcoin_p2mr_pqc::p2mr::{P2MR_LEAF_VERSION, P2mrControlBlock};
use bitcoin_p2mr_pqc::taproot::{LeafVersion, TapNodeHash};
use thiserror::Error;

use super::p2mr_output::{P2mrMerkleRoot, is_valid_p2mr_control_byte};

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

/// Expand `P2mrControlBlock::serialize()` output to the wire layout this module decodes.
///
/// Serialization omits the 32-byte base padding between the control byte and merkle branch.
#[must_use]
pub(crate) fn control_block_bytes_for_enforcer(serialized: &[u8]) -> Vec<u8> {
    if serialized.is_empty() {
        return vec![0xc1];
    }
    let mut padded = vec![serialized[0]];
    padded.extend_from_slice(&[0u8; 32]);
    if serialized.len() > 1 {
        padded.extend_from_slice(&serialized[1..]);
    }
    padded
}

/// Verify that `leaf_script` is committed to `merkle_root` via `control_block_bytes`.
pub fn verify_merkle_path(
    leaf_script: &Script,
    merkle_root: P2mrMerkleRoot,
    control_block_bytes: &[u8],
) -> Result<(), MerkleError> {
    if control_block_bytes.is_empty() {
        return Err(MerkleError::InvalidControlBlockSize);
    }
    if !is_valid_p2mr_control_byte(control_block_bytes[0]) {
        return Err(MerkleError::InvalidControlByte);
    }
    let control_block =
        P2mrControlBlock::decode(control_block_bytes).map_err(|_| MerkleError::DecodeFailed)?;

    let p2mr_script = bitcoin_p2mr_pqc::Script::from_bytes(leaf_script.as_bytes());
    let leaf_version =
        LeafVersion::from_consensus(P2MR_LEAF_VERSION).map_err(|_| MerkleError::DecodeFailed)?;
    let mut curr_hash = TapNodeHash::from_script(p2mr_script, leaf_version);
    for sibling in &control_block.merkle_branch {
        curr_hash = TapNodeHash::from_node_hashes(curr_hash, *sibling);
    }
    let expected_root = TapNodeHash::assume_hidden(merkle_root);
    if curr_hash != expected_root {
        return Err(MerkleError::MerkleRootMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
