use std::num::TryFromIntError;

use bitcoin::{Amount, BlockHash, OutPoint, TxOut, Work};
use hashlink::LinkedHashMap;
use serde::{Deserialize, Serialize};

pub type Hash256 = [u8; 32];

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct SidechainNumber(pub u8);

impl From<u8> for SidechainNumber {
    #[inline(always)]
    fn from(sidechain_number: u8) -> Self {
        Self(sidechain_number)
    }
}

// Used by protos
impl TryFrom<u32> for SidechainNumber {
    type Error = TryFromIntError;

    #[inline(always)]
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        value.try_into().map(SidechainNumber)
    }
}

impl From<SidechainNumber> for u8 {
    #[inline(always)]
    fn from(sidechain_number: SidechainNumber) -> Self {
        sidechain_number.0
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Ctip {
    pub outpoint: OutPoint,
    pub value: Amount,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Sidechain {
    pub sidechain_number: SidechainNumber,
    pub data: Vec<u8>,
    pub vote_count: u16,
    pub proposal_height: u32,
    pub activation_height: u32,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SidechainProposal {
    pub sidechain_number: SidechainNumber,
    pub data: Vec<u8>,
    pub vote_count: u16,
    pub proposal_height: u32,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PendingM6id {
    pub m6id: Hash256,
    pub vote_count: u16,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TreasuryUtxo {
    pub outpoint: OutPoint,
    pub address: Option<Vec<u8>>,
    pub total_value: Amount,
    pub previous_total_value: Amount,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Deposit {
    pub sidechain_id: SidechainNumber,
    pub sequence_number: u64,
    pub outpoint: OutPoint,
    pub output: TxOut,
}

#[derive(Clone, Copy, Debug)]
pub struct HeaderInfo {
    pub block_hash: BlockHash,
    pub prev_block_hash: BlockHash,
    pub height: u32,
    pub work: Work,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum WithdrawalBundleEventKind {
    Submitted,
    Failed,
    Succeeded,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WithdrawalBundleEvent {
    pub sidechain_id: SidechainNumber,
    pub m6id: Hash256,
    pub kind: WithdrawalBundleEventKind,
}

/// BMM commitments for a single block
pub type BmmCommitments = LinkedHashMap<SidechainNumber, Hash256>;

#[derive(Clone, Debug, Default)]
pub struct BlockInfo {
    pub deposits: Vec<Deposit>,
    pub withdrawal_bundle_events: Vec<WithdrawalBundleEvent>,
    /// Sequential map of sidechain IDs to BMM commitments
    pub bmm_commitments: BmmCommitments,
}

/// Two-way peg data for a single block
#[derive(Clone, Debug)]
pub struct TwoWayPegData {
    pub header_info: HeaderInfo,
    pub block_info: BlockInfo,
}

#[derive(Clone, Debug)]
pub enum Event {
    ConnectBlock {
        header_info: HeaderInfo,
        block_info: BlockInfo,
    },
    DisconnectBlock {
        block_hash: BlockHash,
    },
}
