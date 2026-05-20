//! App errors

use std::net::SocketAddr;

use cusf_enforcer_mempool::mempool::{InitialSyncMempoolError, SyncTaskError};
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
