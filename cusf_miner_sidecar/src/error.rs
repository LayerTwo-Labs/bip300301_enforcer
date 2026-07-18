//! Error types for the CUSF miner sidecar.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid transaction hex: {0}")]
    InvalidTxHex(String),

    #[error("transaction decode failed: {0}")]
    TxDecode(#[from] bitcoin::consensus::encode::Error),

    #[error("duplicate transaction already in inventory: {0}")]
    DuplicateTxid(bitcoin::Txid),

    #[error(
        "inventory full: max {max} transactions (have {have}); clear or mine before adding more"
    )]
    InventoryTxCap { have: usize, max: usize },

    #[error(
        "inventory size limit: adding {add_bytes} bytes would exceed max {max_bytes} total \
         (currently {have_bytes})"
    )]
    InventoryByteCap {
        have_bytes: usize,
        add_bytes: usize,
        max_bytes: usize,
    },

    #[error("empty inventory: nothing to mine")]
    EmptyInventory,

    #[error("coinbasevalue missing from getblocktemplate")]
    MissingCoinbaseValue,

    #[error("failed to compute witness merkle root")]
    WitnessRoot,

    #[error("failed to compute transaction merkle root")]
    TxMerkleRoot,

    #[error("submitblock rejected: {0}")]
    BlockRejected(String),

    #[error("bitcoind RPC error ({method}): {source}")]
    Rpc {
        method: String,
        #[source]
        source: bitcoin_jsonrpsee::jsonrpsee::core::ClientError,
    },

    #[error("failed to create bitcoind RPC client: {0}")]
    CreateClient(#[from] bitcoin_jsonrpsee::Error),

    #[error("invalid RPC address: {0}")]
    InvalidAddr(String),

    #[error("invalid coinbase address: {0}")]
    InvalidCoinbaseAddress(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Whether this error is a client/input problem (HTTP 400).
    #[must_use]
    pub fn is_client_error(&self) -> bool {
        matches!(
            self,
            Self::InvalidTxHex(_)
                | Self::TxDecode(_)
                | Self::DuplicateTxid(_)
                | Self::InventoryTxCap { .. }
                | Self::InventoryByteCap { .. }
                | Self::EmptyInventory
                | Self::InvalidAddr(_)
                | Self::InvalidCoinbaseAddress(_)
        )
    }
}
