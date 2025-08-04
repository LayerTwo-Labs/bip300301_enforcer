use std::collections::{HashMap, HashSet, hash_map};

use bitcoin::{
    Amount, BlockHash, Script, ScriptBuf, Transaction, TxOut,
    amount::CheckedSum,
    hashes::{Hash, sha256d},
    opcodes::{
        OP_TRUE,
        all::{OP_PUSHBYTES_1, OP_RETURN},
    },
    script::{Instruction, Instructions, PushBytesBuf},
};
use byteorder::{ByteOrder, LittleEndian};
use miette::Diagnostic;
use nom::{
    IResult,
    branch::alt,
    bytes::complete::{tag, take},
    combinator::{fail, rest},
    multi::many0,
};
use thiserror::Error;

use crate::{
    errors::ErrorChain,
    proto::{StatusBuilder, ToStatus},
    types::{
        BmmCommitment, M6id, OP_DRIVECHAIN, SidechainDeclaration, SidechainDescription,
        SidechainNumber, SidechainProposal, SidechainProposalId,
    },
};

#[derive(Debug)]
pub struct M1ProposeSidechain {
    pub sidechain_number: SidechainNumber,
    pub description: SidechainDescription,
}

impl M1ProposeSidechain {
    pub const TAG: [u8; 4] = [0xD5, 0xE0, 0xC4, 0xAF];

    fn parse(input: &[u8]) -> IResult<&[u8], Self> {
        let (input, sidechain_number) = take(1usize)(input)?;
        let sidechain_number = sidechain_number[0];
        let (input, description) = rest(input)?;
        let description = description.to_vec().into();
        let message = Self {
            sidechain_number: SidechainNumber::from(sidechain_number),
            description,
        };
        Ok((input, message))
    }
}

impl TryFrom<M1ProposeSidechain> for ScriptBuf {
    type Error = bitcoin::script::PushBytesError;

    fn try_from(m1: M1ProposeSidechain) -> Result<Self, Self::Error> {
        let M1ProposeSidechain {
            sidechain_number,
            description,
        } = m1;
        let message = [
            &M1ProposeSidechain::TAG[..],
            &[sidechain_number.into()],
            &description.0,
        ]
        .concat();
        let data = PushBytesBuf::try_from(message)?;
        Ok(ScriptBuf::new_op_return(&data))
    }
}

#[derive(Debug)]
pub struct M2AckSidechain {
    pub sidechain_number: SidechainNumber,
    pub description_hash: sha256d::Hash,
}

impl M2AckSidechain {
    pub const TAG: [u8; 4] = [0xD6, 0xE1, 0xC5, 0xDF];

    fn parse(input: &[u8]) -> IResult<&[u8], Self> {
        let (input, sidechain_number) = take(1usize)(input)?;
        let sidechain_number = sidechain_number[0];
        let (input, description_hash) = take(32usize)(input)?;
        let description_hash: [u8; 32] = description_hash.try_into().unwrap();
        let message = Self {
            sidechain_number: SidechainNumber::from(sidechain_number),
            description_hash: sha256d::Hash::from_byte_array(description_hash),
        };
        Ok((input, message))
    }
}

impl TryFrom<M2AckSidechain> for ScriptBuf {
    type Error = bitcoin::script::PushBytesError;

    fn try_from(m2: M2AckSidechain) -> Result<Self, Self::Error> {
        let M2AckSidechain {
            sidechain_number,
            description_hash,
        } = m2;
        let message = [
            &M2AckSidechain::TAG[..],
            &[sidechain_number.into()],
            description_hash.as_byte_array(),
        ]
        .concat();
        let data = PushBytesBuf::try_from(message)?;
        Ok(ScriptBuf::new_op_return(&data))
    }
}

#[derive(Debug)]
pub struct M3ProposeBundle {
    pub sidechain_number: SidechainNumber,
    pub bundle_txid: [u8; 32],
}

impl M3ProposeBundle {
    pub const TAG: [u8; 4] = [0xD4, 0x5A, 0xA9, 0x43];

    fn parse(input: &[u8]) -> IResult<&[u8], Self> {
        let (input, sidechain_number) = take(1usize)(input)?;
        let sidechain_number = sidechain_number[0];
        let (input, bundle_txid) = take(32usize)(input)?;
        let bundle_txid: [u8; 32] = bundle_txid.try_into().unwrap();
        let message = Self {
            sidechain_number: SidechainNumber::from(sidechain_number),
            bundle_txid,
        };
        Ok((input, message))
    }
}

impl TryFrom<M3ProposeBundle> for ScriptBuf {
    type Error = bitcoin::script::PushBytesError;

