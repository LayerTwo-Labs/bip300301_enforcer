//! Shared test utilities for `crate::validator` tests.
//! This module is gated behind `#[cfg(test)]` in the parent module.

use super::dbs::Dbs;
use crate::types::{
    Sidechain, SidechainDescription, SidechainNumber, SidechainProposal, SidechainProposalStatus,
};

pub fn create_test_dbs() -> (temp_dir::TempDir, Dbs) {
    let dir = temp_dir::TempDir::new().expect("failed to create temp dir");
    let dbs = Dbs::new(dir.path(), bitcoin::Network::Regtest).expect("failed to create test dbs");
    (dir, dbs)
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
