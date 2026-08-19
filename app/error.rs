//! App errors

use std::net::SocketAddr;

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
    pub fn is_resyncable(&self) -> bool {
        let (Self::SyncTask(inner) | Self::InitialSync(inner)) = self else {
            return false;
        };
        matches!(
            inner,
            SyncTaskError::SequenceStream(_)
                | SyncTaskError::InitialSyncEnforcer(InitialSyncError::SequenceStream(_))
        )
    }
}

/// Whether a node RPC call failed at the transport layer rather than being
/// answered with an error.
///
/// bitcoind closes idle HTTP connections, so a request that races that close
/// fails with `connection closed before message completed`.
fn is_transport_error(err: &ClientError) -> bool {
    matches!(err, ClientError::Transport(_))
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
        _ => false,
    }
}
