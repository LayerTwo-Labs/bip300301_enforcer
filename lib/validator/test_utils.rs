//! Shared test utilities for `crate::validator` tests.
//! This module is gated behind `#[cfg(test)]` in the parent module.

use bitcoin::{
    Amount, Block, BlockHash, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid,
    hashes::Hash as _,
};
use bitcoin_jsonrpsee::jsonrpsee;
use miette::IntoDiagnostic;

use super::{Validator, dbs::Dbs, main_rest_client::MainRestClient};
use crate::types::{
    M6id, NetworkParams, OpDrivechain, Sidechain, SidechainDescription, SidechainNumber,
    SidechainProposal, SidechainProposalStatus,
};

pub fn create_test_dbs() -> miette::Result<(temp_dir::TempDir, Dbs)> {
    let dir = temp_dir::TempDir::new().into_diagnostic()?;
    let dbs = Dbs::new(dir.path(), bitcoin::Network::Regtest).into_diagnostic()?;
    Ok((dir, dbs))
}

/// Validator with a fresh DB in `dir`, and mainchain clients that point at
/// nothing. For tests that only exercise DB-backed methods.
pub fn dummy_validator(dir: &std::path::Path) -> Validator {
    let mainchain_client = jsonrpsee::http_client::HttpClientBuilder::default()
        .build("http://127.0.0.1:1")
        .expect("build dummy rpc client");
    let mainchain_rest_client =
        MainRestClient::new(url::Url::parse("http://127.0.0.1:1").expect("valid url"));
    Validator::new(
        mainchain_client,
        Some(mainchain_rest_client),
        None,
        dir,
        bitcoin::Network::Regtest,
        NetworkParams::for_network(bitcoin::Network::Regtest),
    )
    .expect("construct validator")
}

pub fn test_sidechain(sidechain_number: u8, proposal_height: u32) -> Sidechain {
    Sidechain {
        proposal: SidechainProposal {
            sidechain_number: SidechainNumber(sidechain_number),
            description: SidechainDescription(vec![0x00, sidechain_number]),
        },
        status: SidechainProposalStatus {
            vote_count: 0,
            proposal_height,
            activation_height: None,
        },
    }
}

pub fn test_m6id(byte: u8) -> M6id {
    M6id(Txid::from_byte_array([byte; 32]))
}

/// Minimal block header for tests — only `prev_blockhash` is meaningful
pub fn test_block_header(prev_blockhash: BlockHash) -> bitcoin::block::Header {
    bitcoin::block::Header {
        version: bitcoin::block::Version::TWO,
        prev_blockhash,
        merkle_root: bitcoin::TxMerkleNode::all_zeros(),
        time: 0,
        bits: bitcoin::CompactTarget::from_consensus(0x2000_0000),
        nonce: 0,
    }
}

pub fn build_m5_deposit_tx(
    sidechain_number: SidechainNumber,
    old_ctip_outpoint: OutPoint,
    old_ctip_value: Amount,
    deposit_amount: Amount,
) -> Transaction {
    let treasury_output = OpDrivechain::NOP5
        .create_m5_deposit_output(sidechain_number, old_ctip_value, deposit_amount)
        .unwrap();
    let address_output = TxOut {
        script_pubkey: ScriptBuf::new_op_return(
            bitcoin::script::PushBytesBuf::try_from(b"sidechain_address".to_vec()).unwrap(),
        ),
        value: Amount::ZERO,
    };
    Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: old_ctip_outpoint,
            ..TxIn::default()
        }],
        output: vec![treasury_output, address_output],
    }
}

#[derive(Default)]
pub struct TestBlockParts {
    pub extra_coinbase_outputs: Vec<TxOut>,
    pub extra_txs: Vec<Transaction>,
}

pub fn build_test_block(prev_hash: BlockHash, parts: TestBlockParts) -> Block {
    let mut coinbase_outputs = vec![TxOut {
        script_pubkey: ScriptBuf::new(),
        value: Amount::from_sat(50_0000_0000),
    }];
    coinbase_outputs.extend(parts.extra_coinbase_outputs);
    let coinbase_tx = Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::all_zeros(),
                vout: 0xFFFFFFFF,
            },
            ..TxIn::default()
        }],
        output: coinbase_outputs,
    };
    let mut txdata = vec![coinbase_tx];
    txdata.extend(parts.extra_txs);
    Block {
        header: bitcoin::block::Header {
            version: bitcoin::block::Version::TWO,
            prev_blockhash: prev_hash,
            merkle_root: bitcoin::TxMerkleNode::all_zeros(),
            time: 0,
            bits: bitcoin::CompactTarget::from_consensus(0x2000_0000),
            nonce: 0,
        },
        txdata,
    }
}
