//! Block assembly: coinbase + inventory → grind → `submitblock`.
//!
//! Mirrors the integration harness (`bip360_block`) and enforcer wallet miner
//! patterns without depending on `integration_tests`.

use bitcoin::{
    Amount, Block, BlockHash, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxMerkleNode,
    TxOut, Witness,
    absolute::LockTime,
    block::Header,
    consensus::encode::serialize_hex,
    hashes::Hash as _,
    merkle_tree,
    opcodes::OP_0,
    script::{Builder as ScriptBuilder, PushBytesBuf},
    transaction::Version,
};
use bitcoin_jsonrpsee::{
    MainClient,
    client::{BlockTemplate, BlockTemplateRequest, CoinbaseTxnOrValue},
    jsonrpsee::http_client::HttpClient,
};

use crate::error::Error;

const WITNESS_RESERVED_VALUE: [u8; 32] = [0; 32];

/// Result of a successful mine+submit.
#[derive(Clone, Debug)]
pub struct MineResult {
    pub block_hash: BlockHash,
    pub height: u32,
    /// Number of non-coinbase transactions included (inventory size).
    pub tx_count: usize,
}

/// Raw GBT `coinbasevalue` (subsidy + fees of `template.transactions`).
fn gbt_coinbase_value_sats(template: &BlockTemplate) -> Result<u64, Error> {
    match &template.coinbase_txn_or_value {
        CoinbaseTxnOrValue::ValueSats(v) => Ok(*v),
        CoinbaseTxnOrValue::Txn(txn) => {
            let tx: Transaction =
                bitcoin::consensus::deserialize(&txn.data).map_err(Error::TxDecode)?;
            let mut total = 0u64;
            for out in &tx.output {
                total = total.saturating_add(out.value.to_sat());
            }
            if total == 0 {
                return Err(Error::MissingCoinbaseValue);
            }
            Ok(total)
        }
    }
}

/// Sum of fees GBT attributes to `template.transactions` (non-negative).
fn template_mempool_fees_sats(template: &BlockTemplate) -> u64 {
    template
        .transactions
        .iter()
        .map(|t| t.fee.to_sat().max(0) as u64)
        .sum()
}

/// Subtract template mempool fees from GBT `coinbasevalue` for inventory-only blocks.
///
/// Under-claiming is consensus-valid; over-claiming yields `bad-cb-amount`.
#[must_use]
pub fn inventory_only_value_from_parts(
    gbt_coinbase_value_sats: u64,
    template_fees_sats: u64,
) -> u64 {
    gbt_coinbase_value_sats.saturating_sub(template_fees_sats)
}

/// Coinbase amount for an **inventory-only** block (drops `template.transactions`).
///
/// GBT `coinbasevalue` = block subsidy + fees of template mempool txs. We do not
/// include those template txs, so claiming the full value risks `bad-cb-amount`.
///
/// Inventory fees are not known without a UTXO set, so we claim **subsidy only**
/// (GBT value minus template mempool fees). Under-claiming is consensus-valid;
/// over-claiming is not.
pub fn inventory_only_coinbase_value_sats(template: &BlockTemplate) -> Result<u64, Error> {
    let gbt_value = gbt_coinbase_value_sats(template)?;
    let template_fees = template_mempool_fees_sats(template);
    Ok(inventory_only_value_from_parts(gbt_value, template_fees))
}

fn build_coinbase(
    template: &BlockTemplate,
    coinbase_spk: &ScriptBuf,
    value_sats: u64,
) -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            // Height-only scriptSig can be 1 byte and fails Core's bad-cb-length;
            // append OP_0 like the wallet / harness miners do.
            script_sig: ScriptBuilder::new()
                .push_int(template.height as i64)
                .push_opcode(OP_0)
                .into_script(),
            sequence: Sequence::MAX,
            witness: Witness::from_slice(&[WITNESS_RESERVED_VALUE]),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(value_sats),
            script_pubkey: coinbase_spk.clone(),
        }],
    }
}

