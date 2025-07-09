use bitcoin::BlockHash;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use serde::{Serialize, Serializer};

use crate::{
    server::custom_json_rpc_err,
    types::{Ctip, HeaderInfo, SidechainBlockInfo, SidechainNumber},
};

#[derive(Clone, Copy, Debug)]
pub struct Pong;

impl Serialize for Pong {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str("Pong")
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BlockInfoItem {
    pub header_info: HeaderInfo,
    pub block_info: SidechainBlockInfo,
}

type BlockInfoResponse = Vec<BlockInfoItem>;

#[rpc(namespace = "validator", namespace_separator = ".", server)]
pub trait Rpc {
    #[method(name = "ping")]
    fn ping(&self) -> RpcResult<Pong>;

    #[method(name = "ctip")]
    fn get_ctip(&self, sidechain_number: SidechainNumber) -> RpcResult<Option<Ctip>>;

    #[method(name = "get_block_info")]
    fn get_block_info(
        &self,
        block_hash: BlockHash,
        sidechain_id: SidechainNumber,
        max_ancestors: Option<usize>,
    ) -> RpcResult<BlockInfoResponse>;
}

impl RpcServer for crate::validator::Validator {
    fn ping(&self) -> RpcResult<Pong> {
        Ok(Pong)
    }

    fn get_ctip(&self, sidechain_number: SidechainNumber) -> RpcResult<Option<Ctip>> {
        self.try_get_ctip(sidechain_number)
            .map_err(custom_json_rpc_err)
    }

    fn get_block_info(
        &self,
        block_hash: BlockHash,
        sidechain_id: SidechainNumber,
        max_ancestors: Option<usize>,
    ) -> RpcResult<BlockInfoResponse> {
        // Get block infos with ancestors
        let max_ancestors = max_ancestors.unwrap_or(0);
        let block_infos = self
            .try_get_block_infos(&block_hash, max_ancestors)
            .map_err(custom_json_rpc_err)?;
        let res = match block_infos {
            None => Vec::new(),
            Some(block_infos) => block_infos
                .into_iter()
                .map(|(header_info, block_info)| BlockInfoItem {
                    header_info,
                    block_info: block_info.only_sidechain(sidechain_id).to_owned(),
                })
                .collect(),
        };
        Ok(res)
    }
}
