use bitcoin::{
    hashes::Hash,
    opcodes::{
        all::{OP_NOP5, OP_PUSHBYTES_1, OP_RETURN},
        OP_TRUE,
    },
    script::{Instruction, Instructions, PushBytesBuf},
    Amount, Opcode, Script, ScriptBuf, Transaction, TxOut,
};
use byteorder::{ByteOrder, LittleEndian};
use nom::{
    branch::alt,
    bytes::complete::{tag, take},
    combinator::{fail, rest},
    multi::many0,
    IResult,
};

use crate::types::SidechainNumber;

pub const OP_DRIVECHAIN: Opcode = OP_NOP5;

pub struct CoinbaseBuilder {
    messages: Vec<CoinbaseMessage>,
}

impl CoinbaseBuilder {
    pub fn new() -> Self {
        CoinbaseBuilder { messages: vec![] }
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

    pub fn propose_sidechain(mut self, sidechain_number: u8, data: &[u8]) -> Self {
        let message = CoinbaseMessage::M1ProposeSidechain {
            sidechain_number,
            data: data.to_vec(),
        };
        self.messages.push(message);
        self
    }

    pub fn ack_sidechain(mut self, sidechain_number: u8, data_hash: &[u8; 32]) -> Self {
        let message = CoinbaseMessage::M2AckSidechain {
            sidechain_number,
            data_hash: *data_hash,
        };
        self.messages.push(message);
        self
    }

    pub fn propose_bundle(mut self, sidechain_number: u8, bundle_hash: &[u8; 32]) -> Self {
        let message = CoinbaseMessage::M3ProposeBundle {
            sidechain_number,
            bundle_txid: *bundle_hash,
        };
        self.messages.push(message);
        self
    }

    pub fn ack_bundles(mut self, m4_ack_bundles: M4AckBundles) -> Self {
        let message = CoinbaseMessage::M4AckBundles(m4_ack_bundles);
        self.messages.push(message);
        self
    }

    pub fn bmm_accept(mut self, sidechain_number: u8, bmm_hash: &[u8; 32]) -> Self {
        let message = CoinbaseMessage::M7BmmAccept {
            sidechain_number,
            sidechain_block_hash: *bmm_hash,
        };
        self.messages.push(message);
        self
    }
}

#[derive(Debug)]
pub enum CoinbaseMessage {
    M1ProposeSidechain {
        sidechain_number: u8,
        data: Vec<u8>,
    },
    M2AckSidechain {
        sidechain_number: u8,
        data_hash: [u8; 32],
    },
    M3ProposeBundle {
        sidechain_number: u8,
        bundle_txid: [u8; 32],
    },
    M4AckBundles(M4AckBundles),
    M7BmmAccept {
        sidechain_number: u8,
        sidechain_block_hash: [u8; 32],
    },
}

#[derive(Debug)]
pub struct M8BmmRequest {
    pub sidechain_number: SidechainNumber,
    // Also called H* or critical hash, critical data hash, hash critical
    pub sidechain_block_hash: [u8; 32],
    pub prev_mainchain_block_hash: [u8; 32],
}

pub const M1_PROPOSE_SIDECHAIN_TAG: &[u8] = &[0xD5, 0xE0, 0xC4, 0xAF];
pub const M2_ACK_SIDECHAIN_TAG: &[u8] = &[0xD6, 0xE1, 0xC5, 0xDF];
pub const M3_PROPOSE_BUNDLE_TAG: &[u8] = &[0xD4, 0x5A, 0xA9, 0x43];
pub const M4_ACK_BUNDLES_TAG: &[u8] = &[0xD7, 0x7D, 0x17, 0x76];
pub const M7_BMM_ACCEPT_TAG: &[u8] = &[0xD1, 0x61, 0x73, 0x68];
pub const M8_BMM_REQUEST_TAG: &[u8] = &[0x00, 0xBF, 0x00];

pub const ABSTAIN_ONE_BYTE: u8 = 0xFF;
pub const ABSTAIN_TWO_BYTES: u16 = 0xFFFF;

pub const ALARM_ONE_BYTE: u8 = 0xFE;
pub const ALARM_TWO_BYTES: u16 = 0xFFFE;

#[derive(Debug)]
pub enum M4AckBundles {
    RepeatPrevious,
    OneByte { upvotes: Vec<u8> },
    TwoBytes { upvotes: Vec<u16> },
    LeadingBy50,
}

const REPEAT_PREVIOUS_TAG: &[u8] = &[0x00];
const ONE_BYTE_TAG: &[u8] = &[0x01];
const TWO_BYTES_TAG: &[u8] = &[0x02];
const LEADING_BY_50_TAG: &[u8] = &[0x03];

/// 0xFF
// 0xFFFF
// const ABSTAIN_TAG: &[u8] = &[0xFF];

/// 0xFE
// 0xFFFE
// const ALARM_TAG: &[u8] = &[0xFE];

impl M4AckBundles {
    fn tag(&self) -> u8 {
        match self {
            Self::RepeatPrevious => REPEAT_PREVIOUS_TAG[0],
            Self::OneByte { .. } => ONE_BYTE_TAG[0],
            Self::TwoBytes { .. } => TWO_BYTES_TAG[0],
            Self::LeadingBy50 { .. } => LEADING_BY_50_TAG[0],
        }
    }
}

pub fn parse_coinbase_script(script: &Script) -> IResult<&[u8], CoinbaseMessage> {
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
        tag(M1_PROPOSE_SIDECHAIN_TAG),
        tag(M2_ACK_SIDECHAIN_TAG),
        tag(M3_PROPOSE_BUNDLE_TAG),
        tag(M4_ACK_BUNDLES_TAG),
    ))(input)?;
    if message_tag == M1_PROPOSE_SIDECHAIN_TAG {
        return parse_m1_propose_sidechain(input);
    } else if message_tag == M2_ACK_SIDECHAIN_TAG {
        return parse_m2_ack_sidechain(input);
    } else if message_tag == M3_PROPOSE_BUNDLE_TAG {
        return parse_m3_propose_bundle(input);
    } else if message_tag == M4_ACK_BUNDLES_TAG {
        return parse_m4_ack_bundles(input);
    } else if message_tag == M7_BMM_ACCEPT_TAG {
        return parse_m7_bmm_accept(input);
    }
    fail(input)
}

