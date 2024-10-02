use std::ops::Add;

use bip300301_messages::bitcoin::{hashes::Hash as _, BlockHash, Txid};
use either::Either;
use futures::{
    stream::{self, BoxStream},
    Stream, StreamExt, TryStreamExt as _,
};
use thiserror::Error;
use zeromq::{Socket as _, SocketRecv as _, ZmqError, ZmqMessage};

#[derive(Clone, Copy, Debug)]
pub enum SequenceMessage {
    BlockHashConnected(#[allow(dead_code)] BlockHash, u32),
    BlockHashDisconnected(#[allow(dead_code)] BlockHash, u32),
    /// Tx hash added to mempool
    TxHashAdded {
        #[allow(dead_code)]
        txid: Txid,
        mempool_seq: u64,
        zmq_seq: u32,
    },
    /// Tx hash removed from mempool for non-block inclusion reason
    TxHashRemoved {
        #[allow(dead_code)]
        txid: Txid,
        mempool_seq: u64,
        zmq_seq: u32,
    },
}

impl SequenceMessage {
    fn mempool_seq(&self) -> Option<u64> {
        match self {
            Self::TxHashAdded { mempool_seq, .. } | Self::TxHashRemoved { mempool_seq, .. } => {
                Some(*mempool_seq)
            }
            Self::BlockHashConnected(_, _) | Self::BlockHashDisconnected(_, _) => None,
        }
    }

    fn zmq_seq(&self) -> u32 {
        match self {
            Self::TxHashAdded { zmq_seq, .. }
            | Self::TxHashRemoved { zmq_seq, .. }
            | Self::BlockHashConnected(_, zmq_seq)
            | Self::BlockHashDisconnected(_, zmq_seq) => *zmq_seq,
        }
    }
}

#[derive(Debug, Error)]
pub enum DeserializeSequenceMessageError {
    #[error("Missing hash (frame 1 bytes at index [0-31])")]
    MissingHash,
    #[error("Missing mempool sequence (frame 1 bytes at index [#33 - #40])")]
    MissingMempoolSequence,
    #[error("Missing message type (frame 1 index 32)")]
    MissingMessageType,
    #[error("Missing `sequence` prefix (frame 0 first 8 bytes)")]
    MissingPrefix,
    #[error("Missing ZMQ sequence (frame 2 first 4 bytes)")]
    MissingZmqSequence,
    #[error("Unknown message type: {0:x}")]
    UnknownMessageType(u8),
}

impl TryFrom<ZmqMessage> for SequenceMessage {
    type Error = DeserializeSequenceMessageError;

    fn try_from(msg: ZmqMessage) -> Result<Self, Self::Error> {
        let msgs = &msg.into_vec();
        let Some(b"sequence") = msgs.first().map(|msg| &**msg) else {
            return Err(Self::Error::MissingPrefix);
        };
        let Some((hash, rest)) = msgs.get(1).and_then(|msg| msg.split_first_chunk()) else {
            return Err(Self::Error::MissingHash);
        };
        let mut hash = *hash;
        hash.reverse();
        let Some(([message_type], rest)) = rest.split_first_chunk() else {
            return Err(Self::Error::MissingMessageType);
        };
        let Some((zmq_seq, _rest)) = msgs.get(2).and_then(|msg| msg.split_first_chunk()) else {
            return Err(Self::Error::MissingZmqSequence);
        };
        let zmq_seq = u32::from_le_bytes(*zmq_seq);
        let res = match *message_type {
            b'C' => Self::BlockHashConnected(BlockHash::from_byte_array(hash), zmq_seq),
            b'D' => Self::BlockHashDisconnected(BlockHash::from_byte_array(hash), zmq_seq),
            b'A' => {
                let Some((mempool_seq, _rest)) = rest.split_first_chunk() else {
                    return Err(Self::Error::MissingMempoolSequence);
                };
                Self::TxHashAdded {
                    txid: Txid::from_byte_array(hash),
                    mempool_seq: u64::from_le_bytes(*mempool_seq),
                    zmq_seq,
                }
            }
            b'R' => {
                let Some((mempool_seq, _rest)) = rest.split_first_chunk() else {
                    return Err(Self::Error::MissingMempoolSequence);
                };
                SequenceMessage::TxHashRemoved {
                    txid: Txid::from_byte_array(hash),
                    mempool_seq: u64::from_le_bytes(*mempool_seq),
                    zmq_seq,
                }
            }
            message_type => return Err(Self::Error::UnknownMessageType(message_type)),
        };
        Ok(res)
    }
}

#[derive(Debug, Error)]
pub enum SequenceStreamError {
    #[error("Error deserializing message")]
    Deserialize(#[from] DeserializeSequenceMessageError),
    #[error("Missing message with mempool sequence {0}")]
    MissingMempoolSequence(u64),
    #[error("Missing message with zmq sequence {0}")]
    MissingZmqSequence(u32),
    #[error("ZMQ error")]
    Zmq(#[from] ZmqError),
}

pub struct SequenceStream<'a>(BoxStream<'a, Result<SequenceMessage, SequenceStreamError>>);

impl<'a> Stream for SequenceStream<'a> {
    type Item = Result<SequenceMessage, SequenceStreamError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.get_mut().0.poll_next_unpin(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

/// Returns `Left(true)` if the sequence number is equal to the next sequence
/// number, and increments the next sequence number.
/// Returns `Left(true)` if the next sequence number is `None`, and sets the
/// next sequence number to the successor of the sequence number.
/// Returns `Left(false)` if the next sequence number is `1` greater than the
/// sequence number (ignore duplicate messages).
/// Otherwise, returns `Right(next_seq)`.
fn check_seq_number<Seq>(next_seq: &mut Option<Seq>, seq: Seq) -> Either<bool, &mut Seq>
where
    Seq: Add<Seq, Output = Seq> + Copy + Eq + num_traits::ConstOne,
{
    match next_seq {
        None => {
            *next_seq = Some(seq + Seq::ONE);
            Either::Left(true)
        }
        Some(next_seq) => {
            if seq + Seq::ONE == *next_seq {
                // Ignore duplicates
                Either::Left(false)
            } else if seq == *next_seq {
                *next_seq = seq + Seq::ONE;
                Either::Left(true)
            } else {
                Either::Right(next_seq)
            }
        }
    }
}

/// See [`check_seq_number`]
fn check_mempool_seq(
    next_mempool_seq: &mut Option<u64>,
    msg: SequenceMessage,
) -> Result<Option<SequenceMessage>, SequenceStreamError> {
    let Some(mempool_seq) = msg.mempool_seq() else {
        return Ok(Some(msg));
    };
    match check_seq_number(next_mempool_seq, mempool_seq) {
        Either::Left(true) => Ok(Some(msg)),
        Either::Left(false) => Ok(None),
        Either::Right(next_mempool_seq) => Err(SequenceStreamError::MissingMempoolSequence(
            *next_mempool_seq,
        )),
    }
}

/// See [`check_seq_number`]
fn check_zmq_seq(
    next_zmq_seq: &mut Option<u32>,
    msg: SequenceMessage,
) -> Result<Option<SequenceMessage>, SequenceStreamError> {
    match check_seq_number(next_zmq_seq, msg.zmq_seq()) {
        Either::Left(true) => Ok(Some(msg)),
        Either::Left(false) => Ok(None),
        Either::Right(next_zmq_seq) => Err(SequenceStreamError::MissingZmqSequence(*next_zmq_seq)),
    }
}

fn check_seq_numbers(
    next_mempool_seq: &mut Option<u64>,
    next_zmq_seq: &mut Option<u32>,
    msg: SequenceMessage,
) -> Result<Option<SequenceMessage>, SequenceStreamError> {
    let Some(msg) = check_mempool_seq(next_mempool_seq, msg)? else {
        return Ok(None);
    };
    check_zmq_seq(next_zmq_seq, msg)
}

#[tracing::instrument]
pub async fn subscribe_sequence<'a>(
    zmq_addr_rawblock: &str,
) -> Result<SequenceStream<'a>, ZmqError> {
    tracing::debug!("Attempting to connect to ZMQ server...");
    let mut socket = zeromq::SubSocket::new();
    socket.connect(zmq_addr_rawblock).await?;
    tracing::info!("Connected to ZMQ server");
    tracing::debug!("Attempting to subscribe to `sequence` topic...");
    socket.subscribe("sequence").await?;
    tracing::info!("Subscribed to `sequence`");
    let inner = stream::try_unfold(socket, |mut socket| async {
        let msg: SequenceMessage = socket.recv().await?.try_into()?;
        Ok(Some((msg, socket)))
    })
    .try_filter_map({
        let mut next_mempool_seq: Option<u64> = None;
        let mut next_zmq_seq: Option<u32> = None;
        move |sequence_msg| {
            let res = check_seq_numbers(&mut next_mempool_seq, &mut next_zmq_seq, sequence_msg);
            futures::future::ready(res)
        }
    })
    .boxed();
    Ok(SequenceStream(inner))
}