/// Assemble a block header+txdata from a GBT template, coinbase, and non-coinbase txs.
///
/// Does **not** include template mempool transactions — only the provided inventory
/// (plus the coinbase). That is intentional: stock Core mempool will not hold
/// enforcer-format v2 spends; fee claim must use [`inventory_only_coinbase_value_sats`].
pub fn assemble_block_with_inventory(
    template: &BlockTemplate,
    coinbase: Transaction,
    non_coinbase_txs: Vec<Transaction>,
) -> Result<Block, Error> {
    let mut txdata = Vec::with_capacity(1 + non_coinbase_txs.len());
    txdata.push(coinbase);
    txdata.extend(non_coinbase_txs);

    // Prefer mintime when current_time would be too old vs MTP.
    let time = u32::try_from(template.current_time.max(template.mintime)).unwrap_or(u32::MAX);

    let header = Header {
        version: template.version,
        prev_blockhash: template.prev_blockhash,
        merkle_root: TxMerkleNode::all_zeros(),
        time,
        bits: template.compact_target,
        nonce: 0,
    };
    let mut block = Block { header, txdata };

    let witness_root = block.witness_root().ok_or(Error::WitnessRoot)?;
    let witness_commitment =
        Block::compute_witness_commitment(&witness_root, &WITNESS_RESERVED_VALUE);
    const WITNESS_COMMITMENT_HEADER: [u8; 4] = [0xaa, 0x21, 0xa9, 0xed];
    let witness_commitment_spk = {
        let mut push_bytes = PushBytesBuf::from(WITNESS_COMMITMENT_HEADER);
        push_bytes
            .extend_from_slice(witness_commitment.as_byte_array())
            .map_err(|e| Error::InvalidTxHex(format!("witness commitment push: {e}")))?;
        ScriptBuf::new_op_return(push_bytes)
    };
    block.txdata[0].output.push(TxOut {
        script_pubkey: witness_commitment_spk,
        value: Amount::ZERO,
    });

    let mut tx_hashes: Vec<_> = block.txdata.iter().map(Transaction::compute_txid).collect();
    block.header.merkle_root = merkle_tree::calculate_root_inline(&mut tx_hashes)
        .ok_or(Error::TxMerkleRoot)?
        .to_raw_hash()
        .into();

    // Regtest / easy targets: grind nonce in-process (same as enforcer wallet miner).
    loop {
        if block.header.validate_pow(block.header.target()).is_ok() {
            break;
        }
        block.header.nonce = block.header.nonce.wrapping_add(1);
        if block.header.nonce == 0 {
            // Extremely unlikely on regtest; bump time and continue.
            block.header.time = block.header.time.saturating_add(1);
        }
    }

    Ok(block)
}

/// Fetch GBT, assemble coinbase + inventory, grind, `submitblock`.
pub async fn mine_and_submit(
    client: &HttpClient,
    coinbase_spk: &ScriptBuf,
    non_coinbase_txs: Vec<Transaction>,
) -> Result<MineResult, Error> {
    if non_coinbase_txs.is_empty() {
        return Err(Error::EmptyInventory);
    }
    let tx_count = non_coinbase_txs.len();

    let template = client
        .get_block_template(BlockTemplateRequest::default())
        .await
        .map_err(|source| Error::Rpc {
            method: "getblocktemplate".to_owned(),
            source,
        })?;

    let height = template.height;
    // Inventory-only: do not claim fees for dropped template.transactions.
    let value = inventory_only_coinbase_value_sats(&template)?;
    let coinbase = build_coinbase(&template, coinbase_spk, value);
    let block = assemble_block_with_inventory(&template, coinbase, non_coinbase_txs)?;
    let block_hash = block.block_hash();
    let block_hex = serialize_hex(&block);

    match client
        .submit_block(block_hex)
        .await
        .map_err(|source| Error::Rpc {
            method: "submitblock".to_owned(),
            source,
        })? {
        None => {
            tracing::info!(
                %block_hash,
                height,
                tx_count,
                coinbase_value_sats = value,
                path = "cusf_sidecar",
                "submitted block via stock Core submitblock (CUSF miner sidecar)"
            );
            Ok(MineResult {
                block_hash,
                height,
                tx_count,
            })
        }
        Some(reason) => Err(Error::BlockRejected(reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_only_subtracts_template_mempool_fees() {
        // GBT: 50 BTC subsidy + 10_000 fee for a mempool tx we will drop.
        let subsidy = 50 * 100_000_000u64;
        let v = inventory_only_value_from_parts(subsidy + 10_000, 10_000);
        assert_eq!(v, subsidy);
    }

    #[test]
    fn inventory_only_empty_mempool_keeps_gbt_value() {
        let subsidy = 50 * 100_000_000u64;
        let v = inventory_only_value_from_parts(subsidy, 0);
        assert_eq!(v, subsidy);
    }

    #[test]
    fn inventory_only_never_underflows() {
        assert_eq!(inventory_only_value_from_parts(100, 500), 0);
    }
}