    fn try_from(m3: M3ProposeBundle) -> Result<Self, Self::Error> {
        let M3ProposeBundle {
            sidechain_number,
            bundle_txid,
        } = m3;
        let message = [
            &M3ProposeBundle::TAG[..],
            &[sidechain_number.into()],
            &bundle_txid,
        ]
        .concat();
        let data = PushBytesBuf::try_from(message)?;
        Ok(ScriptBuf::new_op_return(&data))
    }
}

#[derive(Debug)]
pub enum M4AckBundles {
    RepeatPrevious,
    OneByte { upvotes: Vec<u8> },
    TwoBytes { upvotes: Vec<u16> },
    LeadingBy50,
}

impl M4AckBundles {
    pub const TAG: [u8; 4] = [0xD7, 0x7D, 0x17, 0x76];

    pub const ABSTAIN_ONE_BYTE: u8 = 0xFF;
    pub const ABSTAIN_TWO_BYTES: u16 = 0xFFFF;
    pub const ALARM_ONE_BYTE: u8 = 0xFE;
    pub const ALARM_TWO_BYTES: u16 = 0xFFFE;
    const REPEAT_PREVIOUS_TAG: [u8; 1] = [0x00];
    const ONE_BYTE_TAG: [u8; 1] = [0x01];
    const TWO_BYTES_TAG: [u8; 1] = [0x02];
    const LEADING_BY_50_TAG: [u8; 1] = [0x03];

    fn tag(&self) -> [u8; 1] {
        match self {
            Self::RepeatPrevious => Self::REPEAT_PREVIOUS_TAG,
            Self::OneByte { .. } => Self::ONE_BYTE_TAG,
            Self::TwoBytes { .. } => Self::TWO_BYTES_TAG,
            Self::LeadingBy50 { .. } => Self::LEADING_BY_50_TAG,
        }
    }

    fn parse(input: &[u8]) -> IResult<&[u8], Self> {
        let (input, m4_tag) = alt((
            tag(Self::REPEAT_PREVIOUS_TAG),
            tag(Self::ONE_BYTE_TAG),
            tag(Self::TWO_BYTES_TAG),
            tag(Self::LEADING_BY_50_TAG),
        ))(input)?;

        if m4_tag == Self::REPEAT_PREVIOUS_TAG {
            let message = M4AckBundles::RepeatPrevious;
            return Ok((input, message));
        } else if m4_tag == Self::ONE_BYTE_TAG {
            let (input, upvotes) = rest(input)?;
            let upvotes = upvotes.to_vec();
            let message = M4AckBundles::OneByte { upvotes };
            return Ok((input, message));
        } else if m4_tag == Self::TWO_BYTES_TAG {
            let (input, upvotes) = many0(take(2usize))(input)?;
            let upvotes: Vec<u16> = upvotes.into_iter().map(LittleEndian::read_u16).collect();
            let message = M4AckBundles::TwoBytes { upvotes };
            return Ok((input, message));
        } else if m4_tag == Self::LEADING_BY_50_TAG {
            let message = M4AckBundles::LeadingBy50;
            return Ok((input, message));
        }
        fail(input)
    }
}

impl TryFrom<M4AckBundles> for ScriptBuf {
    type Error = bitcoin::script::PushBytesError;

    fn try_from(m4: M4AckBundles) -> Result<Self, Self::Error> {
        let tag = m4.tag();
        let upvotes = match m4 {
            M4AckBundles::OneByte { upvotes } => upvotes,
            M4AckBundles::TwoBytes { upvotes } => upvotes
                .into_iter()
                .flat_map(|upvote| upvote.to_le_bytes())
                .collect(),
            _ => vec![],
        };
        let message = [&M4AckBundles::TAG[..], &tag, &upvotes].concat();
        let data = PushBytesBuf::try_from(message)?;
        Ok(ScriptBuf::new_op_return(&data))
    }
}

fn parse_bmm_commitment(input: &[u8]) -> IResult<&[u8], BmmCommitment> {
    let (input, bmm_commitment) = take(32usize)(input)?;
    let bmm_commitment = BmmCommitment(bmm_commitment.try_into().unwrap());
    Ok((input, bmm_commitment))
}

#[derive(Debug)]
pub struct M7BmmAccept {
    pub sidechain_number: SidechainNumber,
    pub sidechain_block_hash: BmmCommitment,
}

impl M7BmmAccept {
    pub const TAG: [u8; 4] = [0xD1, 0x61, 0x73, 0x68];

    fn parse(input: &[u8]) -> IResult<&[u8], Self> {
        let (input, sidechain_number) = take(1usize)(input)?;
        let sidechain_number = sidechain_number[0];
        let (input, sidechain_block_hash) = parse_bmm_commitment(input)?;
        let message = Self {
            sidechain_number: SidechainNumber::from(sidechain_number),
            sidechain_block_hash,
        };
        Ok((input, message))
    }
}

