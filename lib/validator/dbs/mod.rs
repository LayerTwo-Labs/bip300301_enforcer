use std::{
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use bitcoin::{Amount, OutPoint};
use fallible_iterator::FallibleIterator;
use heed_types::SerdeBincode;
use ordermap::OrderMap;
use sneed::{DatabaseUnique, Env, RoDatabaseUnique, RoTxn, RwTxn, UnitKey, db, env, rwtxn};
use thiserror::Error;

use crate::types::{
    Ctip, M6id, PendingM6idInfo, Sidechain, SidechainNumber, SidechainProposalId, TreasuryUtxo,
};

mod block_hashes;
pub(in crate::validator) mod diff;

pub use self::block_hashes::{BlockHashDbs, error as block_hash_dbs_error};

pub type PendingM6ids = OrderMap<M6id, PendingM6idInfo>;

/// These DBs should all contain exacty the same keys.
#[derive(Clone)]
pub(super) struct ActiveSidechainDbs {
    ctip: DatabaseUnique<SerdeBincode<SidechainNumber>, SerdeBincode<Ctip>>,
    /// MUST contain ALL keys/values that `ctip` has ever contained.
    /// Associates Ctip outpoints with their value and sequence number
    ctip_outpoint_to_value_seq:
        DatabaseUnique<SerdeBincode<OutPoint>, SerdeBincode<(SidechainNumber, Amount, u64)>>,
    // ALL active sidechains MUST exist as keys
    pending_m6ids: DatabaseUnique<SerdeBincode<SidechainNumber>, SerdeBincode<PendingM6ids>>,
    // ALL active sidechains MUST exist as keys
    sidechain: DatabaseUnique<SerdeBincode<SidechainNumber>, SerdeBincode<Sidechain>>,
    slot_sequence_to_treasury_utxo:
        DatabaseUnique<SerdeBincode<(SidechainNumber, u64)>, SerdeBincode<TreasuryUtxo>>,
    pub treasury_utxo_count:
        DatabaseUnique<SerdeBincode<SidechainNumber>, SerdeBincode<NonZeroU64>>,
}

impl ActiveSidechainDbs {
    const NUM_DBS: u32 = 6;

    fn new(env: &Env, rwtxn: &mut RwTxn) -> Result<Self, env::error::CreateDb> {
        let ctip = DatabaseUnique::create(env, rwtxn, "active_sidechain_number_to_ctip")?;
        let ctip_outpoint_to_value_seq =
            DatabaseUnique::create(env, rwtxn, "active_sidechain_ctip_outpoint_to_value_seq")?;
        let pending_m6ids =
            DatabaseUnique::create(env, rwtxn, "active_sidechain_number_to_pending_m6ids")?;
        let sidechain = DatabaseUnique::create(env, rwtxn, "active_sidechain_number_to_sidechain")?;
        let slot_sequence_to_treasury_utxo = DatabaseUnique::create(
            env,
            rwtxn,
            "active_sidechain_slot_sequence_to_treasury_utxo",
        )?;
        let treasury_utxo_count =
            DatabaseUnique::create(env, rwtxn, "active_sidechain_number_to_treasury_utxo_count")?;
        Ok(Self {
            ctip,
            ctip_outpoint_to_value_seq,
            pending_m6ids,
            sidechain,
            slot_sequence_to_treasury_utxo,
            treasury_utxo_count,
        })
    }

    pub fn ctip(&self) -> &RoDatabaseUnique<SerdeBincode<SidechainNumber>, SerdeBincode<Ctip>> {
        &self.ctip
    }

    pub fn ctip_outpoint_to_value_seq(
        &self,
    ) -> &RoDatabaseUnique<SerdeBincode<OutPoint>, SerdeBincode<(SidechainNumber, Amount, u64)>>
    {
        &self.ctip_outpoint_to_value_seq
    }

    pub fn pending_m6ids(
        &self,
    ) -> &RoDatabaseUnique<SerdeBincode<SidechainNumber>, SerdeBincode<PendingM6ids>> {
        &self.pending_m6ids
    }

    pub fn sidechain(
        &self,
    ) -> &RoDatabaseUnique<SerdeBincode<SidechainNumber>, SerdeBincode<Sidechain>> {
        &self.sidechain
    }

    /// All active sidechain numbers, collected.  
    pub fn numbers(&self, rotxn: &RoTxn) -> Result<Vec<SidechainNumber>, db::Error> {
        let numbers = self
            .sidechain
            .lazy_decode()
            .iter(rotxn)?
            .map(|(n, _)| Ok(n))
            .collect()?;
        Ok(numbers)
    }

    pub fn slot_sequence_to_treasury_utxo(
        &self,
    ) -> &RoDatabaseUnique<SerdeBincode<(SidechainNumber, u64)>, SerdeBincode<TreasuryUtxo>> {
        &self.slot_sequence_to_treasury_utxo
    }

    /// Put ctip, returning the sequence number
    pub fn put_ctip(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: SidechainNumber,
        ctip: &Ctip,
    ) -> Result<u64, db::Error> {
        let treasury_utxo_count: u64 =
            match self.treasury_utxo_count.try_get(rwtxn, &sidechain_number)? {
                Some(treasury_utxo_count) => treasury_utxo_count.get(),
                None => 0,
            };
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
        let new_treasury_utxo_count = NonZeroU64::MIN.checked_add(treasury_utxo_count).unwrap();
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

    /// Delete ctip, as part of a block disconnect
    pub fn delete_ctip(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: SidechainNumber,
    ) -> Result<(), db::Error> {
        let ctip = self.ctip.get(rwtxn, &sidechain_number)?;
        let _ = self
            .ctip_outpoint_to_value_seq
            .delete(rwtxn, &ctip.outpoint)?;
        let treasury_utxo_count = self.treasury_utxo_count.get(rwtxn, &sidechain_number)?;
        let sequence_number = treasury_utxo_count.get() - 1;
        if let Some(sequence_number) = NonZeroU64::new(sequence_number) {
            self.treasury_utxo_count
                .put(rwtxn, &sidechain_number, &sequence_number)?;
        } else {
            let _ = self.treasury_utxo_count.delete(rwtxn, &sidechain_number)?;
        }
        let _ = self
            .slot_sequence_to_treasury_utxo
            .delete(rwtxn, &(sidechain_number, sequence_number))?;
        let prev_ctip_sequence_number = sequence_number.checked_sub(1);
        match prev_ctip_sequence_number {
            Some(prev_ctip_sequence_number) => {
                let prev_treasury_utxo = self
                    .slot_sequence_to_treasury_utxo
                    .get(rwtxn, &(sidechain_number, prev_ctip_sequence_number))?;
                let ctip = Ctip {
                    outpoint: prev_treasury_utxo.outpoint,
                    value: prev_treasury_utxo.total_value,
                };
                self.ctip.put(rwtxn, &sidechain_number, &ctip)?;
            }
            None => {
                let _ = self.ctip.delete(rwtxn, &sidechain_number)?;
            }
        }
        Ok(())
    }

    // Store a new active sidechain
    pub fn put_sidechain(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
        sidechain: &Sidechain,
    ) -> Result<(), db::Error> {
        if !self.pending_m6ids.contains_key(rwtxn, sidechain_number)? {
            self.pending_m6ids
                .put(rwtxn, sidechain_number, &PendingM6ids::new())?;
        }
        self.sidechain.put(rwtxn, sidechain_number, sidechain)?;
        Ok(())
    }

    // Delete an active sidechain, during a block disconnect
    pub fn delete_sidechain(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
    ) -> Result<(), db::Error> {
        let _ = self.pending_m6ids.delete(rwtxn, sidechain_number)?;
        let _ = self.sidechain.delete(rwtxn, sidechain_number)?;
        Ok(())
    }

    /// Apply the provided function to pending withdrawals for an active
    /// sidechain, and write the modified pending withdrawals.
    pub fn with_pending_withdrawals<T, F>(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
        f: F,
    ) -> Result<T, db::Error>
    where
        F: FnOnce(&mut PendingM6ids) -> T,
    {
        let mut pending_m6ids = self.pending_m6ids.get(rwtxn, sidechain_number)?;
        let res = f(&mut pending_m6ids);
        let () = self
            .pending_m6ids
            .put(rwtxn, sidechain_number, &pending_m6ids)?;
        Ok(res)
    }

    /// Apply the provided function to the specified entry for an active
    /// sidechain.
    pub fn with_pending_withdrawal_entry<T, F>(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
        m6id: M6id,
        f: F,
    ) -> Result<T, db::Error>
    where
        F: FnOnce(ordermap::map::Entry<'_, M6id, PendingM6idInfo>) -> T,
    {
        self.with_pending_withdrawals(rwtxn, sidechain_number, |pending_withdrawals| {
            f(pending_withdrawals.entry(m6id))
        })
    }

    /// Store a new pending M6id for an active sidechain.
    pub fn put_pending_m6id(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
        m6id: M6id,
        proposal_height: u32,
    ) -> Result<(), db::Error> {
        self.with_pending_withdrawal_entry(rwtxn, sidechain_number, m6id, |entry| match entry {
            ordermap::map::Entry::Occupied(mut entry) => {
                _ = entry.insert(PendingM6idInfo::new(proposal_height))
            }
            ordermap::map::Entry::Vacant(entry) => {
                _ = entry.insert(PendingM6idInfo::new(proposal_height))
            }
        })
    }

    /// Delete a pending withdrawal for an active sidechain
    pub fn delete_pending_withdrawal(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
        m6id: M6id,
    ) -> Result<(), db::Error> {
        self.with_pending_withdrawal_entry(rwtxn, sidechain_number, m6id, |entry| match entry {
            ordermap::map::Entry::Occupied(entry) => {
                _ = entry.remove();
            }
            ordermap::map::Entry::Vacant(_) => (),
        })
    }

    /// Downvote all pending withdrawals
    pub fn alarm_pending_m6ds(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: &SidechainNumber,
    ) -> Result<(), db::Error> {
        self.with_pending_withdrawals(rwtxn, sidechain_number, |pending_m6ids| {
            pending_m6ids
                .values_mut()
                .for_each(|info| info.vote_count = info.vote_count.saturating_sub(1))
        })
    }
}

#[derive(transitive::Transitive, Debug, Error)]
#[expect(clippy::duplicated_attributes)]
#[transitive(
    from(db::error::Len, db::Error),
    from(db::error::Put, db::Error),
    from(db::error::TryGet, db::Error),
    from(env::error::CreateDb, env::Error),
    from(env::error::OpenEnv, env::Error),
    from(env::error::WriteTxn, env::Error)
)]
pub enum CreateDbsError {
    #[error(transparent)]
    CommitWriteTxn(#[from] rwtxn::error::Commit),
    #[error("Error creating directory (`{path}`)")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Db(Box<db::Error>),
    #[error(transparent)]
    Env(#[from] env::Error),
    #[error(
        "Incompatible validator DB schema in `{db_dir}` \
        (expected version `{expected}`, found {}). \
        Delete this directory to resync from scratch.",
        .found.map(|found| format!("version `{found}`")).unwrap_or_else(|| "none".to_owned())
    )]
    IncompatibleSchema {
        /// `None` if the DBs predate schema versioning
        found: Option<u32>,
        expected: u32,
        db_dir: PathBuf,
    },
}

impl From<db::Error> for CreateDbsError {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

pub type ProposalIdToSidechain =
    DatabaseUnique<SerdeBincode<SidechainProposalId>, SerdeBincode<Sidechain>>;

/// Schema version of the validator DBs.
///
/// MUST be bumped whenever a change makes values written by an earlier version
/// unreadable, so that a stale datadir is rejected when opening the env,
/// instead of failing later with a decode error when reading eg. a block diff.
const VALIDATOR_DB_VERSION: u32 = 1;

#[derive(Clone)]
pub(super) struct Dbs {
    env: Env,
    pub active_sidechains: ActiveSidechainDbs,
    pub block_hashes: BlockHashDbs,
    /// Tip that the enforcer is synced to
    pub current_chain_tip: DatabaseUnique<UnitKey, SerdeBincode<bitcoin::BlockHash>>,
    /// Schema version that the DBs were written with
    pub _db_version: DatabaseUnique<UnitKey, SerdeBincode<u32>>,
    pub _leading_by_50: DatabaseUnique<UnitKey, SerdeBincode<Vec<[u8; 32]>>>,
    pub _previous_votes: DatabaseUnique<UnitKey, SerdeBincode<Vec<[u8; 32]>>>,
    pub proposal_id_to_sidechain: ProposalIdToSidechain,
}

impl Dbs {
    const NUM_DBS: u32 = ActiveSidechainDbs::NUM_DBS + BlockHashDbs::NUM_DBS + 5;

    /// Check the schema version stored in the env, storing the current version
    /// if the env is brand new.
    /// A datadir written by a different schema version is rejected here, rather
    /// than failing later with a decode error, when reading values that an
    /// incompatible version wrote.
    fn check_db_version(
        rwtxn: &mut RwTxn,
        db_version: &DatabaseUnique<UnitKey, SerdeBincode<u32>>,
        block_hashes: &BlockHashDbs,
        db_dir: &Path,
    ) -> Result<(), CreateDbsError> {
        let found = db_version.try_get(rwtxn, &())?;
        if found == Some(VALIDATOR_DB_VERSION) {
            return Ok(());
        }
        // A missing version means either that the env predates schema
        // versioning, or that it is brand new and stores nothing yet.
        if found.is_some() || block_hashes.height().len(rwtxn)? != 0 {
            return Err(CreateDbsError::IncompatibleSchema {
                found,
                expected: VALIDATOR_DB_VERSION,
                db_dir: db_dir.to_owned(),
            });
        }
        let () = db_version.put(rwtxn, &(), &VALIDATOR_DB_VERSION)?;
        Ok(())
    }

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
            let mut env_opts = env::OpenOptions::new();
            let _: &mut env::OpenOptions = env_opts.max_dbs(Self::NUM_DBS).map_size(DB_MAP_SIZE);
            unsafe { Env::open(&env_opts, &db_dir) }?
        };
        let mut rwtxn = env.write_txn()?;
        let active_sidechains = ActiveSidechainDbs::new(&env, &mut rwtxn)?;
        let block_hashes = BlockHashDbs::new(&env, &mut rwtxn)?;
        let current_chain_tip = DatabaseUnique::create(&env, &mut rwtxn, "current_chain_tip")?;
        let db_version = DatabaseUnique::create(&env, &mut rwtxn, "db_version")?;
        let leading_by_50 = DatabaseUnique::create(&env, &mut rwtxn, "leading_by_50")?;
        let previous_votes = DatabaseUnique::create(&env, &mut rwtxn, "previous_votes")?;
        let proposal_id_to_sidechain =
            DatabaseUnique::create(&env, &mut rwtxn, "proposal_id_to_sidechain")?;
        let () = Self::check_db_version(&mut rwtxn, &db_version, &block_hashes, &db_dir)?;
        let () = rwtxn.commit()?;

        tracing::info!("Created validator DBs in {}", db_dir.display());
        Ok(Self {
            env,
            active_sidechains,
            block_hashes,
            current_chain_tip,
            _db_version: db_version,
            _leading_by_50: leading_by_50,
            _previous_votes: previous_votes,
            proposal_id_to_sidechain,
        })
    }

    pub fn read_txn(&self) -> Result<RoTxn<'_, heed::WithTls>, env::error::ReadTxn> {
        self.env.read_txn()
    }

    pub fn nested_write_txn<'p>(
        &'p self,
        parent: &'p mut RwTxn<'_>,
    ) -> Result<RwTxn<'p>, env::error::NestedWriteTxn> {
        self.env.nested_write_txn(parent)
    }

    pub fn write_txn(&self) -> Result<RwTxn<'_>, env::error::WriteTxn> {
        self.env.write_txn()
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{BlockHash, hashes::Hash as _};
    use miette::{IntoDiagnostic, Result};

    use super::{CreateDbsError, Dbs, VALIDATOR_DB_VERSION};
    use crate::validator::test_utils::{create_test_dbs, test_block_header};

    #[test]
    fn fresh_datadir_stores_current_db_version() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let rotxn = dbs.read_txn().into_diagnostic()?;
        assert_eq!(
            dbs._db_version.get(&rotxn, &()).into_diagnostic()?,
            VALIDATOR_DB_VERSION
        );
        Ok(())
    }

    /// A datadir written by a different schema version stores values that the
    /// current version cannot decode, so it MUST be rejected up front.
    #[test]
    fn older_db_version_is_rejected() -> Result<()> {
        let (dir, dbs) = create_test_dbs()?;
        let db_dir = dir.path();
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        dbs._db_version
            .put(&mut rwtxn, &(), &(VALIDATOR_DB_VERSION - 1))
            .into_diagnostic()?;
        let err = Dbs::check_db_version(&mut rwtxn, &dbs._db_version, &dbs.block_hashes, db_dir)
            .expect_err("older schema version must be rejected");
        assert!(matches!(
            err,
            CreateDbsError::IncompatibleSchema {
                found: Some(found),
                expected,
                ..
            } if found == VALIDATOR_DB_VERSION - 1 && expected == VALIDATOR_DB_VERSION
        ));
        Ok(())
    }

    /// A datadir that predates schema versioning stores no version at all, and
    /// MUST be rejected in the same way. An env that stores nothing yet is
    /// brand new, and is initialized with the current version instead.
    #[test]
    fn unversioned_datadir_is_rejected_iff_non_empty() -> Result<()> {
        let (dir, dbs) = create_test_dbs()?;
        let db_dir = dir.path();
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let _ = dbs._db_version.delete(&mut rwtxn, &()).into_diagnostic()?;
        Dbs::check_db_version(&mut rwtxn, &dbs._db_version, &dbs.block_hashes, db_dir)
            .into_diagnostic()?;
        assert_eq!(
            dbs._db_version.get(&rwtxn, &()).into_diagnostic()?,
            VALIDATOR_DB_VERSION
        );
        let _ = dbs._db_version.delete(&mut rwtxn, &()).into_diagnostic()?;
        let header = test_block_header(BlockHash::all_zeros());
        dbs.block_hashes
            .put_headers(&mut rwtxn, &[(header, 0)])
            .into_diagnostic()?;
        let err = Dbs::check_db_version(&mut rwtxn, &dbs._db_version, &dbs.block_hashes, db_dir)
            .expect_err("unversioned non-empty datadir must be rejected");
        assert!(matches!(
            err,
            CreateDbsError::IncompatibleSchema { found: None, .. }
        ));
        Ok(())
    }
}
