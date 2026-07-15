//! Shared helpers for BIP 360 `submitblock` / `invalidateblock` integration trials.

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::mainchain::GetChainTipRequest,
    validator::pqc::{
        multi_leaf::{
            MLDSA_LEAF_INDEX, ThreeAlgorithmP2mrTree, build_block_three_leaf_p2mr_spend,
            swap_multi_leaf_control_block,
        },
        signer_dev::{
            SignAlgorithm, build_block_hybrid_ec_slh_p2mr_spend,
            build_block_kitchen_sink_p2mr_spend, build_block_p2mr_spend, build_checksig_leaf,
            p2mr_script_pubkey, single_leaf_control_block, single_leaf_merkle_root,
        },
    },
};
use bitcoin::sighash::TapSighashType;
use bitcoin::{
    Amount, Block, BlockHash, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxMerkleNode,
    TxOut, Witness,
    block::Header,
    consensus::encode::{deserialize_hex, serialize_hex},
    hashes::Hash as _,
    merkle_tree,
    opcodes::OP_0,
    script::{Builder as ScriptBuilder, PushBytesBuf},
    transaction::Version,
};
use serde::Deserialize;

const WITNESS_RESERVED_VALUE: [u8; 32] = [0; 32];

#[derive(Debug, Deserialize)]
struct SignResult {
    hex: String,
    complete: bool,
}

/// Sign a transaction via the integration-test bitcoind wallet (required for
/// spending wallet-owned coinbase / P2WPKH / P2TR outputs).
pub async fn wallet_sign_transaction(
    post_setup: &PostSetup,
    tx: Transaction,
) -> anyhow::Result<Transaction> {
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "signrawtransactionwithwallet", [serialize_hex(&tx)])
        .run_utf8()
        .await?;
    let signed: SignResult = serde_json::from_str(&json)?;
    anyhow::ensure!(signed.complete, "signrawtransactionwithwallet incomplete");
    Ok(deserialize_hex(&signed.hex)?)
}
use tokio::time::sleep;

use crate::setup::{BitcoindKind, PostSetup, SetupOpts};

pub const P2MR_PREVOUT_VALUE: u64 = 50_000;
pub const P2MR_SPEND_OUTPUT_VALUE: u64 = 40_000;

/// Fixed test entropy for Schnorr P2MR spends (32 bytes).
pub const SCHNORR_ENTROPY: [u8; 32] = [0x11; 32];
/// Fixed test entropy for ML-DSA-44 P2MR spends (128 bytes).
pub const MLDSA_ENTROPY: [u8; 128] = [0x22; 128];
/// Fixed test entropy for SLH-DSA-SHA2-128s P2MR spends (128 bytes).
pub const SLH_ENTROPY: [u8; 128] = [0x88; 128];
/// Fixed test entropy for hybrid EC (secp256k1 Schnorr) side of EC+SLH P2MR spends (32 bytes).
pub const HYBRID_EC_ENTROPY: [u8; 32] = [0x33; 32];

/// Common harness setup: stock Core + BIP 360 active from genesis + generous PQC budget for SLH.
#[must_use]
pub fn bip360_setup_opts() -> SetupOpts {
    SetupOpts {
        bitcoind_kind: BitcoindKind::Unpatched,
        enable_enforcer_wallet: false,
        enforcer_args: vec![
            "--activation-height=0".to_string(),
            "--pqc-verify-budget-ms=5000".to_string(),
        ],
        ..SetupOpts::default()
    }
}

pub struct BlockTemplate {
    pub prev_hash: BlockHash,
    pub height: u32,
    pub coinbasevalue: u64,
    pub bits: bitcoin::pow::CompactTarget,
    pub curtime: u32,
    pub version: i32,
}

