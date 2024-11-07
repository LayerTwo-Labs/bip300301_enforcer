use std::num::TryFromIntError;

use bdk_wallet::chain::{ChainPosition, ConfirmationBlockTime};
use bitcoin::{
    hashes::{sha256d, Hash as _},
    Amount, BlockHash, OutPoint, Txid, Work,
};
use derive_more::derive::Display;
use hashlink::LinkedHashMap;
use miette::Diagnostic;
use nom::Finish;
use nonempty::NonEmpty;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type Hash256 = [u8; 32];

#[derive(
    Clone, Copy, Debug, Deserialize, Display, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
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

#[derive(Clone, Debug, Deserialize, Display, Eq, PartialEq, Serialize)]
#[display("{}", hex::encode(_0))]
#[repr(transparent)]
#[serde(transparent)]
pub struct SidechainDescription(pub Vec<u8>);

impl SidechainDescription {
    pub fn sha256d_hash(&self) -> bitcoin::hashes::sha256d::Hash {
        bitcoin::hashes::sha256d::Hash::hash(&self.0)
    }
}

impl From<Vec<u8>> for SidechainDescription {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl bitcoin::consensus::Encodable for SidechainDescription {
    fn consensus_encode<W: bitcoin::io::Write + ?Sized>(
        &self,
        writer: &mut W,
    ) -> Result<usize, bitcoin::io::Error> {
        self.0.consensus_encode(writer)
    }
}

#[derive(Debug, Error)]
pub enum DeserializeSidechainProposalError {
    #[error("Missing sidechain number")]
    MissingSidechainNumber,
}

#[derive(Clone, Debug, Deserialize, Display, Eq, PartialEq, Serialize)]
#[display(
    "{{ sidechain_number: {}, description: {description} }}",
    sidechain_number.0
)]
pub struct SidechainProposal {
    pub sidechain_number: SidechainNumber,
    pub description: SidechainDescription,
}

impl From<NonEmpty<u8>> for SidechainProposal {
    fn from(bytes: NonEmpty<u8>) -> Self {
        Self {
            sidechain_number: bytes.head.into(),
            description: bytes.tail.into(),
        }
    }
}

