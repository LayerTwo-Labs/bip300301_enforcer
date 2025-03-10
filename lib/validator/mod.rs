use std::{collections::HashMap, path::Path};

use async_broadcast::{broadcast, InactiveReceiver, Sender};
use bip300301::jsonrpsee;
use bitcoin::{self, hashes::Hash, Amount, BlockHash, OutPoint};
use fallible_iterator::{FallibleIterator, IteratorExt};
use futures::{stream::FusedStream, StreamExt};
use miette::{Diagnostic, IntoDiagnostic};
use nonempty::NonEmpty;
use thiserror::Error;

use crate::types::{
    BlockInfo, BmmCommitments, Ctip, Event, HeaderInfo, Sidechain, SidechainNumber,
    SidechainProposalId, TreasuryUtxo, TwoWayPegData,
};

pub mod cusf_enforcer;
mod dbs;
mod task;

use dbs::{db_error, CreateDbsError, Dbs, PendingM6ids};
pub use task::error::ValidateTransaction as ValidateTransactionError;

#[derive(Debug, Error)]
pub enum InitError {
    #[error(transparent)]
    CreateDbs(#[from] CreateDbsError),
    #[error("JSON RPC error (`{method}`)")]
    JsonRpc {
        method: String,
        source: jsonrpsee::core::ClientError,
    },
}

#[derive(Debug, Error)]
enum GetBlockInfoErrorInner {
    #[error(transparent)]
    ReadTxn(#[from] dbs::ReadTxnError),
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

#[derive(Debug, Diagnostic, Error)]
enum GetHeaderInfoErrorInner {
    #[error(transparent)]
    ReadTxn(#[from] dbs::ReadTxnError),
    #[error(transparent)]
    GetHeaderInfo(#[from] dbs::block_hash_dbs_error::GetHeaderInfo),
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

#[derive(Debug, Diagnostic, Error)]
enum GetHeaderInfosErrorInner {
    #[error(transparent)]
    ReadTxn(#[from] dbs::ReadTxnError),
    #[error(transparent)]
    GetHeaderInfo(#[from] dbs::block_hash_dbs_error::GetHeaderInfo),
    #[error(transparent)]
    TryGetHeaderInfo(#[from] dbs::block_hash_dbs_error::TryGetHeaderInfo),
}

#[derive(Debug, Diagnostic, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct GetHeaderInfosError(GetHeaderInfosErrorInner);

impl<T> From<T> for GetHeaderInfosError
where
    GetHeaderInfosErrorInner: From<T>,
{
    fn from(err: T) -> Self {
        Self(err.into())
    }
}

#[derive(Debug, Error)]
enum GetTwoWayPegDataRangeErrorInner {
    #[error(transparent)]
    ReadTxn(#[from] dbs::ReadTxnError),
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
    ReadTxn(#[from] dbs::ReadTxnError),
    #[error(transparent)]
    Iter(#[from] db_error::Iter),
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryGetMainchainTipError {
    #[error(transparent)]
    ReadTxn(#[from] dbs::ReadTxnError),
    #[error(transparent)]
    DbTryGet(#[from] dbs::db_error::TryGet),
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetMainchainTipError {
    #[error(transparent)]
    ReadTxn(#[from] dbs::ReadTxnError),
    #[error(transparent)]
    DbGet(#[from] dbs::db_error::Get),
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryGetMainchainTipHeightError {
    #[error(transparent)]
    ReadTxn(#[from] dbs::ReadTxnError),
    #[error(transparent)]
    DbGet(#[from] dbs::db_error::Get),
    #[error(transparent)]
    DbTryGet(#[from] dbs::db_error::TryGet),
}

#[derive(Debug, Error)]
pub enum TryGetBmmCommitmentsError {
    #[error(transparent)]
    ReadTxn(#[from] dbs::ReadTxnError),
    #[error(transparent)]
    DbTryGet(#[from] dbs::db_error::TryGet),
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetPendingWithdrawalsError {
    #[error(transparent)]
    ReadTxn(#[from] dbs::ReadTxnError),
    #[error(transparent)]
    DbGet(#[from] dbs::db_error::Get),
}

#[derive(Debug, Diagnostic, Error)]
pub enum EventsStreamError {
    #[error("Events stream closed due to overflow")]
    Overflow,
}

#[derive(Debug, Diagnostic, Error)]
pub enum GetSidechainsError {
    #[error(transparent)]
    DbIter(#[from] db_error::Iter),
    #[error(transparent)]
    ReadTxn(#[from] dbs::ReadTxnError),
}

#[derive(Clone)]
pub struct Validator {
    dbs: Dbs,
    events_rx: InactiveReceiver<Event>,
    events_tx: Sender<Event>,
    mainchain_client: jsonrpsee::http_client::HttpClient,
    network: bitcoin::Network,
}

impl Validator {
    pub fn new(
        mainchain_client: jsonrpsee::http_client::HttpClient,
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
            mainchain_client,
            network,
        })
    }

    pub fn network(&self) -> bitcoin::Network {
        self.network
    }

    pub fn subscribe_events(&self) -> impl FusedStream<Item = Result<Event, EventsStreamError>> {
        futures::stream::try_unfold(self.events_rx.activate_cloned(), |mut receiver| async {
            match receiver.recv_direct().await {
                Ok(event) => Ok(Some((event, receiver))),
                Err(async_broadcast::RecvError::Closed) => Ok(None),
                Err(async_broadcast::RecvError::Overflowed(_)) => Err(EventsStreamError::Overflow),
            }
        })
        .fuse()
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
            .map_err(db_error::Iter::from)?
            .collect()
            .map_err(db_error::Iter::from)?;
        Ok(res)
    }

    pub fn get_active_sidechains(&self) -> Result<Vec<Sidechain>, GetSidechainsError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self
            .dbs
            .active_sidechains
            .sidechain()
            .iter(&rotxn)
            .map_err(db_error::Iter::from)?
            .map(|(_sidechain_number, sidechain)| {
                assert!(sidechain.status.activation_height.is_some());
                Ok(sidechain)
            })
            .collect()
            .map_err(db_error::Iter::from)?;
        Ok(res)
    }

    pub fn get_ctip_sequence_number(
        &self,
        sidechain_number: SidechainNumber,
    ) -> Result<Option<u64>, miette::Report> {
        let rotxn = self.dbs.read_txn().into_diagnostic()?;
        let treasury_utxo_count = self
            .dbs
            .active_sidechains
            .treasury_utxo_count
            .try_get(&rotxn, &sidechain_number)
            .into_diagnostic()?;
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
    ) -> Result<Option<Ctip>, miette::Report> {
        let rotxn = self.dbs.read_txn().into_diagnostic()?;
        let ctip = self
            .dbs
            .active_sidechains
            .ctip()
            .try_get(&rotxn, &sidechain_number)
            .into_diagnostic()?;
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
    pub fn get_ctips(&self) -> Result<HashMap<SidechainNumber, Ctip>, miette::Report> {
        let rotxn = self.dbs.read_txn().into_diagnostic()?;
        let res = self
            .dbs
            .active_sidechains
            .ctip()
            .iter(&rotxn)
            .into_diagnostic()?
            .collect()
            .into_diagnostic()?;
        Ok(res)
    }

    /// Returns the sidechain number, value, and sequence for a Ctip outpoint,
    /// if it exists
    pub fn try_get_ctip_value_seq(
        &self,
        outpoint: &OutPoint,
    ) -> Result<Option<(SidechainNumber, Amount, u64)>, miette::Report> {
        let rotxn = self.dbs.read_txn().into_diagnostic()?;
        self.dbs
            .active_sidechains
            .ctip_outpoint_to_value_seq()
            .try_get(&rotxn, outpoint)
            .into_diagnostic()
    }

    /// Get treasury UTXO by sequence number
    pub fn get_treasury_utxo(
        &self,
        sidechain_number: SidechainNumber,
        sequence: u64,
    ) -> Result<TreasuryUtxo, miette::Report> {
        let rotxn = self.dbs.read_txn().into_diagnostic()?;
        self.dbs
            .active_sidechains
            .slot_sequence_to_treasury_utxo()
            .get(&rotxn, &(sidechain_number, sequence))
            .into_diagnostic()
    }

    pub fn get_block_info(&self, block_hash: &BlockHash) -> Result<BlockInfo, GetBlockInfoError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self.dbs.block_hashes.get_block_info(&rotxn, block_hash)?;
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
    pub fn get_header_infos(
        &self,
        block_hash: &BlockHash,
        max_ancestors: usize,
    ) -> Result<NonEmpty<HeaderInfo>, GetHeaderInfosError> {
        let rotxn = self.dbs.read_txn()?;
        let mut ancestors = max_ancestors;
        let header_info = self.dbs.block_hashes.get_header_info(&rotxn, block_hash)?;
        let mut ancestor = header_info.prev_block_hash;
        let mut res = NonEmpty::new(header_info);
        while ancestors > 0 && ancestor != BlockHash::all_zeros() {
            let Some(header_info) = self
                .dbs
                .block_hashes
                .try_get_header_info(&rotxn, &ancestor)?
            else {
                break;
            };
            ancestor = header_info.prev_block_hash;
            ancestors -= 1;
            res.push(header_info);
        }
        Ok(res)
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
            .map_err(db_error::Iter::from)?
            .filter_map(|(block_hash, height)| {
                if height >= start_height {
                    Ok(Some((height, block_hash)))
                } else {
                    Ok(None)
                }
            })
            .collect()
            .map_err(db_error::Iter::from)?;

        res.sort_by(|(first_height, _), (second_height, _)| first_height.cmp(second_height));

        debug_assert!(res
            .clone()
            .is_sorted_by(|(first_height, _), (second_height, _)| {
                first_height < second_height
            }));
        Ok(res)
    }

    /// Get the mainchain tip. Returns `None` if not synced
    pub fn try_get_mainchain_tip(&self) -> Result<Option<BlockHash>, TryGetMainchainTipError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self.dbs.current_chain_tip.try_get(&rotxn, &dbs::UnitKey)?;
        Ok(res)
    }

    /// Get the mainchain tip. Returns an error if not synced
    pub fn get_mainchain_tip(&self) -> Result<BlockHash, GetMainchainTipError> {
        let rotxn = self.dbs.read_txn()?;
        let res = self.dbs.current_chain_tip.get(&rotxn, &dbs::UnitKey)?;
        Ok(res)
    }

    /// Get the mainchain tip height. Returns `None` if not synced
    pub fn try_get_block_height(&self) -> Result<Option<u32>, TryGetMainchainTipHeightError> {
        let rotxn = self.dbs.read_txn()?;
        let Some(tip) = self.dbs.current_chain_tip.try_get(&rotxn, &dbs::UnitKey)? else {
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