impl TryFrom<M7BmmAccept> for ScriptBuf {
    type Error = bitcoin::script::PushBytesError;

    fn try_from(m7: M7BmmAccept) -> Result<Self, Self::Error> {
        let M7BmmAccept {
            sidechain_number,
            sidechain_block_hash,
        } = m7;
        let message = [
            &M7BmmAccept::TAG[..],
            &[sidechain_number.into()],
            &sidechain_block_hash.0[..],
        ]
        .concat();
        let data = PushBytesBuf::try_from(message)?;
        Ok(ScriptBuf::new_op_return(&data))
    }
}

#[derive(Debug)]
pub enum CoinbaseMessage {
    M1ProposeSidechain(M1ProposeSidechain),
    M2AckSidechain(M2AckSidechain),
    M3ProposeBundle(M3ProposeBundle),
    M4AckBundles(M4AckBundles),
    M7BmmAccept(M7BmmAccept),
}

impl CoinbaseMessage {
    pub fn parse(script: &Script) -> IResult<&[u8], Self> {
        use nom::Parser;
        fn instruction_failure<'a>(
            err_msg: Option<&'static str>,
            instructions: Instructions<'a>,
        ) -> nom::Err<nom::error::Error<&'a [u8]>> {
            use nom::error::ContextError as _;
            let input = instructions.as_script().as_bytes();
            let err = nom::error::Error {
                input,
                code: nom::error::ErrorKind::Fail,
            };
            let err = match err_msg {
                Some(err_msg) => nom::error::Error::add_context(input, err_msg, err),
                None => err,
            };
            nom::Err::Failure(err)
        }
        let mut instructions = script.instructions();
        let Some(Ok(Instruction::Op(OP_RETURN))) = instructions.next() else {
            return Err(instruction_failure(
                Some("expected OP_RETURN instruction"),
                instructions,
            ));
        };
        let Some(Ok(Instruction::PushBytes(data))) = instructions.next() else {
            return Err(instruction_failure(
                Some("expected PushBytes instruction"),
                instructions,
            ));
        };
        let input = data.as_bytes();
        let (input, message_tag) = alt((
            tag(M1ProposeSidechain::TAG),
            tag(M2AckSidechain::TAG),
            tag(M3ProposeBundle::TAG),
            tag(M4AckBundles::TAG),
            tag(M7BmmAccept::TAG),
        ))(input)?;
        if message_tag == M1ProposeSidechain::TAG {
            return M1ProposeSidechain::parse
                .map(Self::M1ProposeSidechain)
                .parse(input);
        } else if message_tag == M2AckSidechain::TAG {
            return M2AckSidechain::parse.map(Self::M2AckSidechain).parse(input);
        } else if message_tag == M3ProposeBundle::TAG {
            return M3ProposeBundle::parse
                .map(Self::M3ProposeBundle)
                .parse(input);
        } else if message_tag == M4AckBundles::TAG {
            return M4AckBundles::parse.map(Self::M4AckBundles).parse(input);
        } else if message_tag == M7BmmAccept::TAG {
            return M7BmmAccept::parse.map(Self::M7BmmAccept).parse(input);
        }
        fail(input)
    }
}

impl From<M1ProposeSidechain> for CoinbaseMessage {
    fn from(m1: M1ProposeSidechain) -> Self {
        Self::M1ProposeSidechain(m1)
    }
}

impl From<M2AckSidechain> for CoinbaseMessage {
    fn from(m2: M2AckSidechain) -> Self {
        Self::M2AckSidechain(m2)
    }
}

impl From<M3ProposeBundle> for CoinbaseMessage {
    fn from(m3: M3ProposeBundle) -> Self {
        Self::M3ProposeBundle(m3)
    }
}

impl From<M4AckBundles> for CoinbaseMessage {
    fn from(m4: M4AckBundles) -> Self {
        Self::M4AckBundles(m4)
    }
}

impl From<M7BmmAccept> for CoinbaseMessage {
    fn from(m7: M7BmmAccept) -> Self {
        Self::M7BmmAccept(m7)
    }
}

impl TryFrom<CoinbaseMessage> for ScriptBuf {
    type Error = bitcoin::script::PushBytesError;

