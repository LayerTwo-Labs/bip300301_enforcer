use cusf_enforcer_mempool::cusf_enforcer::CusfEnforcer;
use miette::Diagnostic;
use thiserror::Error;

use crate::{
    messages::CoinbaseMessagesError,
    proto::{StatusBuilder, ToStatus},
    types::SidechainNumber,
    validator::Validator,
};

#[derive(Debug, Diagnostic, Error)]
pub enum InitDbConnection {
    #[error(transparent)]
    Migration(#[from] rusqlite_migration::Error),
    #[error(transparent)]
    Rusqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Diagnostic, Error)]
enum GetBundleProposalsInner {
    #[error(transparent)]
    DecodeBlindedM6(#[from] crate::types::BlindedM6DecodeError),
    #[error(transparent)]
    GetPendingWithdrawals(#[from] crate::validator::GetPendingWithdrawalsError),
    #[error(transparent)]
    GetSidechains(#[from] crate::validator::GetSidechainsError),
    #[error("rusqlite error")]
    Rusqlite(#[from] rusqlite::Error),
}

impl ToStatus for GetBundleProposalsInner {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::DecodeBlindedM6(err) => err.builder(),
            Self::GetPendingWithdrawals(err) => err.builder(),
            Self::GetSidechains(err) => err.builder(),
            Self::Rusqlite(_) => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
#[error("failed to get bundle proposals")]
#[repr(transparent)]
pub struct GetBundleProposals(#[source] GetBundleProposalsInner);

impl<T> From<T> for GetBundleProposals
where
    GetBundleProposalsInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

impl ToStatus for GetBundleProposals {
    fn builder(&self) -> StatusBuilder<'_> {
        StatusBuilder::with_code(self, self.0.builder())
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GenerateCoinbaseTxouts {
    #[error(transparent)]
    CoinbaseMessages(#[from] crate::messages::CoinbaseMessagesError),
    #[error("transparent")]
    GetBundleProposals(#[from] GetBundleProposals),
    #[error(transparent)]
    GetPendingWithdrawals(#[from] crate::validator::GetPendingWithdrawalsError),
    #[error(transparent)]
    GetSidechains(#[from] crate::validator::GetSidechainsError),
    #[error(transparent)]
    PushBytes(#[from] bitcoin::script::PushBytesError),
    #[error("rusqlite error")]
    Rusqlite(#[from] rusqlite::Error),
}

impl ToStatus for GenerateCoinbaseTxouts {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::CoinbaseMessages(err) => err.builder(),
            Self::GetBundleProposals(err) => err.builder(),
            Self::GetPendingWithdrawals(err) => err.builder(),
            Self::GetSidechains(err) => err.builder(),
            Self::PushBytes(err) => StatusBuilder::new(err),
            Self::Rusqlite(_) => StatusBuilder::new(self),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum GenerateSuffixTxs {
    #[error(transparent)]
    GetBundleProposals(#[from] GetBundleProposals),
    #[error(transparent)]
    M6(#[from] crate::types::AmountUnderflowError),
    #[error("Missing ctip for sidechain {sidechain_id}")]
    MissingCtip { sidechain_id: SidechainNumber },
}

impl ToStatus for GenerateSuffixTxs {
    fn builder(&self) -> StatusBuilder<'_> {
        match self {
            Self::GetBundleProposals(err) => err.builder(),
            Self::M6(err) => err.builder(),
            Self::MissingCtip { .. } => StatusBuilder::new(self),
        }
    }
}

/// `connect_block` for the producer: the validator's error, plus the policy-table
/// maintenance that follows an accepted block. Wallet's `ConnectBlock` wraps this
/// and adds the BDK failures on top.
#[derive(Debug, Diagnostic, Error)]
pub enum ConnectBlock {
    #[error(transparent)]
    Validator(#[from] <Validator as CusfEnforcer>::ConnectBlockError),
    #[error(transparent)]
    GetBlockInfos(#[from] crate::validator::GetBlockInfosError),
    #[error("rusqlite error")]
    Rusqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Diagnostic, Error)]
pub(in crate::block_producer) enum InitialBlockTemplateInner {
    #[error(transparent)]
    CoinbaseMessages(#[from] CoinbaseMessagesError),
    #[error(transparent)]
    GetMainchainTip(#[from] crate::validator::GetMainchainTipError),
    #[error(transparent)]
    GetSeenBmmRequestsForParentBlock(
        #[from] crate::validator::GetSeenBmmRequestsForParentBlockError,
    ),
    #[error(transparent)]
    GenerateCoinbaseTxouts(#[from] GenerateCoinbaseTxouts),
    #[error(transparent)]
    GenerateSuffixTxs(#[from] GenerateSuffixTxs),
    #[error("Failed to read the ACK-all-proposals setting")]
    GetAckAllProposals(#[source] rusqlite::Error),
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct InitialBlockTemplate(InitialBlockTemplateInner);

impl<Err> From<Err> for InitialBlockTemplate
where
    InitialBlockTemplateInner: From<Err>,
{
    fn from(err: Err) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
pub(in crate::block_producer) enum FinalizeBlockTemplateInner {
    #[error(transparent)]
    CoinbaseMessages(#[from] CoinbaseMessagesError),
    #[error("Failed to apply initial block template: {reason}")]
    InitialBlockTemplate { reason: String },
    #[error("Failed to generate coinbase txouts suffix")]
    GenerateSuffixCoinbaseTxouts(#[source] bitcoin::script::PushBytesError),
    #[error(transparent)]
    GenerateSuffixTxs(#[from] GenerateSuffixTxs),
    #[error(transparent)]
    GetCtipsAfter(#[from] crate::validator::cusf_enforcer::GetCtipsAfterError),
    #[error(transparent)]
    GetHeaderInfo(#[from] crate::validator::GetHeaderInfoError),
    #[error(transparent)]
    TryGetMainchainTip(#[from] crate::validator::TryGetMainchainTipError),
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct FinalizeBlockTemplate(FinalizeBlockTemplateInner);

impl<Err> From<Err> for FinalizeBlockTemplate
where
    FinalizeBlockTemplateInner: From<Err>,
{
    fn from(err: Err) -> Self {
        Self(err.into())
    }
}
