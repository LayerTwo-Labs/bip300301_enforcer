//! Minimal tapscript parser for overloaded OP_CHECKSIG-family opcodes in P2MR leaves.

use bitcoin::Script;
use bitcoin::blockdata::opcodes::Opcode;
use bitcoin::blockdata::opcodes::all::{
    OP_BOOLAND, OP_CHECKMULTISIG, OP_CHECKMULTISIGVERIFY, OP_CHECKSIG, OP_CHECKSIGADD,
    OP_CHECKSIGVERIFY, OP_PUSHBYTES_0, OP_PUSHBYTES_32, OP_PUSHNUM_1, OP_PUSHNUM_16, OP_SUBSTR,
    OP_VERIFY,
};
use bitcoin::blockdata::script::Instruction;
use thiserror::Error;

use super::limits::{
    MAX_P2MR_LEAF_SCRIPT_SIZE, ML_DSA_44_PUBLIC_KEY_SIZE, SLH_DSA_PUBLIC_KEY_SIZE,
};

/// Overloaded tapscript signature-check opcodes (BIP 342 family + multisig pair).
const SIG_CHECK_OPCODES: [Opcode; 5] = [
    OP_CHECKSIG,
    OP_CHECKSIGVERIFY,
    OP_CHECKSIGADD,
    OP_CHECKMULTISIG,
    OP_CHECKMULTISIGVERIFY,
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LeafScriptError {
    #[error("P2MR leaf script uses OP_SUBSTR (0x7f); use overloaded OP_CHECKSIG instead")]
    OpSubstrForbidden,
    #[error("invalid or truncated push in P2MR leaf script")]
    InvalidPush,
    #[error("invalid opcode in P2MR leaf script")]
    InvalidOpcode,
    #[error("forbidden opcode {opcode} in P2MR leaf script")]
    ForbiddenOpcode { opcode: Opcode },
    #[error("P2MR leaf script contains no overloaded signature-check opcode")]
    NoSigCheckOpcode,
    #[error("signature-check opcode missing preceding pubkey push")]
    MissingPubkeyPush,
    #[error("invalid pubkey size {size} at signature-check site (expected 32 or 1312 bytes)")]
    InvalidPubkeySize { size: usize },
    #[error(
        "witness signature count {witness_sigs} does not match script signature sites {script_sites}"
    )]
    WitnessSigCountMismatch {
        witness_sigs: usize,
        script_sites: usize,
    },
    #[error("OP_CHECKMULTISIG missing preceding OP_N pubkey count")]
    MultisigMissingKeyCount,
    #[error("OP_CHECKMULTISIG expects {expected} pubkey pushes, found {found} in script")]
    MultisigPubkeyCountMismatch { expected: usize, found: usize },
    #[error("P2MR leaf script has OP_N pubkey count without OP_CHECKMULTISIG")]
    DanglingMultisigKeyCount,
    #[error("P2MR leaf script exceeds maximum size")]
    LeafScriptTooLarge,
    #[error("empty P2MR leaf script")]
    EmptyScript,
}

/// One overloaded signature-check site extracted from a leaf script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigCheckSite {
    /// Public key bytes pushed immediately before the signature-check opcode.
    pub pubkey: Vec<u8>,
    /// Opcode at this site (for diagnostics; verification uses pubkey+sig sizes).
    pub opcode: Opcode,
}

/// Parsed P2MR leaf script with signature-check sites in execution order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedLeafScript {
    pub sig_sites: Vec<SigCheckSite>,
}

fn is_sig_check_opcode(opcode: Opcode) -> bool {
    SIG_CHECK_OPCODES.contains(&opcode)
}

fn is_pushnum_opcode(opcode: Opcode) -> bool {
    let code = opcode.to_u8();
    code >= OP_PUSHNUM_1.to_u8() && code <= OP_PUSHNUM_16.to_u8()
}

fn decode_pushnum(opcode: Opcode) -> Option<usize> {
    let code = opcode.to_u8();
    if is_pushnum_opcode(opcode) {
        Some((code - OP_PUSHNUM_1.to_u8() + 1) as usize)
    } else {
        None
    }
}

fn is_allowed_between_sig_sites(opcode: Opcode) -> bool {
    matches!(opcode, OP_BOOLAND | OP_VERIFY | OP_PUSHBYTES_0) || is_pushnum_opcode(opcode)
}