    fn try_from(msg: CoinbaseMessage) -> Result<Self, Self::Error> {
        match msg {
            CoinbaseMessage::M1ProposeSidechain(m1) => m1.try_into(),
            CoinbaseMessage::M2AckSidechain(m2) => m2.try_into(),
            CoinbaseMessage::M3ProposeBundle(m3) => m3.try_into(),
            CoinbaseMessage::M4AckBundles(m4) => m4.try_into(),
            CoinbaseMessage::M7BmmAccept(m7) => m7.try_into(),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum CoinbaseMessagesError {
    #[error(
        "M1 sidechain proposal for slot `{}` with sidechain description hash `{}` already included at index `{}`",
        .slot,
        .sidechain_description_hash,
        .index
    )]
    DuplicateM1 {
        index: usize,
        slot: SidechainNumber,
        sidechain_description_hash: sha256d::Hash,
    },
    #[error("M2 that acks proposal for slot `{slot}` already included at index `{index}`")]
    DuplicateM2 { index: usize, slot: SidechainNumber },
    #[error("M4 already included at index `{index}`")]
    DuplicateM4 { index: usize },
    #[error("M7 for slot `{slot}` already included at index `{index}`")]
    DuplicateM7 { index: usize, slot: SidechainNumber },
}

impl ToStatus for CoinbaseMessagesError {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::DuplicateM1 { .. }
            | Self::DuplicateM2 { .. }
            | Self::DuplicateM4 { .. }
            | Self::DuplicateM7 { .. } => StatusBuilder::new(self),
        }
    }
}

/// Valid, ordered list of coinbase messages
#[derive(Debug, Default)]
pub struct CoinbaseMessages {
    /// Coinbase messages, with vout index
    messages: Vec<(CoinbaseMessage, usize)>,
    m1_sidechain_proposal_id_to_index: HashMap<SidechainProposalId, usize>,
    m2_ack_slot_to_index: HashMap<SidechainNumber, usize>,
    m4_index: Option<usize>,
    /// Maps M7 slots to commitment and index
    m7_slot_to_commitment_index: HashMap<SidechainNumber, (BmmCommitment, usize)>,
}

impl CoinbaseMessages {
    pub fn m1_sidechain_proposal_ids(&self) -> HashSet<SidechainProposalId> {
        self.m1_sidechain_proposal_id_to_index
            .keys()
            .copied()
            .collect()
    }

    pub fn m2_ack_slot_vout(&self, slot: &SidechainNumber) -> Option<usize> {
        self.m2_ack_slot_to_index.get(slot).copied()
    }

    pub fn m2_ack_slots(&self) -> HashSet<SidechainNumber> {
        self.m2_ack_slot_to_index.keys().copied().collect()
    }

    pub fn m4_exists(&self) -> bool {
        self.m4_index.is_some()
    }

    pub fn m7_bmm_accept_slot_vout(&self, slot: &SidechainNumber) -> Option<usize> {
        self.m7_slot_to_commitment_index
            .get(slot)
            .map(|(_, vout)| *vout)
    }

    pub fn m7_bmm_accepts(&self) -> HashMap<SidechainNumber, BmmCommitment> {
        self.m7_slot_to_commitment_index
            .iter()
            .map(|(sidechain_number, (commitment, _))| (*sidechain_number, *commitment))
            .collect()
    }

    // TODO: ensure that M3 pushes are valid
    pub fn push(&mut self, msg: CoinbaseMessage, vout: usize) -> Result<(), CoinbaseMessagesError> {
        match &msg {
            CoinbaseMessage::M1ProposeSidechain(sidechain_proposal) => {
                let sidechain_proposal_id = SidechainProposalId {
                    sidechain_number: sidechain_proposal.sidechain_number,
                    description_hash: sidechain_proposal.description.sha256d_hash(),
                };
                match self
                    .m1_sidechain_proposal_id_to_index
                    .entry(sidechain_proposal_id)
                {
                    hash_map::Entry::Occupied(entry) => Err(CoinbaseMessagesError::DuplicateM1 {
                        index: *entry.get(),
                        slot: sidechain_proposal_id.sidechain_number,
                        sidechain_description_hash: sidechain_proposal_id.description_hash,
                    }),
                    hash_map::Entry::Vacant(entry) => {
                        entry.insert(vout);
                        self.messages.push((msg, vout));
                        Ok(())
                    }
                }
            }
            CoinbaseMessage::M2AckSidechain(m2) => {
                match self.m2_ack_slot_to_index.entry(m2.sidechain_number) {
                    hash_map::Entry::Occupied(entry) => Err(CoinbaseMessagesError::DuplicateM2 {
                        index: *entry.get(),
                        slot: m2.sidechain_number,
                    }),
                    hash_map::Entry::Vacant(entry) => {
                        entry.insert(vout);
                        self.messages.push((msg, vout));
                        Ok(())
                    }
                }
            }
            CoinbaseMessage::M4AckBundles(_) => {
                if let Some(index) = self.m4_index {
                    Err(CoinbaseMessagesError::DuplicateM4 { index })
                } else {
                    self.m4_index = Some(vout);
                    self.messages.push((msg, vout));
                    Ok(())
                }
            }
            CoinbaseMessage::M7BmmAccept(bmm_accept) => {
                match self
                    .m7_slot_to_commitment_index
                    .entry(bmm_accept.sidechain_number)
                {
                    hash_map::Entry::Occupied(entry) => {
                        let (_, index) = entry.get();
                        Err(CoinbaseMessagesError::DuplicateM7 {
                            index: *index,
                            slot: bmm_accept.sidechain_number,
                        })
                    }
                    hash_map::Entry::Vacant(entry) => {
                        entry.insert((bmm_accept.sidechain_block_hash, vout));
                        self.messages.push((msg, vout));
                        Ok(())
                    }
                }
            }
            CoinbaseMessage::M3ProposeBundle(_) => {
                self.messages.push((msg, vout));
                Ok(())
            }
        }
    }

