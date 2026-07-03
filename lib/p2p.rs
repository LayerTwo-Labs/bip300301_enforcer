//! P2P related functions and types

use std::net::{Ipv4Addr, SocketAddrV4};

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