pub async fn fetch_block_template(post_setup: &mut PostSetup) -> anyhow::Result<BlockTemplate> {
    let template_json = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>(
            [],
            "getblocktemplate",
            ["{\"rules\":[\"segwit\"]}"].map(String::from),
        )
        .run_utf8()
        .await?;
    let template: serde_json::Value = serde_json::from_str(&template_json)?;
    let prev_hash: BlockHash = template["previousblockhash"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing previousblockhash"))?
        .parse()?;
    let height = template["height"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing height"))? as u32;
    let coinbasevalue = template["coinbasevalue"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing coinbasevalue"))?;
    let bits = template["bits"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing bits"))?;
    let bits = bitcoin::pow::CompactTarget::from_consensus(u32::from_str_radix(bits, 16)?);
    let curtime = template["curtime"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing curtime"))? as u32;
    let version = template["version"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("missing version"))? as i32;
    Ok(BlockTemplate {
        prev_hash,
        height,
        coinbasevalue,
        bits,
        curtime,
        version,
    })
}

#[must_use]
pub fn build_coinbase(post_setup: &PostSetup, template: &BlockTemplate) -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            // Height-only scriptSig can be 1 byte (OP_2, etc.) and fails Core's
            // `bad-cb-length` check; append OP_0 like the wallet miner does.
            script_sig: ScriptBuilder::new()
                .push_int(template.height as i64)
                .push_opcode(OP_0)
                .into_script(),
            sequence: Sequence::MAX,
            witness: Witness::from_slice(&[WITNESS_RESERVED_VALUE]),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(template.coinbasevalue),
            script_pubkey: post_setup.mining_address.script_pubkey(),
        }],
    }
}

/// Mature coinbase outpoint from block #1 (requires ≥101 blocks mined in setup).
pub async fn funding_prevout(post_setup: &PostSetup) -> anyhow::Result<OutPoint> {
    let block_hash = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblockhash", ["1"])
        .run_utf8()
        .await?;
    let block_hex = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [block_hash.trim(), "0"])
        .run_utf8()
        .await?;
    let block: Block = deserialize_hex(block_hex.trim())?;
    Ok(OutPoint {
        txid: block.txdata[0].compute_txid(),
        vout: 0,
    })
}

/// Fetch the current GBT once and return `(template, coinbase)` for building txs and blocks.
pub async fn prepare_coinbase(
    post_setup: &mut PostSetup,
) -> anyhow::Result<(BlockTemplate, Transaction)> {
    let template = fetch_block_template(post_setup).await?;
    let coinbase = build_coinbase(post_setup, &template);
    Ok((template, coinbase))
}

/// Assemble a block from a template and coinbase already tied to that template.
pub async fn assemble_block(
    post_setup: &PostSetup,
    template: &BlockTemplate,
    coinbase: Transaction,
    non_coinbase_txs: Vec<Transaction>,
) -> anyhow::Result<Block> {
    let mut txdata = Vec::with_capacity(1 + non_coinbase_txs.len());
    txdata.push(coinbase);
    txdata.extend(non_coinbase_txs);

    let header = Header {
        version: bitcoin::block::Version::from_consensus(template.version),
        prev_blockhash: template.prev_hash,
        merkle_root: TxMerkleNode::all_zeros(),
        time: template.curtime,
        bits: template.bits,
        nonce: 0,
    };
    let mut block = Block { header, txdata };

    let witness_root = block
        .witness_root()
        .ok_or_else(|| anyhow::anyhow!("failed to compute witness merkle root"))?;
    let witness_commitment =
        Block::compute_witness_commitment(&witness_root, &WITNESS_RESERVED_VALUE);
    const WITNESS_COMMITMENT_HEADER: [u8; 4] = [0xaa, 0x21, 0xa9, 0xed];
    let witness_commitment_spk = {
        let mut push_bytes = PushBytesBuf::from(WITNESS_COMMITMENT_HEADER);
        push_bytes.extend_from_slice(witness_commitment.as_byte_array())?;
        ScriptBuf::new_op_return(push_bytes)
    };
    block.txdata[0].output.push(TxOut {
        script_pubkey: witness_commitment_spk,
        value: Amount::ZERO,
    });

    let mut tx_hashes: Vec<_> = block.txdata.iter().map(Transaction::compute_txid).collect();
    block.header.merkle_root = merkle_tree::calculate_root_inline(&mut tx_hashes)
        .ok_or_else(|| anyhow::anyhow!("failed to compute tx merkle root"))?
        .to_raw_hash()
        .into();

    let header_hex = post_setup
        .bitcoin_util()?
        .command::<String, _, _, _, _>([], "grind", [serialize_hex(&block.header)])
        .run_utf8()
        .await?;
    block.header = deserialize_hex(header_hex.trim())?;
    Ok(block)
}