    pub fn new(coinbase_txouts: &[TxOut]) -> Result<Self, CoinbaseMessagesError> {
        let mut this = Self::default();
        for (vout, output) in coinbase_txouts.iter().enumerate() {
            let message = match CoinbaseMessage::parse(&output.script_pubkey) {
                Ok((rest, message)) => {
                    if !rest.is_empty() {
                        tracing::warn!("Extra data in coinbase script: `{}`", hex::encode(rest));
                    }
                    message
                }
                Err(err) => {
                    // Happens all the time. Would be nice to differentiate between "this isn't a BIP300 message"
                    // and "we failed real bad".
                    tracing::trace!(
                        script = hex::encode(output.script_pubkey.to_bytes()),
                        "Failed to parse coinbase script: {:#}",
                        ErrorChain::new(&err)
                    );
                    continue;
                }
            };
            this.push(message, vout)?;
        }
        Ok(this)
    }

    pub fn iter(&self) -> impl Iterator<Item = &(CoinbaseMessage, usize)> {
        self.messages.iter()
    }
}

impl IntoIterator for CoinbaseMessages {
    type IntoIter = <Vec<(CoinbaseMessage, usize)> as IntoIterator>::IntoIter;
    type Item = (CoinbaseMessage, usize);
    fn into_iter(self) -> Self::IntoIter {
        self.messages.into_iter()
    }
}

#[must_use]
pub struct CoinbaseBuilder<'a> {
    coinbase_txouts: &'a mut Vec<TxOut>,
    messages: CoinbaseMessages,
}

impl<'a> CoinbaseBuilder<'a> {
    pub fn new(coinbase_txouts: &'a mut Vec<TxOut>) -> Result<Self, CoinbaseMessagesError> {
        Ok(CoinbaseBuilder {
            messages: CoinbaseMessages::new(coinbase_txouts)?,
            coinbase_txouts,
        })
    }

    pub fn messages(&self) -> &CoinbaseMessages {
        &self.messages
    }

    /// Return the original coinbase_txouts and the extension to the
    /// coinbase txouts, without extending the original coinbase txouts.
    fn finalize(self) -> Result<(&'a mut Vec<TxOut>, Vec<TxOut>), bitcoin::script::PushBytesError> {
        let extention = self
            .messages
            .into_iter()
            .skip_while({
                let coinbase_txouts_len = self.coinbase_txouts.len();
                move |(_message, vout)| *vout < coinbase_txouts_len
            })
            .map(|(message, _)| {
                let script_pubkey = message.try_into()?;
                Ok::<_, bitcoin::script::PushBytesError>(TxOut {
                    value: Amount::from_sat(0),
                    script_pubkey,
                })
            })
            .collect::<Result<_, _>>()?;
        Ok((self.coinbase_txouts, extention))
    }

    /// Return the extension to the coinbase txouts, without extending the
    /// original coinbase txouts.
    pub fn build_extension(self) -> Result<Vec<TxOut>, bitcoin::script::PushBytesError> {
        let (_, extension) = self.finalize()?;
        Ok(extension)
    }

    /// Extend the original coinbase txouts with new messages
    pub fn build(self) -> Result<(), bitcoin::script::PushBytesError> {
        let (coinbase_txouts, extension) = self.finalize()?;
        coinbase_txouts.extend(extension);
        Ok(())
    }

    fn next_vout_index(&self) -> usize {
        if let Some((_message, vout)) = self.messages.messages.last() {
            std::cmp::max(self.coinbase_txouts.len(), vout + 1)
        } else {
            self.coinbase_txouts.len()
        }
    }

