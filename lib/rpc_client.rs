use bitcoin_jsonrpsee::{
    MainClient,
    jsonrpsee::{core::ClientError, http_client::HttpClient},
};
use miette::miette;

use crate::{cli::NodeRpcConfig, errors::ErrorChain};

pub fn create_client(
    conf: &NodeRpcConfig,
    enable_mempool: bool,
) -> Result<HttpClient, miette::Report> {
    if conf.user.is_none() != conf.pass.is_none() {
        return Err(miette!("RPC user and password must be set together"));
    }

    if conf.user.is_none() == conf.cookie_path.is_none() {
        return Err(miette!("precisely one of RPC user and cookie must be set"));
    }

    let mut conf_user = conf.user.clone().unwrap_or_default();
    let mut conf_pass = conf.pass.clone().unwrap_or_default();

    if conf.cookie_path.is_some() {
        let cookie_path = conf.cookie_path.clone().unwrap();
        let auth = std::fs::read_to_string(cookie_path.clone())
            .map_err(|err| miette!("unable to read bitcoind cookie at {}: {}", cookie_path, err))?;

        let mut auth = auth.split(':');

        conf_user = auth
            .next()
            .ok_or(miette!("failed to get rpcuser"))?
            .to_string()
            .clone();

        conf_pass = auth
            .next()
            .ok_or(miette!("failed to get rpcpassword"))?
            .to_string()
            .to_string()
            .clone();
    }

    let client_builder = if enable_mempool {
        // A mempool of default size might contain >300k txs.
        // batch Requesting 300k txs requires ~30MiB,
        // so 100MiB should be enough
        const MAX_REQUEST_SIZE: u32 = 100 * (1 << 20);
        // Default mempool size is 300MB, so 1GiB should be enough
        const MAX_RESPONSE_SIZE: u32 = 1 << 30;
        const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
        let client_builder =
            bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClientBuilder::default()
                .max_request_size(MAX_REQUEST_SIZE)
                .max_response_size(MAX_RESPONSE_SIZE)
                .request_timeout(REQUEST_TIMEOUT);
        Some(client_builder)
    } else {
        None
    };

    bitcoin_jsonrpsee::client(conf.addr, client_builder, &conf_pass, &conf_user)
        .map_err(|err| miette!("failed to create mainchain RPC client: {err:#}"))
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
