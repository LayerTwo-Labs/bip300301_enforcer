use bip300301::jsonrpsee::core::client::Error as JsonRpcError;
use miette::{diagnostic, Diagnostic};
use serde::Deserialize;
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, Diagnostic, Error)]
#[diagnostic(
    code(electrum_error),
    help("The error is from the Electrum server. Check the message for more details.")
)]
#[error("electrum error `{code}`: `{message}`")]
pub struct ElectrumError {
    code: i32,
    message: String,
}

impl From<ElectrumError> for tonic::Status {
    fn from(error: ElectrumError) -> Self {
        let code = match error.code {
            // https://github.com/bitcoin/bitcoin/blob/e8f72aefd20049eac81b150e7f0d33709acd18ed/src/common/messages.cpp
            -25 => tonic::Code::InvalidArgument,
            _ => tonic::Code::Unknown,
        };
        Self::new(code, error.to_string())
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("Bitcoin Core RPC error `{method}")]
#[diagnostic(code(bitcoin_core_rpc_error))]
pub struct BitcoinCoreRPC {
    pub method: String,
    #[source]
    pub error: JsonRpcError,
}

#[derive(Debug, Diagnostic, Error)]
#[error("failed to consensus encode block")]
#[diagnostic(code(encode_block_error))]
pub struct EncodeBlock(#[from] pub bitcoin::io::Error);
