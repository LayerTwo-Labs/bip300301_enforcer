//! App errors

use std::net::SocketAddr;

use bip300301_enforcer_lib::{validator::Validator, wallet::Wallet};
use cusf_enforcer_mempool::mempool::{InitialSyncMempoolError, SyncTaskError};
use either::Either;
use miette::Diagnostic;
use thiserror::Error;

#[derive(Debug, Diagnostic, Error)]
pub enum GrpcServer {
    #[error("unable to serve gRPC at `{addr}`")]
    #[diagnostic(code(grpc_server::serve))]
    Serve {
        addr: SocketAddr,
        source: tonic::transport::Error,
    },
    #[error("unable to build reflection service")]
    #[diagnostic(code(grpc_server::reflection))]
    Reflection(#[from] tonic_reflection::server::Error),
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

#[derive(educe::Educe, Diagnostic, Error)]
#[educe(Debug(bound(SyncTaskError<Enforcer>: std::fmt::Debug)))]
pub enum Task<Enforcer>
where
    Enforcer: cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer + 'static,
{
    #[error("CUSF enforcer task w/mempool error")]
    Mempool(#[from] MempoolTask<Enforcer>),
    #[error("CUSF enforcer task w/o mempool error")]
    NoMempool(#[from] cusf_enforcer_mempool::cusf_enforcer::TaskError<Enforcer>),
}

#[derive(Debug, Diagnostic, Error)]
pub enum EnforcerTask {
    #[error(transparent)]
    MempoolValidator(#[from] MempoolTask<Validator>),
    #[error(transparent)]
    MempoolWallet(#[from] MempoolTask<Wallet>),
    #[error(transparent)]
    NoMempool(#[from] Task<Either<Validator, Wallet>>),
}
