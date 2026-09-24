//! Send a transaction to a single node over the P2P protocol: handshake,
//! announce the transaction with `inv`, and send it once the node asks for it
//! with `getdata`. A node that already has the transaction never asks, so the
//! exchange then ends in a timeout.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bitcoin::{
    Transaction,
    consensus::encode::{self, deserialize_partial},
    hashes::{Hash as _, sha256d},
    p2p::{
        Address, Magic, ServiceFlags,
        message::{CommandString, NetworkMessage, RawNetworkMessage},
        message_blockdata::Inventory,
        message_network::VersionMessage,
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader},
    net::TcpStream,
    time::timeout,
};

const HEADER_LEN: usize = 24;

/// Bitcoin Core's `MAX_PROTOCOL_MESSAGE_LENGTH`
const MAX_PAYLOAD_LEN: u32 = 4_000_000;

/// [BIP 14](https://github.com/bitcoin/bips/blob/master/bip-0014.mediawiki) `/Name:Version/`
const USER_AGENT: &str = concat!("/bip300301_enforcer:", env!("CARGO_PKG_VERSION"), "/");

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub(super) struct Config {
    /// Start height sent in the version message
    pub block_height: i32,
    pub magic: Magic,
    /// Covers the handshake through sending the transaction
    pub send_timeout: Duration,
    /// How long to wait for the node to confirm it has processed the
    /// transaction, before closing the connection regardless
    pub flush_timeout: Duration,
}

impl Config {
    pub fn new(block_height: i32, magic: Magic) -> Self {
        Self {
            block_height,
            magic,
            send_timeout: Duration::from_secs(30),
            flush_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("timed out")]
    Timeout,
}

/// The messages the exchange acts on. Everything else is carried by command
/// name only, so a message this code has no use for can never fail to decode.
#[derive(Debug)]
enum Message {
    Version(VersionMessage),
    Verack,
    GetData(Vec<Inventory>),
    Ping(u64),
    Pong(u64),
    Other(CommandString),
}

fn decode<T: encode::Decodable>(command: &CommandString, payload: &[u8]) -> Result<T, Error> {
    // Trailing bytes are ignored, as newer protocol versions may append fields
    deserialize_partial(payload)
        .map(|(value, _consumed)| value)
        .map_err(|err| Error::Protocol(format!("failed to decode `{command}` message: {err}")))
}

async fn read_message<R>(reader: &mut R, magic: Magic) -> Result<Message, Error>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0; HEADER_LEN];
    reader.read_exact(&mut header).await?;
    let message_magic = Magic::from_bytes(header[0..4].try_into().unwrap());
    if message_magic != magic {
        return Err(Error::Protocol(format!(
            "message with network magic {message_magic}, expected {magic}"
        )));
    }
    let command: CommandString = encode::deserialize(&header[4..16]).map_err(|err| {
        let command = String::from_utf8_lossy(&header[4..16]);
        Error::Protocol(format!("invalid command `{command}`: {err}"))
    })?;
    let payload_len = u32::from_le_bytes(header[16..20].try_into().unwrap());
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(Error::Protocol(format!(
            "`{command}` message of {payload_len} bytes exceeds the {MAX_PAYLOAD_LEN} byte limit"
        )));
    }
    let mut payload = vec![0; payload_len as usize];
    reader.read_exact(&mut payload).await?;
    if sha256d::Hash::hash(&payload)[..4] != header[20..24] {
        return Err(Error::Protocol(format!(
            "`{command}` message has a bad checksum"
        )));
    }
    let message = match command.as_ref() {
        "version" => Message::Version(decode(&command, &payload)?),
        "verack" => Message::Verack,
        "getdata" => Message::GetData(decode(&command, &payload)?),
        "ping" => Message::Ping(decode(&command, &payload)?),
        "pong" => Message::Pong(decode(&command, &payload)?),
        _ => Message::Other(command),
    };
    Ok(message)
}

async fn write_message<W>(
    writer: &mut W,
    magic: Magic,
    message: NetworkMessage,
) -> Result<(), Error>
where
    W: AsyncWrite + Unpin,
{
    let command = message.command();
    writer
        .write_all(&encode::serialize(&RawNetworkMessage::new(magic, message)))
        .await?;
    writer.flush().await?;
    tracing::trace!("sent `{command}` message");
    Ok(())
}

fn version_message(peer: SocketAddr, block_height: i32) -> VersionMessage {
    let services = ServiceFlags::WITNESS;
    let unspecified = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_secs() as i64);
    VersionMessage::new(
        services,
        timestamp,
        Address::new(&peer, services),
        Address::new(&unspecified, services),
        rand::random(),
        USER_AGENT.to_owned(),
        block_height,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    AwaitingVersion,
    AwaitingVerack,
    AwaitingGetData,
    Done,
}

struct Exchange<'a, W> {
    writer: W,
    magic: Magic,
    tx: &'a Transaction,
    state: State,
}

