use futures::TryFutureExt as _;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
    types::ErrorObject,
};
use bitcoin::hashes::Hash as _;

use crate::types::{BDKWalletTransaction, SidechainNumber};
use hex;

#[rpc(namespace = "wallet", namespace_separator = ".", server)]
pub trait Rpc {
    #[method(name = "list_sidechain_deposit_transactions")]
    async fn list_sidechain_deposit_transactions(
        &self,
    ) -> RpcResult<Vec<(SidechainNumber, BDKWalletTransaction)>>;

    #[method(name = "create_bmm_critical_data_transaction")]
    async fn create_bmm_critical_data_transaction(
        &self,
        sidechain_id: u32,
        value_sats: u64,
        height: u32,
        critical_hash: String,
        prev_bytes: String,
    ) -> RpcResult<String>;

    
}

fn custom_err<Error>(error: Error) -> ErrorObject<'static>
where
    Error: std::error::Error,
{
    let err_msg = format!("{:#}", crate::errors::ErrorChain::new(&error));
    ErrorObject::owned(-1, err_msg, Option::<()>::None)
}

#[async_trait]
impl RpcServer for crate::wallet::Wallet {
    async fn list_sidechain_deposit_transactions(
        &self,
    ) -> RpcResult<Vec<(SidechainNumber, BDKWalletTransaction)>> {
        self.list_sidechain_deposit_transactions()
            .map_err(custom_err)
            .await
    }

    async fn create_bmm_critical_data_transaction(
        &self,
        sidechain_id: u32,
        value_sats: u64,
        height: u32,
        critical_hash: String,
        prev_bytes: String,
    ) -> RpcResult<String> {
        let sidechain_number = SidechainNumber::from(sidechain_id as u8);
        let amount = bdk_wallet::bitcoin::Amount::from_sat(value_sats);
        let locktime = bdk_wallet::bitcoin::absolute::LockTime::from_height(height)
            .map_err(custom_err)?;
        
        let critical_hash_bytes = hex::decode(&critical_hash)
            .map_err(|e| ErrorObject::owned(-1, format!("Invalid critical_hash hex: {}", e), Option::<()>::None))?;
        
        if critical_hash_bytes.len() != 32 {
            return Err(ErrorObject::owned(-1, "critical_hash must be 32 bytes", Option::<()>::None));
        }
        
        let critical_hash_array: [u8; 32] = critical_hash_bytes.try_into()
            .map_err(|_| ErrorObject::owned(-1, "Failed to convert critical_hash to array", Option::<()>::None))?;
        
        let mut prev_bytes_vec = hex::decode(&prev_bytes)
            .map_err(|e| ErrorObject::owned(-1, format!("Invalid prev_bytes hex: {}", e), Option::<()>::None))?;
        prev_bytes_vec.reverse();
        let prev_block_hash = bdk_wallet::bitcoin::BlockHash::from_byte_array(
            prev_bytes_vec.try_into()
                .map_err(|_| ErrorObject::owned(-1, "prev_bytes must be 32 bytes", Option::<()>::None))?
        );

        let tx = self
            .create_bmm_request(
                sidechain_number,
                prev_block_hash,
                critical_hash_array,
                amount,
                locktime,
            )
            .await
            .map_err(custom_err)?
            .ok_or_else(|| {
                ErrorObject::owned(
                    -1,
                    "BMM request with same sidechain_number and prev_bytes already exists",
                    Option::<()>::None,
                )
            })?;

        let txid = tx.compute_txid();
        let mut txid_bytes = txid.to_byte_array();
        txid_bytes.reverse();
        Ok(hex::encode(txid_bytes))
    }
}
