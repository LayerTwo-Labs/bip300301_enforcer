use std::{cell::RefCell, collections::HashSet};

use bdk::{
    self,
    bitcoin::{Network, Transaction, Txid},
    blockchain::{
        rpc::RpcBlockchainFactory, BlockchainFactory as _, ElectrumBlockchain, Progress,
        RpcBlockchain,
    },
    database::BatchDatabase,
    electrum_client::ConfigBuilder,
};
use miette::{miette, IntoDiagnostic as _, Result};

use crate::{
    cli::{NodeRpcConfig, WalletConfig},
    rpc_client,
};

// Wrapper for different blockchain backends, that allow the wallet to dynamically
// choose one at run-time.
pub(crate) enum Type {
    Electrum(ElectrumBlockchain),
    Rpc(RpcBlockchain),
}

impl Type {
    pub(crate) fn new<T: BatchDatabase>(
        network: Network,
        node_config: &NodeRpcConfig,
        wallet_config: &WalletConfig,
        wallet: &bdk::Wallet<T>,
    ) -> Result<Self> {
        if network == Network::Regtest {
            tracing::debug!("Creating RPC blockchain client");

            let factory = RpcBlockchainFactory {
                url: node_config.addr.to_string(),
                auth: rpc_client::create_auth(node_config)?,
                network,
                wallet_name_prefix: None,
                sync_params: None,
                default_skip_blocks: 0,
            };

            let blockchain = factory.build_for_wallet(wallet, None).into_diagnostic()?;

            return Ok(Type::Rpc(blockchain));
        }

        let electrum_url = format!(
            "{}:{}",
            wallet_config.electrum_host, wallet_config.electrum_port
        );

        tracing::debug!("Creating electrum blockchain client: {electrum_url}");

        // Apply a reasonably short timeout to prevent the wallet from hanging
        let timeout = 5;
        let config = ConfigBuilder::new().timeout(Some(timeout)).build();

        let electrum_client = bdk::electrum_client::Client::from_config(&electrum_url, config)
            .map_err(|err| miette!("failed to create electrum client: {err:#}"))?;

        Ok(Type::Electrum(ElectrumBlockchain::from(electrum_client)))
    }
}

impl bdk::blockchain::Blockchain for Type {
    fn get_capabilities(&self) -> HashSet<bdk::blockchain::Capability> {
        match self {
            Type::Electrum(b) => b.get_capabilities(),
            Type::Rpc(b) => b.get_capabilities(),
        }
    }

    fn broadcast(&self, tx: &bdk::bitcoin::Transaction) -> Result<(), bdk::Error> {
        match self {
            Type::Electrum(b) => b.broadcast(tx),
            Type::Rpc(b) => b.broadcast(tx),
        }
    }

    fn estimate_fee(&self, target: usize) -> Result<bdk::FeeRate, bdk::Error> {
        match self {
            Type::Electrum(b) => b.estimate_fee(target),
            Type::Rpc(b) => b.estimate_fee(target),
        }
    }
}

impl bdk::blockchain::GetBlockHash for Type {
    fn get_block_hash(&self, height: u64) -> Result<bdk::bitcoin::BlockHash, bdk::Error> {
        match self {
            Type::Electrum(b) => b.get_block_hash(height),
            Type::Rpc(b) => b.get_block_hash(height),
        }
    }
}

impl bdk::blockchain::GetTx for Type {
    fn get_tx(&self, txid: &Txid) -> Result<Option<Transaction>, bdk::Error> {
        match self {
            Type::Electrum(b) => b.get_tx(txid),
            Type::Rpc(b) => b.get_tx(txid),
        }
    }
}

impl bdk::blockchain::GetHeight for Type {
    fn get_height(&self) -> Result<u32, bdk::Error> {
        match self {
            Type::Electrum(b) => b.get_height(),
            Type::Rpc(b) => b.get_height(),
        }
    }
}

impl bdk::blockchain::WalletSync for Type {
    fn wallet_setup<D: BatchDatabase>(
        &self,
        database: &RefCell<D>,
        progress_update: Box<dyn Progress>,
    ) -> Result<(), bdk::Error> {
        match self {
            Type::Electrum(b) => b.wallet_setup(database, progress_update),
            Type::Rpc(b) => b.wallet_setup(database, progress_update),
        }
    }

    fn wallet_sync<D: BatchDatabase>(
        &self,
        database: &RefCell<D>,
        progress_update: Box<dyn Progress>,
    ) -> Result<(), bdk::Error> {
        match self {
            Type::Electrum(b) => b.wallet_sync(database, progress_update),
            Type::Rpc(b) => b.wallet_sync(database, progress_update),
        }
    }
}
