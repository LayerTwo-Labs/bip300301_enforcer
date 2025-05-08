use std::collections::{hash_map, HashMap};

use bitcoin::{
    amount::CheckedSum,
    hashes::{sha256d, Hash},
    opcodes::{
        all::{OP_PUSHBYTES_1, OP_RETURN},
        OP_TRUE,
    },
    script::{Instruction, Instructions, PushBytesBuf},
    Amount, Script, ScriptBuf, Transaction, TxOut,
};
use byteorder::{ByteOrder, LittleEndian};
use miette::Diagnostic;
use nom::{
    branch::alt,
    bytes::complete::{tag, take},
    combinator::{fail, rest},
    multi::many0,
    IResult,
};
use thiserror::Error;

use crate::{
    proto::{StatusBuilder, ToStatus},
    types::{
        M6id, SidechainDeclaration, SidechainDescription, SidechainNumber, SidechainProposal,
        OP_DRIVECHAIN,
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

#[derive(Debug)]
pub struct M7BmmAccept {
    pub sidechain_number: SidechainNumber,
    pub sidechain_block_hash: [u8; 32],
}

impl M7BmmAccept {
    pub const TAG: [u8; 4] = [0xD1, 0x61, 0x73, 0x68];

    fn parse(input: &[u8]) -> IResult<&[u8], Self> {
        let (input, sidechain_number) = take(1usize)(input)?;
        let sidechain_number = sidechain_number[0];
        let (input, sidechain_block_hash) = take(32usize)(input)?;
        // Unwrap here is fine, because if we didn't get exactly 32 bytes we'd fail on the previous
        // line.
        let sidechain_block_hash = sidechain_block_hash.try_into().unwrap();
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
            &sidechain_block_hash,
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
    #[error("M2 that acks proposal for slot `{slot}` already included at index `{index}`")]
    DuplicateM2 { index: usize, slot: SidechainNumber },
    #[error("M4 already included at index `{index}`")]
    DuplicateM4 { index: usize },
}

impl ToStatus for CoinbaseMessagesError {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::DuplicateM2 { .. } | Self::DuplicateM4 { .. } => StatusBuilder::new(self),
        }
    }
}

/// Valid, ordered list of coinbase messages
#[derive(Debug, Default)]
pub struct CoinbaseMessages {
    messages: Vec<CoinbaseMessage>,
    m2_ack_slot_to_index: HashMap<SidechainNumber, usize>,
    m4_index: Option<usize>,
}

impl CoinbaseMessages {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn m2_acks(&self) -> Vec<SidechainNumber> {
        let mut res: Vec<_> = self.m2_ack_slot_to_index.keys().copied().collect();
        res.sort();
        res
    }

    pub fn m4_exists(&self) -> bool {
        self.m4_index.is_some()
    }

    // TODO: ensure that M1, M3, and M7 pushes are valid
    pub fn push(&mut self, msg: CoinbaseMessage) -> Result<(), CoinbaseMessagesError> {
        match &msg {
            CoinbaseMessage::M2AckSidechain(m2) => {
                match self.m2_ack_slot_to_index.entry(m2.sidechain_number) {
                    hash_map::Entry::Occupied(entry) => Err(CoinbaseMessagesError::DuplicateM2 {
                        index: *entry.get(),
                        slot: m2.sidechain_number,
                    }),
                    hash_map::Entry::Vacant(entry) => {
                        entry.insert(self.messages.len());
                        self.messages.push(msg);
                        Ok(())
                    }
                }
            }
            CoinbaseMessage::M4AckBundles(_) => {
                if let Some(index) = self.m4_index {
                    Err(CoinbaseMessagesError::DuplicateM4 { index })
                } else {
                    self.m4_index = Some(self.messages.len());
                    self.messages.push(msg);
                    Ok(())
                }
            }
            CoinbaseMessage::M1ProposeSidechain(_)
            | CoinbaseMessage::M3ProposeBundle(_)
            | CoinbaseMessage::M7BmmAccept(_) => {
                self.messages.push(msg);
                Ok(())
            }
        }
    }

    pub fn extend<I>(&mut self, iter: I) -> Result<(), CoinbaseMessagesError>
    where
        I: IntoIterator<Item = CoinbaseMessage>,
    {
        iter.into_iter().try_for_each(|msg| self.push(msg))
    }
}

impl<'a> IntoIterator for &'a CoinbaseMessages {
    type IntoIter = <&'a Vec<CoinbaseMessage> as IntoIterator>::IntoIter;
    type Item = &'a CoinbaseMessage;
    fn into_iter(self) -> Self::IntoIter {
        self.messages.iter()
    }
}

impl IntoIterator for CoinbaseMessages {
    type IntoIter = <Vec<CoinbaseMessage> as IntoIterator>::IntoIter;
    type Item = CoinbaseMessage;
    fn into_iter(self) -> Self::IntoIter {
        self.messages.into_iter()
    }
}

pub struct CoinbaseBuilder {
    messages: CoinbaseMessages,
}

impl CoinbaseBuilder {
    pub fn new() -> Self {
        CoinbaseBuilder {
            messages: CoinbaseMessages::new(),
        }
    }

    pub fn messages(&self) -> &CoinbaseMessages {
        &self.messages
    }

    pub fn build(self) -> Result<Vec<TxOut>, bitcoin::script::PushBytesError> {
        self.messages
            .into_iter()
            .map(|message| {
                let script_pubkey = message.try_into()?;
                Ok(TxOut {
                    value: Amount::from_sat(0),
                    script_pubkey,
                })
            })
            .collect()
    }

    pub fn propose_sidechain(
        &mut self,
        proposal: SidechainProposal,
    ) -> Result<&mut Self, CoinbaseMessagesError> {
        let message = CoinbaseMessage::M1ProposeSidechain(M1ProposeSidechain {
            sidechain_number: proposal.sidechain_number,
            description: proposal.description,
        });
        self.messages.push(message)?;
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
        self.messages.push(message)?;
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
        self.messages.push(message)?;
        Ok(self)
    }

    pub fn ack_bundles(
        &mut self,
        m4_ack_bundles: M4AckBundles,
    ) -> Result<&mut Self, CoinbaseMessagesError> {
        let message = CoinbaseMessage::M4AckBundles(m4_ack_bundles);
        self.messages.push(message)?;
        Ok(self)
    }

    pub fn bmm_accept(
        &mut self,
        sidechain_number: SidechainNumber,
        bmm_hash: &[u8; 32],
    ) -> Result<&mut Self, CoinbaseMessagesError> {
        let message = CoinbaseMessage::M7BmmAccept(M7BmmAccept {
            sidechain_number,
            sidechain_block_hash: *bmm_hash,
        });
        self.messages.push(message)?;
        Ok(self)
    }
}

impl Default for CoinbaseBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct M8BmmRequest {
    pub sidechain_number: SidechainNumber,
    // Also called H* or critical hash, critical data hash, hash critical
    pub sidechain_block_hash: [u8; 32],
    pub prev_mainchain_block_hash: [u8; 32],
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
        let (input, sidechain_block_hash) = take(32usize)(input)?;
        let (input, prev_mainchain_block_hash) = take(32usize)(input)?;
        let sidechain_block_hash = sidechain_block_hash.try_into().unwrap();
        let prev_mainchain_block_hash = prev_mainchain_block_hash.try_into().unwrap();
        let message = Self {
            sidechain_number: sidechain_number.into(),
            sidechain_block_hash,
            prev_mainchain_block_hash,
        };
        Ok((input, message))
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

    let proposal = SidechainProposal {
        sidechain_number,
        description: description.clone(),
    };

    let mut builder = CoinbaseBuilder::new();
    builder.propose_sidechain(proposal).unwrap();
    let txouts = builder.build()?; // TODO: better error

    assert!(txouts.len() == 1);
    let tx_out = txouts.first().unwrap().clone();
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
        assert_eq!(result.sidechain_block_hash.to_vec(), critical_bytes);
        assert_eq!(
            result.prev_mainchain_block_hash.to_vec(),
            prev_mainchain_block_hash
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
