//! P2P related functions and types

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use bitcoin::p2p::Magic;

pub const SIGNET_MAGIC_BYTES: [u8; 4] = [0xe4, 0x09, 0xe9, 0x68];

pub const SIGNET_MINER_P2P_ADDR: SocketAddrV4 =
    SocketAddrV4::new(Ipv4Addr::new(172, 105, 148, 135), 38333);

pub const fn default_p2p_broadcast_addr(
    network: bitcoin::Network,
    magic_bytes: [u8; 4],
) -> Option<SocketAddrV4> {
    match (network, magic_bytes) {
        (bitcoin::Network::Signet, SIGNET_MAGIC_BYTES) => Some(SIGNET_MINER_P2P_ADDR),
        (_, _) => None,
    }
}

/// A peer address for p2p tx broadcast: either a socket address, or a
/// `HOST:PORT` pair that is resolved via DNS at broadcast time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BroadcastAddr {
    Sock(SocketAddr),
    Host { host: String, port: u16 },
}

#[derive(Debug, thiserror::Error)]
pub enum ParseBroadcastAddrError {
    #[error("expected `IP:PORT` or `HOST:PORT`, got `{0}`")]
    MissingPort(String),
    #[error("invalid host in broadcast address `{0}`")]
    InvalidHost(String),
    #[error("invalid port in broadcast address `{addr}`")]
    InvalidPort {
        addr: String,
        source: std::num::ParseIntError,
    },
}

impl std::str::FromStr for BroadcastAddr {
    type Err = ParseBroadcastAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(sock) = s.parse::<SocketAddr>() {
            return Ok(Self::Sock(sock));
        }
        let (host, port) = s
            .rsplit_once(':')
            .ok_or_else(|| ParseBroadcastAddrError::MissingPort(s.to_owned()))?;
        // A colon in the host means an IPv6 address, which must be bracketed
        // and is handled by the `SocketAddr` parse above.
        if host.is_empty() || host.contains([':', '[', ']']) {
            return Err(ParseBroadcastAddrError::InvalidHost(s.to_owned()));
        }
        let port = port
            .parse::<u16>()
            .map_err(|source| ParseBroadcastAddrError::InvalidPort {
                addr: s.to_owned(),
                source,
            })?;
        Ok(Self::Host {
            host: host.to_owned(),
            port,
        })
    }
}

impl std::fmt::Display for BroadcastAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sock(sock) => sock.fmt(f),
            Self::Host { host, port } => write!(f, "{host}:{port}"),
        }
    }
}

impl From<SocketAddr> for BroadcastAddr {
    fn from(sock: SocketAddr) -> Self {
        Self::Sock(sock)
    }
}

impl From<SocketAddrV4> for BroadcastAddr {
    fn from(sock: SocketAddrV4) -> Self {
        Self::Sock(sock.into())
    }
}

