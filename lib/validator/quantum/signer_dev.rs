//! Shared P2MR script-path signing helpers for the `p2mr_signer` example and tests.
//!
//! **Demo-only:** deterministic entropy on the CLI is visible in process listings and shell
//! history. Never use mainnet secrets with this tooling.

use bitcoin::blockdata::opcodes::all::OP_CHECKSIG;
use bitcoin::blockdata::script::Builder;
use bitcoin::consensus::Encodable;
use bitcoin::hashes::Hash as _;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::{Keypair, Message, SecretKey};
use bitcoin::sighash::{Prevouts, SighashCache, TapSighashType};
use bitcoin::taproot::{LeafVersion, TapLeafHash};
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness, XOnlyPublicKey,
    transaction::Version,
};
use bitcoin_p2mr_pqc::p2mr::P2MR_LEAF_VERSION;
use bitcoin_p2mr_pqc::taproot::{LeafVersion as P2mrLeafVersion, TapNodeHash};
use bitcoinpqc::{Algorithm, generate_keypair, sign};

use super::leaf_script::build_hybrid_ec_slh_leaf;
use super::limits::ML_DSA_44_PUBLIC_KEY_SIZE;

/// Signature algorithm for single-leaf `PUSH <pk> OP_CHECKSIG` spends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignAlgorithm {
    Schnorr,
    Mldsa,
    Slh,
}

impl SignAlgorithm {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Schnorr => "schnorr",
            Self::Mldsa => "mldsa",
            Self::Slh => "slh",
        }
    }

    pub(crate) const fn to_bitcoinpqc(self) -> Algorithm {
        match self {
            Self::Schnorr => Algorithm::SECP256K1_SCHNORR,
            Self::Mldsa => Algorithm::ML_DSA_44,
            Self::Slh => Algorithm::SLH_DSA_SHA2_128S,
        }
    }
}

/// Canonical sighash names matching the `p2mr_signer` CLI `--sighash` values.
#[must_use]
pub fn sighash_label(sighash_type: TapSighashType) -> &'static str {
    match sighash_type {
        TapSighashType::Default | TapSighashType::All => "all",
        TapSighashType::None => "none",
        TapSighashType::Single => "single",
        TapSighashType::AllPlusAnyoneCanPay => "all,anyonecanpay",
        TapSighashType::NonePlusAnyoneCanPay => "none,anyonecanpay",
        TapSighashType::SinglePlusAnyoneCanPay => "single,anyonecanpay",
    }
}

/// Output of a single-leaf P2MR script-path sign operation.
#[derive(Clone, Debug)]
pub struct SignedP2mrSpend {
    pub algorithm: SignAlgorithm,
    pub sighash_type: TapSighashType,
    pub public_key: Vec<u8>,
    pub leaf_script: ScriptBuf,
    pub merkle_root: [u8; 32],
    pub script_pubkey: ScriptBuf,
    pub control_block: Vec<u8>,
    pub sighash: [u8; 32],
    pub signature: Vec<u8>,
    pub witness: Witness,
    pub funding_tx: Transaction,
    pub unsigned_spend_tx: Transaction,
    pub signed_spend_tx: Transaction,
}

/// Core signing artifacts shared by output-only and coinbase-funded spend builders.
struct P2mrSpendSigningResult {
    public_key: Vec<u8>,
    leaf_script: ScriptBuf,
    merkle_root: [u8; 32],
    script_pubkey: ScriptBuf,
    control_block: Vec<u8>,
    sighash: [u8; 32],
    witness_sig: Vec<u8>,
    witness: Witness,
    unsigned_spend_tx: Transaction,
    signed_spend_tx: Transaction,
}

/// Validate entropy length for the chosen algorithm.
pub fn validate_entropy(entropy: &[u8], algorithm: SignAlgorithm) -> Result<(), String> {
    match algorithm {
        SignAlgorithm::Schnorr if entropy.len() != 32 => Err(
            "--entropy-hex must be exactly 32 bytes (64 hex chars) for schnorr; \
             bitcoinpqc uses only the first 32 bytes as the secret key scalar"
                .into(),
        ),
        SignAlgorithm::Mldsa | SignAlgorithm::Slh if entropy.len() < 128 => {
            Err("--entropy-hex must be at least 128 bytes (256 hex chars) for mldsa / slh".into())
        }
        _ => Ok(()),
    }
}

/// Validate spend output value does not exceed the funding prevout value.
pub fn validate_output_value(output_value: u64, prevout_value: u64) -> Result<(), String> {
    if output_value > prevout_value {
        return Err(format!(
            "--output-value ({output_value}) must be <= --prevout-value ({prevout_value})"
        ));
    }
    Ok(())
}

