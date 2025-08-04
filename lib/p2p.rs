//! P2P related functions and types

use std::net::{Ipv4Addr, SocketAddrV4};

use bitcoin::p2p::Magic;

pub const SIGNET_MAGIC_BYTES: [u8; 4] = [0xd1, 0xf5, 0x77, 0x6b];

pub const SIGNET_MINER_P2P_ADDR: SocketAddrV4 =
    SocketAddrV4::new(Ipv4Addr::new(172, 105, 148, 135), 38333);

/// Broadcasts a non-standard transaction directly to a specified node via
/// p2p.
/// Returns `true` if submitted successfully, `false` on timeout.
/// Note that if the specified node already has the tx, the broadcast will
/// time out.
pub async fn broadcast_nonstandard_tx(
    p2p_address: std::net::SocketAddr,
    block_height: i32,
    magic: Magic,
    tx: bitcoin::Transaction,
) -> Result<bool, bitcoin_send_tx_p2p::Error> {
    use bitcoin_send_tx_p2p::{Config, Error, send_tx_p2p_over_clearnet};
    let mut config = Config::default();
    config.block_height = block_height;
    config.magic = magic;
    match send_tx_p2p_over_clearnet(p2p_address, tx, Some(config)).await {
        Ok(()) => Ok(true),
        Err(Error::Timeout(_)) => Ok(false),
        Err(err) => Err(err),
    }
}

pub fn compute_signet_magic(signet_challenge: &bitcoin::Script) -> Magic {
    use bitcoin::hashes::{Hash as _, HashEngine as _, sha256d};
    let mut hasher = sha256d::Hash::engine();
    hasher.input(&[0x25]);
    hasher.input(signet_challenge.as_bytes());
    let hash = sha256d::Hash::from_engine(hasher);
    Magic::from_bytes(hash[..=3].try_into().unwrap())
}