/// Build a block using a coinbase that was created from the same `BlockTemplate`.
pub async fn build_block_with_coinbase(
    post_setup: &PostSetup,
    template: &BlockTemplate,
    coinbase: Transaction,
    non_coinbase_txs: Vec<Transaction>,
) -> anyhow::Result<Block> {
    assemble_block(post_setup, template, coinbase, non_coinbase_txs).await
}

pub async fn build_block_from_template(
    post_setup: &mut PostSetup,
    non_coinbase_txs: Vec<Transaction>,
) -> anyhow::Result<Block> {
    let (template, coinbase) = prepare_coinbase(post_setup).await?;
    build_block_with_coinbase(post_setup, &template, coinbase, non_coinbase_txs).await
}

/// Poll until bitcoind's block height reaches at least `min_height`.
pub async fn wait_for_bitcoind_height(
    post_setup: &PostSetup,
    min_height: u32,
) -> anyhow::Result<()> {
    const POLL_INTERVAL: Duration = Duration::from_millis(250);
    const TIMEOUT: Duration = Duration::from_secs(60);

    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let height: u32 = post_setup
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "getblockcount", [])
            .run_utf8()
            .await?
            .trim()
            .parse()?;
        if height >= min_height {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "bitcoind did not reach height {min_height} within {TIMEOUT:?} (stuck at {height})"
        );
        sleep(POLL_INTERVAL).await;
    }
}

/// Poll until the enforcer chain tip height catches up to bitcoind's current height.
pub async fn wait_for_enforcer_synced(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    const POLL_INTERVAL: Duration = Duration::from_millis(250);
    const TIMEOUT: Duration = Duration::from_secs(60);

    let target_height: u32 = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    tracing::debug!("waiting for enforcer to sync to bitcoind height {target_height}");

    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let tip_height = match post_setup
            .validator_service_client
            .get_chain_tip(GetChainTipRequest::default())
            .await
        {
            Ok(resp) => resp
                .into_owned()
                .block_header_info
                .into_option()
                .map(|info| info.height)
                .unwrap_or(0),
            Err(err) => {
                tracing::trace!("enforcer get_chain_tip not ready ({err:#}), waiting...");
                0
            }
        };
        if tip_height >= target_height {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "enforcer did not sync to bitcoind height {target_height} within {TIMEOUT:?} \
             (stuck at {tip_height})"
        );
        sleep(POLL_INTERVAL).await;
    }
}

/// Poll until the enforcer tip height reaches at least `min_height`.
pub async fn wait_for_enforcer_height(
    post_setup: &mut PostSetup,
    min_height: u32,
) -> anyhow::Result<()> {
    const POLL_INTERVAL: Duration = Duration::from_millis(250);
    const TIMEOUT: Duration = Duration::from_secs(60);

    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let tip_height = match post_setup
            .validator_service_client
            .get_chain_tip(GetChainTipRequest::default())
            .await
        {
            Ok(resp) => resp
                .into_owned()
                .block_header_info
                .into_option()
                .map(|info| info.height)
                .unwrap_or(0),
            Err(err) => {
                tracing::trace!("enforcer get_chain_tip not ready ({err:#}), waiting...");
                0
            }
        };
        if tip_height >= min_height {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "enforcer did not reach height {min_height} within {TIMEOUT:?} (stuck at {tip_height})"
        );
        sleep(POLL_INTERVAL).await;
    }
}