fn validate_pubkey_size(pubkey: &[u8]) -> Result<(), LeafScriptError> {
    let size = pubkey.len();
    if size == SLH_DSA_PUBLIC_KEY_SIZE || size == ML_DSA_44_PUBLIC_KEY_SIZE {
        Ok(())
    } else {
        Err(LeafScriptError::InvalidPubkeySize { size })
    }
}

fn push_single_sig_site(
    sig_sites: &mut Vec<SigCheckSite>,
    pubkey: Vec<u8>,
    opcode: Opcode,
) -> Result<(), LeafScriptError> {
    validate_pubkey_size(&pubkey)?;
    sig_sites.push(SigCheckSite { pubkey, opcode });
    Ok(())
}

fn push_multisig_sites(
    sig_sites: &mut Vec<SigCheckSite>,
    push_buffer: &mut Vec<Vec<u8>>,
    key_count: usize,
    opcode: Opcode,
) -> Result<(), LeafScriptError> {
    if push_buffer.len() < key_count {
        return Err(LeafScriptError::MultisigPubkeyCountMismatch {
            expected: key_count,
            found: push_buffer.len(),
        });
    }
    let start = push_buffer.len() - key_count;
    for pubkey in push_buffer.drain(start..) {
        push_single_sig_site(sig_sites, pubkey, opcode)?;
    }
    Ok(())
}

/// Walk `script` and extract overloaded signature-check sites in execution order.
///
/// Supports `PUSH <pubkey> OP_CHECKSIG` / `OP_CHECKSIGVERIFY` / `OP_CHECKSIGADD`,
/// classic `OP_0 PUSH <pk>… OP_N OP_CHECKMULTISIG` patterns, and trailing
/// `OP_BOOLAND` / `OP_VERIFY` / `OP_TRUE` combinators. Rejects `OP_SUBSTR` (0x7f).
pub fn parse_leaf_script(script: &Script) -> Result<ParsedLeafScript, LeafScriptError> {
    let bytes = script.as_bytes();
    if bytes.is_empty() {
        return Err(LeafScriptError::EmptyScript);
    }
    if bytes.len() > MAX_P2MR_LEAF_SCRIPT_SIZE {
        return Err(LeafScriptError::LeafScriptTooLarge);
    }

    let mut sig_sites = Vec::new();
    let mut last_push: Option<Vec<u8>> = None;
    let mut push_buffer: Vec<Vec<u8>> = Vec::new();
    let mut pending_multisig_keys: Option<usize> = None;

    for result in script.instructions() {
        let instruction = result.map_err(|_| LeafScriptError::InvalidOpcode)?;
        match instruction {
            Instruction::PushBytes(push) => {
                let data = push.as_bytes().to_vec();
                // OP_0 (empty push) is the CHECKMULTISIG dummy; not a pubkey.
                if data.is_empty() {
                    last_push = None;
                } else {
                    push_buffer.push(data);
                    last_push = push_buffer.last().cloned();
                }
            }
            Instruction::Op(opcode) => {
                if opcode == OP_SUBSTR {
                    return Err(LeafScriptError::OpSubstrForbidden);
                }
                if is_sig_check_opcode(opcode) {
                    if opcode == OP_CHECKMULTISIG || opcode == OP_CHECKMULTISIGVERIFY {
                        let key_count = pending_multisig_keys
                            .take()
                            .ok_or(LeafScriptError::MultisigMissingKeyCount)?;
                        push_multisig_sites(&mut sig_sites, &mut push_buffer, key_count, opcode)?;
                        last_push = None;
                        continue;
                    }

                    let pubkey = last_push.take().ok_or(LeafScriptError::MissingPubkeyPush)?;
                    push_single_sig_site(&mut sig_sites, pubkey, opcode)?;
                    push_buffer.clear();
                    pending_multisig_keys = None;
                    continue;
                }

                if let Some(key_count) = decode_pushnum(opcode) {
                    pending_multisig_keys = Some(key_count);
                    last_push = None;
                    continue;
                }

                if is_allowed_between_sig_sites(opcode) {
                    last_push = None;
                } else {
                    return Err(LeafScriptError::ForbiddenOpcode { opcode });
                }
            }
        }
    }

    if pending_multisig_keys.is_some() {
        return Err(LeafScriptError::DanglingMultisigKeyCount);
    }

    if sig_sites.is_empty() {
        return Err(LeafScriptError::NoSigCheckOpcode);
    }

    Ok(ParsedLeafScript { sig_sites })
}

