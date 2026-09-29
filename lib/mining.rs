//! Finishing a block for submission: the witness commitment, the BIP325
//! signet solution, and grinding its proof-of-work.
//!
//! Shared by the enforcer's own miner and the integration test harness.

use bitcoin::{
    Amount, Block, BlockHash, OutPoint, Script, ScriptBuf, Sequence, Transaction, TxIn, TxOut,
    Witness,
    absolute::LockTime,
    block::Header,
    consensus::encode::serialize,
    hashes::{Hash as _, HashEngine as _, sha256, sha256d},
    merkle_tree,
    opcodes::{OP_0, all::OP_RETURN},
    script::{Builder, PushBytesBuf},
    transaction,
};
use thiserror::Error;

/// Marks the witness commitment's push (BIP141).
const WITNESS_COMMITMENT_HEADER: [u8; 4] = [0xaa, 0x21, 0xa9, 0xed];

/// The coinbase's only witness item.
pub const WITNESS_RESERVED_VALUE: [u8; 32] = [0; 32];

/// Marks the signet solution's push in the witness commitment output
/// (BIP325).
const SIGNET_HEADER: [u8; 4] = [0xec, 0xc7, 0xda, 0xa2];

#[derive(Debug, Error)]
#[error("block has no witness commitment to carry a signet solution")]
pub struct NoWitnessCommitment;

/// Set the coinbase's witness, append the witness commitment output, and
/// update the merkle root.
///
/// Panics if the block has no coinbase.
pub fn add_witness_commitment(block: &mut Block) {
    block.txdata[0].input[0].witness = Witness::from_slice(&[WITNESS_RESERVED_VALUE]);
    let witness_root = block.witness_root().expect("block has a coinbase");
    let commitment = Block::compute_witness_commitment(&witness_root, &WITNESS_RESERVED_VALUE);
    let mut push = PushBytesBuf::from(WITNESS_COMMITMENT_HEADER);
    push.extend_from_slice(commitment.as_byte_array())
        .expect("36 bytes is a valid push");
    block.txdata[0].output.push(TxOut {
        value: Amount::ZERO,
        script_pubkey: ScriptBuf::new_op_return(push),
    });
    block.header.merkle_root = block.compute_merkle_root().expect("block has a coinbase");
}

/// Index of the coinbase output Bitcoin Core treats as the witness
/// commitment: the last one that looks like it.
fn witness_commitment_index(block: &Block) -> Result<usize, NoWitnessCommitment> {
    let prefix = [&[OP_RETURN.to_u8(), 0x24][..], &WITNESS_COMMITMENT_HEADER].concat();
    block.txdata[0]
        .output
        .iter()
        .rposition(|output| {
            let script = output.script_pubkey.as_bytes();
            script.len() >= 38 && script.starts_with(&prefix)
        })
        .ok_or(NoWitnessCommitment)
}

/// The BIP325 transactions whose signature solves a signet block. Signing
/// `to_sign`'s only input, which spends `to_spend`'s only output, yields the
/// scriptSig and witness for [`add_signet_solution`].
pub struct SignetTxs {
    pub to_spend: Transaction,
    pub to_sign: Transaction,
}

impl SignetTxs {
    /// For a block with a witness commitment but no solution yet.
    pub fn new(block: &Block, challenge: &Script) -> Result<Self, NoWitnessCommitment> {
        // The solution commits to the block with its solution pushed as just
        // the header, which is what validation strips it back to.
        let mut coinbase = block.txdata[0].clone();
        coinbase.output[witness_commitment_index(block)?]
            .script_pubkey
            .push_slice(SIGNET_HEADER);
        let merkle_root = merkle_tree::calculate_root(
            std::iter::once(&coinbase)
                .chain(&block.txdata[1..])
                .map(|tx| tx.compute_txid().to_raw_hash()),
        )
        .expect("block has a coinbase");
        let block_data = [
            &block.header.version.to_consensus().to_le_bytes()[..],
            block.header.prev_blockhash.as_byte_array(),
            merkle_root.as_byte_array(),
            &block.header.time.to_le_bytes(),
        ]
        .concat();
        let to_spend = Transaction {
            version: transaction::Version(0),
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: Builder::new()
                    .push_opcode(OP_0)
                    .push_slice(PushBytesBuf::try_from(block_data).expect("72 bytes"))
                    .into_script(),
                sequence: Sequence::ZERO,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: challenge.to_owned(),
            }],
        };
        let to_sign = Transaction {
            version: transaction::Version(0),
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(to_spend.compute_txid(), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ZERO,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: Builder::new().push_opcode(OP_RETURN).into_script(),
            }],
        };
        Ok(Self { to_spend, to_sign })
    }
}

/// Push the solution for [`SignetTxs::to_sign`]'s input into the witness
/// commitment, and update the merkle root.
pub fn add_signet_solution(
    block: &mut Block,
    script_sig: &Script,
    witness: &Witness,
) -> Result<(), NoWitnessCommitment> {
    let index = witness_commitment_index(block)?;
    let push = [
        &SIGNET_HEADER[..],
        &serialize(script_sig),
        &serialize(witness),
    ]
    .concat();
    block.txdata[0].output[index]
        .script_pubkey
        .push_slice(PushBytesBuf::try_from(push).expect("under the push size limit"));
    block.header.merkle_root = block.compute_merkle_root().expect("block has a coinbase");
    Ok(())
}

/// Find a nonce that meets `header`'s target, or `None` if there is none.
///
/// Hashes from the midstate of the header's first 64 bytes, on every core.
/// Bitcoin Core's `bitcoin-util grind` does neither, and never enables its
/// hardware SHA-256 either.
pub fn grind(header: Header) -> Option<Header> {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let target = header.target();
    let most_significant_target_byte = target.to_le_bytes()[31];
    let serialized = serialize(&header);
    let midstate_engine = {
        let mut engine = sha256::HashEngine::default();
        engine.input(&serialized[..64]);
        engine
    };
    let tail: [u8; 16] = serialized[64..].try_into().expect("80-byte header");
    let found = AtomicBool::new(false);
    let nonce = AtomicU64::new(u64::MAX);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()) as u32;
    std::thread::scope(|scope| {
        for first in 0..threads {
            let (found, nonce, midstate_engine) = (&found, &nonce, &midstate_engine);
            scope.spawn(move || {
                let mut tail = tail;
                let mut candidate = first;
                while !found.load(Ordering::Relaxed) {
                    // Check `found` only every so often; it is the hot loop.
                    for _ in 0..4096 {
                        tail[12..].copy_from_slice(&candidate.to_le_bytes());
                        let mut engine = midstate_engine.clone();
                        engine.input(&tail);
                        let hash = sha256::Hash::hash(sha256::Hash::from_engine(engine).as_ref());
                        // Cheap reject on the most significant byte before the
                        // full comparison.
                        if hash.as_byte_array()[31] <= most_significant_target_byte
                            && target.is_met_by(BlockHash::from_raw_hash(
                                sha256d::Hash::from_byte_array(hash.to_byte_array()),
                            ))
                        {
                            if !found.swap(true, Ordering::Relaxed) {
                                nonce.store(candidate.into(), Ordering::Relaxed);
                            }
                            return;
                        }
                        let Some(next) = candidate.checked_add(threads) else {
                            return;
                        };
                        candidate = next;
                    }
                }
            });
        }
    });
    match nonce.load(Ordering::Relaxed) {
        u64::MAX => None,
        nonce => Some(Header {
            nonce: nonce as u32,
            ..header
        }),
    }
}
