use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use async_broadcast::{InactiveReceiver, Sender as BroadcastSender, broadcast};
use bitcoin::{self, Amount, BlockHash, OutPoint, Txid};
use bitcoin_jsonrpsee::jsonrpsee;
use fallible_iterator::{FallibleIterator, IteratorExt};
use futures::{StreamExt, stream::FusedStream};
use miette::{Diagnostic, IntoDiagnostic};
use nonempty::NonEmpty;
use sneed::{db, env};
use thiserror::Error;
use tokio::sync::watch::Receiver as WatchReceiver;

use crate::{
    proto::{StatusBuilder, ToStatus, mainchain::HeaderSyncProgress},
    types::{
        BlockInfo, BmmCommitment, BmmCommitments, Ctip, Event, HeaderInfo, Sidechain,
        SidechainNumber, SidechainProposalId, TreasuryUtxo, TwoWayPegData,
    },
    validator::main_rest_client::MainRestClient,
};

pub mod cusf_enforcer;
mod dbs;
pub mod main_rest_client;
mod task;

use self::dbs::{Dbs, PendingM6ids};

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
    fn builder(&self) -> StatusBuilder {
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
    fn builder(&self) -> StatusBuilder {
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
    fn builder(&self) -> StatusBuilder {
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
    fn builder(&self) -> StatusBuilder {
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
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::DbGet(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
enum GetHeaderInfoErrorInner {
    #[error(transparent)]
    GetHeaderInfo(#[from] dbs::block_hash_dbs_error::GetHeaderInfo),
    #[error(transparent)]
    ReadTxn(#[from] env::error::ReadTxn),
}

impl ToStatus for GetHeaderInfoErrorInner {
    fn builder(&self) -> StatusBuilder {
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
    fn builder(&self) -> StatusBuilder {
        self.0.builder()
    }
}

#[derive(Debug, Diagnostic, Error)]
enum TryGetHeaderInfosErrorInner {
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
enum GetBlockInfoErrorInner {
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
enum TryGetBlockInfosErrorInner {
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
enum GetBlockInfosErrorInner {
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
enum GetTwoWayPegDataRangeErrorInner {
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
pub enum ListHeadersError {
    #[error(transparent)]
    Iter(#[from] db::error::Iter),
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
    fn builder(&self) -> StatusBuilder {
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
    fn builder(&self) -> StatusBuilder {
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
    fn builder(&self) -> StatusBuilder {
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
    ReadTxn(#[from] env::error::ReadTxn),
}

impl ToStatus for GetPendingWithdrawalsError {
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::DbGet(err) => StatusBuilder::new(err),
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
    fn builder(&self) -> StatusBuilder {
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
    fn builder(&self) -> StatusBuilder {
        match self {
            Self::DbIter(err) => StatusBuilder::new(err),
            Self::ReadTxn(err) => StatusBuilder::new(err),
        }
    }
}

#[derive(Clone)]
pub struct Validator {
    dbs: Dbs,
    events_rx: InactiveReceiver<Event>,
    events_tx: BroadcastSender<Event>,
    header_sync_progress_rx: Arc<parking_lot::RwLock<Option<WatchReceiver<HeaderSyncProgress>>>>,
    mainchain_client: jsonrpsee::http_client::HttpClient,
    mainchain_rest_client: MainRestClient,
    network: bitcoin::Network,
}

impl Validator {
    pub fn new(
        mainchain_client: jsonrpsee::http_client::HttpClient,
        mainchain_rest_client: MainRestClient,
        data_dir: &Path,
        network: bitcoin::Network,
    ) -> Result<Self, InitError> {
        const EVENTS_CHANNEL_CAPACITY: usize = 256;

        let (events_tx, mut events_rx) = broadcast(EVENTS_CHANNEL_CAPACITY);
        events_rx.set_await_active(false);
        events_rx.set_overflow(true);

        let dbs = Dbs::new(data_dir, network)?;
        Ok(Self {
            dbs,
            events_rx: events_rx.deactivate(),
            events_tx,
            header_sync_progress_rx: Arc::new(parking_lot::RwLock::new(None)),
            mainchain_client,
            mainchain_rest_client,
            network,
        })
    }

    pub fn network(&self) -> bitcoin::Network {
        self.network
    }

    pub fn subscribe_events(
        &self,
    ) -> impl FusedStream<Item = Result<Event, EventsStreamError>> + use<> {
        futures::stream::try_unfold(self.events_rx.activate_cloned(), |mut receiver| async {
            match receiver.recv_direct().await {
                Ok(event) => Ok(Some((event, receiver))),
                Err(async_broadcast::RecvError::Closed) => Ok(None),
                Err(async_broadcast::RecvError::Overflowed(_)) => Err(EventsStreamError::Overflow),
            }
        })
        .fuse()
    }

    /// Returns `None` if there is not a header sync in progress
    pub fn subscribe_header_sync_progress(&self) -> Option<WatchReceiver<HeaderSyncProgress>> {
        self.header_sync_progress_rx.read().clone()
    }

    /// Get (possibly unactivated) sidechains
    pub fn get_sidechains(
        &self,
    ) -> Result<Vec<(SidechainProposalId, Sidechain)>, GetSidechainsError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self
            .dbs
            .proposal_id_to_sidechain
            .iter(&rotxn)
            .map_err(db::error::Iter::from)?
            .collect()
            .map_err(db::error::Iter::from)?;
        Ok(res)
    }

    pub fn get_active_sidechains(&self) -> Result<Vec<Sidechain>, GetSidechainsError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self
            .dbs
            .active_sidechains
            .sidechain()
            .iter(&rotxn)
            .map_err(db::error::Iter::from)?
            .map(|(_sidechain_number, sidechain)| {
                assert!(sidechain.status.activation_height.is_some());
                Ok(sidechain)
            })
            .collect()
            .map_err(db::error::Iter::from)?;
        Ok(res)
    }

    pub fn get_ctip_sequence_number(
        &self,
        sidechain_number: SidechainNumber,
    ) -> Result<Option<u64>, GetCtipSequenceNumberError> {
        let rotxn = self.dbs.read_txn()?;
        let treasury_utxo_count = self
            .dbs
            .active_sidechains
            .treasury_utxo_count
            .try_get(&rotxn, &sidechain_number)?;
        // Sequence numbers begin at 0, so the total number of treasury utxos in the database
        // gives us the *next* sequence number.
        // In order to get the current sequence number we decrement it by one.
        let sequence_number =
            treasury_utxo_count.map(|treasury_utxo_count| treasury_utxo_count - 1);
        Ok(sequence_number)
    }

    /// Returns `Some` with the Ctip for the given sidechain number. `None`
    /// if there's no Ctip for the given sidechain number.
    pub fn try_get_ctip(
        &self,
        sidechain_number: SidechainNumber,
    ) -> Result<Option<Ctip>, TryGetCtipError> {
        let rotxn = self.dbs.read_txn()?;
        let ctip = self
            .dbs
            .active_sidechains
            .ctip()
            .try_get(&rotxn, &sidechain_number)?;
        Ok(ctip)
    }

    /// Returns the Ctip for the specified sidechain, or an error
    /// if there is no Ctip.
    pub fn get_ctip(&self, sidechain_number: SidechainNumber) -> Result<Ctip, miette::Report> {
        let rotxn = self.dbs.read_txn().into_diagnostic()?;
        self.dbs
            .active_sidechains
            .ctip()
            .get(&rotxn, &sidechain_number)
            .into_diagnostic()
    }

    /// Returns Ctips for each active sidechain with a ctip
    pub fn get_ctips(&self) -> Result<HashMap<SidechainNumber, Ctip>, GetCtipsError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self
            .dbs
            .active_sidechains
            .ctip()
            .iter(&rotxn)
            .map_err(db::error::Iter::from)?
            .map_err(db::error::Iter::from)
            .collect()?;
        Ok(res)
    }

    /// Returns the sidechain number, value, and sequence for a Ctip outpoint,
    /// if it exists
    pub fn try_get_ctip_value_seq(
        &self,
        outpoint: &OutPoint,
    ) -> Result<Option<(SidechainNumber, Amount, u64)>, TryGetCtipValueSeqError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self
            .dbs
            .active_sidechains
            .ctip_outpoint_to_value_seq()
            .try_get(&rotxn, outpoint)?;
        Ok(res)
    }

    /// Get treasury UTXO by sequence number
    pub fn get_treasury_utxo(
        &self,
        sidechain_number: SidechainNumber,
        sequence: u64,
    ) -> Result<TreasuryUtxo, GetTreasuryUtxoError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self
            .dbs
            .active_sidechains
            .slot_sequence_to_treasury_utxo()
            .get(&rotxn, &(sidechain_number, sequence))?;
        Ok(res)
    }

    pub fn get_header_info(
        &self,
        block_hash: &BlockHash,
    ) -> Result<HeaderInfo, GetHeaderInfoError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self.dbs.block_hashes.get_header_info(&rotxn, block_hash)?;
        Ok(res)
    }

    /// Get header infos for the specified block hash, and up to max_ancestors
    /// ancestors.
    /// Returns header infos newest-first.
    pub fn try_get_header_infos(
        &self,
        block_hash: &BlockHash,
        max_ancestors: usize,
    ) -> Result<Option<NonEmpty<HeaderInfo>>, TryGetHeaderInfosError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self
            .dbs
            .block_hashes
            .try_get_header_infos(&rotxn, block_hash, max_ancestors)?;
        Ok(res)
    }

    pub fn get_block_info(&self, block_hash: &BlockHash) -> Result<BlockInfo, GetBlockInfoError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self.dbs.block_hashes.get_block_info(&rotxn, block_hash)?;
        Ok(res)
    }

    /// Get block infos for the specified block hash, and up to max_ancestors
    /// ancestors.
    /// Returns block infos newest-first.
    pub fn try_get_block_infos(
        &self,
        block_hash: &BlockHash,
        max_ancestors: usize,
    ) -> Result<Option<NonEmpty<(HeaderInfo, BlockInfo)>>, TryGetBlockInfosError> {
        let rotxn = self.dbs.read_txn()?;
        let Some(header_infos) =
            self.dbs
                .block_hashes
                .try_get_header_infos(&rotxn, block_hash, max_ancestors)?
        else {
            return Ok(None);
        };
        let Some(info) = self
            .dbs
            .block_hashes
            .try_get_block_info(&rotxn, block_hash)?
        else {
            return Ok(None);
        };
        let mut res = NonEmpty::new((header_infos.head, info));
        for header_info in header_infos.tail {
            if let Some(info) = self
                .dbs
                .block_hashes
                .try_get_block_info(&rotxn, &header_info.block_hash)?
            {
                res.push((header_info, info));
            } else {
                break;
            }
        }
        Ok(Some(res))
    }

    pub fn get_block_infos(
        &self,
        block_hash: &BlockHash,
        max_ancestors: usize,
    ) -> Result<NonEmpty<(HeaderInfo, BlockInfo)>, GetBlockInfosError> {
        match self.try_get_block_infos(block_hash, max_ancestors)? {
            Some(res) => Ok(res),
            None => Err(GetBlockInfosErrorInner::MissingHeaderBlock(*block_hash).into()),
        }
    }

    // Lists known block heights and their corresponding header hashes in ascending order.
    pub fn list_headers(
        &self,
        start_height: u32,
    ) -> Result<Vec<(u32, BlockHash)>, ListHeadersError> {
        let rotxn = self.dbs.read_txn()?;
        let mut res: Vec<(u32, BlockHash)> = self
            .dbs
            .block_hashes
            .height()
            .iter(&rotxn)
            .map_err(db::error::Iter::from)?
            .filter_map(|(block_hash, height)| {
                if height >= start_height {
                    Ok(Some((height, block_hash)))
                } else {
                    Ok(None)
                }
            })
            .collect()
            .map_err(db::error::Iter::from)?;

        res.sort_by(|(first_height, _), (second_height, _)| first_height.cmp(second_height));

        debug_assert!(
            res.clone()
                .is_sorted_by(|(first_height, _), (second_height, _)| {
                    first_height < second_height
                })
        );
        Ok(res)
    }

    /// Get the mainchain tip. Returns `None` if not synced
    pub fn try_get_mainchain_tip(&self) -> Result<Option<BlockHash>, TryGetMainchainTipError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self.dbs.current_chain_tip.try_get(&rotxn, &())?;
        Ok(res)
    }

    /// Get the mainchain tip. Returns an error if not synced
    pub fn get_mainchain_tip(&self) -> Result<BlockHash, GetMainchainTipError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self.dbs.current_chain_tip.get(&rotxn, &())?;
        Ok(res)
    }

    /// Get the mainchain tip height. Returns `None` if not synced
    pub fn try_get_block_height(&self) -> Result<Option<u32>, TryGetMainchainTipHeightError> {
        let rotxn = self.dbs.read_txn()?;
        let Some(tip) = self.dbs.current_chain_tip.try_get(&rotxn, &())? else {
            return Ok(None);
        };
        let height = self.dbs.block_hashes.height().get(&rotxn, &tip)?;
        Ok(Some(height))
    }

    pub fn get_two_way_peg_data(
        &self,
        start_block: Option<BlockHash>,
        end_block: BlockHash,
    ) -> Result<Vec<TwoWayPegData>, GetTwoWayPegDataRangeError> {
        let rotxn = self.dbs.read_txn()?;
        let res =
            self.dbs
                .block_hashes
                .get_two_way_peg_data_range(&rotxn, start_block, end_block)?;
        Ok(res)
    }

    /// Get BMM commitments for the specified block hash, and up to
    /// max_ancestors ancestors.
    /// The returned vector will be empty if BMM commitments for the specified
    /// block hash were not found.
    /// Returns BMM commitments newest-first.
    pub fn try_get_bmm_commitments(
        &self,
        block_hash: &BlockHash,
        max_ancestors: usize,
    ) -> Result<Vec<BmmCommitments>, TryGetBmmCommitmentsError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self
            .dbs
            .block_hashes
            .ancestor_headers(&rotxn, *block_hash)
            .take(max_ancestors + 1)
            .map(|(block_hash, _)| {
                self.dbs
                    .block_hashes
                    .bmm_commitments()
                    .try_get(&rotxn, &block_hash)
            })
            .take_while(|bmm_commitments| Ok(bmm_commitments.is_some()))
            .flat_map(|bmm_commitments| {
                Ok(bmm_commitments
                    .into_iter()
                    .map(Ok)
                    .transpose_into_fallible())
            })
            .collect()?;
        Ok(res)
    }

    pub fn get_pending_withdrawals(
        &self,
        sidechain_number: &SidechainNumber,
    ) -> Result<PendingM6ids, GetPendingWithdrawalsError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self
            .dbs
            .active_sidechains
            .pending_m6ids()
            .get(&rotxn, sidechain_number)?;
        Ok(res)
    }

    pub fn get_seen_bmm_requests_for_parent_block(
        &self,
        parent_block_hash: BlockHash,
    ) -> Result<
        HashMap<SidechainNumber, HashMap<BmmCommitment, HashSet<Txid>>>,
        GetSeenBmmRequestsForParentBlockError,
    > {
        let rotxn = self.dbs.read_txn()?;
        let res = self
            .dbs
            .block_hashes
            .get_seen_bmm_requests_for_parent_block(&rotxn, parent_block_hash)?;
        Ok(res)
    }

    /*
    pub fn get_deposits(&self, sidechain_number: u8) -> Result<Vec<Deposit>> {
        let txn = self.env.read_txn().into_diagnostic()?;
        let treasury_utxos_range = self
            .sidechain_number_sequence_number_to_treasury_utxo
            .range(&txn, &((sidechain_number, 0)..(sidechain_number, u64::MAX)))
            .into_diagnostic()?;
        let mut deposits = vec![];
        for item in treasury_utxos_range {
            let ((_, sequence_number), treasury_utxo) = item.into_diagnostic()?;
            if treasury_utxo.total_value > treasury_utxo.previous_total_value
                && treasury_utxo.address.is_some()
            {
                let deposit = Deposit {
                    sequence_number,
                    address: treasury_utxo.address.unwrap(),
                    value: treasury_utxo.total_value - treasury_utxo.previous_total_value,
                };
                deposits.push(deposit);
            }
        }
        Ok(deposits)
    }
    */

    /*
    pub fn get_accepted_bmm_hashes(&self) -> Result<Vec<(u32, Vec<[u8; 32]>)>> {
        let mut block_height_accepted_bmm_hashes = vec![];
        let txn = self.env.read_txn().into_diagnostic()?;
        for item in self
            .block_height_to_accepted_bmm_block_hashes
            .iter(&txn)
            .into_diagnostic()?
        {
            let (block_height, accepted_bmm_hashes) = item.into_diagnostic()?;
            block_height_accepted_bmm_hashes.push((block_height, accepted_bmm_hashes.to_vec()));
        }
        Ok(block_height_accepted_bmm_hashes)
    }
    */
}
