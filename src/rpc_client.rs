use bip300301::jsonrpsee::http_client::HttpClient;
use miette::{miette, IntoDiagnostic};

use crate::cli::NodeRpcConfig;

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
        let client_builder = bip300301::jsonrpsee::http_client::HttpClientBuilder::default()
            .max_request_size(MAX_REQUEST_SIZE)
            .max_response_size(MAX_RESPONSE_SIZE)
            .request_timeout(REQUEST_TIMEOUT);
        Some(client_builder)
    } else {
        None
    };

    bip300301::client(conf.addr, client_builder, &conf_pass, &conf_user).into_diagnostic()
}
