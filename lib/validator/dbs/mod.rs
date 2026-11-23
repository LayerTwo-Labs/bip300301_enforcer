use std::{
    borrow::Cow,
    num::NonZeroU64,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use bitcoin::{Amount, OutPoint};
use fallible_iterator::FallibleIterator;
use heed::{BoxedError, BytesDecode, BytesEncode};
use heed_types::SerdeBincode;
use ordermap::OrderMap;
use serde::{Deserialize, Serialize};
use sneed::{DatabaseUnique, Env, RoDatabaseUnique, RoTxn, RwTxn, UnitKey, db, env, rwtxn};
use thiserror::Error;

use crate::types::{
    Ctip, M6id, PendingM6idInfo, Sidechain, SidechainNumber, SidechainProposalId, TreasuryUtxo,
};

mod block_hashes;
pub use self::block_hashes::{BlockHashDbs, error as block_hash_dbs_error};
pub(in crate::validator) mod diff;

/// Heed codec for deposit sequence keys: `(SidechainNumber, u32)` encoded as
/// one sidechain number byte followed by the big-endian sequence number, so
/// that keys sort lexicographically in sequence order.
struct DepositSequenceKey;

impl<'a> BytesEncode<'a> for DepositSequenceKey {
    type EItem = (SidechainNumber, u32);

    fn bytes_encode(item: &'a Self::EItem) -> Result<Cow<'a, [u8]>, BoxedError> {
        let (sidechain_number, sequence_number) = item;
        let mut bytes = Vec::with_capacity(1 + 4);
        bytes.push(sidechain_number.0);
        bytes.extend_from_slice(&sequence_number.to_be_bytes());
        Ok(Cow::Owned(bytes))
    }
}

impl<'a> BytesDecode<'a> for DepositSequenceKey {
    type DItem = (SidechainNumber, u32);

    fn bytes_decode(bytes: &'a [u8]) -> Result<Self::DItem, BoxedError> {
        let (sidechain_number, sequence_number) = bytes.split_first().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "empty deposit sequence key",
            )
        })?;
        let sequence_number: [u8; 4] = sequence_number.try_into().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid deposit sequence key",
            )
        })?;
        Ok((
            SidechainNumber(*sidechain_number),
            u32::from_be_bytes(sequence_number),
        ))
    }
}

/// The enforcer's own version, as written to the validator database.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl From<semver::Version> for Version {
    fn from(version: semver::Version) -> Self {
        let semver::Version {
            major,
            minor,
            patch,
            pre: _,
            build: _,
        } = version;
        Self {
            major,
            minor,
            patch,
        }
    }
}

// Do not make this public outside of this crate, as it could break semver
pub(crate) static VERSION: LazyLock<Version> = LazyLock::new(|| {
    const VERSION_STR: &str = env!("CARGO_PKG_VERSION");
    semver::Version::parse(VERSION_STR).unwrap().into()
});

pub type PendingM6ids = OrderMap<M6id, PendingM6idInfo>;

/// These DBs should all contain exacty the same keys.
#[derive(Clone)]
pub(super) struct ActiveSidechainDbs {
    // ALL active sidechains MUST exist as keys
    pending_m6ids: DatabaseUnique<SerdeBincode<SidechainNumber>, SerdeBincode<PendingM6ids>>,
    // ALL active sidechains MUST exist as keys
    sidechain: DatabaseUnique<SerdeBincode<SidechainNumber>, SerdeBincode<Sidechain>>,
    /// MUST contain ALL keys/values that `treasury_utxo` has ever contained.
    /// Associates treasury outpoints with their value and sequence number
    treasury_outpoint_to_value_seq:
        DatabaseUnique<SerdeBincode<OutPoint>, SerdeBincode<(SidechainNumber, Amount, u64)>>,
    treasury_utxo: DatabaseUnique<SerdeBincode<SidechainNumber>, SerdeBincode<Ctip>>,
    treasury_utxo_sequence_to_treasury_utxo:
        DatabaseUnique<SerdeBincode<(SidechainNumber, u64)>, SerdeBincode<TreasuryUtxo>>,
    pub treasury_utxo_count:
        DatabaseUnique<SerdeBincode<SidechainNumber>, SerdeBincode<NonZeroU64>>,
    /// Maps each sidechain's deposit sequence numbers to the deposit's ctip.
    /// Its length for a sidechain is the sidechain's deposit counter.
    deposit_sequence_to_deposit_ctip: DatabaseUnique<DepositSequenceKey, SerdeBincode<Ctip>>,
    /// MUST contain ALL keys/values that `deposit_sequence_to_deposit_ctip`
    /// has ever contained. Associates deposit ctip outpoints with their
    /// sidechain number, value, and deposit sequence number.
    deposit_ctip_outpoint_to_value_seq:
        DatabaseUnique<SerdeBincode<OutPoint>, SerdeBincode<(SidechainNumber, Amount, u64)>>,
}

