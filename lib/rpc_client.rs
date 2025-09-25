use bitcoin_jsonrpsee::{
    MainClient,
    jsonrpsee::{core::ClientError, http_client::HttpClient},
};
use miette::Diagnostic;

use crate::{cli::NodeRpcConfig, errors::ErrorChain};

#[derive(Debug, Diagnostic, thiserror::Error)]
pub enum Error {
    #[error("RPC user and password must be set together")]
    UserAndPasswordMustBeSetTogether,
    #[error("precisely one of RPC user and cookie must be set")]
    UserOrCookieMustBeSet,
    #[error("unable to read bitcoind cookie at {cookie_path}")]
    ReadCookie {
        cookie_path: String,
        source: std::io::Error,
    },
    #[error("failed to get RPC user name")]
    GetRpcUser,
    #[error("failed to get RPC password")]
    GetRpcPassword,
    #[error("failed to create mainchain RPC client")]
    CreateClient(#[source] bitcoin_jsonrpsee::Error),
}

pub fn create_client(conf: &NodeRpcConfig) -> Result<HttpClient, Error> {
    if conf.user.is_none() != conf.pass.is_none() {
        return Err(Error::UserAndPasswordMustBeSetTogether);
    }

    if conf.user.is_none() == conf.cookie_path.is_none() {
        return Err(Error::UserOrCookieMustBeSet);
    }

    let mut conf_user = conf.user.clone().unwrap_or_default();
    let mut conf_pass = conf.pass.clone().unwrap_or_default();

    if conf.cookie_path.is_some() {
        let cookie_path = conf.cookie_path.clone().unwrap();
        let auth =
            std::fs::read_to_string(cookie_path.clone()).map_err(|err| Error::ReadCookie {
                cookie_path: cookie_path.clone(),
                source: err,
            })?;

        let mut auth = auth.split(':');

        conf_user = auth.next().ok_or(Error::GetRpcUser)?.to_string().clone();

        conf_pass = auth
            .next()
            .ok_or(Error::GetRpcPassword)?
            .to_string()
            .clone();
    }

    // A mempool of default size might contain >300k txs.
    // batch Requesting 300k txs requires ~30MiB,
    // so 100MiB should be enough
    const MAX_REQUEST_SIZE: u32 = 100 * (1 << 20);

    // Default mempool size is 300MB, so 1GiB should be enough
    //
    // TODO: it'd be nice to extract what this setting is from
    // the RPC client at call site. Would require wrapping
    // the RPC client into a struct containing the config values?
    const MAX_RESPONSE_SIZE: u32 = 1 << 30;
    const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

    let client_builder = bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClientBuilder::default()
        .max_request_size(MAX_REQUEST_SIZE)
        .max_response_size(MAX_RESPONSE_SIZE)
        .request_timeout(REQUEST_TIMEOUT);

    let client = bitcoin_jsonrpsee::client(conf.addr, Some(client_builder), &conf_pass, &conf_user)
        .map_err(Error::CreateClient)?;

    Ok(client)
}

/// Broadcasts a transaction to the Bitcoin network.
/// Returns `Some(txid)` if broadcast successfully, `None` if the tx failed to
/// broadcast due to the node not supporting OP_DRIVECHAIN
pub async fn broadcast_transaction<RpcClient>(
    rpc_client: &RpcClient,
    tx: &bdk_wallet::bitcoin::Transaction,
) -> Result<Option<bitcoin::Txid>, ClientError>
where
    RpcClient: MainClient + Sync,
{
    // Note: there's a `broadcast` method on `bitcoin_blockchain`. We're NOT using that,
    // because we're broadcasting transactions that "burn" bitcoin (from a BIP-300/1 unaware
    // perspective). To get around this we have to pass a `maxburnamount` parameter, and
    // that's not possible if going through the ElectrumBlockchain interface.
    //
    // For the interested reader, the flow of ElectrumBlockchain::broadcast is this:
    // 1. Send the raw TX from our Electrum client
    // 2. Electrum server implements this by sending it into Bitcoin Core
    // 3. Bitcoin Core responds with an error, because we're burning money.
    const MAX_BURN_AMOUNT: f64 = 21_000_000.0;
    let encoded_tx = bitcoin::consensus::encode::serialize_hex(tx);
    match rpc_client
        .send_raw_transaction(encoded_tx, None, Some(MAX_BURN_AMOUNT))
        .await
    {
        Ok(txid) => {
            tracing::debug!(%txid, "broadcast tx successfully");
            Ok(Some(txid))
        }
        Err(err) => {
            const OP_DRIVECHAIN_NOT_SUPPORTED_ERR_MSG: &str =
                "non-mandatory-script-verify-flag (NOPx reserved for soft-fork upgrades)";
            tracing::error!("failed to broadcast tx: {:#}", ErrorChain::new(&err));
            match err {
                ClientError::Call(err) if err.message() == OP_DRIVECHAIN_NOT_SUPPORTED_ERR_MSG => {
                    Ok(None)
                }
                err => Err(err),
            }
        }
    }
}