    pub fn propose_sidechain(
        &mut self,
        proposal: SidechainProposal,
    ) -> Result<&mut Self, CoinbaseMessagesError> {
        let message = CoinbaseMessage::M1ProposeSidechain(M1ProposeSidechain {
            sidechain_number: proposal.sidechain_number,
            description: proposal.description,
        });
        self.messages.push(message, self.next_vout_index())?;
        Ok(self)
    }

    pub fn ack_sidechain(
        &mut self,
        sidechain_number: SidechainNumber,
        description_hash: sha256d::Hash,
    ) -> Result<&mut Self, CoinbaseMessagesError> {
        let message = CoinbaseMessage::M2AckSidechain(M2AckSidechain {
            sidechain_number,
            description_hash,
        });
        self.messages.push(message, self.next_vout_index())?;
        Ok(self)
    }

    pub fn propose_bundle(
        &mut self,
        sidechain_number: SidechainNumber,
        m6id: M6id,
    ) -> Result<&mut Self, CoinbaseMessagesError> {
        let message = CoinbaseMessage::M3ProposeBundle(M3ProposeBundle {
            sidechain_number,
            bundle_txid: m6id.0.to_byte_array(),
        });
        self.messages.push(message, self.next_vout_index())?;
        Ok(self)
    }

    pub fn ack_bundles(
        &mut self,
        m4_ack_bundles: M4AckBundles,
    ) -> Result<&mut Self, CoinbaseMessagesError> {
        let message = CoinbaseMessage::M4AckBundles(m4_ack_bundles);
        self.messages.push(message, self.next_vout_index())?;
        Ok(self)
    }

    pub fn bmm_accept(
        &mut self,
        sidechain_number: SidechainNumber,
        sidechain_block_hash: BmmCommitment,
    ) -> Result<&mut Self, CoinbaseMessagesError> {
        let message = CoinbaseMessage::M7BmmAccept(M7BmmAccept {
            sidechain_number,
            sidechain_block_hash,
        });
        self.messages.push(message, self.next_vout_index())?;
        Ok(self)
    }
}

#[derive(Debug)]
pub struct M8BmmRequest {
    pub sidechain_number: SidechainNumber,
    // Also called H* or critical hash, critical data hash, hash critical
    pub sidechain_block_hash: BmmCommitment,
    pub prev_mainchain_block_hash: BlockHash,
}

impl M8BmmRequest {
    pub const TAG: [u8; 3] = [0x00, 0xBF, 0x00];

    pub fn parse(input: &[u8]) -> IResult<&[u8], Self> {
        const HEADER_LENGTH: u8 = 3;
        const SIDECHAIN_NUMBER_LENGTH: u8 = 1;
        const SIDECHAIN_BLOCK_HASH_LENGTH: u8 = 32;
        const PREV_MAINCHAIN_BLOCK_HASH_LENGTH: u8 = 32;

        const M8_BMM_REQUEST_LENGTH: u8 = HEADER_LENGTH
            + SIDECHAIN_NUMBER_LENGTH
            + SIDECHAIN_BLOCK_HASH_LENGTH
            + PREV_MAINCHAIN_BLOCK_HASH_LENGTH;

        let (input, _) = tag(&[OP_RETURN.to_u8(), M8_BMM_REQUEST_LENGTH])(input)?;
        let (input, _) = tag(Self::TAG)(input)?;
        let (input, sidechain_number) = take(1usize)(input)?;
        let sidechain_number = sidechain_number[0];
        let (input, sidechain_block_hash) = parse_bmm_commitment(input)?;
        let (input, prev_mainchain_block_hash) = take(32usize)(input)?;
        let prev_mainchain_block_hash: [u8; 32] = prev_mainchain_block_hash.try_into().unwrap();
        let prev_mainchain_block_hash = BlockHash::from_byte_array(prev_mainchain_block_hash);
        let message = Self {
            sidechain_number: sidechain_number.into(),
            sidechain_block_hash,
            prev_mainchain_block_hash,
        };
        Ok((input, message))
    }
}

pub(crate) fn parse_m8_tx(transaction: &Transaction) -> Option<M8BmmRequest> {
    let output = transaction.output.first()?;
    let script = output.script_pubkey.to_bytes();
    if let Ok((_input, bmm_request)) = M8BmmRequest::parse(&script) {
        Some(bmm_request)
    } else {
        None
    }
}

pub fn parse_op_drivechain(input: &[u8]) -> IResult<&[u8], SidechainNumber> {
    let (input, _op_drivechain_tag) = tag(&[OP_DRIVECHAIN.to_u8(), OP_PUSHBYTES_1.to_u8()])(input)?;
    let (input, sidechain_number) = take(1usize)(input)?;
    let sidechain_number = sidechain_number[0];
    tag(&[OP_TRUE.to_u8()])(input)?;
    Ok((input, SidechainNumber::from(sidechain_number)))
}