/// Ensure `witness_sig_count` matches the number of signature-check sites in `script`.
pub fn validate_witness_sig_count(
    script: &Script,
    witness_sig_count: usize,
) -> Result<ParsedLeafScript, LeafScriptError> {
    let parsed = parse_leaf_script(script)?;
    let script_sites = parsed.sig_sites.len();
    if witness_sig_count != script_sites {
        return Err(LeafScriptError::WitnessSigCountMismatch {
            witness_sigs: witness_sig_count,
            script_sites,
        });
    }
    Ok(parsed)
}

/// Build a Schnorr-only leaf: `PUSH32 <pk> OP_CHECKSIG`.
#[cfg(test)]
pub(crate) fn build_push32_checksig_leaf(pubkey: &[u8; 32]) -> bitcoin::ScriptBuf {
    use bitcoin::blockdata::script::Builder;
    Builder::new()
        .push_slice(pubkey)
        .push_opcode(OP_CHECKSIG)
        .into_script()
}

/// Build a Schnorr-only leaf: `PUSH32 <pk> OP_CHECKSIGVERIFY`.
#[cfg(test)]
pub(crate) fn build_push32_checksigverify_leaf(pubkey: &[u8; 32]) -> bitcoin::ScriptBuf {
    use bitcoin::blockdata::script::Builder;
    Builder::new()
        .push_slice(pubkey)
        .push_opcode(OP_CHECKSIGVERIFY)
        .into_script()
}

/// Build a Schnorr-only leaf: `PUSH32 <pk> OP_CHECKSIGADD`.
#[cfg(test)]
pub(crate) fn build_push32_checksigadd_leaf(pubkey: &[u8; 32]) -> bitcoin::ScriptBuf {
    use bitcoin::blockdata::script::Builder;
    Builder::new()
        .push_slice(pubkey)
        .push_opcode(OP_CHECKSIGADD)
        .into_script()
}

/// Build a 2-of-2 multisig leaf: `OP_0 PUSH32 <pk1> PUSH32 <pk2> OP_2 OP_CHECKMULTISIG`.
#[cfg(test)]
pub(crate) fn build_2of2_multisig_leaf(pk1: &[u8; 32], pk2: &[u8; 32]) -> bitcoin::ScriptBuf {
    use bitcoin::blockdata::opcodes::all::OP_PUSHNUM_2;
    use bitcoin::blockdata::script::Builder;
    Builder::new()
        .push_opcode(OP_PUSHBYTES_0)
        .push_slice(pk1)
        .push_slice(pk2)
        .push_opcode(OP_PUSHNUM_2)
        .push_opcode(OP_CHECKMULTISIG)
        .into_script()
}

/// Build a 2-of-2 multisig leaf with `OP_CHECKMULTISIGVERIFY`.
#[cfg(test)]
pub(crate) fn build_2of2_multisigverify_leaf(pk1: &[u8; 32], pk2: &[u8; 32]) -> bitcoin::ScriptBuf {
    use bitcoin::blockdata::opcodes::all::OP_PUSHNUM_2;
    use bitcoin::blockdata::script::Builder;
    Builder::new()
        .push_opcode(OP_PUSHBYTES_0)
        .push_slice(pk1)
        .push_slice(pk2)
        .push_opcode(OP_PUSHNUM_2)
        .push_opcode(OP_CHECKMULTISIGVERIFY)
        .into_script()
}

/// Build an ML-DSA leaf: `PUSHDATA2(1312) <pk> OP_CHECKSIG`.
#[cfg(test)]
pub(crate) fn build_mldsa_checksig_leaf(pubkey: &[u8]) -> bitcoin::ScriptBuf {
    let mut bytes = Vec::with_capacity(3 + pubkey.len() + 1);
    bytes.push(0x4d); // OP_PUSHDATA2
    bytes.extend_from_slice(&(pubkey.len() as u16).to_le_bytes());
    bytes.extend_from_slice(pubkey);
    bytes.push(OP_CHECKSIG.to_u8());
    bitcoin::ScriptBuf::from_bytes(bytes)
}

/// Build hybrid EC+SLH via `OP_CHECKSIGADD` + `OP_CHECKSIG` + `OP_BOOLAND OP_VERIFY`.
#[cfg(test)]
pub(crate) fn build_hybrid_ec_slh_checksigadd_leaf(
    ec_pk: &[u8; 32],
    slh_pk: &[u8; 32],
) -> bitcoin::ScriptBuf {
    use bitcoin::blockdata::script::Builder;
    Builder::new()
        .push_slice(ec_pk)
        .push_opcode(OP_CHECKSIGADD)
        .push_slice(slh_pk)
        .push_opcode(OP_CHECKSIG)
        .push_opcode(OP_BOOLAND)
        .push_opcode(OP_VERIFY)
        .into_script()
}

