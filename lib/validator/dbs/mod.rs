use std::path::{Path, PathBuf};

use bitcoin::{Amount, OutPoint};
use fallible_iterator::FallibleIterator as _;
use heed::{types::SerdeBincode, EnvOpenOptions, RoTxn};
use miette::Diagnostic;
use ordermap::OrderMap;
use thiserror::Error;

use crate::types::{
    Ctip, M6id, PendingM6idInfo, Sidechain, SidechainNumber, SidechainProposalId, TreasuryUtxo,
};

mod block_hashes;
mod util;

pub use self::{
    block_hashes::{error as block_hash_dbs_error, BlockHashDbs},
    util::{
        db_error, CommitWriteTxnError, Database, Env, NestedWriteTxnError, ReadTxnError,
        RoDatabase, RwTxn, UnitKey, WriteTxnError,
    },
};

#[derive(Debug, Diagnostic, Error)]
pub enum PutCtipError {
    #[error(transparent)]
    Put(#[from] db_error::Put),
    #[error(transparent)]
    TryGet(#[from] db_error::TryGet),
}

#[derive(Debug, Diagnostic, Error)]
pub enum PutActiveSidechainError {
    #[error(transparent)]
    Put(#[from] db_error::Put),
    #[error(transparent)]
    TryGet(#[from] db_error::TryGet),
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryWithPendingWithdrawalsError {
    #[error(transparent)]
    Put(#[from] db_error::Put),
    #[error(transparent)]
    TryGet(#[from] db_error::TryGet),
}

#[derive(Debug, Diagnostic, Error)]
pub enum TryUpvotePendingWithdrawalError {
    #[error(transparent)]
    Put(#[from] db_error::Put),
    #[error(transparent)]
    TryGet(#[from] db_error::TryGet),
}

#[derive(Debug, Diagnostic, Error)]
pub enum RetainPendingWithdrawalsError {
    #[error(transparent)]
    Iter(#[from] db_error::Iter),
    #[error(transparent)]
    Put(#[from] db_error::Put),
}

pub type PendingM6ids = OrderMap<M6id, PendingM6idInfo>;

/// These DBs should all contain exacty the same keys.
#[derive(Clone)]
pub(super) struct ActiveSidechainDbs {
    ctip: Database<SerdeBincode<SidechainNumber>, SerdeBincode<Ctip>>,
    /// MUST contain ALL keys/values that `ctip` has ever contained.
    /// Associates Ctip outpoints with their value and sequence number
    ctip_outpoint_to_value_seq:
        Database<SerdeBincode<OutPoint>, SerdeBincode<(SidechainNumber, Amount, u64)>>,
    // ALL active sidechains MUST exist as keys
    pending_m6ids: Database<SerdeBincode<SidechainNumber>, SerdeBincode<PendingM6ids>>,
    // ALL active sidechains MUST exist as keys
    sidechain: Database<SerdeBincode<SidechainNumber>, SerdeBincode<Sidechain>>,
    slot_sequence_to_treasury_utxo:
        Database<SerdeBincode<(SidechainNumber, u64)>, SerdeBincode<TreasuryUtxo>>,
    pub treasury_utxo_count: Database<SerdeBincode<SidechainNumber>, SerdeBincode<u64>>,
}

impl ActiveSidechainDbs {
    const NUM_DBS: u32 = 6;

    fn new(env: &Env, rwtxn: &mut RwTxn) -> Result<Self, util::CreateDbError> {
        let ctip = env.create_db(rwtxn, "active_sidechain_number_to_ctip")?;
        let ctip_outpoint_to_value_seq =
            env.create_db(rwtxn, "active_sidechain_ctip_outpoint_to_value_seq")?;
        let pending_m6ids = env.create_db(rwtxn, "active_sidechain_number_to_pending_m6ids")?;
        let sidechain = env.create_db(rwtxn, "active_sidechain_number_to_sidechain")?;
        let slot_sequence_to_treasury_utxo =
            env.create_db(rwtxn, "active_sidechain_slot_sequence_to_treasury_utxo")?;
        let treasury_utxo_count =
            env.create_db(rwtxn, "active_sidechain_number_to_treasury_utxo_count")?;
        Ok(Self {
            ctip,
            ctip_outpoint_to_value_seq,
            pending_m6ids,
            sidechain,
            slot_sequence_to_treasury_utxo,
            treasury_utxo_count,
        })
    }

    pub fn ctip(&self) -> &RoDatabase<SerdeBincode<SidechainNumber>, SerdeBincode<Ctip>> {
        &self.ctip
    }

    pub fn ctip_outpoint_to_value_seq(
        &self,
    ) -> &RoDatabase<SerdeBincode<OutPoint>, SerdeBincode<(SidechainNumber, Amount, u64)>> {
        &self.ctip_outpoint_to_value_seq
    }

    pub fn pending_m6ids(
        &self,
    ) -> &RoDatabase<SerdeBincode<SidechainNumber>, SerdeBincode<PendingM6ids>> {
        &self.pending_m6ids
    }

    pub fn sidechain(&self) -> &RoDatabase<SerdeBincode<SidechainNumber>, SerdeBincode<Sidechain>> {
        &self.sidechain
    }

    pub fn slot_sequence_to_treasury_utxo(
        &self,
    ) -> &RoDatabase<SerdeBincode<(SidechainNumber, u64)>, SerdeBincode<TreasuryUtxo>> {
        &self.slot_sequence_to_treasury_utxo
    }

    /// Put ctip, returning the sequence number
    pub fn put_ctip(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: SidechainNumber,
        ctip: &Ctip,
    ) -> Result<u64, PutCtipError> {
        let treasury_utxo_count = self
            .treasury_utxo_count
            .try_get(rwtxn, &sidechain_number)?
            .unwrap_or(0);
        // Sequence numbers begin at 0, so the total number of treasury utxos in the database
        // gives us the *next* sequence number.
        let sequence_number = treasury_utxo_count;
        let old_treasury_value = self
            .ctip()
            .try_get(rwtxn, &sidechain_number)?
            .map(|old_ctip| old_ctip.value)
            .unwrap_or(Amount::ZERO);
        let treasury_utxo = TreasuryUtxo {
            sidechain_number,
            outpoint: ctip.outpoint,
            total_value: ctip.value,
            previous_total_value: old_treasury_value,
        };
        self.slot_sequence_to_treasury_utxo.put(
            rwtxn,
            &(sidechain_number, sequence_number),
            &treasury_utxo,
        )?;
        let new_treasury_utxo_count = treasury_utxo_count + 1;
        self.treasury_utxo_count
            .put(rwtxn, &sidechain_number, &new_treasury_utxo_count)?;
        self.ctip.put(rwtxn, &sidechain_number, ctip)?;
        self.ctip_outpoint_to_value_seq.put(
            rwtxn,
            &ctip.outpoint,
            &(sidechain_number, ctip.value, sequence_number),
        )?;
        Ok(sequence_number)
    }

    // Store a new active sidechain
    pub fn put_sidechain(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
        sidechain: &Sidechain,
    ) -> Result<(), PutActiveSidechainError> {
        if !self.pending_m6ids.contains_key(rwtxn, sidechain_number)? {
            self.pending_m6ids
                .put(rwtxn, sidechain_number, &PendingM6ids::new())?;
        }
        self.sidechain.put(rwtxn, sidechain_number, sidechain)?;
        Ok(())
    }

    /// Apply the provided function to pending withdrawals.
    /// Returns `None` if the sidechain is not active.
    pub fn try_with_pending_withdrawals<T, F>(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
        f: F,
    ) -> Result<Option<T>, TryWithPendingWithdrawalsError>
    where
        F: FnOnce(&mut PendingM6ids) -> T,
    {
        let Some(mut pending_m6ids) = self.pending_m6ids.try_get(rwtxn, sidechain_number)? else {
            return Ok(None);
        };
        let res = f(&mut pending_m6ids);
        let () = self
            .pending_m6ids
            .put(rwtxn, sidechain_number, &pending_m6ids)?;
        Ok(Some(res))
    }

    /// Apply the provided function to the specified entry.
    /// Returns `None` if the sidechain is not active.
    pub fn try_with_pending_withdrawal_entry<T, F>(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
        m6id: M6id,
        f: F,
    ) -> Result<Option<T>, TryWithPendingWithdrawalsError>
    where
        F: FnOnce(ordermap::map::Entry<'_, M6id, PendingM6idInfo>) -> T,
    {
        self.try_with_pending_withdrawals(rwtxn, sidechain_number, |pending_withdrawals| {
            f(pending_withdrawals.entry(m6id))
        })
    }

    /// Store a new pending M6id.
    /// Returns `true` if the sidechain is active.
    pub fn try_put_pending_m6id(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
        m6id: M6id,
        proposal_height: u32,
    ) -> Result<bool, TryWithPendingWithdrawalsError> {
        self.try_with_pending_withdrawal_entry(rwtxn, sidechain_number, m6id, |entry| match entry {
            ordermap::map::Entry::Occupied(mut entry) => {
                _ = entry.insert(PendingM6idInfo::new(proposal_height))
            }
            ordermap::map::Entry::Vacant(entry) => {
                _ = entry.insert(PendingM6idInfo::new(proposal_height))
            }
        })
        .map(|res| res.is_some())
    }

    /// Downvote all pending withdrawals
    /// Returns `true` if the sidechain is active.
    pub fn try_alarm_pending_m6ids(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
    ) -> Result<bool, TryWithPendingWithdrawalsError> {
        self.try_with_pending_withdrawals(rwtxn, sidechain_number, |pending_m6ids| {
            pending_m6ids
                .values_mut()
                .for_each(|info| info.vote_count = info.vote_count.saturating_sub(1))
        })
        .map(|res| res.is_some())
    }

    /// Upvote the pending withdrawal at the specified index
    /// Returns `true` if the sidechain is active, and there is a pending
    /// withdrawal at the specified index.
    pub fn try_upvote_pending_withdrawal(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
        index: usize,
    ) -> Result<bool, TryUpvotePendingWithdrawalError> {
        let Some(mut pending_m6ids) = self.pending_m6ids.try_get(rwtxn, sidechain_number)? else {
            return Ok(false);
        };
        if let Some((_m6id, info)) = pending_m6ids.get_index_mut(index) {
            info.vote_count = info.vote_count.saturating_add(1);
            let () = self
                .pending_m6ids
                .put(rwtxn, sidechain_number, &pending_m6ids)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Retain pending withdrawals for which the predicate returns `true`
    pub fn retain_pending_withdrawals<F>(
        &self,
        rwtxn: &mut RwTxn,
        mut f: F,
    ) -> Result<(), RetainPendingWithdrawalsError>
    where
        F: FnMut(SidechainNumber, &M6id, &PendingM6idInfo) -> bool,
    {
        let active_sidechains: Vec<_> = self
            .pending_m6ids
            .lazy_decode()
            .iter(rwtxn)
            .map_err(db_error::Iter::from)?
            .map(|(sidechain_number, _)| Ok(sidechain_number))
            .collect()
            .map_err(db_error::Iter::from)?;
        for sidechain_number in active_sidechains {
            let mut pending_m6ids = self
                .pending_m6ids
                .get(rwtxn, &sidechain_number)
                .expect("sidechain number should exist as key");
            pending_m6ids.retain(|m6id, info| f(sidechain_number, m6id, info));
            let () = self
                .pending_m6ids
                .put(rwtxn, &sidechain_number, &pending_m6ids)?;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum CreateDbsError {
    #[error(transparent)]
    CommitWriteTxn(#[from] util::CommitWriteTxnError),
    #[error(transparent)]
    CreateDb(#[from] util::CreateDbError),
    #[error("Error creating directory (`{path}`)")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    OpenEnv(#[from] util::OpenEnvError),
    #[error(transparent)]
    WriteTxn(#[from] util::WriteTxnError),
}

#[derive(Clone)]
pub(super) struct Dbs {
    env: Env,
    pub active_sidechains: ActiveSidechainDbs,
    pub block_hashes: BlockHashDbs,
    /// Tip that the enforcer is synced to
    pub current_chain_tip: Database<SerdeBincode<UnitKey>, SerdeBincode<bitcoin::BlockHash>>,
    pub _leading_by_50: Database<SerdeBincode<UnitKey>, SerdeBincode<Vec<[u8; 32]>>>,
    pub _previous_votes: Database<SerdeBincode<UnitKey>, SerdeBincode<Vec<[u8; 32]>>>,
    pub proposal_id_to_sidechain:
        Database<SerdeBincode<SidechainProposalId>, SerdeBincode<Sidechain>>,
}

impl Dbs {
    const NUM_DBS: u32 = ActiveSidechainDbs::NUM_DBS + BlockHashDbs::NUM_DBS + 4;

    pub fn new(data_dir: &Path, network: bitcoin::Network) -> Result<Self, CreateDbsError> {
        let db_dir = data_dir.join(format!("{network}.mdb"));
        if let Err(err) = std::fs::create_dir_all(&db_dir) {
            let err = CreateDbsError::CreateDirectory {
                path: db_dir,
                source: err,
            };
            return Err(err);
        }
        let env = {
            // 1 GB
            const GB: usize = 1024 * 1024 * 1024;
            // 10 GB
            const DB_MAP_SIZE: usize = 10 * GB;
            let mut env_opts = EnvOpenOptions::new();
            let _: &mut EnvOpenOptions = env_opts.max_dbs(Self::NUM_DBS).map_size(DB_MAP_SIZE);
            unsafe { Env::open(&env_opts, db_dir.clone()) }?
        };
        let mut rwtxn = env.write_txn()?;
        let active_sidechains = ActiveSidechainDbs::new(&env, &mut rwtxn)?;
        let block_hashes = BlockHashDbs::new(&env, &mut rwtxn)?;
        let current_chain_tip = env.create_db(&mut rwtxn, "current_chain_tip")?;
        let leading_by_50 = env.create_db(&mut rwtxn, "leading_by_50")?;
        let previous_votes = env.create_db(&mut rwtxn, "previous_votes")?;
        let proposal_id_to_sidechain = env.create_db(&mut rwtxn, "proposal_id_to_sidechain")?;
        let () = rwtxn.commit()?;

        tracing::info!("Created validator DBs in {}", db_dir.display());
        Ok(Self {
            env,
            active_sidechains,
            block_hashes,
            current_chain_tip,
            _leading_by_50: leading_by_50,
            _previous_votes: previous_votes,
            proposal_id_to_sidechain,
        })
    }

    pub fn read_txn(&self) -> Result<RoTxn<'_>, ReadTxnError> {
        self.env.read_txn()
    }

    pub fn nested_write_txn<'p>(
        &'p self,
        parent: &'p mut RwTxn<'_>,
    ) -> Result<RwTxn<'p>, NestedWriteTxnError> {
        self.env.nested_write_txn(parent)
    }

    pub fn write_txn(&self) -> Result<RwTxn<'_>, WriteTxnError> {
        self.env.write_txn()
    }
}
