//! Structural validation of P2MR `scriptPubKey` outputs.

use bitcoin::Script;
use bitcoin_p2mr_pqc::p2mr::P2MR_CONTROL_BYTE;
use thiserror::Error;

/// Raw 32-byte P2MR merkle root extracted from a `scriptPubKey`.
pub type P2mrMerkleRoot = [u8; 32];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum P2mrOutputError {
    #[error("script is not a P2MR output (expected witness v2 program)")]
    NotP2mr,
    #[error("P2MR scriptPubKey has invalid length")]
    InvalidLength,
    #[error("P2MR merkle root extraction failed")]
    InvalidMerkleRoot,
}

/// Returns true if `script` is a BIP 360 P2MR `scriptPubKey` (witness version 2).
#[must_use]
pub fn is_p2mr_script(script: &Script) -> bool {
    let bytes = script.as_bytes();
    bytes.len() == 34 && bytes[0] == 0x52 && bytes[1] == 0x20
}

/// Validate structural correctness of a P2MR output script.
pub fn validate_p2mr_output(script: &Script) -> Result<P2mrMerkleRoot, P2mrOutputError> {
    if !is_p2mr_script(script) {
        return Err(P2mrOutputError::NotP2mr);
    }
    let bytes = script.as_bytes();
    // OP_2 (0x52) + OP_PUSHBYTES_32 (0x20) + 32-byte merkle root
    if bytes.len() != 34 || bytes[0] != 0x52 || bytes[1] != 0x20 {
        return Err(P2mrOutputError::InvalidLength);
    }
    let merkle_root_bytes: [u8; 32] = bytes[2..34]
        .try_into()
        .map_err(|_| P2mrOutputError::InvalidMerkleRoot)?;
    Ok(merkle_root_bytes)
}

/// P2MR control byte must have parity bit set (no key-path spend).
#[must_use]
pub fn is_valid_p2mr_control_byte(byte: u8) -> bool {
    byte == P2MR_CONTROL_BYTE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_merkle_root() {
        let root = [0xAB; 32];
        let mut bytes = vec![0x52, 0x20];
        bytes.extend_from_slice(&root);
        let script = bitcoin::ScriptBuf::from_bytes(bytes);
        let extracted = validate_p2mr_output(script.as_script()).unwrap();
        assert_eq!(extracted, root);
    }

    #[test]
    fn non_p2mr_script_rejected() {
        let script = bitcoin::ScriptBuf::from_bytes(vec![0x00, 0x14]);
        assert!(matches!(
            validate_p2mr_output(script.as_script()),
            Err(P2mrOutputError::NotP2mr)
        ));
    }
}
