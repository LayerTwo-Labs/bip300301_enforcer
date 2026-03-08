//! P2P related functions and types

use std::net::{Ipv4Addr, SocketAddrV4};

use bitcoin::p2p::Magic;

pub const SIGNET_MAGIC_BYTES: [u8; 4] = [0xe4, 0x09, 0xe9, 0x68];

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

// https://github.com/kallewoof/bips/blob/master/bip-0325.mediawiki#message-start
pub fn compute_signet_magic(signet_challenge: &bitcoin::Script) -> Magic {
    use bitcoin::hashes::{Hash as _, HashEngine as _, sha256d};
    let mut hasher = sha256d::Hash::engine();
    hasher.input(&[signet_challenge.len() as u8]);
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
}