impl TryFrom<Vec<u8>> for SidechainProposal {
    type Error = DeserializeSidechainProposalError;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        match NonEmpty::from_vec(bytes) {
            Some(non_empty) => Ok(non_empty.into()),
            None => Err(Self::Error::MissingSidechainNumber),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct SidechainProposalStatus {
    pub vote_count: u16,
    pub proposal_height: u32,
    pub activation_height: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Sidechain {
    pub proposal: SidechainProposal,
    pub status: SidechainProposalStatus,
}

#[derive(Debug, Error, Diagnostic)]
pub enum ParseSidechainDeclarationError {
    #[error("Invalid UTF-8 sequence in title")]
    #[diagnostic(code(sidechain_proposal::invalid_utf8_title))]
    InvalidUtf8Title(std::str::Utf8Error),

    #[error("Invalid UTF-8 sequence in description")]
    #[diagnostic(code(sidechain_proposal::invalid_utf8_description))]
    InvalidUtf8Description(std::str::Utf8Error),

    #[error("Failed to deserialize sidechain declaration")]
    #[diagnostic(code(sidechain_proposal::failed_to_deserialize))]
    FailedToDeserialize(nom::error::Error<Vec<u8>>),

    #[error("Unknown sidechain declaration version: {0}")]
    #[diagnostic(code(sidechain_proposal::unknown_version))]
    UnknownVersion(u8),
}

/// We know how to deserialize M1 v1 messages from BIP300
/// https://github.com/bitcoin/bips/blob/master/bip-0300.mediawiki#m1----propose-sidechain
///
///    1-byte nSidechain
///    4-byte nVersion
///    1-byte title length
///    x-byte title
///    x-byte description
///    32-byte hashID1f
///    20-byte hashID2
///
/// Description length is total_data_length - length_of_all_other_fields
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SidechainDeclaration {
    pub title: String,
    pub description: String,
    pub hash_id_1: [u8; 32],
    pub hash_id_2: [u8; 20],
}

impl SidechainDeclaration {
    /// Parse the sidechain declaration, returning "raw" nom errors. This also means we're
    /// not validating that the title and description are valid UTF-8. This is probably possible
    /// with nom, but the author doesn't know how. It's fine to do this in the outer function,
    /// anyways.
    fn try_parse(input: &[u8]) -> nom::IResult<&[u8], Self, ParseSidechainDeclarationError> {
        use nom::{
            bytes::complete::{tag, take},
            number::complete::be_u8,
        };
        fn failed_to_deserialize(
            err: nom::Err<nom::error::Error<&[u8]>>,
        ) -> nom::Err<ParseSidechainDeclarationError> {
            err.to_owned()
                .map(ParseSidechainDeclarationError::FailedToDeserialize)
        }
        const VERSION_0: u8 = 0;
        let (input, _) =
            tag(&[VERSION_0])(input).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
                nom::Err::Error(ParseSidechainDeclarationError::UnknownVersion(input[0]))
            })?;

        let (input, title_length) = be_u8(input).map_err(failed_to_deserialize)?;
        let (input, title_bytes) = take(title_length)(input).map_err(failed_to_deserialize)?;
        let title = std::str::from_utf8(title_bytes).map_err(|err| {
            nom::Err::Error(ParseSidechainDeclarationError::InvalidUtf8Title(err))
        })?;

        const HASH_ID_1_LENGTH: usize = 32;
        const HASH_ID_2_LENGTH: usize = 20;

        let description_length = input.len() - HASH_ID_1_LENGTH - HASH_ID_2_LENGTH;
        let (input, description_bytes) =
            take(description_length)(input).map_err(failed_to_deserialize)?;
        let description = std::str::from_utf8(description_bytes).map_err(|err| {
            nom::Err::Error(ParseSidechainDeclarationError::InvalidUtf8Description(err))
        })?;

        let (input, hash_id_1) = take(HASH_ID_1_LENGTH)(input).map_err(failed_to_deserialize)?;
        let (input, hash_id_2) = take(HASH_ID_2_LENGTH)(input).map_err(failed_to_deserialize)?;

        let hash_id_1: [u8; HASH_ID_1_LENGTH] = hash_id_1.try_into().unwrap();
        let hash_id_2: [u8; HASH_ID_2_LENGTH] = hash_id_2.try_into().unwrap();

        let parsed = Self {
            title: title.to_owned(),
            description: description.to_owned(),
            hash_id_1,
            hash_id_2,
        };

        Ok((input, parsed))
    }
}

impl TryFrom<&SidechainDescription> for SidechainDeclaration {
    type Error = ParseSidechainDeclarationError;

