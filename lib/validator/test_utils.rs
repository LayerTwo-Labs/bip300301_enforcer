//! Shared test utilities for `crate::validator` tests.
//! This module is gated behind `#[cfg(test)]` in the parent module.

use bitcoin::{BlockHash, Txid, hashes::Hash as _};
use bitcoin_jsonrpsee::jsonrpsee;
use miette::IntoDiagnostic;

use super::{Validator, dbs::Dbs, main_rest_client::MainRestClient};
use crate::types::{
    M6id, NetworkParams, Sidechain, SidechainDescription, SidechainNumber, SidechainProposal,
    SidechainProposalStatus,
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
