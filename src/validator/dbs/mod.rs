use std::path::{Path, PathBuf};

use heed::{types::SerdeBincode, EnvOpenOptions, RoTxn};
use thiserror::Error;

use crate::types::{
    Ctip, Hash256, PendingM6id, Sidechain, SidechainNumber, SidechainProposal, TreasuryUtxo,
};

mod block_hashes;
mod util;

pub use block_hashes::{error as block_hash_dbs_error, BlockHashDbs};
pub use util::{
    db_error, CommitWriteTxnError, Database, Env, ReadTxnError, RwTxn, UnitKey, WriteTxnError,
};

/// These DBs should all contain exacty the same keys.
#[derive(Clone)]
pub(super) struct SidechainNumberDbs {
    pub ctip: Database<SerdeBincode<SidechainNumber>, SerdeBincode<Ctip>>,
    pub pending_m6ids: Database<SerdeBincode<SidechainNumber>, SerdeBincode<Vec<PendingM6id>>>,
    pub sidechain: Database<SerdeBincode<SidechainNumber>, SerdeBincode<Sidechain>>,
    pub treasury_utxo_count: Database<SerdeBincode<SidechainNumber>, SerdeBincode<u64>>,
}

impl SidechainNumberDbs {
    const NUM_DBS: u32 = 4;

    fn new(env: &Env, rwtxn: &mut RwTxn) -> Result<Self, util::CreateDbError> {
        let ctip = env.create_db(rwtxn, "sidechain_number_to_ctip")?;
        let pending_m6ids = env.create_db(rwtxn, "sidechain_number_to_pending_m6ids")?;
        let sidechain = env.create_db(rwtxn, "sidechain_number_to_sidechain")?;
        let treasury_utxo_count =
            env.create_db(rwtxn, "sidechain_number_to_treasury_utxo_count")?;
        Ok(Self {
            ctip,
            pending_m6ids,
            sidechain,
            treasury_utxo_count,
        })
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
    pub block_hashes: BlockHashDbs,
    /// Tip that the enforcer is synced to
    pub current_chain_tip: Database<SerdeBincode<UnitKey>, SerdeBincode<bitcoin::BlockHash>>,
    pub data_hash_to_sidechain_proposal:
        Database<SerdeBincode<Hash256>, SerdeBincode<SidechainProposal>>,
    pub _leading_by_50: Database<SerdeBincode<UnitKey>, SerdeBincode<Vec<Hash256>>>,
    pub _previous_votes: Database<SerdeBincode<UnitKey>, SerdeBincode<Vec<Hash256>>>,
    pub sidechain_number_sequence_number_to_treasury_utxo:
        Database<SerdeBincode<(SidechainNumber, u64)>, SerdeBincode<TreasuryUtxo>>,
    pub sidechain_numbers: SidechainNumberDbs,
}

impl Dbs {
    const NUM_DBS: u32 = BlockHashDbs::NUM_DBS + SidechainNumberDbs::NUM_DBS + 5;

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
        let block_hashes = BlockHashDbs::new(&env, &mut rwtxn)?;
        let current_chain_tip = env.create_db(&mut rwtxn, "current_chain_tip")?;
        let data_hash_to_sidechain_proposal =
            env.create_db(&mut rwtxn, "data_hash_to_sidechain_proposal")?;
        let leading_by_50 = env.create_db(&mut rwtxn, "leading_by_50")?;
        let previous_votes = env.create_db(&mut rwtxn, "previous_votes")?;
        let sidechain_number_sequence_number_to_treasury_utxo = env.create_db(
            &mut rwtxn,
            "sidechain_number_sequence_number_to_treasury_utxo",
        )?;
        let sidechain_numbers = SidechainNumberDbs::new(&env, &mut rwtxn)?;
        let () = rwtxn.commit()?;

        tracing::info!("Created validator DBs in {}", db_dir.display());
        Ok(Self {
            env,
            block_hashes,
            current_chain_tip,
            data_hash_to_sidechain_proposal,
            _leading_by_50: leading_by_50,
            _previous_votes: previous_votes,
            sidechain_number_sequence_number_to_treasury_utxo,
            sidechain_numbers,
        })
    }

    pub fn read_txn(&self) -> Result<RoTxn<'_>, ReadTxnError> {
        self.env.read_txn()
    }

    pub fn write_txn(&self) -> Result<RwTxn<'_>, WriteTxnError> {
        self.env.write_txn()
    }
}