pub fn parse_op_drivechain(input: &[u8]) -> IResult<&[u8], u8> {
    let (input, _op_drivechain_tag) = tag(&[OP_DRIVECHAIN.to_u8(), OP_PUSHBYTES_1.to_u8()])(input)?;
    let (input, sidechain_number) = take(1usize)(input)?;
    let sidechain_number = sidechain_number[0];
    tag(&[OP_TRUE.to_u8()])(input)?;
    Ok((input, sidechain_number))
}

fn parse_m1_propose_sidechain(input: &[u8]) -> IResult<&[u8], CoinbaseMessage> {
    let (input, sidechain_number) = take(1usize)(input)?;
    let sidechain_number = sidechain_number[0];
    let (input, data) = rest(input)?;
    let data = data.to_vec();
    let message = CoinbaseMessage::M1ProposeSidechain {
        sidechain_number,
        data,
    };
    Ok((input, message))
}

fn parse_m2_ack_sidechain(input: &[u8]) -> IResult<&[u8], CoinbaseMessage> {
    let (input, sidechain_number) = take(1usize)(input)?;
    let sidechain_number = sidechain_number[0];
    let (input, data_hash) = take(32usize)(input)?;
    let data_hash: [u8; 32] = data_hash.try_into().unwrap();
    let message = CoinbaseMessage::M2AckSidechain {
        sidechain_number,
        data_hash,
    };
    Ok((input, message))
}

fn parse_m3_propose_bundle(input: &[u8]) -> IResult<&[u8], CoinbaseMessage> {
    let (input, sidechain_number) = take(1usize)(input)?;
    let sidechain_number = sidechain_number[0];
    let (input, bundle_txid) = take(32usize)(input)?;
    let bundle_txid: [u8; 32] = bundle_txid.try_into().unwrap();
    let message = CoinbaseMessage::M3ProposeBundle {
        sidechain_number,
        bundle_txid,
    };
    Ok((input, message))
}

