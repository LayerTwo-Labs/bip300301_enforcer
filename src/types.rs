use bitcoin::hashes::Hash;
use bitcoin::{OutPoint, Txid};
use byteorder::{BigEndian, ByteOrder};
use redb::{RedbValue, TypeName};
use serde::{Deserialize, Serialize};
use std::mem::size_of;

pub type Hash256 = [u8; 32];

#[derive(Debug)]
pub struct Ctip {
    pub outpoint: OutPoint,
    pub value: u64,
}

impl RedbValue for Ctip {
    type SelfType<'a> = Ctip;
    type AsBytes<'a> = [u8; size_of::<Ctip>()];

    fn type_name() -> TypeName {
        TypeName::new("Ctip")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        [
            value.outpoint.txid.to_byte_array().to_vec(),
            value.outpoint.vout.to_be_bytes().to_vec(),
            value.value.to_be_bytes().to_vec(),
        ]
        .concat()
        .try_into()
        .unwrap()
    }

    fn fixed_width() -> Option<usize> {
        Some(size_of::<Ctip>())
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let txid = Txid::from_slice(&data[0..32]).unwrap();
        let data = &data[32..];
        let vout = BigEndian::read_u32(data);
        let data = &data[4..];
        let value = BigEndian::read_u64(data);
        Ctip {
            outpoint: OutPoint { txid, vout },
            value,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Deposit {
    pub address: Hash256,
    pub value: u64,
    pub total_value: u64,
}

impl RedbValue for Deposit {
    type SelfType<'a> = Deposit;
    type AsBytes<'a> = [u8; size_of::<Hash256>() + size_of::<u64>() + size_of::<u64>()];

    fn type_name() -> TypeName {
        TypeName::new("Deposit")
    }

    fn fixed_width() -> Option<usize> {
        Some(size_of::<Hash256>() + size_of::<u64>())
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let address: Hash256 = data[0..size_of::<Hash256>()].try_into().unwrap();
        let data = &data[size_of::<Hash256>()..];
        let value = BigEndian::read_u64(data);
        let data = &data[size_of::<u64>()..];
        let total_value = BigEndian::read_u64(data);
        Deposit {
            address,
            value,
            total_value,
        }
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        let mut data = [0; size_of::<Hash256>() + size_of::<u64>() + size_of::<u64>()];
        data[0..size_of::<Hash256>()].copy_from_slice(&value.address);
        BigEndian::write_u64(
            &mut data[size_of::<Hash256>()..size_of::<Hash256>() + size_of::<u64>()],
            value.value,
        );
        BigEndian::write_u64(
            &mut data[size_of::<Hash256>() + size_of::<u64>()
                ..size_of::<Hash256>() + 2 * size_of::<u64>()],
            value.total_value,
        );
        data
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Sidechain {
    pub sidechain_number: u8,
    pub data: Vec<u8>,
    pub vote_count: u16,
    pub proposal_height: u32,
    pub activation_height: u32,
}

impl RedbValue for Sidechain {
    type SelfType<'a> = Sidechain;
    type AsBytes<'a> = Vec<u8>;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        bincode::deserialize(data).unwrap()
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        bincode::serialize(value).unwrap()
    }

    fn type_name() -> redb::TypeName {
        TypeName::new("Sidechain")
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SidechainProposal {
    pub sidechain_number: u8,
    pub data: Vec<u8>,
    pub vote_count: u16,
    pub proposal_height: u32,
}

impl RedbValue for SidechainProposal {
    type SelfType<'a> = SidechainProposal;
    type AsBytes<'a> = Vec<u8>;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        bincode::deserialize(data).unwrap()
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        bincode::serialize(value).unwrap()
    }

    fn type_name() -> redb::TypeName {
        TypeName::new("SidechainProposal")
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Bundle {
    pub bundle_txid: Hash256,
    pub vote_count: u16,
}

impl RedbValue for Bundle {
    type SelfType<'a> = Bundle;
    type AsBytes<'a> = Vec<u8>;

    fn type_name() -> TypeName {
        TypeName::new("Bundle")
    }

    fn fixed_width() -> Option<usize> {
        None
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'a,
        Self: 'b,
    {
        bincode::serialize(value).unwrap()
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        bincode::deserialize(data).unwrap()
    }
}
