use bitcoin::BlockHash;
use bitcoin_jsonrpsee::jsonrpsee;
use miette::Diagnostic;
use sneed::{db, env};
use thiserror::Error;

use crate::{
    proto::{StatusBuilder, ToStatus},
    validator::dbs,
};

#[derive(Debug, Error)]
pub enum InitError {
    #[error(transparent)]
    CreateDbs(#[from] dbs::CreateDbsError),
    #[error("JSON RPC error (`{method}`)")]
    JsonRpc {
        method: String,
        source: jsonrpsee::core::ClientError,
    },
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetCtipSequenceNumberError {
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
    #[error(transparent)]
    TryGet(#[from] db::error::TryGet),
}

impl ToStatus for GetCtipSequenceNumberError {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::ReadTxn(err) => StatusBuilder::new(err),
            Self::TryGet(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryGetCtipError {
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
    #[error(transparent)]
    TryGet(#[from] db::error::TryGet),
}

impl ToStatus for TryGetCtipError {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::ReadTxn(err) => StatusBuilder::new(err),
            Self::TryGet(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetCtipsError {
    #[error(transparent)]
    DbIter(#[from] db::error::Iter),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

impl ToStatus for GetCtipsError {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbIter(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryGetCtipValueSeqError {
    #[error(transparent)]
    DbTryGet(#[from] db::error::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

impl ToStatus for TryGetCtipValueSeqError {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbTryGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetTreasuryUtxoError {
    #[error(transparent)]
    DbGet(#[from] db::error::Get),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

impl ToStatus for GetTreasuryUtxoError {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub(super) enum GetHeaderInfoErrorInner {
    #[error(transparent)]
    GetHeaderInfo(#[from] dbs::block_hash_dbs_error::GetHeaderInfo),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

impl ToStatus for GetHeaderInfoErrorInner {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::GetHeaderInfo(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct GetHeaderInfoError(GetHeaderInfoErrorInner);

impl<T> From<T> for GetHeaderInfoError
where
    GetHeaderInfoErrorInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

impl ToStatus for GetHeaderInfoError {
    fn builder(&self) -> StatusBuilder<'_> {
        self.0.builder()
    }
}

#[derive(Debug, Diagnostic, Error)]
pub(super) enum TryGetHeaderInfosErrorInner {
    #[error(transparent)]
    Db(#[from] db::Error),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct TryGetHeaderInfosError(TryGetHeaderInfosErrorInner);

impl<T> From<T> for TryGetHeaderInfosError
where
    TryGetHeaderInfosErrorInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub(super) enum GetBlockInfoErrorInner {
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
    #[error(transparent)]
    GetBlockInfo(#[from] dbs::block_hash_dbs_error::GetBlockInfo),
}

#[derive(Debug, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct GetBlockInfoError(GetBlockInfoErrorInner);

impl<T> From<T> for GetBlockInfoError
where
    GetBlockInfoErrorInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub(super) enum TryGetBlockInfosErrorInner {
    #[error(transparent)]
    Db(#[from] db::Error),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

#[derive(Debug, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct TryGetBlockInfosError(TryGetBlockInfosErrorInner);

impl<T> From<T> for TryGetBlockInfosError
where
    TryGetBlockInfosErrorInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub(super) enum GetBlockInfosErrorInner {
    #[error("Missing header or block: {0}")]
    MissingHeaderBlock(BlockHash),
    #[error(transparent)]
    TryGetBlockInfos(#[from] TryGetBlockInfosError),
}

#[derive(Debug, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct GetBlockInfosError(GetBlockInfosErrorInner);

impl<T> From<T> for GetBlockInfosError
where
    GetBlockInfosErrorInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub(super) enum GetTwoWayPegDataRangeErrorInner {
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
    #[error(transparent)]
    GetTwoWayPegDataRange(#[from] dbs::block_hash_dbs_error::GetTwoWayPegDataRange),
}

#[derive(Debug, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct GetTwoWayPegDataRangeError(GetTwoWayPegDataRangeErrorInner);

impl<T> From<T> for GetTwoWayPegDataRangeError
where
    GetTwoWayPegDataRangeErrorInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub(super) enum ChainDiffErrorInner {
    #[error(transparent)]
    Db(#[from] db::Error),
    #[error(transparent)]
    DbGet(#[from] db::error::Get),
    #[error(transparent)]
    DbTryGet(#[from] db::error::TryGet),
    #[error(transparent)]
    LastCommonAncestor(#[from] dbs::block_hash_dbs_error::LastCommonAncestor),
    #[error("missing block info for connected block {0}")]
    MissingBlockInfo(BlockHash),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

#[derive(Debug, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct ChainDiffError(ChainDiffErrorInner);

impl<T> From<T> for ChainDiffError
where
    ChainDiffErrorInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub enum ListHeadersError {
    #[error(transparent)]
    DbGet(#[from] db::error::Get),
    #[error(transparent)]
    DbTryGet(#[from] db::error::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryGetMainchainTipError {
    #[error(transparent)]
    DbTryGet(#[from] db::error::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

impl ToStatus for TryGetMainchainTipError {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbTryGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetMainchainTipError {
    #[error(transparent)]
    DbGet(#[from] db::error::Get),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

impl ToStatus for GetMainchainTipError {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryGetMainchainTipHeightError {
    #[error(transparent)]
    DbGet(#[from] db::error::Get),
    #[error(transparent)]
    DbTryGet(#[from] db::error::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

impl ToStatus for TryGetMainchainTipHeightError {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbGet(err) => StatusBuilder::new(err),
            Self::DbTryGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Error)]
pub enum TryGetBmmCommitmentsError {
    #[error(transparent)]
    DbTryGet(#[from] db::error::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetPendingWithdrawalsError {
    #[error(transparent)]
    DbGet(#[from] db::error::Get),
    #[error(transparent)]
    DbTryGet(#[from] db::error::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

impl ToStatus for GetPendingWithdrawalsError {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbGet(err) => StatusBuilder::new(err),
            Self::DbTryGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetSeenBmmRequestsForParentBlockError {
    #[error(transparent)]
    DbRange(#[from] db::error::Range),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

#[derive(Debug, Diagnostic, Error)]
pub enum EventsStreamError {
    #[error("Events stream closed due to overflow")]
    Overflow,
}

impl ToStatus for EventsStreamError {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::Overflow => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetSidechainsError {
    #[error(transparent)]
    DbIter(#[from] db::error::Iter),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

impl ToStatus for GetSidechainsError {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbIter(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}
