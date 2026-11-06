use bitcoin::BlockHash;
use miette::Diagnostic;
use sneed::{db::error as db, env::error as env};
use thiserror::Error;

use crate::{
    proto::{StatusBuilder, ToStatus},
    validator::dbs,
};

#[derive(Debug, Error)]
pub enum Init {
    #[error(transparent)]
    CreateDbs(#[from] dbs::CreateDbsError),
    #[error("JSON RPC error (`{method}`)")]
    JsonRpc {
        method: String,
        source: jsonrpsee::core::ClientError,
    },
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetTreasuryUtxoSequenceNumber {
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
    #[error(transparent)]
    TryGet(#[from] db::TryGet),
}

impl ToStatus for GetTreasuryUtxoSequenceNumber {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::ReadTxn(err) => StatusBuilder::new(err),
            Self::TryGet(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryGetTreasuryUtxo {
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
    #[error(transparent)]
    TryGet(#[from] db::TryGet),
}

impl ToStatus for TryGetTreasuryUtxo {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::ReadTxn(err) => StatusBuilder::new(err),
            Self::TryGet(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetTreasuryUtxos {
    #[error(transparent)]
    DbIter(#[from] db::Iter),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

impl ToStatus for GetTreasuryUtxos {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbIter(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryGetTreasuryOutpointValueSeq {
    #[error(transparent)]
    DbTryGet(#[from] db::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

impl ToStatus for TryGetTreasuryOutpointValueSeq {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbTryGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryGetDepositCtipValueSeq {
    #[error(transparent)]
    DbTryGet(#[from] db::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

impl ToStatus for TryGetDepositCtipValueSeq {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbTryGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetTreasuryTxo {
    #[error(transparent)]
    DbGet(#[from] db::Get),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

impl ToStatus for GetTreasuryTxo {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
enum GetHeaderInfoInner {
    #[error(transparent)]
    GetHeaderInfo(#[from] dbs::block_hash_dbs_error::GetHeaderInfo),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

impl ToStatus for GetHeaderInfoInner {
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
pub struct GetHeaderInfo(GetHeaderInfoInner);

impl<T> From<T> for GetHeaderInfo
where
    GetHeaderInfoInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

impl ToStatus for GetHeaderInfo {
    fn builder(&self) -> StatusBuilder<'_> {
        self.0.builder()
    }
}

#[derive(Debug, Diagnostic, Error)]
enum TryGetHeaderInfosInner {
    #[error(transparent)]
    Db(#[from] db::Error),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct TryGetHeaderInfos(TryGetHeaderInfosInner);

impl<T> From<T> for TryGetHeaderInfos
where
    TryGetHeaderInfosInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub(in crate::validator) enum GetBlockInfoInner {
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
    #[error(transparent)]
    GetBlockInfo(#[from] dbs::block_hash_dbs_error::GetBlockInfo),
}

#[derive(Debug, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct GetBlockInfo(GetBlockInfoInner);

impl<T> From<T> for GetBlockInfo
where
    GetBlockInfoInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub(in crate::validator) enum TryGetBlockInfosInner {
    #[error(transparent)]
    Db(#[from] db::Error),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

#[derive(Debug, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct TryGetBlockInfos(TryGetBlockInfosInner);

impl<T> From<T> for TryGetBlockInfos
where
    TryGetBlockInfosInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub(in crate::validator) enum GetBlockInfosInner {
    #[error("Missing header or block: {0}")]
    MissingHeaderBlock(BlockHash),
    #[error(transparent)]
    TryGetBlockInfos(#[from] TryGetBlockInfos),
}

#[derive(Debug, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct GetBlockInfos(GetBlockInfosInner);

impl<T> From<T> for GetBlockInfos
where
    GetBlockInfosInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub(in crate::validator) enum GetTwoWayPegDataRangeInner {
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
    #[error(transparent)]
    GetTwoWayPegDataRange(#[from] dbs::block_hash_dbs_error::GetTwoWayPegDataRange),
}

#[derive(Debug, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct GetTwoWayPegDataRange(GetTwoWayPegDataRangeInner);

impl<T> From<T> for GetTwoWayPegDataRange
where
    GetTwoWayPegDataRangeInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub(in crate::validator) enum ChainDiffInner {
    #[error(transparent)]
    Db(#[from] db::Error),
    #[error(transparent)]
    DbGet(#[from] db::Get),
    #[error(transparent)]
    DbTryGet(#[from] db::TryGet),
    #[error(transparent)]
    LastCommonAncestor(#[from] dbs::block_hash_dbs_error::LastCommonAncestor),
    #[error("missing block info for connected block {0}")]
    MissingBlockInfo(BlockHash),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

#[derive(Debug, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct ChainDiff(ChainDiffInner);

impl<T> From<T> for ChainDiff
where
    ChainDiffInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub enum ListHeaders {
    #[error(transparent)]
    DbGet(#[from] db::Get),
    #[error(transparent)]
    DbTryGet(#[from] db::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryGetMainchainTip {
    #[error(transparent)]
    DbTryGet(#[from] db::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

impl ToStatus for TryGetMainchainTip {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbTryGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetMainchainTip {
    #[error(transparent)]
    DbGet(#[from] db::Get),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

impl ToStatus for GetMainchainTip {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryGetMainchainTipHeight {
    #[error(transparent)]
    DbGet(#[from] db::Get),
    #[error(transparent)]
    DbTryGet(#[from] db::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

impl ToStatus for TryGetMainchainTipHeight {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbGet(err) => StatusBuilder::new(err),
            Self::DbTryGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Error)]
pub enum TryGetBmmCommitments {
    #[error(transparent)]
    DbTryGet(#[from] db::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetPendingWithdrawals {
    #[error(transparent)]
    DbGet(#[from] db::Get),
    #[error(transparent)]
    DbTryGet(#[from] db::TryGet),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

impl ToStatus for GetPendingWithdrawals {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbGet(err) => StatusBuilder::new(err),
            Self::DbTryGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetSeenBmmRequestsForParentBlock {
    #[error(transparent)]
    DbRange(#[from] db::Range),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

#[derive(Debug, Diagnostic, Error)]
pub enum EventsStream {
    #[error("Events stream closed due to overflow")]
    Overflow,
}

impl ToStatus for EventsStream {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::Overflow => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetSidechains {
    #[error(transparent)]
    DbIter(#[from] db::Iter),
    #[error(transparent)]
    ReadTxn(#[from] env::ReadTxn),
}

impl ToStatus for GetSidechains {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DbIter(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}