/// Build a single-sig `PUSH <pk> OP_CHECKSIG` leaf script.
pub fn build_checksig_leaf(pubkey: &[u8]) -> Result<ScriptBuf, String> {
    if pubkey.len() == ML_DSA_44_PUBLIC_KEY_SIZE {
        let mut bytes = Vec::with_capacity(3 + pubkey.len() + 1);
        bytes.push(0x4d); // OP_PUSHDATA2
        bytes.extend_from_slice(&(pubkey.len() as u16).to_le_bytes());
        bytes.extend_from_slice(pubkey);
        bytes.push(OP_CHECKSIG.to_u8());
        Ok(ScriptBuf::from_bytes(bytes))
    } else {
        let pk32: [u8; 32] = pubkey
            .try_into()
            .map_err(|_| format!("expected 32-byte pubkey, got {} bytes", pubkey.len()))?;
        Ok(Builder::new()
            .push_slice(pk32)
            .push_opcode(OP_CHECKSIG)
            .into_script())
    }
}

/// Compute the merkle root for a single-leaf P2MR tree.
#[must_use]
pub fn single_leaf_merkle_root(leaf_script: &ScriptBuf) -> [u8; 32] {
    let leaf_version =
        P2mrLeafVersion::from_consensus(P2MR_LEAF_VERSION).expect("valid P2MR leaf version");
    TapNodeHash::from_script(
        bitcoin_p2mr_pqc::Script::from_bytes(leaf_script.as_bytes()),
        leaf_version,
    )
    .to_byte_array()
}

/// Single-leaf P2MR control block (`0xc1` + 32-byte base padding, empty merkle branch).
#[must_use]
pub fn single_leaf_control_block() -> Vec<u8> {
    let mut control_block = vec![0xc1];
    control_block.extend_from_slice(&[0u8; 32]);
    control_block
}

/// Build P2MR `scriptPubKey` from a merkle root (`0x52 0x20 <root>`).
#[must_use]
pub fn p2mr_script_pubkey(merkle_root: [u8; 32]) -> ScriptBuf {
    let mut spk = vec![0x52, 0x20];
    spk.extend_from_slice(&merkle_root);
    ScriptBuf::from_bytes(spk)
}

/// Append tapscript sighash byte when not `SIGHASH_ALL`.
#[must_use]
pub fn append_sighash(mut sig: Vec<u8>, sighash_type: TapSighashType) -> Vec<u8> {
    if sighash_type != TapSighashType::All {
        sig.push(sighash_type as u8);
    }
    sig
}

/// Encode a transaction as consensus hex.
#[must_use]
pub fn tx_to_hex(tx: &Transaction) -> String {
    let mut bytes = Vec::new();
    tx.consensus_encode(&mut bytes)
        .expect("consensus encode transaction");
    hex::encode(bytes)
}

fn generate_p2mr_keypair(
    algorithm: SignAlgorithm,
    entropy: &[u8],
) -> Result<bitcoinpqc::KeyPair, String> {
    validate_entropy(entropy, algorithm)?;
    generate_keypair(algorithm.to_bitcoinpqc(), entropy)
        .map_err(|e| format!("key generation failed: {e}"))
}

/// Shared sighash and witness construction for P2MR script-path spends.
fn sign_p2mr_spend_core(
    keypair: &bitcoinpqc::KeyPair,
    sighash_type: TapSighashType,
    prevout: TxOut,
    spend_previous_output: OutPoint,
    output_value: u64,
    spend_destination: ScriptBuf,
) -> Result<P2mrSpendSigningResult, String> {
    validate_output_value(output_value, prevout.value.to_sat())?;

    let leaf_script = build_checksig_leaf(&keypair.public_key.bytes)?;
    let merkle_root = single_leaf_merkle_root(&leaf_script);
    let script_pubkey = p2mr_script_pubkey(merkle_root);
    let control_block = single_leaf_control_block();

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

    let mut signed_spend_tx = unsigned_spend_tx.clone();
    signed_spend_tx.input[0].witness = witness.clone();

    Ok(P2mrSpendSigningResult {
        public_key: keypair.public_key.bytes.clone(),
        leaf_script,
        merkle_root,
        script_pubkey,
        control_block,
        sighash: sighash.to_byte_array(),
        witness_sig,
        witness,
        unsigned_spend_tx,
        signed_spend_tx,
    })
}

