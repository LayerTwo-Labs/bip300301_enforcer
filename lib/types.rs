use std::{borrow::Cow, num::TryFromIntError, sync::Arc};

use bdk_wallet::chain::{ChainPosition, ConfirmationBlockTime};
use bitcoin::{
    Amount, BlockHash, Opcode, OutPoint, ScriptBuf, Transaction, Txid, Work,
    amount::CheckedSum as _,
    hashes::{Hash as _, sha256d},
    opcodes::{
        OP_TRUE,
        all::{OP_NOP5, OP_RETURN},
    },
    script::{Instruction, Instructions},
};
use derive_more::derive::{self, Display};
use hashlink::LinkedHashMap;
use miette::Diagnostic;
use nom::Finish;
use nonempty::NonEmpty;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::proto::{StatusBuilder, ToStatus};

pub const WITHDRAWAL_BUNDLE_MAX_AGE: u16 = 10;
pub const WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD: u16 = WITHDRAWAL_BUNDLE_MAX_AGE / 2; // 5

#[derive(derive::Debug, Clone, Copy, Display, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[debug("{}", hex::encode(_0))]
#[display("{}", hex::encode(_0))]
#[repr(transparent)]
pub struct BmmCommitment(pub [u8; 32]);

impl bitcoin::consensus::Decodable for BmmCommitment {
    fn consensus_decode<R: bitcoin::io::Read + ?Sized>(
        reader: &mut R,
    ) -> Result<Self, bitcoin::consensus::encode::Error> {
        bitcoin::consensus::Decodable::consensus_decode(reader).map(Self)
    }
}

impl bitcoin::consensus::Encodable for BmmCommitment {
    fn consensus_encode<W>(&self, writer: &mut W) -> Result<usize, bitcoin::io::Error>
    where
        W: bitcoin::io::Write + ?Sized,
    {
        self.0.consensus_encode(writer)
    }
}

impl<'de> Deserialize<'de> for BmmCommitment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            hex::serde::deserialize(deserializer).map(Self)
        } else {
            Deserialize::deserialize(deserializer).map(Self)
        }
    }
}

impl From<[u8; 32]> for BmmCommitment {
    fn from(inner: [u8; 32]) -> Self {
        Self(inner)
    }
}

impl Serialize for BmmCommitment {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            hex::serde::serialize(self.0, serializer)
        } else {
            self.0.serialize(serializer)
        }
    }
}

#[derive(
    Clone, Copy, Debug, Deserialize, Display, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct SidechainNumber(pub u8);

impl SidechainNumber {
    pub const MIN: Self = Self(u8::MIN);

    pub const MAX: Self = Self(u8::MAX);
}

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

#[derive(Clone, Copy, Debug, Deserialize, Display, Eq, Hash, PartialEq, Serialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct M6id(pub Txid);

