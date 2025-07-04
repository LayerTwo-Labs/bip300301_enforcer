use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::ErrorObject};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::str::FromStr;
use hex;

use crate::types::{Ctip, HeaderInfo, SidechainNumber};

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
    pub infos: Vec<BlockInfoItem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockInfoItem {
    pub header_info: HeaderInfo,
    pub block_info: FilteredBlockInfo,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FilteredBlockInfo {
    pub bmm_commitment: Option<String>,
    pub events: Vec<crate::types::BlockEvent>,
}

#[rpc(namespace = "validator", namespace_separator = ".", server)]
pub trait Rpc {
    #[method(name = "ping")]
    fn ping(&self) -> RpcResult<Pong>;

    #[method(name = "ctip")]
    fn get_ctip(&self, sidechain_number: SidechainNumber) -> RpcResult<Ctip>;

    #[method(name = "get_block_info")]
    fn get_block_info(&self, block_hash: String, sidechain_id: u32, max_ancestors: Option<u32>) -> RpcResult<BlockInfoResponse>;
    
}

impl RpcServer for crate::validator::Validator {
    fn ping(&self) -> RpcResult<Pong> {
        Ok(Pong)
    }

    fn get_ctip(&self, sidechain_number: SidechainNumber) -> RpcResult<Ctip> {
        self.get_ctip(sidechain_number)
            .map_err(|e| ErrorObject::owned(-1, e.to_string(), Option::<()>::None))
    }

    fn get_block_info(&self, block_hash: String, sidechain_id: u32, max_ancestors: Option<u32>) -> RpcResult<BlockInfoResponse> {
        // Parse the block hash from hex string
        let block_hash = bitcoin::BlockHash::from_str(&block_hash)
            .map_err(|e| ErrorObject::owned(-1, format!("Invalid block hash: {}", e), Option::<()>::None))?;
        
        // Convert sidechain_id to SidechainNumber
        let sidechain_number = SidechainNumber::try_from(sidechain_id)
            .map_err(|e| ErrorObject::owned(-1, format!("Invalid sidechain_id: {}", e), Option::<()>::None))?;
        
        // Get block infos with ancestors
        let max_ancestors = max_ancestors.unwrap_or(0) as usize;
        let block_infos = self.try_get_block_infos(&block_hash, max_ancestors)
            .map_err(|e| ErrorObject::owned(-1, format!("Failed to get block infos: {}", e), Option::<()>::None))?;
        
        let infos = match block_infos {
            None => Vec::new(),
            Some(block_infos) => block_infos
                .into_iter()
                .map(|(header_info, block_info)| {
                    // Filter BMM commitment for the specific sidechain
                    let bmm_commitment = block_info.bmm_commitments.get(&sidechain_number)
                        .map(|commitment| hex::encode(commitment));
                    
                    // Filter events for the specific sidechain
                    let events = block_info.events
                        .iter()
                        .filter_map(|event| {
                            let (event_sidechain_number, _): (SidechainNumber, crate::proto::mainchain::block_info::event::Event) = Option::<_>::from(event)?;
                            if event_sidechain_number == sidechain_number {
                                Some(event.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    
                    BlockInfoItem {
                        header_info,
                        block_info: FilteredBlockInfo {
                            bmm_commitment,
                            events,
                        },
                    }
                })
                .collect(),
        };
        
        Ok(BlockInfoResponse { infos })
    }

}
