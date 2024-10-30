use bdk::blockchain::rpc::Auth;
use bip300301::jsonrpsee::http_client::HttpClient;
use miette::{miette, IntoDiagnostic, Result};

use crate::cli::NodeRpcConfig;

pub fn create_auth(conf: &NodeRpcConfig) -> Result<bdk::blockchain::rpc::Auth> {
    if let Some(ref cookie) = conf.cookie_path {
        return Ok(Auth::Cookie {
            file: cookie.into(),
        });
    }

    let Some(ref user) = conf.user else {
        return Err(miette!("RPC user must be set"));
    };

    let Some(ref pass) = conf.pass else {
        return Err(miette!("RPC password must be set"));
    };

    Ok(Auth::UserPass {
        username: user.clone(),
        password: pass.clone(),
    })
}

pub fn create_client(conf: &NodeRpcConfig) -> Result<HttpClient> {
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
    bip300301::client(conf.addr, None, &conf_pass, &conf_user).into_diagnostic()
}