/// Sign a single-leaf P2MR script-path spend (overload model).
pub fn sign_p2mr_script_path_spend(
    algorithm: SignAlgorithm,
    entropy: &[u8],
    sighash_type: TapSighashType,
    prevout_value: u64,
    output_value: u64,
) -> Result<SignedP2mrSpend, String> {
    validate_output_value(output_value, prevout_value)?;
    let keypair = generate_p2mr_keypair(algorithm, entropy)?;

    let leaf_script = build_checksig_leaf(&keypair.public_key.bytes)?;
    let script_pubkey = p2mr_script_pubkey(single_leaf_merkle_root(&leaf_script));

    // Synthetic funding tx: output-only (no inputs). Valid for submitblock harnesses.
    let funding_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![],
        output: vec![TxOut {
            value: Amount::from_sat(prevout_value),
            script_pubkey: script_pubkey.clone(),
        }],
    };
    let funding_txid = funding_tx.compute_txid();
    let prevout = funding_tx.output[0].clone();

    let signed = sign_p2mr_spend_core(
        &keypair,
        sighash_type,
        prevout,
        OutPoint {
            txid: funding_txid,
            vout: 0,
        },
        output_value,
        ScriptBuf::new(),
    )?;

    Ok(SignedP2mrSpend {
        algorithm,
        sighash_type,
        public_key: signed.public_key,
        leaf_script: signed.leaf_script,
        merkle_root: signed.merkle_root,
        script_pubkey: signed.script_pubkey,
        control_block: signed.control_block,
        sighash: signed.sighash,
        signature: signed.witness_sig,
        witness: signed.witness,
        funding_tx,
        unsigned_spend_tx: signed.unsigned_spend_tx,
        signed_spend_tx: signed.signed_spend_tx,
    })
}

/// Build a coinbase-funded P2MR output and signed script-path spend for block harnesses.
///
/// Unlike [`sign_p2mr_script_path_spend`], the funding transaction spends a real
/// `coinbase_outpoint` so the pair can be included in a `submitblock` template.
pub fn build_block_p2mr_spend(
    algorithm: SignAlgorithm,
    entropy: &[u8],
    sighash_type: TapSighashType,
    coinbase_outpoint: OutPoint,
    prevout_value: u64,
    output_value: u64,
    spend_destination: ScriptBuf,
) -> Result<(Transaction, Transaction), String> {
    validate_output_value(output_value, prevout_value)?;
    let keypair = generate_p2mr_keypair(algorithm, entropy)?;

    let script_pubkey = p2mr_script_pubkey(single_leaf_merkle_root(&build_checksig_leaf(
        &keypair.public_key.bytes,
    )?));

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
            script_pubkey: script_pubkey.clone(),
        }],
    };
    let funding_txid = funding_tx.compute_txid();
    let prevout = funding_tx.output[0].clone();

    let signed = sign_p2mr_spend_core(
        &keypair,
        sighash_type,
        prevout,
        OutPoint {
            txid: funding_txid,
            vout: 0,
        },
        output_value,
        spend_destination,
    )?;

    Ok((funding_tx, signed.signed_spend_tx))
}

/// Validate entropy lengths for hybrid EC (32 B) + SLH (≥128 B) P2MR spends.
pub fn validate_hybrid_entropy(ec_entropy: &[u8], slh_entropy: &[u8]) -> Result<(), String> {
    if ec_entropy.len() != 32 {
        return Err(
            "EC entropy must be exactly 32 bytes (64 hex chars) for secp256k1 Schnorr".into(),
        );
    }
    if slh_entropy.len() < 128 {
        return Err(
            "SLH entropy must be at least 128 bytes (256 hex chars) for SLH-DSA-SHA2-128s".into(),
        );
    }
    Ok(())
}

fn ec_keypair_from_entropy(ec_entropy: &[u8; 32]) -> Result<Keypair, String> {
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(ec_entropy).map_err(|e| format!("invalid EC entropy: {e}"))?;
    Ok(Keypair::from_secret_key(&secp, &sk))
}

fn slh_pk32(keypair: &bitcoinpqc::KeyPair) -> Result<[u8; 32], String> {
    keypair.public_key.bytes.as_slice().try_into().map_err(|_| {
        format!(
            "expected 32-byte SLH pubkey, got {} bytes",
            keypair.public_key.bytes.len()
        )
    })
}

