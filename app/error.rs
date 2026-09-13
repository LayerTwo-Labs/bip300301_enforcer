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

/// Whether, and on what schedule, re-syncing can clear a failed task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resync {
    /// Re-syncing cannot clear this.
    No,
    /// Re-sync promptly, within the caller's consecutive-resync budget.
    Now,
    /// The node knows the block header but cannot serve the body yet, and
    /// clears that on its own. An AssumeUTXO background sync takes minutes to
    /// hours, so this waits on a slower schedule and outside the burst budget,
    /// which exists to bound a node that is failing outright.
    AfterNodeCatchesUp,
}

impl Resync {
    /// [`Resync::Now`] if `cond` holds, [`Resync::No`] otherwise.
    fn now_if(cond: bool) -> Self {
        if cond { Self::Now } else { Self::No }
    }
}

impl<Enforcer> MempoolTask<Enforcer>
where
    Enforcer: cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer + 'static,
{
    /// Whether, and on what schedule, a fresh mempool sync can clear this.
    ///
    /// bitcoind drops ZMQ notifications once the publisher's high-water mark
    /// is reached. The sync task rightly refuses to continue across the gap
    /// since a dropped message means its mempool view is stale. Only a re-sync
    /// clears it, and re-syncing is strictly better than exiting the process and
    /// taking the gRPC and block template servers down with it.
    ///
    /// A node RPC transport failure is recoverable on the same terms, as in
    /// [`enforcer_task_is_resyncable`], and the enforcer's own initial sync
    /// can also fail on a transient node error.
    pub fn is_resyncable(&self) -> Resync {
        let (Self::SyncTask(inner) | Self::InitialSync(inner)) = self else {
            return Resync::No;
        };
        match inner {
            SyncTaskError::SequenceStream(_)
            | SyncTaskError::InitialSyncEnforcer(InitialSyncError::SequenceStream(_)) => {
                Resync::Now
            }
            SyncTaskError::JsonRpc(err)
            | SyncTaskError::InitialSyncEnforcer(InitialSyncError::JsonRpc(err)) => {
                Resync::now_if(is_transport_error(err))
            }
            SyncTaskError::InitialSyncEnforcer(InitialSyncError::CusfEnforcer(err))
                if is_transient_missing_block_body(err) =>
            {
                Resync::AfterNodeCatchesUp
            }
            // The crate does not export this variant's error type, so its
            // `ClientError` is only reachable through the source chain.
            SyncTaskError::Request(_) => Resync::now_if(chain_has_transport_error(inner)),
            _ => Resync::No,
        }
    }
}

/// Whether the node knows the block's header but cannot serve its body yet.
///
/// Bitcoin Core answers `Block not found on disk` when the data cannot be read
/// (yet), which happens regularly while syncing blocks, and `Block not
/// available (not fully downloaded)` for a block an AssumeUTXO background sync
/// has not reached. Both clear once the node has the body.
///
/// `Block not available (pruned data)` is deliberately not matched: the node
/// discarded that body on purpose and will never serve it, so no amount of
/// re-syncing clears it.
fn is_transient_missing_block_body<E>(err: &E) -> bool
where
    E: std::error::Error,
{
    let msg = ErrorChain::new(err).to_string();
    msg.contains("Block not found on disk")
        || msg.contains("Block not available (not fully downloaded)")
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

/// Whether, and on what schedule, a fresh sync can clear this. The no-mempool
/// counterpart of [`MempoolTask::is_resyncable`].
pub fn enforcer_task_is_resyncable<Enforcer>(err: &TaskError<Enforcer>) -> Resync
where
    Enforcer: CusfEnforcer,
{
    match err {
        TaskError::ZmqSequence(_) | TaskError::InitialSync(InitialSyncError::SequenceStream(_)) => {
            Resync::Now
        }
        TaskError::JsonRpc(err) | TaskError::InitialSync(InitialSyncError::JsonRpc(err)) => {
            Resync::now_if(is_transport_error(err))
        }
        TaskError::InitialSync(InitialSyncError::CusfEnforcer(err))
            if is_transient_missing_block_body(err) =>
        {
            Resync::AfterNodeCatchesUp
        }
        _ => Resync::No,
    }
}

#[cfg(test)]
mod tests {
    use cusf_enforcer_mempool::{
        cusf_enforcer::{DefaultEnforcer, InitialSyncError},
        mempool::SyncTaskError,
    };
    use jsonrpsee::core::ClientError;
    use thiserror::Error;

    use super::{MempoolTask, Resync, is_transient_missing_block_body};

    type Task = MempoolTask<DefaultEnforcer>;

    /// Stands in for the enforcer's own sync error, whose inner type is not
    /// reachable from here. Only its `Display` matters: a node response is
    /// classified by message.
    #[derive(Debug, Error)]
    #[error("{0}")]
    struct NodeResponse(&'static str);

    fn transport() -> ClientError {
        ClientError::Transport("connection closed before message completed".into())
    }

    #[test]
    fn a_transport_error_is_resyncable() {
        assert_eq!(
            Task::SyncTask(SyncTaskError::JsonRpc(transport())).is_resyncable(),
            Resync::Now,
            "bitcoind closing an idle connection must not kill a mempool sync"
        );
        assert_eq!(
            Task::InitialSync(SyncTaskError::InitialSyncEnforcer(
                InitialSyncError::JsonRpc(transport())
            ))
            .is_resyncable(),
            Resync::Now,
            "nor during the initial sync"
        );
    }

    #[test]
    fn an_answered_rpc_error_is_not_resyncable() {
        assert_eq!(
            Task::SyncTask(SyncTaskError::JsonRpc(ClientError::RequestTimeout)).is_resyncable(),
            Resync::No,
            "re-syncing does not clear an error the node itself reported"
        );
    }

    #[test]
    fn an_ended_sequence_stream_is_not_resyncable() {
        assert_eq!(
            Task::SyncTask(SyncTaskError::SequenceStreamEnded).is_resyncable(),
            Resync::No,
            "widening to transport errors must not sweep in the rest of the enum"
        );
    }

    /// Bitcoin Core has more than one way of saying "I know that header, but I
    /// cannot serve the body yet". Matching only the first one exits the
    /// process on the second, which is what an AssumeUTXO node answers for
    /// every block its background chainstate has not validated yet.
    #[test]
    fn a_missing_block_body_is_waited_out() {
        for msg in [
            "Block not found on disk",
            "Block not available (not fully downloaded)",
        ] {
            assert!(
                is_transient_missing_block_body(&NodeResponse(msg)),
                "the node clears `{msg}` on its own, so it must not be fatal"
            );
        }
    }

    #[test]
    fn a_pruned_block_body_is_not_resyncable() {
        assert!(
            !is_transient_missing_block_body(&NodeResponse("Block not available (pruned data)")),
            "a pruned body is gone for good, and retrying for hours would only \
             hide that"
        );
    }
}
