use jsonrpsee::{core::RpcResult, proc_macros::rpc, types::ErrorObject};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::types::{Ctip, SidechainNumber};

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

#[rpc(namespace = "validator", namespace_separator = ".", server)]
pub trait Rpc {
    #[method(name = "ping")]
    fn ping(&self) -> RpcResult<Pong>;

    #[method(name = "ctip")]
    fn get_ctip(&self, sidechain_number: SidechainNumber) -> RpcResult<Ctip>;
}

impl RpcServer for crate::validator::Validator {
    fn ping(&self) -> RpcResult<Pong> {
        Ok(Pong)
    }

    fn get_ctip(&self, sidechain_number: SidechainNumber) -> RpcResult<Ctip> {
        self.get_ctip(sidechain_number)
            .map_err(|e| ErrorObject::owned(-1, e.to_string(), Option::<()>::None))
    }
}