pub async fn submit_block(post_setup: &mut PostSetup, block: &Block) -> anyhow::Result<BlockHash> {
    let block_hash = block.block_hash();
    let submit_resp = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "submitblock", [serialize_hex(block)])
        .run_utf8()
        .await?;
    anyhow::ensure!(
        submit_resp.is_empty(),
        "submitblock unexpectedly rejected: `{submit_resp}`"
    );
    Ok(block_hash)
}

/// Fixed-entropy three-leaf P2MR tree shared by integration trials.
pub fn fixed_three_leaf_tree() -> anyhow::Result<ThreeAlgorithmP2mrTree> {
    ThreeAlgorithmP2mrTree::from_entropy(&SCHNORR_ENTROPY, &MLDSA_ENTROPY, &SLH_ENTROPY)
        .map_err(anyhow::Error::msg)
}

/// Build coinbase + three-leaf P2MR funding + signed spend revealing `leaf_index`.
///
/// Leaf indices: 0 = Schnorr, 1 = ML-DSA-44, 2 = SLH-DSA (fixed test entropy).
pub async fn build_three_leaf_p2mr_spend_txs(
    post_setup: &PostSetup,
    funding_prevout: OutPoint,
    leaf_index: usize,
) -> anyhow::Result<(Transaction, Transaction)> {
    let (funding_tx, spend_tx, _) =
        build_three_leaf_p2mr_spend_txs_with_tree(post_setup, funding_prevout, leaf_index).await?;
    Ok((funding_tx, spend_tx))
}

/// Like [`build_three_leaf_p2mr_spend_txs`] but also returns the tree (avoids rebuilding).
pub async fn build_three_leaf_p2mr_spend_txs_with_tree(
    post_setup: &PostSetup,
    funding_prevout: OutPoint,
    leaf_index: usize,
) -> anyhow::Result<(Transaction, Transaction, ThreeAlgorithmP2mrTree)> {
    let tree = fixed_three_leaf_tree()?;
    let (funding_tx, spend_tx) = build_block_three_leaf_p2mr_spend(
        &tree,
        leaf_index,
        TapSighashType::All,
        funding_prevout,
        P2MR_PREVOUT_VALUE,
        P2MR_SPEND_OUTPUT_VALUE,
        post_setup.mining_address.script_pubkey(),
    )
    .map_err(anyhow::Error::msg)?;
    let funding_tx = wallet_sign_transaction(post_setup, funding_tx).await?;
    Ok((funding_tx, spend_tx, tree))
}

/// Build a multi-leaf spend with the correct script but a control block from another leaf.
pub async fn build_multi_leaf_wrong_control_block_spend_txs(
    post_setup: &PostSetup,
    funding_prevout: OutPoint,
    reveal_leaf_index: usize,
    wrong_control_leaf_index: usize,
) -> anyhow::Result<(Transaction, Transaction)> {
    let (funding_tx, mut spend_tx, tree) =
        build_three_leaf_p2mr_spend_txs_with_tree(post_setup, funding_prevout, reveal_leaf_index)
            .await?;
    swap_multi_leaf_control_block(&mut spend_tx, &tree, wrong_control_leaf_index)
        .map_err(anyhow::Error::msg)?;
    Ok((funding_tx, spend_tx))
}

/// Build a multi-leaf ML-DSA spend with a tampered witness signature (same-block invalid).
pub async fn build_multi_leaf_tampered_signature_spend_txs(
    post_setup: &PostSetup,
    funding_prevout: OutPoint,
) -> anyhow::Result<(Transaction, Transaction)> {
    let (funding_tx, mut spend_tx, _) =
        build_three_leaf_p2mr_spend_txs_with_tree(post_setup, funding_prevout, MLDSA_LEAF_INDEX)
            .await?;
    tamper_witness_signature(&mut spend_tx)?;
    Ok((funding_tx, spend_tx))
}

