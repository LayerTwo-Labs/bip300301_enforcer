use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::ErrorObject};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::str::FromStr;

use crate::types::{BlockInfo, Ctip, HeaderInfo, SidechainNumber};

#[derive(Clone, Copy, Debug)]
pub struct Pong;

impl<'de> Deserialize<'de> for Pong {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        if s == "Pong" {
            Ok(Pong)
        } else {
            Err(serde::de::Error::custom("expected 'Pong'"))
        }
    }
}

impl Serialize for Pong {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str("Pong")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockInfoResponse {
    pub header_info: HeaderInfo,
    pub block_info: BlockInfo,
}

#[rpc(namespace = "validator", namespace_separator = ".", server)]
pub trait Rpc {
    #[method(name = "ping")]
    fn ping(&self) -> RpcResult<Pong>;

    #[method(name = "ctip")]
    fn get_ctip(&self, sidechain_number: SidechainNumber) -> RpcResult<Ctip>;

    #[method(name = "get_block_info")]
    fn get_block_info(&self, block_hash: String) -> RpcResult<BlockInfoResponse>;
}

impl RpcServer for crate::validator::Validator {
    fn ping(&self) -> RpcResult<Pong> {
        Ok(Pong)
    }

    fn get_ctip(&self, sidechain_number: SidechainNumber) -> RpcResult<Ctip> {
        self.get_ctip(sidechain_number)
            .map_err(|e| ErrorObject::owned(-1, e.to_string(), Option::<()>::None))
    }

    fn get_block_info(&self, block_hash: String) -> RpcResult<BlockInfoResponse> {
        // Parse the block hash from hex string
        let block_hash = bitcoin::BlockHash::from_str(&block_hash)
            .map_err(|e| ErrorObject::owned(-1, format!("Invalid block hash: {}", e), Option::<()>::None))?;
        
        // Get header info and block info
        let header_info = self.get_header_info(&block_hash)
            .map_err(|e| ErrorObject::owned(-1, format!("Failed to get header info: {}", e), Option::<()>::None))?;
        
        let block_info = self.get_block_info(&block_hash)
            .map_err(|e| ErrorObject::owned(-1, format!("Failed to get block info: {}", e), Option::<()>::None))?;
        
        Ok(BlockInfoResponse {
            header_info,
            block_info,
        })
    }
}