impl<W> Exchange<'_, W>
where
    W: AsyncWrite + Unpin,
{
    fn out_of_order(&self, what: &str) -> Error {
        Error::Protocol(format!(
            "received {what} out of order, in state {:?}",
            self.state
        ))
    }

    async fn send(&mut self, message: NetworkMessage) -> Result<(), Error> {
        write_message(&mut self.writer, self.magic, message).await
    }

    async fn handle(&mut self, message: Message) -> Result<(), Error> {
        match message {
            Message::Version(version) => {
                if self.state != State::AwaitingVersion {
                    return Err(self.out_of_order("version"));
                }
                if !version.relay {
                    return Err(Error::Protocol(
                        "node does not relay transactions".to_owned(),
                    ));
                }
                if !version.services.has(ServiceFlags::WITNESS) {
                    return Err(Error::Protocol("node does not support segwit".to_owned()));
                }
                self.send(NetworkMessage::Verack).await?;
                self.state = State::AwaitingVerack;
            }
            Message::Verack => {
                if self.state != State::AwaitingVerack {
                    return Err(self.out_of_order("verack"));
                }
                let inv = Inventory::Transaction(self.tx.compute_txid());
                self.send(NetworkMessage::Inv(vec![inv])).await?;
                self.state = State::AwaitingGetData;
            }
            Message::GetData(inv) => {
                if self.state != State::AwaitingGetData {
                    return Err(self.out_of_order("getdata"));
                }
                let txid = self.tx.compute_txid();
                let requested = inv.contains(&Inventory::Transaction(txid))
                    || inv.contains(&Inventory::WitnessTransaction(txid));
                if !requested {
                    return Err(Error::Protocol(format!(
                        "getdata does not request transaction {txid}"
                    )));
                }
                self.send(NetworkMessage::Tx(self.tx.clone())).await?;
                self.state = State::Done;
            }
            Message::Ping(nonce) => self.send(NetworkMessage::Pong(nonce)).await?,
            Message::Pong(_) => (),
            // Includes feature negotiation (`wtxidrelay`, `sendaddrv2`, ...)
            // that modern nodes send before `verack`. We opt into none of it.
            Message::Other(command) => tracing::trace!("ignoring `{command}` message"),
        }
        Ok(())
    }
}

/// Handshake, announce the transaction, and send it once requested
async fn exchange<R, W>(
    reader: &mut R,
    exchange: &mut Exchange<'_, W>,
    peer: SocketAddr,
    block_height: i32,
) -> Result<(), Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    exchange
        .send(NetworkMessage::Version(version_message(peer, block_height)))
        .await?;
    while exchange.state != State::Done {
        let message = read_message(reader, exchange.magic).await?;
        exchange.handle(message).await?;
    }
    Ok(())
}

/// A node processes a peer's messages in order, so its pong to a ping sent
/// after the transaction means it has processed the transaction. Closing the
/// connection before then can lose it.
async fn await_processed<R, W>(reader: &mut R, exchange: &mut Exchange<'_, W>) -> Result<(), Error>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let nonce = rand::random();
    exchange.send(NetworkMessage::Ping(nonce)).await?;
    loop {
        match read_message(reader, exchange.magic).await? {
            Message::Pong(pong) if pong == nonce => return Ok(()),
            message => exchange.handle(message).await?,
        }
    }
}

pub(super) async fn send_tx(
    peer: SocketAddr,
    tx: &Transaction,
    config: &Config,
) -> Result<(), Error> {
    let mut stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(peer))
        .await
        .map_err(|_| Error::Timeout)??;
    tracing::info!("connected to node at {peer}");
    let (reader, writer) = stream.split();
    let mut reader = BufReader::new(reader);
    let mut exchange = Exchange {
        writer,
        magic: config.magic,
        tx,
        state: State::AwaitingVersion,
    };
    let res = match timeout(
        config.send_timeout,
        self::exchange(&mut reader, &mut exchange, peer, config.block_height),
    )
    .await
    {
        Ok(res) => res,
        Err(_) => Err(Error::Timeout),
    };
    if res.is_ok() {
        tracing::info!("sent tx successfully");
        match timeout(
            config.flush_timeout,
            await_processed(&mut reader, &mut exchange),
        )
        .await
        {
            Ok(Ok(())) => tracing::trace!("node processed the transaction"),
            Ok(Err(err)) => tracing::warn!(
                "sent tx, but lost the node before it confirmed processing it: {:#}",
                crate::errors::ErrorChain::new(&err)
            ),
            Err(_) => tracing::warn!(
                "sent tx, but the node did not confirm processing it within {:?}",
                config.flush_timeout
            ),
        }
    }
    tracing::trace!("disconnecting");
    // The transaction may already be sent, so a failed shutdown is not fatal
    if let Err(err) = stream.shutdown().await {
        tracing::error!("{err:#}");
    }
    res
}

#[cfg(test)]
mod tests;