fn parse_m4_ack_bundles(input: &[u8]) -> IResult<&[u8], CoinbaseMessage> {
    let (input, m4_tag) = alt((
        tag(REPEAT_PREVIOUS_TAG),
        tag(ONE_BYTE_TAG),
        tag(TWO_BYTES_TAG),
        tag(LEADING_BY_50_TAG),
    ))(input)?;

    if m4_tag == REPEAT_PREVIOUS_TAG {
        let message = CoinbaseMessage::M4AckBundles(M4AckBundles::RepeatPrevious);
        return Ok((input, message));
    } else if m4_tag == ONE_BYTE_TAG {
        let (input, upvotes) = rest(input)?;
        let upvotes = upvotes.to_vec();
        let message = CoinbaseMessage::M4AckBundles(M4AckBundles::OneByte { upvotes });
        return Ok((input, message));
    } else if m4_tag == TWO_BYTES_TAG {
        let (input, upvotes) = many0(take(2usize))(input)?;
        let upvotes: Vec<u16> = upvotes.into_iter().map(LittleEndian::read_u16).collect();
        let message = CoinbaseMessage::M4AckBundles(M4AckBundles::TwoBytes { upvotes });
        return Ok((input, message));
    } else if m4_tag == LEADING_BY_50_TAG {
        let message = CoinbaseMessage::M4AckBundles(M4AckBundles::LeadingBy50);
        return Ok((input, message));
    }
    fail(input)
}

fn parse_m7_bmm_accept(input: &[u8]) -> IResult<&[u8], CoinbaseMessage> {
    let (input, sidechain_number) = take(1usize)(input)?;
    let sidechain_number = sidechain_number[0];
    let (input, sidechain_block_hash) = take(32usize)(input)?;
    // Unwrap here is fine, because if we didn't get exactly 32 bytes we'd fail on the previous
    // line.
    let sidechain_block_hash = sidechain_block_hash.try_into().unwrap();
    let message = CoinbaseMessage::M7BmmAccept {
        sidechain_number,
        sidechain_block_hash,
    };
    Ok((input, message))
}

pub fn parse_m8_bmm_request(input: &[u8]) -> IResult<&[u8], M8BmmRequest> {
    const HEADER_LENGTH: u8 = 3;
    const SIDECHAIN_NUMBER_LENGTH: u8 = 1;
    const SIDECHAIN_BLOCK_HASH_LENGTH: u8 = 32;
    const PREV_MAINCHAIN_BLOCK_HASH_LENGTH: u8 = 32;

    const M8_BMM_REQUEST_LENGTH: u8 = HEADER_LENGTH
        + SIDECHAIN_NUMBER_LENGTH
        + SIDECHAIN_BLOCK_HASH_LENGTH
        + PREV_MAINCHAIN_BLOCK_HASH_LENGTH;

    let (input, _) = tag(&[OP_RETURN.to_u8(), M8_BMM_REQUEST_LENGTH])(input)?;
    let (input, _) = tag(M8_BMM_REQUEST_TAG)(input)?;
    let (input, sidechain_number) = take(1usize)(input)?;
    let sidechain_number = sidechain_number[0];
    let (input, sidechain_block_hash) = take(32usize)(input)?;
    let (input, prev_mainchain_block_hash) = take(32usize)(input)?;
    let sidechain_block_hash = sidechain_block_hash.try_into().unwrap();
    let prev_mainchain_block_hash = prev_mainchain_block_hash.try_into().unwrap();
    let message = M8BmmRequest {
        sidechain_number: sidechain_number.into(),
        sidechain_block_hash,
        prev_mainchain_block_hash,
    };
    Ok((input, message))
}

impl TryFrom<CoinbaseMessage> for ScriptBuf {
    type Error = bitcoin::script::PushBytesError;