/// Build coinbase + P2MR funding + signed spend for a valid same-block P2MR path spend.
pub async fn build_valid_p2mr_spend_txs(
    post_setup: &PostSetup,
    funding_prevout: OutPoint,
    algorithm: SignAlgorithm,
    entropy: &[u8],
) -> anyhow::Result<(Transaction, Transaction)> {
    let (funding_tx, spend_tx) = build_block_p2mr_spend(
        algorithm,
        entropy,
        TapSighashType::All,
        funding_prevout,
        P2MR_PREVOUT_VALUE,
        P2MR_SPEND_OUTPUT_VALUE,
        post_setup.mining_address.script_pubkey(),
    )
    .map_err(anyhow::Error::msg)?;
    let funding_tx = wallet_sign_transaction(post_setup, funding_tx).await?;
    Ok((funding_tx, spend_tx))
}

/// Build coinbase + P2MR funding + signed kitchen-sink (Schnorr + ML-DSA + SLH) script-path spend.
pub async fn build_valid_kitchen_sink_spend_txs(
    post_setup: &PostSetup,
    funding_prevout: OutPoint,
) -> anyhow::Result<(Transaction, Transaction)> {
    let (funding_tx, spend_tx) = build_block_kitchen_sink_p2mr_spend(
        &HYBRID_EC_ENTROPY,
        &MLDSA_ENTROPY,
        &SLH_ENTROPY,
        TapSighashType::All,
        funding_prevout,
        P2MR_PREVOUT_VALUE,
        P2MR_SPEND_OUTPUT_VALUE,
        post_setup.mining_address.script_pubkey(),
    )
    .map_err(anyhow::Error::msg)?;
    let funding_tx = wallet_sign_transaction(post_setup, funding_tx).await?;
    Ok((funding_tx, spend_tx))
}

/// Build coinbase + P2MR funding + signed hybrid EC+SLH script-path spend.
pub async fn build_valid_hybrid_ec_slh_p2mr_spend_txs(
    post_setup: &PostSetup,
    funding_prevout: OutPoint,
) -> anyhow::Result<(Transaction, Transaction)> {
    let (funding_tx, spend_tx) = build_block_hybrid_ec_slh_p2mr_spend(
        &HYBRID_EC_ENTROPY,
        &SLH_ENTROPY,
        TapSighashType::All,
        funding_prevout,
        P2MR_PREVOUT_VALUE,
        P2MR_SPEND_OUTPUT_VALUE,
        post_setup.mining_address.script_pubkey(),
    )
    .map_err(anyhow::Error::msg)?;
    let funding_tx = wallet_sign_transaction(post_setup, funding_tx).await?;
    Ok((funding_tx, spend_tx))
}

fn witness_stack_element<'a>(
    witness: &'a Witness,
    index: usize,
    label: &'static str,
) -> anyhow::Result<&'a [u8]> {
    witness
        .nth(index)
        .ok_or_else(|| anyhow::anyhow!("P2MR witness stack missing {label} (element {index})"))
}

/// Flip the first byte of the EC (Schnorr) signature in a hybrid witness (stack item 0).
pub fn tamper_hybrid_witness_ec_signature(spend_tx: &mut Transaction) -> anyhow::Result<()> {
    let witness = &spend_tx.input[0].witness;
    let mut bad_sig = witness_stack_element(witness, 0, "EC signature")?.to_vec();
    bad_sig[0] ^= 0x01;
    let mut bad_witness = Witness::new();
    bad_witness.push(bad_sig);
    bad_witness.push(witness_stack_element(witness, 1, "SLH signature")?);
    bad_witness.push(witness_stack_element(witness, 2, "leaf script")?);
    bad_witness.push(witness_stack_element(witness, 3, "control block")?);
    spend_tx.input[0].witness = bad_witness;
    Ok(())
}

/// Flip the first byte of the SLH signature in a hybrid witness (stack item 1).
pub fn tamper_hybrid_witness_slh_signature(spend_tx: &mut Transaction) -> anyhow::Result<()> {
    let witness = &spend_tx.input[0].witness;
    let mut bad_sig = witness_stack_element(witness, 1, "SLH signature")?.to_vec();
    bad_sig[0] ^= 0x01;
    let mut bad_witness = Witness::new();
    bad_witness.push(witness_stack_element(witness, 0, "EC signature")?);
    bad_witness.push(bad_sig);
    bad_witness.push(witness_stack_element(witness, 2, "leaf script")?);
    bad_witness.push(witness_stack_element(witness, 3, "control block")?);
    spend_tx.input[0].witness = bad_witness;
    Ok(())
}

