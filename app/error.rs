//! App errors

use std::net::SocketAddr;

use bip300301_enforcer_lib::errors::ErrorChain;
use cusf_enforcer_mempool::{
    cusf_enforcer::{CusfEnforcer, InitialSyncError, TaskError},
    mempool::{InitialSyncMempoolError, SyncTaskError},
};
use jsonrpsee::core::ClientError;
use miette::Diagnostic;
use thiserror::Error;

#[derive(Debug, Diagnostic, Error)]
pub enum ConnectServer {
    #[error("unable to bind ConnectRPC server to `{addr}`: {source}")]
    #[diagnostic(code(connectrpc_server::bind))]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("unable to serve ConnectRPC at `{addr}`: {source}")]
    #[diagnostic(code(connectrpc_server::serve))]
    Serve {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error("unable to build gRPC reflection service: {0}")]
    #[diagnostic(code(connectrpc_server::reflection))]
    Reflection(#[source] connectrpc_reflection::ReflectionError),
}

#[derive(educe::Educe, Diagnostic, Error)]
#[educe(Debug(bound(SyncTaskError<Enforcer>: std::fmt::Debug)))]
pub enum MempoolTask<Enforcer>
where
    Enforcer: cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer + 'static,
{
    #[error("mempool initial sync error")]
    InitialSync(#[source] InitialSyncMempoolError<Enforcer>),
    #[error("mempool task sync error")]
    SyncTask(#[source] SyncTaskError<Enforcer>),
    #[error("failed to check if ZMQ address is reachable: failed to connect to {addr}")]
    ZmqCheck {
        addr: String,
        source: std::io::Error,
    },
    #[error("ZMQ address for mempool sync is not reachable: {zmq_addr_sequence}")]
    ZmqNotReachable { zmq_addr_sequence: String },
}

impl<Enforcer> MempoolTask<Enforcer>
where
    Enforcer: cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer + 'static,
{
    /// Whether a fresh mempool sync can clear this.
    ///
    /// bitcoind drops ZMQ notifications once the publisher's high-water mark
    /// is reached. The sync task rightly refuses to continue across the gap
    /// since a dropped message means its mempool view is stale. Only a re-sync
    /// clears it, and re-syncing is strictly better than exiting the process and
    /// taking the gRPC and block template servers down with it.
    ///
    /// A node RPC transport failure is recoverable on the same terms, as in
    /// [`enforcer_task_is_resyncable`], and the enforcer's own initial sync
    /// can also fail on a transient node error, or on a reorg that lands while
    /// it is syncing.
    pub fn is_resyncable(&self) -> bool {
        let (Self::SyncTask(inner) | Self::InitialSync(inner)) = self else {
            return false;
        };
        match inner {
            SyncTaskError::SequenceStream(_)
            | SyncTaskError::InitialSyncEnforcer(InitialSyncError::SequenceStream(_)) => true,
            SyncTaskError::JsonRpc(err)
            | SyncTaskError::InitialSyncEnforcer(InitialSyncError::JsonRpc(err)) => {
                is_transport_error(err)
            }
            SyncTaskError::InitialSyncEnforcer(InitialSyncError::CusfEnforcer(err)) => {
                is_block_not_found_on_disk(err) || is_block_not_in_active_chain(err)
            }
            // The crate does not export this variant's error type, so its
            // `ClientError` is only reachable through the source chain.
            SyncTaskError::Request(_) => chain_has_transport_error(inner),
            _ => false,
        }
    }
}

// Bitcoin Core responds with 'Block not found on disk' if it knows the header
// but the data cannot be read (yet). Happens regularly while syncing blocks.
fn is_block_not_found_on_disk<E>(err: &E) -> bool
where
    E: std::error::Error,
{
    ErrorChain::new(err)
        .to_string()
        .contains("Block not found on disk")
}

// A reorg while the enforcer is syncing leaves the hash it was walking from off
// the active chain, and the sync rightly refuses to continue against a chain
// the node has abandoned. Only a re-sync clears it, since the new tip is picked
// up at the start of the next sync.
fn is_block_not_in_active_chain<E>(err: &E) -> bool
where
    E: std::error::Error,
{
    ErrorChain::new(err)
        .to_string()
        .contains("Block not in active chain")
}

/// Whether a node RPC call failed at the transport layer rather than being
/// answered with an error.
///
/// bitcoind closes idle HTTP connections, so a request that races that close
/// fails with `connection closed before message completed`.
fn is_transport_error(err: &ClientError) -> bool {
    matches!(err, ClientError::Transport(_))
}

/// [`is_transport_error`] for a [`ClientError`] behind an unnameable type.
fn chain_has_transport_error(err: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(err), |err| err.source()).any(|err| {
        err.downcast_ref::<ClientError>()
            .is_some_and(is_transport_error)
    })
}

/// Whether a fresh sync can clear this. The no-mempool counterpart of
/// [`MempoolTask::is_resyncable`].
pub fn enforcer_task_is_resyncable<Enforcer>(err: &TaskError<Enforcer>) -> bool
where
    Enforcer: CusfEnforcer,
{
    match err {
        TaskError::ZmqSequence(_) | TaskError::InitialSync(InitialSyncError::SequenceStream(_)) => {
            true
        }
        TaskError::JsonRpc(err) | TaskError::InitialSync(InitialSyncError::JsonRpc(err)) => {
            is_transport_error(err)
        }
        TaskError::InitialSync(InitialSyncError::CusfEnforcer(err)) => {
            is_block_not_found_on_disk(err) || is_block_not_in_active_chain(err)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use cusf_enforcer_mempool::{
        cusf_enforcer::{DefaultEnforcer, InitialSyncError},
        mempool::SyncTaskError,
    };
    use jsonrpsee::core::ClientError;

    use super::{MempoolTask, is_block_not_in_active_chain};

    type Task = MempoolTask<DefaultEnforcer>;

    fn transport() -> ClientError {
        ClientError::Transport("connection closed before message completed".into())
    }

    /// Stands in for the enforcer's sync error, which arrives with the message
    /// `validator::task::error::Sync::BlockNotInActiveChain` renders behind a
    /// `#[source]`.
    #[derive(Debug, thiserror::Error)]
    #[error("enforcer sync error")]
    struct EnforcerSync(#[source] BlockNotInActiveChain);

    #[derive(Debug, thiserror::Error)]
    #[error("Block not in active chain: `{0}`")]
    struct BlockNotInActiveChain(&'static str);

    #[test]
    fn a_transport_error_is_resyncable() {
        assert!(
            Task::SyncTask(SyncTaskError::JsonRpc(transport())).is_resyncable(),
            "bitcoind closing an idle connection must not kill a mempool sync"
        );
        assert!(
            Task::InitialSync(SyncTaskError::InitialSyncEnforcer(
                InitialSyncError::JsonRpc(transport())
            ))
            .is_resyncable(),
            "nor during the initial sync"
        );
    }

    #[test]
    fn a_block_not_in_active_chain_error_is_resyncable() {
        let reorged = EnforcerSync(BlockNotInActiveChain(
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f",
        ));
        assert!(
            is_block_not_in_active_chain(&reorged),
            "a reorg during header sync must re-sync against the new tip, not exit"
        );
        assert!(
            !is_block_not_in_active_chain(&transport()),
            "an unrelated failure must not be mistaken for a reorg"
        );
    }

    #[test]
    fn an_answered_rpc_error_is_not_resyncable() {
        assert!(
            !Task::SyncTask(SyncTaskError::JsonRpc(ClientError::RequestTimeout)).is_resyncable(),
            "re-syncing does not clear an error the node itself reported"
        );
    }

    #[test]
    fn an_ended_sequence_stream_is_not_resyncable() {
        assert!(
            !Task::SyncTask(SyncTaskError::SequenceStreamEnded).is_resyncable(),
            "widening to transport errors must not sweep in the rest of the enum"
        );
    }
}