    fn try_from(val: CoinbaseMessage) -> Result<Self, Self::Error> {
        match val {
            CoinbaseMessage::M1ProposeSidechain {
                sidechain_number,
                data,
            } => {
                let message = [M1_PROPOSE_SIDECHAIN_TAG, &[sidechain_number], &data].concat();

                let data = PushBytesBuf::try_from(message)?;
                Ok(ScriptBuf::new_op_return(&data))
            }
            CoinbaseMessage::M2AckSidechain {
                sidechain_number,
                data_hash,
            } => {
                let message = [M2_ACK_SIDECHAIN_TAG, &[sidechain_number], &data_hash].concat();

                let data = PushBytesBuf::try_from(message)?;
                Ok(ScriptBuf::new_op_return(&data))
            }
            CoinbaseMessage::M3ProposeBundle {
                sidechain_number,
                bundle_txid,
            } => {
                let message = [M3_PROPOSE_BUNDLE_TAG, &[sidechain_number], &bundle_txid].concat();

                let data = PushBytesBuf::try_from(message)?;
                Ok(ScriptBuf::new_op_return(&data))
            }
            CoinbaseMessage::M4AckBundles(m4_ack_bundles) => {
                let upvotes = match &m4_ack_bundles {
                    M4AckBundles::OneByte { upvotes } => upvotes.clone(),
                    M4AckBundles::TwoBytes { upvotes } => upvotes
                        .iter()
                        .flat_map(|upvote| upvote.to_le_bytes())
                        .collect(),
                    _ => vec![],
                };
                let message = [M4_ACK_BUNDLES_TAG, &[m4_ack_bundles.tag()], &upvotes].concat();

                let data = PushBytesBuf::try_from(message)?;
                Ok(ScriptBuf::new_op_return(&data))
            }
            CoinbaseMessage::M7BmmAccept {
                sidechain_number,
                sidechain_block_hash,
            } => {
                let message = [
                    M7_BMM_ACCEPT_TAG,
                    &[sidechain_number],
                    &sidechain_block_hash,
                ]
                .concat();

                let data = PushBytesBuf::try_from(message)?;
                Ok(ScriptBuf::new_op_return(&data))
            }
        }
    }
}

pub fn sha256d(data: &[u8]) -> [u8; 32] {
    bitcoin::hashes::sha256d::Hash::hash(data).to_byte_array()
}

pub fn m6_to_id(m6: &Transaction, previous_treasury_utxo_total: u64) -> [u8; 32] {
    let mut m6 = m6.clone();
    /*
    1. Remove the single input spending the previous treasury UTXO from the `vin`
       vector, so that the `vin` vector is empty.
            */
    m6.input.clear();
    /*
    2. Compute `P_total` by summing the `nValue`s of all pay out outputs in this
       `M6`, so `P_total` = sum of `nValue`s of all outputs of this `M6` except for
       the new treasury UTXO at index 0.
            */
    let p_total: Amount = m6.output[1..].iter().map(|o| o.value).sum();
    /*
    3. Set `T_n` equal to the `nValue` of the treasury UTXO created in this `M6`.
        */
    let t_n = m6.output[0].value.to_sat();
    /*
    4. Compute `F_total = T_n-1 - T_n - P_total`, since we know that `T_n = T_n-1 -
       P_total - F_total`, `T_n-1` was passed as an argument, and `T_n` and
       `P_total` were computed in previous steps..
        */
    let t_n_minus_1 = previous_treasury_utxo_total;
    let f_total = t_n_minus_1 - t_n - p_total.to_sat();
    /*
    5. Encode `F_total` as `F_total_be_bytes`, an array of 8 bytes encoding the 64
       bit unsigned integer in big endian order.
        */
    let f_total_be_bytes = f_total.to_be_bytes();
    /*
    6. Push an output to the end of `vout` of this `M6` with the `nValue = 0` and
       `scriptPubKey = OP_RETURN F_total_be_bytes`.
        */
    let script_bytes = [vec![OP_RETURN.to_u8()], f_total_be_bytes.to_vec()].concat();
    let script_pubkey = ScriptBuf::from_bytes(script_bytes);
    let txout = TxOut {
        script_pubkey,
        value: Amount::ZERO,
    };
    m6.output.push(txout);
    /*
    At this point we have constructed `M6_blinded`.
        */
    let m6_blinded = m6;
    m6_blinded.compute_txid().as_raw_hash().to_byte_array()
}

// Move all non-consensus components out of Bitcoin Core.
// Over time *delete* all of that code.
// Simplify everything.
//
// AKM not H&K G11.

// ... existing code ...

#[cfg(test)]
mod tests {
    use super::*;

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

        let (remaining, result) = parse_m8_bmm_request(&input).unwrap();

        assert!(remaining.is_empty());
        assert_eq!(result.sidechain_number, sidechain_number);
        assert_eq!(result.sidechain_block_hash.to_vec(), critical_bytes);
        assert_eq!(
            result.prev_mainchain_block_hash.to_vec(),
            prev_mainchain_block_hash
        );
    }
}