/// Flip the first byte of the EC (Schnorr) signature in a kitchen-sink witness (stack item 0).
pub fn tamper_kitchen_sink_witness_ec_signature(spend_tx: &mut Transaction) -> anyhow::Result<()> {
    let witness = &spend_tx.input[0].witness;
    let mut bad_sig = witness_stack_element(witness, 0, "EC signature")?.to_vec();
    bad_sig[0] ^= 0x01;
    let mut bad_witness = Witness::new();
    bad_witness.push(bad_sig);
    bad_witness.push(witness_stack_element(witness, 1, "ML-DSA signature")?);
    bad_witness.push(witness_stack_element(witness, 2, "SLH signature")?);
    bad_witness.push(witness_stack_element(witness, 3, "leaf script")?);
    bad_witness.push(witness_stack_element(witness, 4, "control block")?);
    spend_tx.input[0].witness = bad_witness;
    Ok(())
}

/// Swap EC and SLH signature positions in a hybrid witness (stack items 0 and 1).
pub fn swap_hybrid_witness_signatures(spend_tx: &mut Transaction) -> anyhow::Result<()> {
    let witness = &spend_tx.input[0].witness;
    let mut bad_witness = Witness::new();
    bad_witness.push(witness_stack_element(witness, 1, "SLH signature")?);
    bad_witness.push(witness_stack_element(witness, 0, "EC signature")?);
    bad_witness.push(witness_stack_element(witness, 2, "leaf script")?);
    bad_witness.push(witness_stack_element(witness, 3, "control block")?);
    spend_tx.input[0].witness = bad_witness;
    Ok(())
}

/// Flip the first byte of the witness signature element (stack item 0).
pub fn tamper_witness_signature(spend_tx: &mut Transaction) -> anyhow::Result<()> {
    let witness = &spend_tx.input[0].witness;
    let mut bad_sig = witness_stack_element(witness, 0, "signature")?.to_vec();
    bad_sig[0] ^= 0x01;
    let mut bad_witness = Witness::new();
    bad_witness.push(bad_sig);
    bad_witness.push(witness_stack_element(witness, 1, "leaf script")?);
    bad_witness.push(witness_stack_element(witness, 2, "control block")?);
    spend_tx.input[0].witness = bad_witness;
    Ok(())
}

/// Corrupt the merkle-path portion of the witness control block (stack item 2).
///
/// Keeps the `0xc1` leaf version byte intact so the enforcer reports
/// `merkle root mismatch for leaf script` (not `invalid P2MR control byte`).
/// Mirrors the multi-leaf wrong-control-block pattern: tamper branch bytes only.
pub fn tamper_witness_control_block(spend_tx: &mut Transaction) -> anyhow::Result<()> {
    let witness = &spend_tx.input[0].witness;
    let mut bad_control = witness_stack_element(witness, 2, "control block")?.to_vec();
    anyhow::ensure!(
        bad_control.first() == Some(&0xc1),
        "expected P2MR control block starting with 0xc1"
    );
    // Wire layout: 0xc1 || 32-byte base padding || merkle branch (32 bytes per sibling).
    const BASE_LEN: usize = 33;
    if bad_control.len() < BASE_LEN {
        bad_control.resize(BASE_LEN, 0);
        bad_control[0] = 0xc1;
    }
    if bad_control.len() == BASE_LEN {
        // Single-leaf: append a bogus merkle sibling (no branch in valid control block).
        bad_control.extend_from_slice(&[0xBB; 32]);
    } else {
        // Flip a byte in the merkle branch (past base padding), not the version byte.
        let branch_idx = bad_control.len() - 1;
        bad_control[branch_idx] ^= 0x01;
    }
    let mut bad_witness = Witness::new();
    bad_witness.push(witness_stack_element(witness, 0, "signature")?);
    bad_witness.push(witness_stack_element(witness, 1, "leaf script")?);
    bad_witness.push(bad_control);
    spend_tx.input[0].witness = bad_witness;
    Ok(())
}

