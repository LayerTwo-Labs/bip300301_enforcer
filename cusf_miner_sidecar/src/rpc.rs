//! Bitcoind JSON-RPC client helpers.

use std::{fmt, net::SocketAddr};

use bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient;

use crate::error::Error;

/// Configuration for connecting to stock bitcoind.
///
/// [`Debug`] redacts the password.
#[derive(Clone)]
pub struct RpcConfig {
    pub addr: SocketAddr,
    pub user: String,
    pub pass: String,
}

impl fmt::Debug for RpcConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcConfig")
            .field("addr", &self.addr)
            .field("user", &self.user)
            .field("pass", &"<redacted>")
            .finish()
    }
}

impl RpcConfig {
    #[must_use]
    pub fn new(addr: SocketAddr, user: impl Into<String>, pass: impl Into<String>) -> Self {
        Self {
            addr,
            user: user.into(),
            pass: pass.into(),
        }
    }

    /// Parse `host:port` into a socket address (IPv4/IPv6 host as required by SocketAddr).
    pub fn parse_addr(host_port: &str) -> Result<SocketAddr, Error> {
        host_port
            .parse()
            .map_err(|e| Error::InvalidAddr(format!("{host_port}: {e}")))
    }
}

/// Build an authenticated HTTP JSON-RPC client for bitcoind.
pub fn connect_bitcoind(conf: &RpcConfig) -> Result<HttpClient, Error> {
    const MAX_REQUEST_SIZE: u32 = 100 * (1 << 20);
    const MAX_RESPONSE_SIZE: u32 = 1 << 30;
    const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

    let builder = bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClientBuilder::default()
        .max_request_size(MAX_REQUEST_SIZE)
        .max_response_size(MAX_RESPONSE_SIZE)
        .request_timeout(REQUEST_TIMEOUT);

    Ok(bitcoin_jsonrpsee::client(
        conf.addr,
        Some(builder),
        &conf.pass,
        &conf.user,
    )?)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    #[test]
    fn debug_redacts_password() {
        let cfg = RpcConfig::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 18443),
            "user",
            "super-secret",
        );
        let s = format!("{cfg:?}");
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("super-secret"));
    }
}