impl ActiveSidechainDbs {
    const NUM_DBS: u32 = 8;

    fn new(env: &Env, rwtxn: &mut RwTxn) -> Result<Self, env::error::CreateDb> {
        let pending_m6ids =
            DatabaseUnique::create(env, rwtxn, "active_sidechain_number_to_pending_m6ids")?;
        let sidechain = DatabaseUnique::create(env, rwtxn, "active_sidechain_number_to_sidechain")?;
        let treasury_outpoint_to_value_seq = DatabaseUnique::create(
            env,
            rwtxn,
            "active_sidechain_treasury_outpoint_to_value_seq",
        )?;
        let treasury_utxo =
            DatabaseUnique::create(env, rwtxn, "active_sidechain_number_to_treasury_utxo")?;
        let treasury_utxo_sequence_to_treasury_utxo = DatabaseUnique::create(
            env,
            rwtxn,
            "active_sidechain_treasury_utxo_sequence_to_treasury_utxo",
        )?;
        let treasury_utxo_count =
            DatabaseUnique::create(env, rwtxn, "active_sidechain_number_to_treasury_utxo_count")?;
        let deposit_sequence_to_deposit_ctip = DatabaseUnique::create(
            env,
            rwtxn,
            "active_sidechain_deposit_sequence_to_deposit_ctip",
        )?;
        let deposit_ctip_outpoint_to_value_seq = DatabaseUnique::create(
            env,
            rwtxn,
            "active_sidechain_deposit_ctip_outpoint_to_value_seq",
        )?;
        Ok(Self {
            pending_m6ids,
            sidechain,
            treasury_utxo,
            treasury_outpoint_to_value_seq,
            treasury_utxo_sequence_to_treasury_utxo,
            treasury_utxo_count,
            deposit_sequence_to_deposit_ctip,
            deposit_ctip_outpoint_to_value_seq,
        })
    }

    pub fn treasury_utxo(
        &self,
    ) -> &RoDatabaseUnique<SerdeBincode<SidechainNumber>, SerdeBincode<Ctip>> {
        &self.treasury_utxo
    }