/// Build a P2MR spend with ML-DSA-sized signature but a 32-byte Schnorr-style leaf pubkey.
///
/// The funding output uses the wrong-leaf merkle root **before** the spend prevout is bound,
/// so Core accepts `submitblock` and the enforcer rejects on pubkey/sig size mismatch.
pub async fn build_mldsa_sig_wrong_pubkey_spend_txs(
    post_setup: &PostSetup,
    funding_prevout: OutPoint,
) -> anyhow::Result<(Transaction, Transaction)> {
    // Reuse ML-DSA signature bytes from a valid spend; leaf script is swapped below.
    let (_, valid_spend_tx) = build_block_p2mr_spend(
        SignAlgorithm::Mldsa,
        &MLDSA_ENTROPY,
        TapSighashType::All,
        funding_prevout,
        P2MR_PREVOUT_VALUE,
        P2MR_SPEND_OUTPUT_VALUE,
        post_setup.mining_address.script_pubkey(),
    )
    .map_err(anyhow::Error::msg)?;
    let mldsa_sig = witness_stack_element(&valid_spend_tx.input[0].witness, 0, "ML-DSA signature")?;

    let wrong_leaf = build_checksig_leaf(&[0xAB; 32]).map_err(anyhow::Error::msg)?;
    let wrong_script_pubkey = p2mr_script_pubkey(single_leaf_merkle_root(&wrong_leaf));

    let funding_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: funding_prevout,
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(P2MR_PREVOUT_VALUE),
            script_pubkey: wrong_script_pubkey,
        }],
    };
    let funding_txid = funding_tx.compute_txid();

    let mut bad_witness = Witness::new();
    bad_witness.push(mldsa_sig);
    bad_witness.push(wrong_leaf.as_bytes());
    bad_witness.push(single_leaf_control_block());

    let spend_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: funding_txid,
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: bad_witness,
        }],
        output: vec![TxOut {
            value: Amount::from_sat(P2MR_SPEND_OUTPUT_VALUE),
            script_pubkey: post_setup.mining_address.script_pubkey(),
        }],
    };

    let funding_tx = wallet_sign_transaction(post_setup, funding_tx).await?;
    Ok((funding_tx, spend_tx))
}

/// Submit a funding block then a spend block that consumes the confirmed P2MR output.
pub async fn submit_cross_block_p2mr_spend_with_txs(
    post_setup: &mut PostSetup,
    funding_tx: Transaction,
    spend_tx: Transaction,
) -> anyhow::Result<(BlockHash, BlockHash)> {
    let (funding_template, funding_coinbase) = prepare_coinbase(post_setup).await?;

    let funding_block = build_block_with_coinbase(
        post_setup,
        &funding_template,
        funding_coinbase,
        vec![funding_tx],
    )
    .await?;
    let funding_block_hash = submit_block(post_setup, &funding_block).await?;
    wait_for_enforcer_height(post_setup, funding_template.height).await?;

    let (spend_template, spend_coinbase) = prepare_coinbase(post_setup).await?;
    let spend_block =
        build_block_with_coinbase(post_setup, &spend_template, spend_coinbase, vec![spend_tx])
            .await?;
    let spend_block_hash = submit_block(post_setup, &spend_block).await?;
    Ok((funding_block_hash, spend_block_hash))
}

/// Submit a valid cross-block P2MR spend (funding in block N, spend in block N+1).
pub async fn submit_cross_block_p2mr_spend(
    post_setup: &mut PostSetup,
    algorithm: SignAlgorithm,
    entropy: &[u8],
) -> anyhow::Result<(BlockHash, BlockHash)> {
    let funding_prevout = funding_prevout(post_setup).await?;
    let (funding_tx, spend_tx) =
        build_valid_p2mr_spend_txs(post_setup, funding_prevout, algorithm, entropy).await?;
    submit_cross_block_p2mr_spend_with_txs(post_setup, funding_tx, spend_tx).await
}
