use std::num::TryFromIntError;

use bitcoin::{Amount, BlockHash, OutPoint, TxOut, Work};
use hashlink::LinkedHashMap;
use miette::Result;
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

impl SidechainProposal {
    /// Deserialize the sidechain proposal, returning "raw" nom errors. This also means we're
    /// not validating that the title and description are valid UTF-8. This is probably possible
    /// with nom, but the author doesn't know how. It's fine to do this in the outer function,
    /// anyways.
    fn try_deserialize_unvalidated(
        &self,
    ) -> nom::IResult<&[u8], (u8, UnvalidatedDeserializedSidechainProposalV1)> {
        use nom::bytes::complete::tag;
        use nom::bytes::complete::take;
        use nom::number::complete::be_u8;

        let input = self.data.as_slice();
        let (input, sidechain_number) = be_u8(input)?;

        const VERSION_0: u8 = 0;
        let (input, _) = tag(&[VERSION_0])(input)?;

        let (input, title_length) = be_u8(input)?;
        let (input, title) = take(title_length)(input)?;

        const HASH_ID_1_LENGTH: usize = 32;
        const HASH_ID_2_LENGTH: usize = 20;

        let description_length = input.len() - HASH_ID_1_LENGTH - HASH_ID_2_LENGTH;
        let (input, description) = take(description_length)(input)?;

        let (input, hash_id_1) = take(HASH_ID_1_LENGTH)(input)?;
        let (input, hash_id_2) = take(HASH_ID_2_LENGTH)(input)?;

        let hash_id_1: [u8; HASH_ID_1_LENGTH] = hash_id_1.try_into().unwrap();
        let hash_id_2: [u8; HASH_ID_2_LENGTH] = hash_id_2.try_into().unwrap();

        let parsed = UnvalidatedDeserializedSidechainProposalV1 {
            title: title.to_vec(),
            description: description.to_vec(),
            hash_id_1,
            hash_id_2,
        };

        Ok((input, (sidechain_number, parsed)))
    }

    pub fn try_deserialize(&self) -> Result<(SidechainNumber, DeserializedSidechainProposalV1)> {
        let (input, (sidechain_number, unvalidated)) =
            self.try_deserialize_unvalidated()
                .map_err(|err| match err {
                    nom::Err::Error(nom::error::Error {
                        code: nom::error::ErrorKind::Tag,
                        input,
                    }) => SidechainProposalError::UnknownVersion(input[0]),
                    _ => SidechainProposalError::FailedToDeserialize(err.to_owned()),
                })?;

        // One might think "this would be a good place to check there's no trailing bytes".
        // That doesn't work! Our parsing logic consumes _all_ bytes, because the length of the
        // `description` field is defined as the total length of the entire input minus
        // the length of all the other, known-length fields. We therefore _always_ read
        // the entire input.
        if !input.is_empty() {
            panic!("somehow ended up with trailing bytes")
        }

        let sidechain_number = SidechainNumber::from(sidechain_number);

        let title = String::from_utf8(unvalidated.title)
            .map_err(SidechainProposalError::InvalidUtf8Title)?;

        let description = String::from_utf8(unvalidated.description)
            .map_err(SidechainProposalError::InvalidUtf8Description)?;

        Ok((
            sidechain_number,
            DeserializedSidechainProposalV1 {
                title,
                description,
                hash_id_1: unvalidated.hash_id_1,
                hash_id_2: unvalidated.hash_id_2,
            },
        ))
    }
}

/// We know how to deserialize M1 v1 messages from BIP300
/// https://github.com/bitcoin/bips/blob/master/bip-0300.mediawiki#m1----propose-sidechain
///
///    1-byte nSidechain
///    4-byte nVersion
///    1-byte title length
///    x-byte title
///    x-byte description
///    32-byte hashID1
///    20-byte hashID2
///
/// Description length is total_data_length - length_of_all_other_fields
#[derive(Debug)]
pub struct DeserializedSidechainProposalV1 {
    pub title: String,
    pub description: String,
    pub hash_id_1: [u8; 32],
    pub hash_id_2: [u8; 20],
}

struct UnvalidatedDeserializedSidechainProposalV1 {
    pub title: Vec<u8>,
    pub description: Vec<u8>,
    pub hash_id_1: [u8; 32],
    pub hash_id_2: [u8; 20],
}
use miette::Diagnostic;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum SidechainProposalError {
    #[error("Invalid UTF-8 sequence in title")]
    #[diagnostic(code(sidechain_proposal::invalid_utf8_title))]
    InvalidUtf8Title(std::string::FromUtf8Error),

    #[error("Invalid UTF-8 sequence in description")]
    #[diagnostic(code(sidechain_proposal::invalid_utf8_description))]
    InvalidUtf8Description(std::string::FromUtf8Error),

    #[error("Failed to deserialize sidechain proposal")]
    #[diagnostic(code(sidechain_proposal::failed_to_deserialize))]
    FailedToDeserialize(nom::Err<nom::error::Error<Vec<u8>>>),

    #[error("Unknown sidechain proposal version: {0}")]
    #[diagnostic(code(sidechain_proposal::unknown_version))]
    UnknownVersion(u8),
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

#[cfg(test)]
mod tests {
    use crate::types::SidechainNumber;
    use crate::types::SidechainProposal;

    fn proposal(data: Vec<u8>) -> SidechainProposal {
        SidechainProposal {
            sidechain_number: SidechainNumber(1),
            data,
            vote_count: 0,
            proposal_height: 0,
        }
    }

    const EMPTY_HASH_ID_1: [u8; 32] = [1u8; 32];
    const EMPTY_HASH_ID_2: [u8; 20] = [2u8; 20];

    #[test]
    fn test_try_deserialize_valid_data() {
        let sidechain_proposal = proposal(vec![
            1, // sidechain_number
            0, // version
            5, // title length
            b'H', b'e', b'l', b'l', b'o', // title
            b'W', b'o', b'r', b'l', b'd', // description
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 32, // hash_id_1
            20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2,
            1, // hash_id_2
        ]);

        let result = sidechain_proposal.try_deserialize();
        assert!(result.is_ok());

        let (sidechain_number, deserialized) = result.unwrap();
        assert_eq!(sidechain_number, SidechainNumber::from(1));
        assert_eq!(deserialized.title, "Hello");
        assert_eq!(deserialized.description, "World");

        let expected_hash_id_1 = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 32,
        ];

        let expected_hash_id_2 = [
            20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1,
        ];

        assert_eq!(deserialized.hash_id_1, expected_hash_id_1);
        assert_eq!(deserialized.hash_id_2, expected_hash_id_2);
    }

    #[test]
    fn test_try_deserialize_invalid_utf8_title() {
        let sidechain_proposal = proposal(
            [
                vec![
                    1, // sidechain_number
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

        let result = sidechain_proposal.try_deserialize();
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
                    1, // sidechain_number
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

        let result = sidechain_proposal.try_deserialize();
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

        let result = sidechain_proposal.try_deserialize();
        assert!(result.is_err());
        assert_eq!(
            format!("{}", result.unwrap_err().code().unwrap()),
            "sidechain_proposal::unknown_version"
        );
    }
}