    pub fn treasury_outpoint_to_value_seq(
        &self,
    ) -> &RoDatabaseUnique<SerdeBincode<OutPoint>, SerdeBincode<(SidechainNumber, Amount, u64)>>
    {
        &self.treasury_outpoint_to_value_seq
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

    pub fn treasury_utxo_sequence_to_treasury_utxo(
        &self,
    ) -> &RoDatabaseUnique<SerdeBincode<(SidechainNumber, u64)>, SerdeBincode<TreasuryUtxo>> {
        &self.treasury_utxo_sequence_to_treasury_utxo
    }

    pub fn deposit_ctip_outpoint_to_value_seq(
        &self,
    ) -> &RoDatabaseUnique<SerdeBincode<OutPoint>, SerdeBincode<(SidechainNumber, Amount, u64)>>
    {
        &self.deposit_ctip_outpoint_to_value_seq
    }

    /// Put a treasury utxo, returning the treasury utxo sequence number.
    pub fn put_treasury_utxo(
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
            .treasury_utxo()
            .try_get(rwtxn, &sidechain_number)?
            .map(|old_ctip| old_ctip.value)
            .unwrap_or(Amount::ZERO);
        let treasury_utxo = TreasuryUtxo {
            sidechain_number,
            outpoint: ctip.outpoint,
            total_value: ctip.value,
            previous_total_value: old_treasury_value,
        };
        self.treasury_utxo_sequence_to_treasury_utxo.put(
            rwtxn,
            &(sidechain_number, sequence_number),
            &treasury_utxo,
        )?;
        let new_treasury_utxo_count = NonZeroU64::MIN.checked_add(treasury_utxo_count).unwrap();
        self.treasury_utxo_count
            .put(rwtxn, &sidechain_number, &new_treasury_utxo_count)?;
        self.treasury_utxo.put(rwtxn, &sidechain_number, ctip)?;
        self.treasury_outpoint_to_value_seq.put(
            rwtxn,
            &ctip.outpoint,
            &(sidechain_number, ctip.value, sequence_number),
        )?;
        Ok(sequence_number)
    }

    /// Delete the current treasury utxo, as part of a block disconnect,
    /// restoring the previous treasury utxo.
    pub fn delete_treasury_utxo(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: SidechainNumber,
    ) -> Result<(), db::Error> {
        let ctip = self.treasury_utxo.get(rwtxn, &sidechain_number)?;
        let _ = self
            .treasury_outpoint_to_value_seq
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
            .treasury_utxo_sequence_to_treasury_utxo
            .delete(rwtxn, &(sidechain_number, sequence_number))?;
        let prev_ctip_sequence_number = sequence_number.checked_sub(1);
        match prev_ctip_sequence_number {
            Some(prev_ctip_sequence_number) => {
                let prev_treasury_utxo = self
                    .treasury_utxo_sequence_to_treasury_utxo
                    .get(rwtxn, &(sidechain_number, prev_ctip_sequence_number))?;
                let ctip = Ctip {
                    outpoint: prev_treasury_utxo.outpoint,
                    value: prev_treasury_utxo.total_value,
                };
                self.treasury_utxo.put(rwtxn, &sidechain_number, &ctip)?;
            }
            None => {
                let _ = self.treasury_utxo.delete(rwtxn, &sidechain_number)?;
            }
        }
        Ok(())
    }

    /// The sequence number the sidechain's next deposit will get: the number
    /// of deposits so far. Deposit sequence numbers begin at 0.
    pub(in crate::validator) fn next_deposit_sequence_number(
        &self,
        rotxn: &RoTxn,
        sidechain_number: SidechainNumber,
    ) -> Result<u64, db::Error> {
        // The sidechain's deposits sort contiguously by sequence number, so
        // the latest one is the last of its key range.
        let last_deposit = self
            .deposit_sequence_to_deposit_ctip
            .range(
                rotxn,
                &((sidechain_number, u32::MIN)..=(sidechain_number, u32::MAX)),
            )?
            .last()?;
        let next_sequence_number = last_deposit.map_or(0, |((_, sequence_number), _)| {
            u64::from(sequence_number) + 1
        });
        Ok(next_sequence_number)
    }

    /// Record a deposit, returning its sequence number.
    pub fn put_deposit(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: SidechainNumber,
        ctip: &Ctip,
    ) -> Result<u64, db::Error> {
        let sequence_number = self.next_deposit_sequence_number(rwtxn, sidechain_number)?;
        let sequence_number_u32 = u32::try_from(sequence_number).unwrap();
        self.deposit_sequence_to_deposit_ctip.put(
            rwtxn,
            &(sidechain_number, sequence_number_u32),
            ctip,
        )?;
        self.deposit_ctip_outpoint_to_value_seq.put(
            rwtxn,
            &ctip.outpoint,
            &(sidechain_number, ctip.value, sequence_number),
        )?;
        Ok(sequence_number)
    }

    /// Delete the sidechain's latest deposit, as part of a block disconnect.
    pub fn delete_deposit(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: SidechainNumber,
    ) -> Result<(), db::Error> {
        // Deposits are undone in reverse order, so the deposit to remove is
        // the sidechain's latest.
        let last_deposit = self
            .deposit_sequence_to_deposit_ctip
            .range(
                rwtxn,
                &((sidechain_number, u32::MIN)..=(sidechain_number, u32::MAX)),
            )?
            .last()?;
        let Some(((_, sequence_number), deposit_ctip)) = last_deposit else {
            tracing::error!(
                %sidechain_number,
                "no deposit to delete; the diff and the deposit records disagree"
            );
            return Ok(());
        };
        let _ = self
            .deposit_sequence_to_deposit_ctip
            .delete(rwtxn, &(sidechain_number, sequence_number))?;
        let _ = self
            .deposit_ctip_outpoint_to_value_seq
            .delete(rwtxn, &deposit_ctip.outpoint)?;
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
    from(db::error::TryGet, db::Error),
    from(db::error::Put, db::Error),
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
    Db(#[from] db::Error),
    #[error(transparent)]
    Env(#[from] env::Error),
    #[error(
        "validator database at `{}` is incompatible; \
         delete it and re-sync from scratch",
        .path.display()
    )]
    IncompatibleVersion { path: PathBuf },
}

pub type ProposalIdToSidechain =
    DatabaseUnique<SerdeBincode<SidechainProposalId>, SerdeBincode<Sidechain>>;

#[derive(Clone)]
pub(super) struct Dbs {
    env: Env,
    pub active_sidechains: ActiveSidechainDbs,
    pub block_hashes: BlockHashDbs,
    /// Tip that the enforcer is synced to
    pub current_chain_tip: DatabaseUnique<UnitKey, SerdeBincode<bitcoin::BlockHash>>,
    pub _leading_by_50: DatabaseUnique<UnitKey, SerdeBincode<Vec<[u8; 32]>>>,
    pub _previous_votes: DatabaseUnique<UnitKey, SerdeBincode<Vec<[u8; 32]>>>,
    pub proposal_id_to_sidechain: ProposalIdToSidechain,
    /// Used for determining conflicts for mempool txs.
    pub seen_unconsolidated_ctip_outputs:
        DatabaseUnique<SerdeBincode<OutPoint>, SerdeBincode<(SidechainNumber, Amount)>>,
    _version: DatabaseUnique<UnitKey, SerdeBincode<Version>>,
}

impl Dbs {
    const NUM_DBS: u32 = ActiveSidechainDbs::NUM_DBS + BlockHashDbs::NUM_DBS + 6;

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
        let current_chain_tip = DatabaseUnique::create(&env, &mut rwtxn, "current_chain_tip")?;
        let active_sidechains = ActiveSidechainDbs::new(&env, &mut rwtxn)?;
        let block_hashes = BlockHashDbs::new(&env, &mut rwtxn)?;
        let leading_by_50 = DatabaseUnique::create(&env, &mut rwtxn, "leading_by_50")?;
        let previous_votes = DatabaseUnique::create(&env, &mut rwtxn, "previous_votes")?;
        let proposal_id_to_sidechain =
            DatabaseUnique::create(&env, &mut rwtxn, "proposal_id_to_sidechain")?;
        let seen_unconsolidated_ctip_outputs =
            DatabaseUnique::create(&env, &mut rwtxn, "seen_unconsolidated_ctip_outputs")?;
        let version = DatabaseUnique::create(&env, &mut rwtxn, "version")?;
        match version.try_get(&rwtxn, &())? {
            Some(version) => {
                if version != *VERSION {
                    return Err(CreateDbsError::IncompatibleVersion { path: db_dir });
                }
            }
            None => {
                if current_chain_tip.contains_key(&rwtxn, &())? {
                    return Err(CreateDbsError::IncompatibleVersion { path: db_dir });
                }
                version.put(&mut rwtxn, &(), &*VERSION)?;
            }
        }
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
            seen_unconsolidated_ctip_outputs,
            _version: version,
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
