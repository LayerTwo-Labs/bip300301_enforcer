use futures::TryFutureExt as _;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
    types::ErrorObject,
};

use crate::types::{BDKWalletTransaction, SidechainNumber};

#[rpc(namespace = "wallet", namespace_separator = ".", server)]
pub trait Rpc {
    #[method(name = "list_sidechain_deposit_transactions")]
    async fn list_sidechain_deposit_transactions(
        &self,
    ) -> RpcResult<Vec<(SidechainNumber, BDKWalletTransaction)>>;
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
}
