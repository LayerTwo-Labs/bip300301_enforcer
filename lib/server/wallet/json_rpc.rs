use bitcoin::{BlockHash, Txid};
use futures::TryFutureExt as _;
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use thiserror::Error;

use crate::{
    server::custom_json_rpc_err,
    types::{BmmCommitment, SidechainNumber},
    wallet::SidechainDepositTransaction,
};

#[derive(Debug, Error)]
#[error("BMM request with same sidechain number and previous block hash already exists")]
struct BmmRequestAlreadyExistsError;

#[rpc(namespace = "wallet", namespace_separator = ".", server)]
pub trait Rpc {
    #[method(name = "list_sidechain_deposit_transactions")]
    async fn list_sidechain_deposit_transactions(
        &self,
    ) -> RpcResult<Vec<SidechainDepositTransaction>>;

    #[method(name = "create_bmm_critical_data_transaction")]
    async fn create_bmm_critical_data_transaction(
        &self,
        sidechain_id: SidechainNumber,
        value_sats: u64,
        lock_time: bitcoin::absolute::LockTime,
        critical_hash: BmmCommitment,
        prev_block_hash: BlockHash,
    ) -> RpcResult<Txid>;
}

#[async_trait]
impl RpcServer for crate::wallet::Wallet {
    async fn list_sidechain_deposit_transactions(
        &self,
    ) -> RpcResult<Vec<SidechainDepositTransaction>> {
        self.list_sidechain_deposit_transactions()
            .map_err(custom_json_rpc_err)
            .await
    }

    async fn create_bmm_critical_data_transaction(
        &self,
        sidechain_id: SidechainNumber,
        value_sats: u64,
        lock_time: bitcoin::absolute::LockTime,
        critical_hash: BmmCommitment,
        prev_block_hash: BlockHash,
    ) -> RpcResult<Txid> {
        let amount = bdk_wallet::bitcoin::Amount::from_sat(value_sats);
        let tx = self
            .create_bmm_request(
                sidechain_id,
                prev_block_hash,
                critical_hash,
                amount,
                lock_time,
            )
            .await
            .map_err(custom_json_rpc_err)?
            .ok_or_else(|| custom_json_rpc_err(BmmRequestAlreadyExistsError))?;
        Ok(tx.compute_txid())
    }
}