    fn try_from(sidechain_description: &SidechainDescription) -> Result<Self, Self::Error> {
        let (input, sidechain_declaration) =
            SidechainDeclaration::try_parse(&sidechain_description.0).finish()?;
        // One might think "this would be a good place to check there's no trailing bytes".
        // That doesn't work! Our parsing logic consumes _all_ bytes, because the length of the
        // `description` field is defined as the total length of the entire input minus
        // the length of all the other, known-length fields. We therefore _always_ read
        // the entire input.
        assert!(input.is_empty(), "somehow ended up with trailing bytes");
        Ok(sidechain_declaration)
    }
}

#[derive(Debug, Clone)]
pub struct SidechainAck {
    pub sidechain_number: SidechainNumber,
    pub description_hash: sha256d::Hash,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PendingM6id {
    pub m6id: Hash256,
    pub vote_count: u16,
}

#[derive(derive_more::Debug, Deserialize, Serialize)]
pub struct TreasuryUtxo {
    pub outpoint: OutPoint,
    #[debug("{:?}", address.as_ref().map(hex::encode))]
    pub address: Option<Vec<u8>>,
    pub total_value: Amount,
    pub previous_total_value: Amount,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Deposit {
    pub sidechain_id: SidechainNumber,
    pub sequence_number: u64,
    pub outpoint: OutPoint,
    pub address: Vec<u8>,
    pub value: Amount,
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

#[derive(Clone, Debug)]
pub struct BlockInfo {
    /// Sequential map of sidechain IDs to BMM commitments
    pub bmm_commitments: BmmCommitments,
    pub coinbase_txid: Txid,
    pub deposits: Vec<Deposit>,
    /// Sidechain proposals, sorted by coinbase vout
    pub sidechain_proposals: Vec<(u32, SidechainProposal)>,
    pub withdrawal_bundle_events: Vec<WithdrawalBundleEvent>,
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

#[derive(Debug)]
pub struct BDKWalletTransaction {
    pub txid: bitcoin::Txid,
    pub chain_position: ChainPosition<ConfirmationBlockTime>,
    pub fee: Amount,
    pub received: Amount,
    pub sent: Amount,
}

#[derive(Debug)]
pub enum FeePolicy {
    Absolute(Amount),
    Rate(bitcoin::FeeRate),
}

impl From<Amount> for FeePolicy {
    fn from(amount: Amount) -> Self {
        Self::Absolute(amount)
    }
}

impl From<bitcoin::FeeRate> for FeePolicy {
    fn from(fee_rate: bitcoin::FeeRate) -> Self {
        Self::Rate(fee_rate)
    }
}

#[cfg(test)]
mod tests {
    use miette::Diagnostic as _;

    use crate::types::{SidechainDeclaration, SidechainNumber, SidechainProposal};

    fn proposal(description: Vec<u8>) -> SidechainProposal {
        SidechainProposal {
            sidechain_number: SidechainNumber(1),
            description: description.into(),
        }
    }

    const EMPTY_HASH_ID_1: [u8; 32] = [1u8; 32];
    const EMPTY_HASH_ID_2: [u8; 20] = [2u8; 20];

    #[test]
    fn test_try_parse_valid_data() {
        let sidechain_proposal = proposal(vec![
            0, // version
            5, // title length
            b'H', b'e', b'l', b'l', b'o', // title
            b'W', b'o', b'r', b'l', b'd', // description
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 32, // hash_id_1
            20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2,
            1, // hash_id_2
        ]);

        let result = (&sidechain_proposal.description).try_into();
        assert!(result.is_ok());

        let parsed: SidechainDeclaration = result.unwrap();
        assert_eq!(parsed.title, "Hello");
        assert_eq!(parsed.description, "World");

        let expected_hash_id_1 = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 32,
        ];

        let expected_hash_id_2 = [
            20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1,
        ];

        assert_eq!(parsed.hash_id_1, expected_hash_id_1);
        assert_eq!(parsed.hash_id_2, expected_hash_id_2);
    }

    #[test]
    fn test_try_deserialize_invalid_utf8_title() {
        let sidechain_proposal = proposal(
            [
                vec![
                    0, // version
                    5, // title length
                    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // invalid UTF-8 title
                    b'W', b'o', b'r', b'l', b'd', // description
                ],
                EMPTY_HASH_ID_1.to_vec(),
                EMPTY_HASH_ID_2.to_vec(),
            ]
            .concat(),
        );

        let result: Result<SidechainDeclaration, _> = (&sidechain_proposal.description).try_into();
        assert!(result.is_err());
        assert_eq!(
            format!("{}", result.unwrap_err().code().unwrap()),
            "sidechain_proposal::invalid_utf8_title"
        );
    }

    #[test]
    fn test_try_deserialize_invalid_utf8_description() {
        let sidechain_proposal = proposal(
            [
                vec![
                    0, // version
                    5, // title length
                    b'H', b'e', b'l', b'l', b'o', // titl
                    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // invalid UTF-8 description
                ],
                EMPTY_HASH_ID_1.to_vec(),
                EMPTY_HASH_ID_2.to_vec(),
            ]
            .concat(),
        );

        let result: Result<SidechainDeclaration, _> = (&sidechain_proposal.description).try_into();
        assert!(result.is_err());
        assert_eq!(
            format!("{}", result.unwrap_err().code().unwrap()),
            "sidechain_proposal::invalid_utf8_description"
        );
    }

    #[test]
    fn test_try_deserialize_bad_version() {
        let sidechain_proposal = proposal(vec![
            1, // sidechain_number
            1, // version
            0, // trailing byte
        ]);

        let result: Result<SidechainDeclaration, _> = (&sidechain_proposal.description).try_into();
        assert!(result.is_err());
        assert_eq!(
            format!("{}", result.unwrap_err().code().unwrap()),
            "sidechain_proposal::unknown_version"
        );
    }
}
