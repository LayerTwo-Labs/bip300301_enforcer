use bip300_messages::bitcoin::OutPoint;
use serde::{Deserialize, Serialize};

pub type Hash256 = [u8; 32];

#[derive(Debug, Serialize, Deserialize)]
pub struct Ctip {
    pub outpoint: OutPoint,
    pub value: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Deposit {
    pub address: Vec<u8>,
    pub value: u64,
    pub total_value: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Sidechain {
    pub sidechain_number: u8,
    pub data: Vec<u8>,
    pub vote_count: u16,
    pub proposal_height: u32,
    pub activation_height: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SidechainProposal {
    pub sidechain_number: u8,
    pub data: Vec<u8>,
    pub vote_count: u16,
    pub proposal_height: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Bundle {
    pub bundle_txid: Hash256,
    pub vote_count: u16,
}