/// Build a coinbase-funded hybrid EC+SLH P2MR output and signed script-path spend.
///
/// Witness layout (bottom → top): `[ec_sig (64B), slh_sig (~7856B), leaf_script, control_block]`.
pub fn build_block_hybrid_ec_slh_p2mr_spend(
    ec_entropy: &[u8; 32],
    slh_entropy: &[u8],
    sighash_type: TapSighashType,
    coinbase_outpoint: OutPoint,
    prevout_value: u64,
    output_value: u64,
    spend_destination: ScriptBuf,
) -> Result<(Transaction, Transaction), String> {
    validate_hybrid_entropy(ec_entropy, slh_entropy)?;
    validate_output_value(output_value, prevout_value)?;

    let ec_keypair = ec_keypair_from_entropy(ec_entropy)?;
    let slh_keypair = generate_keypair(Algorithm::SLH_DSA_SHA2_128S, slh_entropy)
        .map_err(|e| format!("SLH key generation failed: {e}"))?;

    let ec_pk: [u8; 32] = XOnlyPublicKey::from_keypair(&ec_keypair).0.serialize();
    let slh_pk = slh_pk32(&slh_keypair)?;
    let leaf_script = build_hybrid_ec_slh_leaf(&ec_pk, &slh_pk);
    let merkle_root = single_leaf_merkle_root(&leaf_script);
    let script_pubkey = p2mr_script_pubkey(merkle_root);

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
            script_pubkey: script_pubkey.clone(),
        }],
    };
    let funding_txid = funding_tx.compute_txid();
    let prevout = funding_tx.output[0].clone();

    let unsigned_spend_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: funding_txid,
                vout: 0,
            },
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

    let secp = Secp256k1::new();
    let ec_sig =
        secp.sign_schnorr_no_aux_rand(&Message::from_digest(sighash.to_byte_array()), &ec_keypair);
    let slh_sig = sign(&slh_keypair.secret_key, sighash.as_byte_array())
        .map_err(|e| format!("SLH signing failed: {e}"))?;

    let mut witness = Witness::new();
    witness.push(ec_sig.serialize());
    witness.push(slh_sig.bytes);
    witness.push(leaf_script.as_bytes());
    witness.push(single_leaf_control_block());

    let mut signed_spend_tx = unsigned_spend_tx;
    signed_spend_tx.input[0].witness = witness;

    Ok((funding_tx, signed_spend_tx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::locktime::absolute::LockTime;

    #[test]
    fn build_block_p2mr_spend_links_coinbase_to_signed_witness() {
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

        let (funding_tx, spend_tx) = build_block_p2mr_spend(
            SignAlgorithm::Schnorr,
            &[0x11; 32],
            TapSighashType::All,
            coinbase_outpoint,
            50_000,
            40_000,
            ScriptBuf::new(),
        )
        .expect("build_block_p2mr_spend");

        assert_eq!(funding_tx.input[0].previous_output, coinbase_outpoint);
        assert_eq!(
            spend_tx.input[0].previous_output.txid,
            funding_tx.compute_txid()
        );
        assert_eq!(spend_tx.input[0].witness.len(), 3);
        assert!(
            !spend_tx.input[0]
                .witness
                .nth(0)
                .expect("signature")
                .is_empty()
        );
        assert!(
            !spend_tx.input[0]
                .witness
                .nth(1)
                .expect("leaf script")
                .is_empty()
        );
        assert!(
            !spend_tx.input[0]
                .witness
                .nth(2)
                .expect("control block")
                .is_empty()
        );
    }

    #[test]
    fn build_block_hybrid_ec_slh_p2mr_spend_links_coinbase_to_hybrid_witness() {
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

        let (funding_tx, spend_tx) = build_block_hybrid_ec_slh_p2mr_spend(
            &[0x33; 32],
            &[0x88; 128],
            TapSighashType::All,
            coinbase_outpoint,
            50_000,
            40_000,
            ScriptBuf::new(),
        )
        .expect("build_block_hybrid_ec_slh_p2mr_spend");

        assert_eq!(funding_tx.input[0].previous_output, coinbase_outpoint);
        assert_eq!(
            spend_tx.input[0].previous_output.txid,
            funding_tx.compute_txid()
        );
        assert_eq!(spend_tx.input[0].witness.len(), 4);
        assert_eq!(spend_tx.input[0].witness.nth(0).expect("ec sig").len(), 64);
        assert!(
            spend_tx.input[0].witness.nth(1).expect("slh sig").len() > 7800,
            "expected SLH-DSA signature size"
        );

        let prevout = funding_tx.output[0].clone();
        let prevouts = Prevouts::All(std::slice::from_ref(&prevout));
        super::super::spend::validate_p2mr_input_spend(
            &spend_tx, 0, &prevout, &prevouts, &mut None,
        )
        .expect("hybrid P2MR spend validates");
    }
}
