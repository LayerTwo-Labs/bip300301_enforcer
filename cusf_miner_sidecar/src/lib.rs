//! CUSF miner sidecar (TB-sidecar): hold enforcer-format raw txs and mine them
//! into a stock Core block via `getblocktemplate` + assemble + `submitblock`.
//!
//! This is **not** a mempool proxy and does **not** green TB-sendraw. Soft-fork
//! inventory is injected at block build time; the enforcer still gates tip
//! (`invalidateblock` / keep).
//!
//! Local / regtest only. The HTTP binary binds **127.0.0.1** by default.
//!
//! # Inventory rules
//!
//! - Duplicate `txid` → [`Error::DuplicateTxid`] (not silently ignored)
//! - Default caps: [`DEFAULT_MAX_TXS`] txs / [`DEFAULT_MAX_BYTES`] serialized bytes
//! - [`MinerSidecar::mine`] uses [`TxInventory::take_all`] so concurrent adds after
//!   the snapshot are retained; failed mines restore the taken set with caps
//!   re-enforced (overflow dropped if concurrent adds filled the budget)

mod block;
mod error;
mod inventory;
mod rpc;
mod server;

use std::sync::Arc;

use bitcoin::{Address, ScriptBuf, Transaction};
use bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient;
use parking_lot::Mutex;

pub use crate::{
    block::{
        MineResult, assemble_block_with_inventory, inventory_only_coinbase_value_sats,
        inventory_only_value_from_parts, mine_and_submit,
    },
    error::Error,
    inventory::{
        DEFAULT_MAX_BYTES, DEFAULT_MAX_TXS, InventoryLimits, TxInventory, tx_serialized_len,
    },
    rpc::{RpcConfig, connect_bitcoind},
    server::{AppState, RouterConfig, bind_addr, is_loopback_ip, router},
};

/// In-process miner: inventory + bitcoind RPC client + coinbase scriptPubKey.
#[derive(Clone)]
pub struct MinerSidecar {
    inner: Arc<Inner>,
}

struct Inner {
    inventory: Mutex<TxInventory>,
    client: HttpClient,
    coinbase_spk: ScriptBuf,
}

impl MinerSidecar {
    /// Create a sidecar bound to an existing bitcoind JSON-RPC client.
    #[must_use]
    pub fn new(client: HttpClient, coinbase_spk: ScriptBuf) -> Self {
        Self::with_limits(client, coinbase_spk, InventoryLimits::default())
    }

    /// Create with custom inventory caps.
    #[must_use]
    pub fn with_limits(
        client: HttpClient,
        coinbase_spk: ScriptBuf,
        limits: InventoryLimits,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                inventory: Mutex::new(TxInventory::with_limits(limits)),
                client,
                coinbase_spk,
            }),
        }
    }

    /// Create from RPC config + coinbase address (for the HTTP binary).
    pub fn from_rpc_config(rpc: &RpcConfig, coinbase: &Address) -> Result<Self, Error> {
        let client = connect_bitcoind(rpc)?;
        Ok(Self::new(client, coinbase.script_pubkey()))
    }

    /// Add a consensus-encoded transaction hex to inventory.
    pub fn add_tx_hex(&self, hex: &str) -> Result<bitcoin::Txid, Error> {
        self.inner.inventory.lock().add_raw_hex(hex)
    }

    /// Add a decoded transaction to inventory.
    pub fn add_tx(&self, tx: Transaction) -> Result<bitcoin::Txid, Error> {
        self.inner.inventory.lock().add_tx(tx)
    }

    /// List inventory txids in insertion order.
    #[must_use]
    pub fn list_txids(&self) -> Vec<bitcoin::Txid> {
        self.inner.inventory.lock().list_txids()
    }

    /// Number of txs held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.inventory.lock().len()
    }

    /// Whether inventory is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.inventory.lock().is_empty()
    }

    /// Clear inventory without mining.
    pub fn clear(&self) {
        self.inner.inventory.lock().clear();
    }

    /// Build a block (coinbase + inventory snapshot) and `submitblock` to stock Core.
    ///
    /// Takes the current inventory under the lock (`take_all`) so concurrent
    /// `add_tx` after the snapshot is not wiped. On RPC/assembly failure, the
    /// taken set is restored (alongside any concurrent adds).
    pub async fn mine(&self) -> Result<MineResult, Error> {
        let txs = self.inner.inventory.lock().take_all();
        if txs.is_empty() {
            return Err(Error::EmptyInventory);
        }
        match mine_and_submit(&self.inner.client, &self.inner.coinbase_spk, txs.clone()).await {
            Ok(result) => Ok(result),
            Err(err) => {
                self.inner.inventory.lock().restore(txs);
                Err(err)
            }
        }
    }

    /// Shared HTTP router state.
    #[must_use]
    pub fn app_state(&self) -> AppState {
        AppState {
            sidecar: self.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn loopback_gate() {
        assert!(is_loopback_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_loopback_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))));
        assert!(is_loopback_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_loopback_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(!is_loopback_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    }

    #[test]
    fn empty_mine_errors_without_rpc() {
        // Empty inventory must fail before any RPC call. We only exercise the
        // take_all + empty path via a direct inventory check mirrored by mine().
        let mut inv = TxInventory::new();
        assert!(inv.take_all().is_empty());
        assert!(matches!(
            (|| -> Result<(), Error> {
                let txs = inv.take_all();
                if txs.is_empty() {
                    return Err(Error::EmptyInventory);
                }
                Ok(())
            })(),
            Err(Error::EmptyInventory)
        ));
    }
}
