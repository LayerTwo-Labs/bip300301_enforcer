use std::path::{Path, PathBuf};

use bip300301_messages::bitcoin::{self, block::Header, hashes::Hash, BlockHash};
use heed::{types::SerdeBincode, EnvOpenOptions, RoTxn};
use thiserror::Error;

use crate::types::{
    BlockInfo, BmmCommitments, Ctip, Deposit, Hash256, HeaderInfo, PendingM6id, Sidechain,
    SidechainNumber, SidechainProposal, TreasuryUtxo, TwoWayPegData, WithdrawalBundleEvent,
};

mod util;

pub use util::{
    db_error, CommitWriteTxnError, Database, Env, ReadTxnError, RwTxn, UnitKey, WriteTxnError,
};

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

#[derive(Debug, Error)]
#[error(
    "Inconsistent dbs: key {} exists in db `{}`, but not in db `{}`",
    hex::encode(.key),
    .exists_in,
    .not_in
)]
pub(crate) struct InconsistentDbsError {
    key: Vec<u8>,
    exists_in: &'static str,
    not_in: &'static str,
}

impl InconsistentDbsError {
    fn new<'a, KC, DC0, DC1>(
        key: &'a KC::EItem,
        db0: &Database<KC, DC0>,
        db1: &Database<KC, DC1>,
    ) -> Self
    where
        KC: heed::BytesEncode<'a>,
    {
        let key_bytes =
            // Safe to unwrap as we know that `key` encodes correctly,
            // since it is in `db0`
            <KC as heed::BytesEncode>::bytes_encode(key).unwrap();
        Self {
            key: key_bytes.to_vec(),
            exists_in: db0.name(),
            not_in: db1.name(),
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum TryGetHeaderInfoError {
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
    #[error(transparent)]
    InconsistentDbs(#[from] InconsistentDbsError),
}

#[derive(Debug, Error)]
pub(crate) enum GetHeaderInfoError {
    #[error("Missing header info for block hash `{block_hash}`")]
    MissingValue { block_hash: BlockHash },
    #[error(transparent)]
    TryGetHeaderInfo(#[from] TryGetHeaderInfoError),
}

#[derive(Debug, Error)]
pub(crate) enum TryGetBlockInfoError {
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
    #[error(transparent)]
    InconsistentDbs(#[from] InconsistentDbsError),
}

#[derive(Debug, Error)]
pub(crate) enum GetBlockInfoError {
    #[error("Missing block info for block hash `{block_hash}`")]
    MissingValue { block_hash: BlockHash },
    #[error(transparent)]
    TryGetBlockInfo(#[from] TryGetBlockInfoError),
}

#[derive(Debug, Error)]
pub(crate) enum TryGetTwoWayPegDataSingleError {
    #[error(transparent)]
    InconsistentDbs(#[from] InconsistentDbsError),
    #[error(transparent)]
    TryGetBlockInfo(#[from] TryGetBlockInfoError),
    #[error(transparent)]
    TryGetHeaderInfo(#[from] TryGetHeaderInfoError),
}

#[derive(Debug, Error)]
pub enum GetTwoWayPegDataError {
    #[error("End block `{end_block}` not found")]
    EndBlockNotFound { end_block: BlockHash },
    #[error("Previous block `{prev_block}` not found for block `{block}`")]
    PreviousBlockNotFound {
        block: BlockHash,
        prev_block: BlockHash,
    },
    #[error(
        "Start block `{}` is not an ancestor of end block `{}`",
        .start_block,
        .end_block
    )]
    StartBlockNotAncestor {
        start_block: BlockHash,
        end_block: BlockHash,
    },
    #[error(transparent)]
    #[non_exhaustive]
    TryGetTwoWayPegDataSingle(TryGetTwoWayPegDataSingleError),
}

#[derive(Clone)]
pub(super) struct Dbs {
    env: Env,
    pub block_hash_to_bmm_commitments:
        Database<SerdeBincode<BlockHash>, SerdeBincode<BmmCommitments>>,
    pub block_hash_to_deposits: Database<SerdeBincode<BlockHash>, SerdeBincode<Vec<Deposit>>>,
    pub block_hash_to_header: Database<SerdeBincode<BlockHash>, SerdeBincode<Header>>,
    pub block_hash_to_height: Database<SerdeBincode<BlockHash>, SerdeBincode<u32>>,
    pub block_hash_to_withdrawal_bundle_events:
        Database<SerdeBincode<BlockHash>, SerdeBincode<Vec<WithdrawalBundleEvent>>>,
    pub block_height_to_accepted_bmm_block_hashes:
        Database<SerdeBincode<u32>, SerdeBincode<Vec<Hash256>>>,
    pub current_block_height: Database<SerdeBincode<UnitKey>, SerdeBincode<u32>>,
    pub current_chain_tip: Database<SerdeBincode<UnitKey>, SerdeBincode<BlockHash>>,
    pub data_hash_to_sidechain_proposal:
        Database<SerdeBincode<Hash256>, SerdeBincode<SidechainProposal>>,
    pub _leading_by_50: Database<SerdeBincode<UnitKey>, SerdeBincode<Vec<Hash256>>>,
    pub _previous_votes: Database<SerdeBincode<UnitKey>, SerdeBincode<Vec<Hash256>>>,
    pub sidechain_number_sequence_number_to_treasury_utxo:
        Database<SerdeBincode<(SidechainNumber, u64)>, SerdeBincode<TreasuryUtxo>>,
    pub sidechain_number_to_ctip: Database<SerdeBincode<SidechainNumber>, SerdeBincode<Ctip>>,
    pub sidechain_number_to_pending_m6ids:
        Database<SerdeBincode<SidechainNumber>, SerdeBincode<Vec<PendingM6id>>>,
    pub sidechain_number_to_sidechain:
        Database<SerdeBincode<SidechainNumber>, SerdeBincode<Sidechain>>,
    pub sidechain_number_to_treasury_utxo_count:
        Database<SerdeBincode<SidechainNumber>, SerdeBincode<u64>>,
}

impl Dbs {
    const NUM_DBS: u32 = 16;

    pub fn new(data_dir: &Path, network: bitcoin::Network) -> Result<Self, CreateDbsError> {
        let db_dir = data_dir
            .join("./bip300301_enforcer")
            .join(format!("{network}.mdb"));
        if let Err(err) = std::fs::create_dir_all(&db_dir) {
            let err = CreateDbsError::CreateDirectory {
                path: db_dir,
                source: err,
            };
            return Err(err);
        }
        let env = {
            // 1GB
            const DB_MAP_SIZE: usize = 1024 * 1024 * 1024;
            let mut env_opts = EnvOpenOptions::new();
            let _: &mut EnvOpenOptions = env_opts.max_dbs(Self::NUM_DBS).map_size(DB_MAP_SIZE);
            unsafe { Env::open(&env_opts, db_dir) }?
        };
        let mut rwtxn = env.write_txn()?;
        let block_hash_to_bmm_commitments =
            env.create_db(&mut rwtxn, "block_hash_to_bmm_commitments")?;
        let block_hash_to_deposits = env.create_db(&mut rwtxn, "block_hash_to_deposits")?;
        let block_hash_to_header = env.create_db(&mut rwtxn, "block_hash_to_header")?;
        let block_hash_to_height = env.create_db(&mut rwtxn, "block_hash_to_height")?;
        let block_hash_to_withdrawal_bundle_events =
            env.create_db(&mut rwtxn, "block_hash_to_withdrawal_bundle_events")?;
        let block_height_to_accepted_bmm_block_hashes =
            env.create_db(&mut rwtxn, "block_height_to_accepted_bmm_block_hashes")?;
        let current_block_height = env.create_db(&mut rwtxn, "current_block_height")?;
        let current_chain_tip = env.create_db(&mut rwtxn, "current_chain_tip")?;
        let data_hash_to_sidechain_proposal =
            env.create_db(&mut rwtxn, "data_hash_to_sidechain_proposal")?;
        let leading_by_50 = env.create_db(&mut rwtxn, "leading_by_50")?;
        let previous_votes = env.create_db(&mut rwtxn, "previous_votes")?;
        let sidechain_number_sequence_number_to_treasury_utxo = env.create_db(
            &mut rwtxn,
            "sidechain_number_sequence_number_to_treasury_utxo",
        )?;
        let sidechain_number_to_ctip = env.create_db(&mut rwtxn, "sidechain_number_to_ctip")?;
        let sidechain_number_to_pending_m6ids =
            env.create_db(&mut rwtxn, "sidechain_number_to_pending_m6ids")?;
        let sidechain_number_to_sidechain =
            env.create_db(&mut rwtxn, "sidechain_number_to_sidechain")?;
        let sidechain_number_to_treasury_utxo_count =
            env.create_db(&mut rwtxn, "sidechain_number_to_treasury_utxo_count")?;
        let () = rwtxn.commit()?;
        Ok(Self {
            env,
            block_hash_to_bmm_commitments,
            block_hash_to_deposits,
            block_hash_to_header,
            block_hash_to_height,
            block_hash_to_withdrawal_bundle_events,
            block_height_to_accepted_bmm_block_hashes,
            current_block_height,
            current_chain_tip,
            data_hash_to_sidechain_proposal,
            _leading_by_50: leading_by_50,
            _previous_votes: previous_votes,
            sidechain_number_sequence_number_to_treasury_utxo,
            sidechain_number_to_ctip,
            sidechain_number_to_pending_m6ids,
            sidechain_number_to_sidechain,
            sidechain_number_to_treasury_utxo_count,
        })
    }

    pub fn read_txn(&self) -> Result<RoTxn<'_>, ReadTxnError> {
        self.env.read_txn()
    }

    pub fn write_txn(&self) -> Result<RwTxn<'_>, WriteTxnError> {
        self.env.write_txn()
    }

    fn try_get_header_info(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
    ) -> Result<Option<HeaderInfo>, TryGetHeaderInfoError> {
        let Some(header) = self.block_hash_to_header.try_get(rotxn, block_hash)? else {
            return Ok(None);
        };
        assert_eq!(header.block_hash(), *block_hash);
        let Some(height) = self.block_hash_to_height.try_get(rotxn, block_hash)? else {
            let err = InconsistentDbsError::new(
                block_hash,
                &self.block_hash_to_header,
                &self.block_hash_to_height,
            );
            return Err(TryGetHeaderInfoError::InconsistentDbs(err));
        };
        let header_info = HeaderInfo {
            block_hash: header.block_hash(),
            prev_block_hash: header.prev_blockhash,
            height,
            work: header.work().to_le_bytes(),
        };
        Ok(Some(header_info))
    }

    pub fn get_header_info(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
    ) -> Result<HeaderInfo, GetHeaderInfoError> {
        self.try_get_header_info(rotxn, block_hash)?.ok_or_else(|| {
            GetHeaderInfoError::MissingValue {
                block_hash: *block_hash,
            }
        })
    }

    fn try_get_block_info(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
    ) -> Result<Option<BlockInfo>, TryGetBlockInfoError> {
        let Some(deposits) = self.block_hash_to_deposits.try_get(rotxn, block_hash)? else {
            return Ok(None);
        };
        let Some(withdrawal_bundle_events) = self
            .block_hash_to_withdrawal_bundle_events
            .try_get(rotxn, block_hash)?
        else {
            let err = InconsistentDbsError::new(
                block_hash,
                &self.block_hash_to_deposits,
                &self.block_hash_to_withdrawal_bundle_events,
            );
            return Err(TryGetBlockInfoError::InconsistentDbs(err));
        };
        let Some(bmm_commitments) = self
            .block_hash_to_bmm_commitments
            .try_get(rotxn, block_hash)?
        else {
            let err = InconsistentDbsError::new(
                block_hash,
                &self.block_hash_to_deposits,
                &self.block_hash_to_bmm_commitments,
            );
            return Err(TryGetBlockInfoError::InconsistentDbs(err));
        };
        let block_info = BlockInfo {
            deposits,
            withdrawal_bundle_events,
            bmm_commitments,
        };
        Ok(Some(block_info))
    }

    pub fn get_block_info(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
    ) -> Result<BlockInfo, GetBlockInfoError> {
        self.try_get_block_info(rotxn, block_hash)?
            .ok_or_else(|| GetBlockInfoError::MissingValue {
                block_hash: *block_hash,
            })
    }

    /// Get two way peg data for a single block
    fn try_get_two_way_peg_data_single(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
    ) -> Result<Option<TwoWayPegData>, TryGetTwoWayPegDataSingleError> {
        let Some(header_info) = self.try_get_header_info(rotxn, block_hash)? else {
            return Ok(None);
        };
        let Some(block_info) = self.try_get_block_info(rotxn, block_hash)? else {
            let err = InconsistentDbsError::new(
                block_hash,
                &self.block_hash_to_header,
                &self.block_hash_to_deposits,
            );
            return Err(TryGetTwoWayPegDataSingleError::InconsistentDbs(err));
        };
        let res = TwoWayPegData {
            header_info,
            block_info,
        };
        Ok(Some(res))
    }

    pub fn get_two_way_peg_data(
        &self,
        rotxn: &RoTxn,
        start_block: Option<BlockHash>,
        end_block: BlockHash,
    ) -> Result<Vec<TwoWayPegData>, GetTwoWayPegDataError> {
        let mut res = Vec::new();
        let Some(two_way_peg_data) = self
            .try_get_two_way_peg_data_single(rotxn, &end_block)
            .map_err(GetTwoWayPegDataError::TryGetTwoWayPegDataSingle)?
        else {
            return Err(GetTwoWayPegDataError::EndBlockNotFound { end_block });
        };
        let mut prev_block = end_block;
        let mut current_block = two_way_peg_data.header_info.prev_block_hash;
        res.push(two_way_peg_data);
        if Some(end_block) == start_block {
            return Ok(res);
        };
        while Some(current_block) != start_block {
            if current_block == BlockHash::all_zeros() {
                if let Some(start_block) = start_block {
                    return Err(GetTwoWayPegDataError::StartBlockNotAncestor {
                        start_block,
                        end_block,
                    });
                } else {
                    break;
                }
            }
            let Some(two_way_peg_data) = self
                .try_get_two_way_peg_data_single(rotxn, &current_block)
                .map_err(GetTwoWayPegDataError::TryGetTwoWayPegDataSingle)?
            else {
                return Err(GetTwoWayPegDataError::PreviousBlockNotFound {
                    block: current_block,
                    prev_block,
                });
            };
            prev_block = current_block;
            current_block = two_way_peg_data.header_info.block_hash;
            res.push(two_way_peg_data);
        }
        res.reverse();
        Ok(res)
    }
}
