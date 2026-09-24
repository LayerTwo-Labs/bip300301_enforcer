//! What the real-node integration tests (`p2p_send_tx`, `peer_bmm_request`)
//! cannot show: that the send only returns once the node has processed the
//! transaction, falling back to further addresses, and rejecting malformed
//! messages.

use std::{
    future::Future,
    net::SocketAddr,
    time::{Duration, Instant},
};

use bitcoin::{
    Amount, ScriptBuf, Transaction, TxIn, TxOut,
    consensus::encode,
    p2p::{
        Address, Magic, ServiceFlags,
        message::{CommandString, NetworkMessage, RawNetworkMessage},
        message_blockdata::Inventory,
        message_network::VersionMessage,
    },
};
use futures::FutureExt as _;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

use super::{Config, MAX_PAYLOAD_LEN, send_tx};
use crate::p2p::send_to_first_reachable;

fn config() -> Config {
    Config {
        block_height: 0,
        magic: Magic::REGTEST,
        send_timeout: Duration::from_secs(5),
        flush_timeout: Duration::from_secs(5),
    }
}

fn tx() -> Transaction {
    Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::absolute::LockTime::ZERO,
        input: vec![TxIn::default()],
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::new(),
        }],
    }
}

fn frame(message: NetworkMessage) -> Vec<u8> {
    encode::serialize(&RawNetworkMessage::new(Magic::REGTEST, message))
}

/// The node side of a connection, decoding with rust-bitcoin independently
/// of the code under test
struct Node(TcpStream);

impl Node {
    async fn recv(&mut self) -> NetworkMessage {
        let mut message = vec![0; 24];
        self.0.read_exact(&mut message).await.unwrap();
        let payload_len = u32::from_le_bytes(message[16..20].try_into().unwrap()) as usize;
        message.resize(24 + payload_len, 0);
        self.0.read_exact(&mut message[24..]).await.unwrap();
        encode::deserialize::<RawNetworkMessage>(&message)
            .unwrap()
            .into_payload()
    }

    async fn send(&mut self, frame: Vec<u8>) {
        self.0.write_all(&frame).await.unwrap();
    }

    async fn expect_closed(&mut self) {
        while let Ok(1..) = self.0.read(&mut [0; 1024]).await {}
    }

    /// Handshake like Bitcoin Core, including feature negotiation we ignore,
    /// then take the announcement
    async fn handshake(&mut self) {
        assert!(matches!(self.recv().await, NetworkMessage::Version(_)));
        let addr = Address::new(&([0, 0, 0, 0], 0).into(), ServiceFlags::NONE);
        let services = ServiceFlags::NETWORK | ServiceFlags::WITNESS;
        let version = VersionMessage {
            relay: true,
            ..VersionMessage::new(services, 0, addr.clone(), addr, 0, String::new(), 0)
        };
        self.send(frame(NetworkMessage::Version(version))).await;
        self.send(frame(NetworkMessage::Unknown {
            command: CommandString::try_from_static("sendtxrcncl").unwrap(),
            payload: vec![0; 12],
        }))
        .await;
        self.send(frame(NetworkMessage::Verack)).await;
        assert_eq!(self.recv().await, NetworkMessage::Verack);
        let inv = Inventory::Transaction(tx().compute_txid());
        assert_eq!(self.recv().await, NetworkMessage::Inv(vec![inv]));
    }

    async fn request_tx(&mut self) {
        let inv = Inventory::WitnessTransaction(tx().compute_txid());
        self.send(frame(NetworkMessage::GetData(vec![inv]))).await;
        assert_eq!(self.recv().await, NetworkMessage::Tx(tx()));
    }
}

/// A node following `script` on the one connection it accepts
async fn spawn<Fut>(
    script: impl FnOnce(Node) -> Fut + Send + 'static,
) -> (SocketAddr, JoinHandle<()>)
where
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let node = tokio::spawn(async move { script(Node(listener.accept().await.unwrap().0)).await });
    (addr, node)
}

/// Fails on a panic in the node's script, or if the sender left it waiting
async fn join(node: JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(10), node)
        .await
        .expect("node script did not finish")
        .expect("node script panicked");
}

/// Only the pong to the ping sent after the transaction means the node has
/// processed it
#[tokio::test]
async fn returns_once_node_has_processed_tx() {
    const PROCESSING: Duration = Duration::from_millis(300);
    let (addr, node) = spawn(|mut node| async move {
        node.handshake().await;
        node.send(frame(NetworkMessage::Ping(7))).await;
        assert_eq!(node.recv().await, NetworkMessage::Pong(7));
        node.request_tx().await;
        let NetworkMessage::Ping(nonce) = node.recv().await else {
            panic!("expected a ping after the transaction");
        };
        node.send(frame(NetworkMessage::Pong(nonce.wrapping_add(1))))
            .await;
        tokio::time::sleep(PROCESSING).await;
        node.send(frame(NetworkMessage::Pong(nonce))).await;
        node.expect_closed().await;
    })
    .await;
    let start = Instant::now();
    let res = send_tx(addr, &tx(), &config()).await;
    let elapsed = start.elapsed();
    join(node).await;
    res.unwrap();
    assert!(elapsed >= PROCESSING, "returned after {elapsed:?}");
}

#[tokio::test]
async fn skips_address_that_refuses() {
    let refused = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let (addr, node) = spawn(|mut node| async move {
        node.handshake().await;
        node.request_tx().await;
        let NetworkMessage::Ping(nonce) = node.recv().await else {
            panic!("expected a ping after the transaction");
        };
        node.send(frame(NetworkMessage::Pong(nonce))).await;
        node.expect_closed().await;
    })
    .await;
    let sent = send_to_first_reachable(&[refused, addr], &tx(), &config()).await;
    join(node).await;
    assert!(sent.unwrap());
}

/// A node that already has the transaction never asks for it, so a timeout
/// counts as not submitted, without trying further addresses
#[tokio::test]
async fn timeout_is_not_submitted_and_stops() {
    let config = Config {
        send_timeout: Duration::from_millis(500),
        ..config()
    };
    let (addr, node) = spawn(|mut node| async move {
        node.handshake().await;
        node.expect_closed().await;
    })
    .await;
    let untried = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sent =
        send_to_first_reachable(&[addr, untried.local_addr().unwrap()], &tx(), &config).await;
    join(node).await;
    assert!(!sent.unwrap());
    assert!(
        untried.accept().now_or_never().is_none(),
        "tried the next address"
    );
}

type Corrupt = fn(&mut [u8]);

/// Rejected from the frame alone, so an oversized message is refused without
/// waiting for a payload that never arrives
#[tokio::test]
async fn rejects_malformed_message() {
    let cases: [(&str, Corrupt); 3] = [
        ("network magic", |frame| frame[0] ^= 0xff),
        ("bad checksum", |frame| frame[20] ^= 0xff),
        ("exceeds", |frame| {
            frame[16..20].copy_from_slice(&(MAX_PAYLOAD_LEN + 1).to_le_bytes())
        }),
    ];
    for (needle, corrupt) in cases {
        let (addr, node) = spawn(move |mut node| async move {
            node.recv().await;
            let mut verack = frame(NetworkMessage::Verack);
            corrupt(&mut verack);
            node.send(verack).await;
            node.expect_closed().await;
        })
        .await;
        let res = send_tx(addr, &tx(), &config()).await;
        join(node).await;
        match res {
            Err(super::Error::Protocol(msg)) if msg.contains(needle) => (),
            res => panic!("expected a protocol error containing `{needle}`, got {res:?}"),
        }
    }
}