pub fn try_parse_op_return_address(script: &Script) -> Option<Vec<u8>> {
    let mut instructions = script.instructions();
    let Some(Ok(Instruction::Op(OP_RETURN))) = instructions.next() else {
        return None;
    };
    let Some(Ok(Instruction::PushBytes(address))) = instructions.next() else {
        return None;
    };
    let None = instructions.next() else {
        return None;
    };
    Some(address.as_bytes().to_owned())
}

pub fn create_m5_deposit_output(
    sidechain_number: SidechainNumber,
    old_ctip_amount: Amount,
    deposit_amount: Amount,
) -> TxOut {
    let script_pubkey = ScriptBuf::from_bytes(vec![
        OP_DRIVECHAIN.to_u8(),
        OP_PUSHBYTES_1.to_u8(),
        sidechain_number.into(),
        OP_TRUE.to_u8(),
    ]);
    TxOut {
        script_pubkey,
        // All deposits INCREASE the amount locked in the OP_DRIVECHAIN output.
        value: old_ctip_amount + deposit_amount,
    }
}

pub fn create_op_return_output<Msg>(
    msg: Msg,
) -> Result<TxOut, <PushBytesBuf as TryFrom<Msg>>::Error>
where
    PushBytesBuf: TryFrom<Msg>,
{
    Ok(TxOut {
        script_pubkey: ScriptBuf::new_op_return(PushBytesBuf::try_from(msg)?),
        value: Amount::ZERO,
    })
}

#[derive(Debug, Error)]
enum M6idErrorInner {
    #[error(
        "Insufficient previous treasury utxo balance to spend {} sats: {} sats",
        spend_sats,
        treasury_sats
    )]
    InsufficientTreasury { spend_sats: u64, treasury_sats: u64 },
    #[error("Invalid spk for treasury output: {}", script_pubkey)]
    InvalidSpk {
        script_pubkey: ScriptBuf,
        source: nom::Err<nom::error::Error<Vec<u8>>>,
    },
    #[error("More than 1 input: {n_inputs}")]
    ManyInputs { n_inputs: usize },
    #[error("Missing treasury input")]
    MissingTreasuryInput,
    #[error("Missing treasury output")]
    MissingTreasuryOutput,
    #[error("Total output amount overflow")]
    TotalOutputAmountOverflow,
    #[error("Withdrawal amount overflow")]
    WithdrawalAmountOverflow,
}