/// Build hybrid EC+SLH via `OP_0 PUSH ec_pk PUSH slh_pk OP_2 OP_CHECKMULTISIG`.
#[cfg(test)]
pub(crate) fn build_hybrid_ec_slh_multisig_leaf(
    ec_pk: &[u8; 32],
    slh_pk: &[u8; 32],
) -> bitcoin::ScriptBuf {
    build_2of2_multisig_leaf(ec_pk, slh_pk)
}

/// Build kitchen-sink leaf: Schnorr + ML-DSA-44 + SLH via three `OP_CHECKSIG` sites.
///
/// Script layout:
/// `PUSH32 <schnorr_pk> OP_CHECKSIG PUSHDATA2 <mldsa_pk> OP_CHECKSIG PUSH32 <slh_pk> OP_CHECKSIG OP_BOOLAND OP_BOOLAND OP_VERIFY`
pub(crate) fn build_kitchen_sink_leaf(
    ec_pk: &[u8; 32],
    mldsa_pk: &[u8],
    slh_pk: &[u8; 32],
) -> bitcoin::ScriptBuf {
    let mut bytes = Vec::with_capacity(3 + 32 + 1 + 3 + mldsa_pk.len() + 1 + 3 + 32 + 4);
    bytes.push(OP_PUSHBYTES_32.to_u8());
    bytes.extend_from_slice(ec_pk);
    bytes.push(OP_CHECKSIG.to_u8());
    bytes.push(0x4d); // OP_PUSHDATA2
    bytes.extend_from_slice(&(mldsa_pk.len() as u16).to_le_bytes());
    bytes.extend_from_slice(mldsa_pk);
    bytes.push(OP_CHECKSIG.to_u8());
    bytes.push(OP_PUSHBYTES_32.to_u8());
    bytes.extend_from_slice(slh_pk);
    bytes.push(OP_CHECKSIG.to_u8());
    bytes.push(OP_BOOLAND.to_u8());
    bytes.push(OP_BOOLAND.to_u8());
    bytes.push(OP_VERIFY.to_u8());
    bitcoin::ScriptBuf::from_bytes(bytes)
}