impl From<[u8; 32]> for M6id {
    fn from(bytes: <Txid as bitcoin::hashes::Hash>::Bytes) -> Self {
        Self(Txid::from_byte_array(bytes))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SidechainProposalId {
    pub sidechain_number: SidechainNumber,
    pub description_hash: sha256d::Hash,
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

impl SidechainProposal {
    pub fn compute_id(&self) -> SidechainProposalId {
        SidechainProposalId {
            sidechain_number: self.sidechain_number,
            description_hash: self.description.sha256d_hash(),
        }
    }
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

#[derive(Clone, Debug)]
pub struct SidechainAck {
    pub sidechain_number: SidechainNumber,
    pub description_hash: sha256d::Hash,
}

#[derive(derive_more::Debug, Deserialize, Serialize)]
pub struct TreasuryUtxo {
    pub sidechain_number: SidechainNumber,
    pub outpoint: OutPoint,
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

#[derive(Clone, Copy, Debug, Serialize)]
pub struct HeaderInfo {
    pub block_hash: BlockHash,
    pub prev_block_hash: BlockHash,
    pub height: u32,
    pub work: Work,
    pub timestamp: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum WithdrawalBundleEventKind {
    Submitted,
    Failed,
    Succeeded {
        sequence_number: u64,
        transaction: bitcoin::Transaction,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WithdrawalBundleEvent {
    pub sidechain_id: SidechainNumber,
    pub m6id: M6id,
    pub kind: WithdrawalBundleEventKind,
}

/// BMM commitments for a single block
pub type BmmCommitments = LinkedHashMap<SidechainNumber, BmmCommitment>;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum BlockEvent {
    Deposit(Deposit),
    SidechainProposal {
        /// Coinbase vout
        vout: u32,
        proposal: SidechainProposal,
    },
    WithdrawalBundle(WithdrawalBundleEvent),
}

impl From<Deposit> for BlockEvent {
    fn from(deposit: Deposit) -> Self {
        Self::Deposit(deposit)
    }
}

impl From<WithdrawalBundleEvent> for BlockEvent {
    fn from(bundle_event: WithdrawalBundleEvent) -> Self {
        Self::WithdrawalBundle(bundle_event)
    }
}

/// Block info specific to some sidechain
#[derive(Clone, Debug, Serialize)]
pub struct SidechainBlockInfo<E = BlockEvent>
where
    E: std::borrow::Borrow<BlockEvent>,
{
    pub bmm_commitment: Option<BmmCommitment>,
    pub events: Vec<E>,
}

impl<E> SidechainBlockInfo<E>
where
    E: std::borrow::Borrow<BlockEvent>,
{
    pub fn to_owned(&self) -> SidechainBlockInfo<BlockEvent> {
        SidechainBlockInfo {
            bmm_commitment: self.bmm_commitment,
            events: self
                .events
                .iter()
                .map(|event| event.borrow().clone())
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BlockInfo {
    /// Sequential map of sidechain IDs to BMM commitments
    pub bmm_commitments: BmmCommitments,
    pub coinbase_txid: Txid,
    pub events: Vec<BlockEvent>,
}

impl BlockInfo {
    // Iterator over (vout, proposal)
    pub fn sidechain_proposals(
        &self,
    ) -> impl DoubleEndedIterator<Item = (u32, &SidechainProposal)> {
        self.events.iter().filter_map(|event| match event {
            BlockEvent::SidechainProposal { vout, proposal } => Some((*vout, proposal)),
            BlockEvent::Deposit(_) | BlockEvent::WithdrawalBundle(_) => None,
        })
    }

    pub fn withdrawal_bundle_events(
        &self,
    ) -> impl DoubleEndedIterator<Item = &WithdrawalBundleEvent> {
        self.events.iter().filter_map(|event| match event {
            BlockEvent::WithdrawalBundle(bundle_event) => Some(bundle_event),
            BlockEvent::Deposit(_) | BlockEvent::SidechainProposal { .. } => None,
        })
    }

    /// Return block info specific to the specified sidechain
    pub fn only_sidechain(
        &self,
        sidechain_number: SidechainNumber,
    ) -> SidechainBlockInfo<&BlockEvent> {
        let bmm_commitment = self.bmm_commitments.get(&sidechain_number).copied();
        let events = self
            .events
            .iter()
            .filter(|event| match event {
                BlockEvent::Deposit(deposit) => deposit.sidechain_id == sidechain_number,
                BlockEvent::SidechainProposal { .. } => false,
                BlockEvent::WithdrawalBundle(bundle_event) => {
                    bundle_event.sidechain_id == sidechain_number
                }
            })
            .collect();
        SidechainBlockInfo {
            bmm_commitment,
            events,
        }
    }
}

/// Two-way peg data for a single block
#[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BDKWalletTransaction {
    pub txid: bitcoin::Txid,
    pub tx: Arc<Transaction>,
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

pub const OP_DRIVECHAIN: Opcode = OP_NOP5;

/// Create an OP_DRIVECHAIN script for the specified sidechain
pub fn op_drivechain_script(sidechain_number: SidechainNumber) -> ScriptBuf {
    let mut res = ScriptBuf::new();
    res.push_opcode(OP_DRIVECHAIN);
    res.push_slice([sidechain_number.0]);
    res.push_opcode(OP_TRUE);
    res
}

#[derive(Debug, Diagnostic, Error)]
#[error("Amount overflow")]
pub struct AmountOverflowError;

impl ToStatus for AmountOverflowError {
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::new(self)
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("Amount underflow")]
pub struct AmountUnderflowError;

impl ToStatus for AmountUnderflowError {
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::new(self)
    }
}

#[derive(Debug, Diagnostic, Error)]
enum BlindedM6FeeOutputError {
    #[error("Invalid spk: {spk}")]
    InvalidSpk { spk: ScriptBuf },
    #[error("Nonzero value")]
    NonZeroValue,
    #[error("Failed to parse script")]
    Script(#[from] bitcoin::script::Error),
}

impl ToStatus for BlindedM6FeeOutputError {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::InvalidSpk { .. } | Self::NonZeroValue | Self::Script(_) => {
                StatusBuilder::new(self)
            }
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
enum BlindedM6ErrorInner {
    #[error("Invalid fee output")]
    InvalidFeeOutput(#[from] BlindedM6FeeOutputError),
    #[error("Missing fee output")]
    MissingFeeOutput,
    #[error("Inputs must be empty")]
    NonEmptyInputs,
    #[error(transparent)]
    OutputAmountOverflow(#[from] AmountOverflowError),
    #[error("Payout cannot be zero")]
    ZeroPayout,
}

impl ToStatus for BlindedM6ErrorInner {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::InvalidFeeOutput(err) => StatusBuilder::with_code(self, err.builder()),
            Self::OutputAmountOverflow(err) => StatusBuilder::new(err),
            Self::MissingFeeOutput | Self::NonEmptyInputs | Self::ZeroPayout => {
                StatusBuilder::new(self)
            }
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("Blinded M6 error")]
pub struct BlindedM6Error(#[from] BlindedM6ErrorInner);

impl From<AmountOverflowError> for BlindedM6Error {
    fn from(err: AmountOverflowError) -> Self {
        BlindedM6ErrorInner::from(err).into()
    }
}

impl From<BlindedM6FeeOutputError> for BlindedM6Error {
    fn from(err: BlindedM6FeeOutputError) -> Self {
        BlindedM6ErrorInner::from(err).into()
    }
}

impl ToStatus for BlindedM6Error {
    fn builder(&self) -> StatusBuilder {
        self.0.builder()
    }
}

#[derive(Clone, Debug)]
pub struct BlindedM6<'a> {
    fee: Amount,
    /// MUST be non-zero
    payout: Amount,
    tx: Cow<'a, bitcoin::Transaction>,
}

#[allow(dead_code, reason = "will be used later for M4/M6")]
impl<'a> BlindedM6<'a> {
    pub fn fee(&self) -> &Amount {
        &self.fee
    }

    pub fn payout(&self) -> &Amount {
        &self.payout
    }

    pub fn tx(self) -> Cow<'a, bitcoin::Transaction> {
        self.tx
    }

    pub fn into_owned(self) -> BlindedM6<'static> {
        BlindedM6 {
            fee: self.fee,
            payout: self.payout,
            tx: Cow::Owned(self.tx.into_owned()),
        }
    }

    pub fn compute_m6id(&self) -> M6id {
        M6id(self.tx.compute_txid())
    }

    // TODO: remove sidechain_number param
    pub fn into_m6(
        self,
        sidechain_number: SidechainNumber,
        treasury_outpoint: OutPoint,
        treasury_value: Amount,
    ) -> Result<bitcoin::Transaction, AmountUnderflowError> {
        let Self { fee, payout, tx } = self;
        let mut tx = tx.into_owned();
        let first_output = tx
            .output
            .first_mut()
            .expect("Blinded M6 should have a fee output at index 0");
        // Push treasury output
        let treasury_output = {
            let value = treasury_value
                .checked_sub(payout)
                .ok_or(AmountUnderflowError)?
                .checked_sub(fee)
                .ok_or(AmountUnderflowError)?;
            bitcoin::TxOut {
                script_pubkey: op_drivechain_script(sidechain_number),
                value,
            }
        };
        *first_output = treasury_output;
        // Push treasury input
        assert!(tx.input.is_empty());
        let treasury_input = bitcoin::TxIn {
            previous_output: treasury_outpoint,
            ..Default::default()
        };
        tx.input.push(treasury_input);
        Ok(tx)
    }
}

impl AsRef<bitcoin::Transaction> for BlindedM6<'_> {
    fn as_ref(&self) -> &bitcoin::Transaction {
        &self.tx
    }
}

impl<'a> TryFrom<Cow<'a, bitcoin::Transaction>> for BlindedM6<'a> {
    type Error = BlindedM6Error;

    fn try_from(tx: Cow<'a, bitcoin::Transaction>) -> Result<Self, Self::Error> {
        if !tx.input.is_empty() {
            return Err(BlindedM6ErrorInner::NonEmptyInputs.into());
        }
        let Some(fee_output) = tx.output.first() else {
            return Err(BlindedM6ErrorInner::MissingFeeOutput.into());
        };
        if fee_output.value != Amount::ZERO {
            return Err(BlindedM6FeeOutputError::NonZeroValue.into());
        }
        let payout = tx
            .output
            .iter()
            .map(|output| output.value)
            .checked_sum()
            .ok_or(AmountOverflowError)?;
        if payout == Amount::ZERO {
            return Err(BlindedM6ErrorInner::ZeroPayout.into());
        }
        let mut instructions = fee_output.script_pubkey.instructions();
        fn next_instruction<'a>(
            instructions: &'a mut Instructions,
        ) -> Result<Option<Instruction<'a>>, BlindedM6FeeOutputError> {
            instructions
                .next()
                .transpose()
                .map_err(BlindedM6FeeOutputError::from)
        }
        fn invalid_spk(tx: Cow<'_, bitcoin::Transaction>) -> BlindedM6FeeOutputError {
            let spk = tx.output.first().unwrap().script_pubkey.clone();
            BlindedM6FeeOutputError::InvalidSpk { spk }
        }
        let Some(Instruction::Op(OP_RETURN)) = next_instruction(&mut instructions)? else {
            return Err(invalid_spk(tx).into());
        };
        let Some(Instruction::PushBytes(fee_be_bytes)) = next_instruction(&mut instructions)?
        else {
            return Err(invalid_spk(tx).into());
        };
        let Some(fee_be_bytes): Option<[u8; 8]> = fee_be_bytes.as_bytes().try_into().ok() else {
            return Err(invalid_spk(tx).into());
        };
        let fee = Amount::from_sat(u64::from_be_bytes(fee_be_bytes));
        if instructions.next().is_none() {
            Ok(Self { fee, payout, tx })
        } else {
            Err(invalid_spk(tx).into())
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct PendingM6idInfo {
    pub vote_count: u16,
    pub proposal_height: u32,
}

impl PendingM6idInfo {
    pub fn new(proposal_height: u32) -> Self {
        Self {
            vote_count: 0,
            proposal_height,
        }
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