impl BroadcastAddr {
    async fn resolve(&self) -> Result<Vec<SocketAddr>, std::io::Error> {
        match self {
            Self::Sock(sock) => Ok(vec![*sock]),
            Self::Host { host, port } => {
                let addrs = tokio::net::lookup_host((host.as_str(), *port)).await?;
                Ok(addrs.collect())
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BroadcastNonstandardTxError {
    #[error("failed to resolve p2p broadcast address `{addr}`")]
    Resolve {
        addr: BroadcastAddr,
        source: std::io::Error,
    },
    #[error("p2p broadcast address `{addr}` resolved to no addresses")]
    ResolvedEmpty { addr: BroadcastAddr },
    #[error(transparent)]
    Send(#[from] bitcoin_send_tx_p2p::Error),
}

/// Sends a tx to each address in order, stopping at the first address that
/// accepts it.
/// `send` must return `true` if the tx was submitted successfully, and `false`
/// on timeout.
/// Returns `true` if any address accepted the tx, and `false` if every address
/// that was dialled successfully timed out.
/// Panics if `socket_addrs` is empty.
async fn broadcast_to_addrs<E, F, Fut>(
    socket_addrs: impl IntoIterator<Item = SocketAddr>,
    mut send: F,
) -> Result<bool, E>
where
    F: FnMut(SocketAddr) -> Fut,
    Fut: std::future::Future<Output = Result<bool, E>>,
{
    let mut timed_out = false;
    let mut last_err = None;
    for socket_addr in socket_addrs {
        match send(socket_addr).await {
            Ok(true) => return Ok(true),
            Ok(false) => {
                tracing::debug!(%socket_addr, "p2p broadcast timed out, trying next address");
                timed_out = true;
            }
            Err(err) => last_err = Some(err),
        }
    }
    // A timeout is not an error, so it takes precedence over any address that
    // failed to connect at all.
    if timed_out {
        return Ok(false);
    }
    let err = last_err.expect("socket_addrs is non-empty, so at least one send was attempted");
    Err(err)
}

/// Broadcasts a non-standard transaction directly to a specified node via
/// p2p.
/// Returns `true` if submitted successfully, `false` on timeout.
/// Note that if the specified node already has the tx, the broadcast will
/// time out.
pub async fn broadcast_nonstandard_tx(
    p2p_address: BroadcastAddr,
    block_height: i32,
    magic: Magic,
    tx: bitcoin::Transaction,
) -> Result<bool, BroadcastNonstandardTxError> {
    use bitcoin_send_tx_p2p::{Config, Error, send_tx_p2p_over_clearnet};
    let socket_addrs =
        p2p_address
            .resolve()
            .await
            .map_err(|source| BroadcastNonstandardTxError::Resolve {
                addr: p2p_address.clone(),
                source,
            })?;
    if socket_addrs.is_empty() {
        return Err(BroadcastNonstandardTxError::ResolvedEmpty { addr: p2p_address });
    }
    // A hostname may resolve to multiple addresses (e.g. `localhost` to both
    // `127.0.0.1` and `::1`) of which only some accept connections, so try
    // each in order until one connects.
    broadcast_to_addrs(socket_addrs, |socket_addr| {
        let tx = tx.clone();
        async move {
            let mut config = Config::default();
            config.block_height = block_height;
            config.magic = magic;
            match send_tx_p2p_over_clearnet(socket_addr, tx, Some(config)).await {
                Ok(()) => Ok(true),
                Err(Error::Timeout(_)) => Ok(false),
                Err(err) => Err(err),
            }
        }
    })
    .await
    .map_err(BroadcastNonstandardTxError::from)
}

// https://github.com/kallewoof/bips/blob/master/bip-0325.mediawiki#message-start
pub fn compute_signet_magic(signet_challenge: &bitcoin::Script) -> Magic {
    use bitcoin::{
        consensus::{Encodable as _, encode::VarInt},
        hashes::{Hash as _, HashEngine as _, sha256d},
    };
    let mut hasher = sha256d::Hash::engine();
    // Core derives the signet magic from `sha256d(CompactSize(len) || challenge)`
    // (streaming the challenge vector writes a CompactSize length prefix), so
    // encode the length with Bitcoin's CompactSize rather than a raw single byte,
    // which would diverge (and wrap mod 256) for challenges >= 253 bytes.
    VarInt(signet_challenge.len() as u64)
        .consensus_encode(&mut hasher)
        .expect("hash engine writes are infallible");
    hasher.input(signet_challenge.as_bytes());
    let hash = sha256d::Hash::from_engine(hasher);
    Magic::from_bytes(hash[..=3].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    // from the BIP325 example
    #[test]
    fn test_bip_compute_signet_magic() {
        let signet_challenge = bitcoin::ScriptBuf::from_hex(
            "512103ad5e0edad18cb1f0fc0d28a3d4f1f3e445640337489abb10404f2d1e086be43051ae",
        )
        .unwrap();
        let magic = compute_signet_magic(signet_challenge.as_script());
        assert_eq!(magic, Magic::from_bytes([0x7e, 0xc6, 0x53, 0xa5]));
    }

    // drivechain signet challenge
    #[test]
    fn test_drivechain_magic() {
        let signet_challenge =
            bitcoin::ScriptBuf::from_hex("a91484fa7c2460891fe5212cb08432e21a4207909aa987").unwrap();
        let magic = compute_signet_magic(signet_challenge.as_script());
        assert_eq!(magic, Magic::from_bytes([0xe4, 0x09, 0xe9, 0x68]));
    }

    #[test]
    fn test_parse_broadcast_addr() {
        assert_eq!(
            "127.0.0.1:8333".parse::<BroadcastAddr>().unwrap(),
            BroadcastAddr::Sock("127.0.0.1:8333".parse().unwrap()),
        );
        assert_eq!(
            "[::1]:8333".parse::<BroadcastAddr>().unwrap(),
            BroadcastAddr::Sock("[::1]:8333".parse().unwrap()),
        );
        assert_eq!(
            "example.com:38333".parse::<BroadcastAddr>().unwrap(),
            BroadcastAddr::Host {
                host: "example.com".to_owned(),
                port: 38333,
            },
        );
        assert_eq!(
            "localhost:8333".parse::<BroadcastAddr>().unwrap(),
            BroadcastAddr::Host {
                host: "localhost".to_owned(),
                port: 8333,
            },
        );
        // no port
        assert!("example.com".parse::<BroadcastAddr>().is_err());
        // invalid port
        assert!("example.com:notaport".parse::<BroadcastAddr>().is_err());
        assert!("example.com:99999".parse::<BroadcastAddr>().is_err());
        // empty host
        assert!(":8333".parse::<BroadcastAddr>().is_err());
        // unbracketed IPv6
        assert!("::1:8333".parse::<BroadcastAddr>().is_err());
    }

    fn broadcast_test_addrs() -> Vec<SocketAddr> {
        vec![
            "127.0.0.1:38333".parse().unwrap(),
            "[::1]:38333".parse().unwrap(),
        ]
    }

    // A timeout on one address must not prevent the remaining addresses from
    // being dialled.
    #[tokio::test]
    async fn test_broadcast_to_addrs_continues_after_timeout() {
        let addrs = broadcast_test_addrs();
        let mut attempted = Vec::new();
        let res = broadcast_to_addrs(addrs.clone(), |socket_addr| {
            attempted.push(socket_addr);
            // the first address times out, the second accepts the tx
            let sent = attempted.len() == 2;
            async move { Ok::<_, &'static str>(sent) }
        })
        .await;
        assert_eq!(res, Ok(true));
        assert_eq!(attempted, addrs);
    }

    #[tokio::test]
    async fn test_broadcast_to_addrs_all_timeout() {
        let addrs = broadcast_test_addrs();
        let mut attempted = Vec::new();
        let res = broadcast_to_addrs(addrs.clone(), |socket_addr| {
            attempted.push(socket_addr);
            async move { Ok::<_, &'static str>(false) }
        })
        .await;
        assert_eq!(res, Ok(false));
        assert_eq!(attempted, addrs);
    }

    // A timeout is a successful connection, so it takes precedence over an
    // address that could not be reached at all.
    #[tokio::test]
    async fn test_broadcast_to_addrs_err_then_timeout() {
        let addrs = broadcast_test_addrs();
        let mut attempted = Vec::new();
        let res = broadcast_to_addrs(addrs.clone(), |socket_addr| {
            attempted.push(socket_addr);
            let timed_out = attempted.len() == 2;
            async move {
                if timed_out {
                    Ok(false)
                } else {
                    Err("connection refused")
                }
            }
        })
        .await;
        assert_eq!(res, Ok(false));
        assert_eq!(attempted, addrs);
    }

    #[tokio::test]
    async fn test_broadcast_to_addrs_all_err() {
        let addrs = broadcast_test_addrs();
        let mut attempted = Vec::new();
        let res = broadcast_to_addrs(addrs.clone(), |socket_addr| {
            attempted.push(socket_addr);
            async move { Err::<bool, _>(socket_addr) }
        })
        .await;
        assert_eq!(res, Err(addrs[1]));
        assert_eq!(attempted, addrs);
    }

    // For challenges >= 253 bytes the length prefix must be a CompactSize, not a
    // single byte, otherwise the derived magic diverges from Bitcoin Core. This
    // 253-byte challenge encodes its length as CompactSize `fd fd 00`; Core logs
    // the derived message-start magic as `ae285478` for it.
    #[test]
    fn test_long_challenge_magic_matches_core() {
        let signet_challenge = bitcoin::ScriptBuf::from_bytes(vec![0x51; 253]);
        let magic = compute_signet_magic(signet_challenge.as_script());
        assert_eq!(magic, Magic::from_bytes([0xae, 0x28, 0x54, 0x78]));
    }
}