/// Build hybrid EC+SLH leaf: two `PUSH32 OP_CHECKSIG` + `OP_BOOLAND OP_VERIFY`.
pub(crate) fn build_hybrid_ec_slh_leaf(ec_pk: &[u8; 32], slh_pk: &[u8; 32]) -> bitcoin::ScriptBuf {
    use bitcoin::blockdata::script::Builder;
    Builder::new()
        .push_slice(ec_pk)
        .push_opcode(OP_CHECKSIG)
        .push_slice(slh_pk)
        .push_opcode(OP_CHECKSIG)
        .push_opcode(OP_BOOLAND)
        .push_opcode(OP_VERIFY)
        .into_script()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::blockdata::opcodes::all::{
        OP_IF, OP_NOTIF, OP_PUSHBYTES_32, OP_PUSHNUM_2, OP_RETURN,
    };

    #[test]
    fn parses_schnorr_only_leaf() {
        let mut bytes = vec![OP_PUSHBYTES_32.to_u8()];
        bytes.extend_from_slice(&[0xCD; 32]);
        bytes.push(OP_CHECKSIG.to_u8());
        let script = bitcoin::ScriptBuf::from_bytes(bytes);
        let parsed = parse_leaf_script(script.as_script()).unwrap();
        assert_eq!(parsed.sig_sites.len(), 1);
        assert_eq!(parsed.sig_sites[0].pubkey.len(), 32);
        assert_eq!(parsed.sig_sites[0].opcode, OP_CHECKSIG);
    }

    #[test]
    fn parses_hybrid_ec_slh_leaf() {
        let script = build_hybrid_ec_slh_leaf(&[0x11; 32], &[0x22; 32]);
        let parsed = parse_leaf_script(script.as_script()).unwrap();
        assert_eq!(parsed.sig_sites.len(), 2);
        assert_eq!(parsed.sig_sites[0].pubkey, vec![0x11; 32]);
        assert_eq!(parsed.sig_sites[1].pubkey, vec![0x22; 32]);
    }

    #[test]
    fn parses_kitchen_sink_leaf() {
        let script =
            build_kitchen_sink_leaf(&[0x11; 32], &[0x22; ML_DSA_44_PUBLIC_KEY_SIZE], &[0x33; 32]);
        let parsed = parse_leaf_script(script.as_script()).unwrap();
        assert_eq!(parsed.sig_sites.len(), 3);
        assert_eq!(parsed.sig_sites[0].pubkey, vec![0x11; 32]);
        assert_eq!(
            parsed.sig_sites[1].pubkey,
            vec![0x22; ML_DSA_44_PUBLIC_KEY_SIZE]
        );
        assert_eq!(parsed.sig_sites[2].pubkey, vec![0x33; 32]);
    }

    #[test]
    fn rejects_op_substr_opcode() {
        let mut bytes = vec![OP_PUSHBYTES_32.to_u8()];
        bytes.extend(std::iter::repeat_n(0xAB, 32));
        bytes.push(OP_SUBSTR.to_u8());
        let script = bitcoin::ScriptBuf::from_bytes(bytes);
        assert_eq!(
            parse_leaf_script(script.as_script()),
            Err(LeafScriptError::OpSubstrForbidden)
        );
    }

    #[test]
    fn allows_0x7f_inside_pubkey_push_data() {
        let mut pk = [0u8; 32];
        pk[0] = 0x7f;
        let script = build_push32_checksig_leaf(&pk);
        parse_leaf_script(script.as_script()).expect("0x7f in push data is not OP_SUBSTR");
    }

    #[test]
    fn rejects_missing_pubkey_push() {
        let script = bitcoin::ScriptBuf::from_bytes(vec![OP_CHECKSIG.to_u8()]);
        assert_eq!(
            parse_leaf_script(script.as_script()),
            Err(LeafScriptError::MissingPubkeyPush)
        );
    }

    #[test]
    fn rejects_invalid_pubkey_size() {
        let mut bytes = vec![0x04]; // push 4 bytes
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.push(OP_CHECKSIG.to_u8());
        let script = bitcoin::ScriptBuf::from_bytes(bytes);
        assert_eq!(
            parse_leaf_script(script.as_script()),
            Err(LeafScriptError::InvalidPubkeySize { size: 4 })
        );
    }

    #[test]
    fn witness_sig_count_must_match_sites() {
        let script = build_hybrid_ec_slh_leaf(&[1; 32], &[2; 32]);
        assert!(validate_witness_sig_count(script.as_script(), 2).is_ok());
        assert_eq!(
            validate_witness_sig_count(script.as_script(), 1),
            Err(LeafScriptError::WitnessSigCountMismatch {
                witness_sigs: 1,
                script_sites: 2,
            })
        );
    }

    #[test]
    fn witness_sig_count_must_match_kitchen_sink_sites() {
        let script =
            build_kitchen_sink_leaf(&[0x11; 32], &[0x22; ML_DSA_44_PUBLIC_KEY_SIZE], &[0x33; 32]);
        assert!(validate_witness_sig_count(script.as_script(), 3).is_ok());
        assert_eq!(
            validate_witness_sig_count(script.as_script(), 2),
            Err(LeafScriptError::WitnessSigCountMismatch {
                witness_sigs: 2,
                script_sites: 3,
            })
        );
    }

    #[test]
    fn parses_checksigverify_leaf() {
        let script = build_push32_checksigverify_leaf(&[0xEE; 32]);
        let parsed = parse_leaf_script(script.as_script()).unwrap();
        assert_eq!(parsed.sig_sites.len(), 1);
        assert_eq!(parsed.sig_sites[0].opcode, OP_CHECKSIGVERIFY);
    }

    #[test]
    fn parses_checksigadd_leaf() {
        let script = build_push32_checksigadd_leaf(&[0xAA; 32]);
        let parsed = parse_leaf_script(script.as_script()).unwrap();
        assert_eq!(parsed.sig_sites.len(), 1);
        assert_eq!(parsed.sig_sites[0].opcode, OP_CHECKSIGADD);
    }

    #[test]
    fn parses_2of2_multisig_leaf() {
        let script = build_2of2_multisig_leaf(&[0x11; 32], &[0x22; 32]);
        let parsed = parse_leaf_script(script.as_script()).unwrap();
        assert_eq!(parsed.sig_sites.len(), 2);
        assert_eq!(parsed.sig_sites[0].pubkey, vec![0x11; 32]);
        assert_eq!(parsed.sig_sites[1].pubkey, vec![0x22; 32]);
        assert_eq!(parsed.sig_sites[0].opcode, OP_CHECKMULTISIG);
        assert_eq!(parsed.sig_sites[1].opcode, OP_CHECKMULTISIG);
        assert!(validate_witness_sig_count(script.as_script(), 2).is_ok());
    }

    #[test]
    fn parses_2of2_multisigverify_leaf() {
        let script = build_2of2_multisigverify_leaf(&[0x33; 32], &[0x44; 32]);
        let parsed = parse_leaf_script(script.as_script()).unwrap();
        assert_eq!(parsed.sig_sites.len(), 2);
        assert_eq!(parsed.sig_sites[0].opcode, OP_CHECKMULTISIGVERIFY);
    }

    #[test]
    fn rejects_multisig_without_key_count() {
        use bitcoin::blockdata::script::Builder;
        let script = Builder::new()
            .push_slice([0xCD; 32])
            .push_opcode(OP_CHECKMULTISIG)
            .into_script();
        assert_eq!(
            parse_leaf_script(script.as_script()),
            Err(LeafScriptError::MultisigMissingKeyCount)
        );
    }

    #[test]
    fn rejects_bitcoin_style_one_of_two_with_single_witness_sig() {
        // CUSF overload: OP_2 multisig requires 2 witness sigs (N sites), not Bitcoin 1-of-2.
        let script = build_2of2_multisig_leaf(&[0x11; 32], &[0x22; 32]);
        assert_eq!(
            validate_witness_sig_count(script.as_script(), 1),
            Err(LeafScriptError::WitnessSigCountMismatch {
                witness_sigs: 1,
                script_sites: 2,
            })
        );
    }

    #[test]
    fn rejects_dangling_op_n_without_multisig() {
        use bitcoin::blockdata::script::Builder;
        let script = Builder::new()
            .push_slice([0xCD; 32])
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_PUSHNUM_2)
            .into_script();
        assert_eq!(
            parse_leaf_script(script.as_script()),
            Err(LeafScriptError::DanglingMultisigKeyCount)
        );
    }

    #[test]
    fn rejects_multisig_pubkey_count_mismatch() {
        use bitcoin::blockdata::script::Builder;
        let script = Builder::new()
            .push_opcode(OP_PUSHBYTES_0)
            .push_slice([0xCD; 32])
            .push_opcode(OP_PUSHNUM_2)
            .push_opcode(OP_CHECKMULTISIG)
            .into_script();
        assert_eq!(
            parse_leaf_script(script.as_script()),
            Err(LeafScriptError::MultisigPubkeyCountMismatch {
                expected: 2,
                found: 1,
            })
        );
    }

    #[test]
    fn rejects_forbidden_op_if() {
        use bitcoin::blockdata::script::Builder;
        let script = Builder::new()
            .push_slice([0xCD; 32])
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_IF)
            .into_script();
        assert!(matches!(
            parse_leaf_script(script.as_script()),
            Err(LeafScriptError::ForbiddenOpcode { opcode }) if opcode == OP_IF
        ));
    }

    #[test]
    fn rejects_forbidden_op_return() {
        use bitcoin::blockdata::script::Builder;
        let script = Builder::new()
            .push_slice([0xCD; 32])
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_RETURN)
            .into_script();
        assert!(matches!(
            parse_leaf_script(script.as_script()),
            Err(LeafScriptError::ForbiddenOpcode { opcode }) if opcode == OP_RETURN
        ));
    }

    #[test]
    fn rejects_forbidden_op_notif() {
        use bitcoin::blockdata::script::Builder;
        let script = Builder::new()
            .push_slice([0xCD; 32])
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_NOTIF)
            .into_script();
        assert!(matches!(
            parse_leaf_script(script.as_script()),
            Err(LeafScriptError::ForbiddenOpcode { opcode }) if opcode == OP_NOTIF
        ));
    }

    #[test]
    fn parses_mldsa_pushdata2_leaf() {
        let pk = vec![0xAB; ML_DSA_44_PUBLIC_KEY_SIZE];
        let script = build_mldsa_checksig_leaf(&pk);
        let parsed = parse_leaf_script(script.as_script()).unwrap();
        assert_eq!(parsed.sig_sites.len(), 1);
        assert_eq!(parsed.sig_sites[0].pubkey.len(), ML_DSA_44_PUBLIC_KEY_SIZE);
    }
}