#[derive(Debug, Error)]
#[error("M6id error")]
pub struct M6idError(#[from] M6idErrorInner);

pub fn compute_m6id(
    mut tx: Transaction,
    previous_treasury_utxo_total: Amount,
) -> Result<(M6id, SidechainNumber), M6idError> {
    // Check that a new treasury UTXO is created at index 0
    let Some((first_output, payout_outputs)) = tx.output.split_first_mut() else {
        return Err(M6idErrorInner::MissingTreasuryOutput.into());
    };
    let (_, sidechain_number) = parse_op_drivechain(first_output.script_pubkey.as_bytes())
        .map_err(|err| M6idErrorInner::InvalidSpk {
            script_pubkey: first_output.script_pubkey.clone(),
            source: err.to_owned(),
        })?;
    // Set `T_n` equal to the `nValue` of the treasury UTXO created in this `M6`.
    let t_n = first_output.value;
    // Remove the single input spending the previous treasury UTXO from the `vin`
    // vector, so that the `vin` vector is empty.
    match tx.input.len() {
        0 => return Err(M6idErrorInner::MissingTreasuryInput.into()),
        1 => (),
        n_inputs => return Err(M6idErrorInner::ManyInputs { n_inputs }.into()),
    }
    tx.input.clear();
    // Compute `P_total` by summing the `nValue`s of all pay out outputs in this
    // `M6`, so `P_total` = sum of `nValue`s of all outputs of this `M6` except for
    // the new treasury UTXO at index 0.
    let p_total: Amount = payout_outputs
        .iter()
        .map(|o| o.value)
        .checked_sum()
        .ok_or(M6idErrorInner::WithdrawalAmountOverflow)?;
    // Compute `F_total = T_n-1 - T_n - P_total`, since we know that `T_n = T_n-1 -
    // P_total - F_total`, `T_n-1` was passed as an argument, and `T_n` and
    // `P_total` were computed in previous steps..
    let t_n_minus_1 = previous_treasury_utxo_total;
    let total_output_amount = t_n
        .checked_add(p_total)
        .ok_or(M6idErrorInner::TotalOutputAmountOverflow)?;
    let f_total = t_n_minus_1
        .checked_sub(total_output_amount)
        .ok_or_else(|| M6idErrorInner::InsufficientTreasury {
            spend_sats: total_output_amount.to_sat(),
            treasury_sats: t_n_minus_1.to_sat(),
        })?;
    // Encode `F_total` as `F_total_be_bytes`, an array of 8 bytes encoding the 64
    // bit unsigned integer in big endian order.
    let f_total_be_bytes: [u8; 8] = f_total.to_sat().to_be_bytes();
    // Replace the treasury output with the fee output
    let fee_output = TxOut {
        script_pubkey: ScriptBuf::new_op_return(f_total_be_bytes),
        value: Amount::ZERO,
    };
    *first_output = fee_output;
    // At this point we have constructed `M6_blinded`
    Ok((M6id(tx.compute_txid()), sidechain_number))
}

// Move all non-consensus components out of Bitcoin Core.
// Over time *delete* all of that code.
// Simplify everything.
//
// AKM not H&K G11.

// ... existing code ...

// Returns the serialized sidechain proposal OP_RETURN output, plus the byte
// slice encoded into the OP_RETURN output.
pub fn create_sidechain_proposal(
    sidechain_number: SidechainNumber,
    declaration: &SidechainDeclaration,
) -> Result<(bitcoin::TxOut, SidechainDescription), bitcoin::script::PushBytesError> {
    // The only defined M1 version.
    const VERSION: u8 = 0;

    let mut description = Vec::<u8>::new();
    description.push(VERSION);
    description.push(declaration.title.len() as u8);
    description.extend_from_slice(declaration.title.as_bytes());
    description.extend_from_slice(declaration.description.as_bytes());
    description.extend_from_slice(&declaration.hash_id_1);
    description.extend_from_slice(&declaration.hash_id_2);
    let description: SidechainDescription = description.into();

    let tx_out = TxOut {
        value: Amount::ZERO,
        script_pubkey: M1ProposeSidechain {
            sidechain_number,
            description: description.clone(),
        }
        .try_into()?,
    };
    Ok((tx_out, description))
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::types::SidechainProposal;

    #[test]
    fn test_parse_m8_bmm_request() {
        // This data was given to /cusf.mainchain.v1.WalletService/CreateBmmCriticalDataTransaction
        let sidechain_number = SidechainNumber(1);
        let critical_bytes =
            hex::decode("1200007c0ca1efd8d128fedf50c73f395b0cceb0ffa823edbd971b4afd913021")
                .unwrap();

        let mut prev_mainchain_block_hash =
            hex::decode("000000e597b4ff2b7b81bcb2b0199b65471b9ece52b9688f18a8a7ed8e275eb1")
                .unwrap();

        // Reverse, because this is a ReverseHex in the API
        prev_mainchain_block_hash.reverse();

        // And the endpoint created this TX
        // https://mempool.drivechain.live/tx/dd465c00860631c3d120d5473a2cf1adb2ddc58e79785aba054ee1ada9bd06c8
        // Raw hex of the OP_RETURN output
        const INPUT: &str = "6a4400bf00011200007c0ca1efd8d128fedf50c73f395b0cceb0ffa823edbd971b4afd913021b15e278eeda7a8188f68b952ce9e1b47659b19b0b2bc817b2bffb497e5000000";

        let input = hex::decode(INPUT).unwrap();

        let (remaining, result) = M8BmmRequest::parse(&input).unwrap();

        assert!(remaining.is_empty());
        assert_eq!(result.sidechain_number, sidechain_number);
        assert_eq!(result.sidechain_block_hash.0.as_slice(), critical_bytes);
        assert_eq!(
            result.prev_mainchain_block_hash.as_byte_array(),
            prev_mainchain_block_hash.as_slice()
        );
    }

    #[test]
    fn test_roundtrip() {
        let declaration = SidechainDeclaration {
            title: "title".to_owned(),
            description: "description".to_owned(),
            hash_id_1: [1u8; 32],
            hash_id_2: [2u8; 20],
        };
        let (tx_out, _) = create_sidechain_proposal(SidechainNumber::from(13), &declaration)
            .expect("Failed to create sidechain proposal");

        let (rest, message) = CoinbaseMessage::parse(&tx_out.script_pubkey)
            .expect("Failed to parse sidechain proposal");

        assert!(rest.is_empty());

        let CoinbaseMessage::M1ProposeSidechain(M1ProposeSidechain {
            sidechain_number,
            description,
        }) = message
        else {
            panic!("Failed to parse sidechain proposal");
        };

        assert_eq!(sidechain_number, 13.into());

        let proposal = SidechainProposal {
            sidechain_number,
            description,
        };

        let parsed: SidechainDeclaration =
            (&proposal.description).try_into().expect("Failed to parse");

        assert_eq!(parsed, declaration);
    }
}
