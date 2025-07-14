use std::time::Instant;

use bitcoin::{block::Header, hashes::Hash as _, BlockHash, Txid, Work};
use fallible_iterator::FallibleIterator;
use heed::{types::SerdeBincode, RoTxn};
use nonempty::NonEmpty;
use tracing::instrument;

use super::util::RoDatabase;
use crate::{
    types::{BlockEvent, BlockInfo, BmmCommitments, HeaderInfo, TwoWayPegData},
    validator::dbs::util::{db_error, CreateDbError, Database, Env, RwTxn},
};

pub mod error {
    use bitcoin::BlockHash;
    use thiserror::Error;

    use crate::validator::dbs::util::db_error;

    #[derive(Debug, Error)]
    #[error("Missing header info for block hash `{block_hash}`")]
    pub(crate) struct MissingHeader {
        pub(super) block_hash: BlockHash,
    }

    #[derive(Debug, Error)]
    #[error("Missing parent for block hash `{block_hash}`: `{prev_block_hash}`")]
    pub(crate) struct MissingParent {
        pub(super) block_hash: BlockHash,
        pub(super) prev_block_hash: BlockHash,
    }

    #[derive(Debug, Error)]
    pub(in crate::validator::dbs::block_hashes) enum PutBlockInfoInner {
        #[error(transparent)]
        DbPut(#[from] db_error::Put),
        #[error(transparent)]
        DbTryGet(#[from] db_error::TryGet),
        #[error(transparent)]
        MissingHeader(#[from] MissingHeader),
        #[error(transparent)]
        MissingParent(#[from] MissingParent),
    }

    #[derive(Debug, Error)]
    #[error("Error storing block info")]
    pub(crate) struct PutBlockInfo(#[source] PutBlockInfoInner);

    impl<Err> From<Err> for PutBlockInfo
    where
        PutBlockInfoInner: From<Err>,
    {
        fn from(err: Err) -> Self {
            Self(err.into())
        }
    }

    #[derive(Debug, Error)]
    pub(crate) enum LastCommonAncestor {
        #[error(transparent)]
        DbGet(#[from] db_error::Get),
        #[error(transparent)]
        DbTryGet(#[from] db_error::TryGet),
        #[error(
            "Missing ancestor at depth {} for {} (height={})",
            .depth,
            .block_hash,
            .height
        )]
        MissingAncestor {
            block_hash: BlockHash,
            depth: u32,
            height: u32,
        },
    }

    #[derive(Debug, Error)]
    pub(crate) enum TryGetHeaderInfo {
        #[error(transparent)]
        DbTryGet(#[from] db_error::TryGet),
        #[error(transparent)]
        InconsistentDbs(#[from] db_error::InconsistentDbs),
    }

    #[derive(Debug, Error)]
    pub(crate) enum GetHeaderInfo {
        #[error(transparent)]
        MissingHeader(#[from] MissingHeader),
        #[error(transparent)]
        TryGetHeaderInfo(#[from] TryGetHeaderInfo),
    }

    #[derive(Debug, Error)]
    pub(crate) enum TryGetBlockInfo {
        #[error(transparent)]
        DbTryGet(#[from] db_error::TryGet),
        #[error(transparent)]
        InconsistentDbs(#[from] db_error::InconsistentDbs),
    }

    #[derive(Debug, Error)]
    pub(crate) enum GetBlockInfo {
        #[error("Missing block info for block hash `{block_hash}`")]
        MissingValue { block_hash: BlockHash },
        #[error(transparent)]
        TryGetBlockInfo(#[from] TryGetBlockInfo),
    }

    #[derive(Debug, Error)]
    pub(crate) enum TryGetTwoWayPegData {
        #[error(transparent)]
        TryGetBlockInfo(#[from] TryGetBlockInfo),
        #[error(transparent)]
        TryGetHeaderInfo(#[from] TryGetHeaderInfo),
    }

    #[derive(Debug, Error)]
    pub(crate) enum GetTwoWayPegDataRange {
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
        TryGetTwoWayPegData(#[from] TryGetTwoWayPegData),
    }
}

#[derive(Clone)]
pub struct BlockHashDbs {
    // All ancestors for each block MUST exist in this DB.
    // All keys in this DB MUST also exist in ALL other DBs.
    bmm_commitments: Database<SerdeBincode<BlockHash>, SerdeBincode<BmmCommitments>>,
    // All ancestors for each block MUST exist in this DB.
    // All keys in this DB MUST also exist in ALL other DBs.
    coinbase_txid: Database<SerdeBincode<BlockHash>, SerdeBincode<Txid>>,
    // All ancestors for each block MUST exist in this DB.
    // All keys in this DB MUST also exist in ALL other DBs.
    cumulative_work: Database<SerdeBincode<BlockHash>, SerdeBincode<Work>>,
    // All ancestors for each block MUST exist in this DB.
    // All keys in this DB MUST also exist in ALL other DBs.
    events: Database<SerdeBincode<BlockHash>, SerdeBincode<Vec<BlockEvent>>>,
    // All keys in this DB MUST also exist in `height`
    header: Database<SerdeBincode<BlockHash>, SerdeBincode<Header>>,
    // All keys in this DB MUST also exist in `header` as keys AND/OR
    // `prev_blockhash` in a value
    height: Database<SerdeBincode<BlockHash>, SerdeBincode<u32>>,
}

impl BlockHashDbs {
    pub const NUM_DBS: u32 = 6;

    pub(super) fn new(env: &Env, rwtxn: &mut RwTxn) -> Result<Self, CreateDbError> {
        let bmm_commitments = env.create_db(rwtxn, "block_hash_to_bmm_commitments")?;
        let coinbase_txid = env.create_db(rwtxn, "block_hash_to_coinbase_txid")?;
        let cumulative_work = env.create_db(rwtxn, "block_hash_to_cumulative_work")?;
        let events = env.create_db(rwtxn, "block_hash_to_events")?;
        let header = env.create_db(rwtxn, "block_hash_to_header")?;
        let height = env.create_db(rwtxn, "block_hash_to_height")?;
        Ok(Self {
            bmm_commitments,
            coinbase_txid,
            cumulative_work,
            events,
            header,
            height,
        })
    }

    pub fn bmm_commitments(
        &self,
    ) -> RoDatabase<SerdeBincode<BlockHash>, SerdeBincode<BmmCommitments>> {
        (*self.bmm_commitments).clone()
    }

    pub fn cumulative_work(&self) -> RoDatabase<SerdeBincode<BlockHash>, SerdeBincode<Work>> {
        (*self.cumulative_work).clone()
    }

    pub fn height(&self) -> RoDatabase<SerdeBincode<BlockHash>, SerdeBincode<u32>> {
        (*self.height).clone()
    }

    /// Check if the database contains the provided header
    pub fn contains_header(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
    ) -> Result<bool, db_error::TryGet> {
        self.header.contains_key(rotxn, block_hash)
    }

    /// Store info for a single header
    #[instrument(skip_all)]
    pub fn put_headers(
        &self,
        rwtxn: &mut RwTxn,
        headers: &[(Header, u32)],
    ) -> Result<(), db_error::Put> {
        let start = Instant::now();

        for (header, height) in headers {
            let block_hash = header.block_hash();
            let () = self.header.put(rwtxn, &block_hash, header)?;
            let () = self.height.put(rwtxn, &block_hash, height)?;
            if header.prev_blockhash != BlockHash::all_zeros() {
                let () = self
                    .height
                    .put(rwtxn, &header.prev_blockhash, &(height - 1))?;
            }
        }

        tracing::debug!(
            "Stored {} block header(s) in {:?}",
            headers.len(),
            start.elapsed(),
        );
        Ok(())
    }

    /// Store info for a single block
    pub fn put_block_info(
        &self,
        rwtxn: &mut RwTxn,
        block_hash: &BlockHash,
        block_info: &BlockInfo,
    ) -> Result<(), error::PutBlockInfo> {
        let Some(header) = self.header.try_get(rwtxn, block_hash)? else {
            let err = error::MissingHeader {
                block_hash: *block_hash,
            };
            return Err(error::PutBlockInfoInner::MissingHeader(err).into());
        };
        let cumulative_work = if header.prev_blockhash == BlockHash::all_zeros() {
            header.work()
        } else if let Some(parent_cumulative_work) = self
            .cumulative_work
            .try_get(rwtxn, &header.prev_blockhash)?
        {
            parent_cumulative_work + header.work()
        } else {
            let err = error::MissingParent {
                block_hash: *block_hash,
                prev_block_hash: header.prev_blockhash,
            };
            return Err(error::PutBlockInfoInner::MissingParent(err).into());
        };
        let () = self
            .bmm_commitments
            .put(rwtxn, block_hash, &block_info.bmm_commitments)?;
        let () = self
            .coinbase_txid
            .put(rwtxn, block_hash, &block_info.coinbase_txid)?;
        let () = self
            .cumulative_work
            .put(rwtxn, block_hash, &cumulative_work)?;
        let () = self.events.put(rwtxn, block_hash, &block_info.events)?;
        Ok(())
    }

    /// Iterate over existing ancestor headers, including the provided block
    /// hash, if it exists in the DB.
    /// Note that ancestor headers may not exist in the DB.
    pub fn ancestor_headers<'a>(
        &'a self,
        rotxn: &'a RoTxn<'_>,
        mut block_hash: BlockHash,
    ) -> impl FallibleIterator<Item = (BlockHash, Header), Error = db_error::TryGet> + 'a {
        fallible_iterator::from_fn(move || {
            let header = self.header.try_get(rotxn, &block_hash)?;
            if let Some(header) = header {
                let header_block_hash = block_hash;
                block_hash = header.prev_blockhash;
                Ok(Some((header_block_hash, header)))
            } else {
                Ok(None)
            }
        })
    }

    /// Find the last common ancestor of two blocks,
    /// if headers for both exist
    pub fn last_common_ancestor(
        &self,
        rotxn: &RoTxn,
        block_hash0: BlockHash,
        block_hash1: BlockHash,
    ) -> Result<BlockHash, error::LastCommonAncestor> {
        use std::cmp::Ordering;
        // if the block hashes are the same, return early
        if block_hash0 == block_hash1 {
            return Ok(block_hash0);
        }
        if block_hash0 == BlockHash::all_zeros() || block_hash1 == BlockHash::all_zeros() {
            return Ok(BlockHash::all_zeros());
        }
        let height0 = self.height.get(rotxn, &block_hash0)?;
        let height1 = self.height.get(rotxn, &block_hash1)?;
        let mut ancestors0 = self.ancestor_headers(rotxn, block_hash0);
        let mut ancestors1 = self.ancestor_headers(rotxn, block_hash1);
        // Equalize heights of the ancestors iterators to min(height0, height1)
        // by skipping elements of the longer ancestry
        match height0.cmp(&height1) {
            Ordering::Equal => (),
            Ordering::Less => {
                for depth in 0..height1 - height0 {
                    if ancestors1.next()?.is_none() {
                        return Err(error::LastCommonAncestor::MissingAncestor {
                            block_hash: block_hash1,
                            depth,
                            height: height1,
                        });
                    };
                }
            }
            Ordering::Greater => {
                for depth in 0..height0 - height1 {
                    if ancestors0.next()?.is_none() {
                        return Err(error::LastCommonAncestor::MissingAncestor {
                            block_hash: block_hash0,
                            depth,
                            height: height0,
                        });
                    };
                }
            }
        }
        for common_depth in 0..=std::cmp::min(height0, height1) {
            let Some((ancestor0, _)) = ancestors0.next()? else {
                let current_height = std::cmp::min(height0, height1) - common_depth;
                return Err(error::LastCommonAncestor::MissingAncestor {
                    block_hash: block_hash0,
                    depth: height0 - current_height,
                    height: height0,
                });
            };
            let Some((ancestor1, _)) = ancestors1.next()? else {
                let current_height = std::cmp::min(height0, height1) - common_depth;
                return Err(error::LastCommonAncestor::MissingAncestor {
                    block_hash: block_hash1,
                    depth: height1 - current_height,
                    height: height1,
                });
            };
            if ancestor0 == ancestor1 {
                return Ok(ancestor0);
            }
        }
        Ok(BlockHash::all_zeros())
    }

    pub fn try_get_header_info(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
    ) -> Result<Option<HeaderInfo>, error::TryGetHeaderInfo> {
        let Some(header) = self.header.try_get(rotxn, block_hash)? else {
            return Ok(None);
        };
        assert_eq!(header.block_hash(), *block_hash);
        let Some(height) = self.height.try_get(rotxn, block_hash)? else {
            let err = db_error::InconsistentDbs::new(block_hash, &self.header, &self.height);
            return Err(error::TryGetHeaderInfo::InconsistentDbs(err));
        };
        let header_info = HeaderInfo {
            block_hash: header.block_hash(),
            prev_block_hash: header.prev_blockhash,
            height,
            work: header.work(),
            timestamp: header.time,
        };
        Ok(Some(header_info))
    }

    pub fn get_header_info(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
    ) -> Result<HeaderInfo, error::GetHeaderInfo> {
        self.try_get_header_info(rotxn, block_hash)?.ok_or_else(|| {
            error::MissingHeader {
                block_hash: *block_hash,
            }
            .into()
        })
    }

    /// Get header infos for the specified block hash, and up to max_ancestors
    /// ancestors.
    /// Returns header infos newest-first.
    pub fn try_get_header_infos(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
        max_ancestors: usize,
    ) -> Result<Option<NonEmpty<HeaderInfo>>, error::TryGetHeaderInfo> {
        let Some(header_info) = self.try_get_header_info(rotxn, block_hash)? else {
            return Ok(None);
        };
        let mut ancestors = max_ancestors;
        let mut ancestor = header_info.prev_block_hash;
        let mut res = NonEmpty::new(header_info);
        while ancestors > 0 && ancestor != BlockHash::all_zeros() {
            let Some(header_info) = self.try_get_header_info(rotxn, &ancestor)? else {
                break;
            };
            ancestor = header_info.prev_block_hash;
            ancestors -= 1;
            res.push(header_info);
        }
        Ok(Some(res))
    }

    pub fn try_get_block_info(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
    ) -> Result<Option<BlockInfo>, error::TryGetBlockInfo> {
        let Some(bmm_commitments) = self.bmm_commitments.try_get(rotxn, block_hash)? else {
            return Ok(None);
        };
        let Some(coinbase_txid) = self.coinbase_txid.try_get(rotxn, block_hash)? else {
            let err = db_error::InconsistentDbs::new(
                block_hash,
                &self.bmm_commitments,
                &self.coinbase_txid,
            );
            return Err(error::TryGetBlockInfo::InconsistentDbs(err));
        };
        let Some(events) = self.events.try_get(rotxn, block_hash)? else {
            let err =
                db_error::InconsistentDbs::new(block_hash, &self.bmm_commitments, &self.events);
            return Err(error::TryGetBlockInfo::InconsistentDbs(err));
        };
        let block_info = BlockInfo {
            bmm_commitments,
            coinbase_txid,
            events,
        };
        Ok(Some(block_info))
    }

    pub fn get_block_info(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
    ) -> Result<BlockInfo, error::GetBlockInfo> {
        self.try_get_block_info(rotxn, block_hash)?.ok_or_else(|| {
            error::GetBlockInfo::MissingValue {
                block_hash: *block_hash,
            }
        })
    }

    /// Get two way peg data for a single block
    pub fn try_get_two_way_peg_data(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
    ) -> Result<Option<TwoWayPegData>, error::TryGetTwoWayPegData> {
        let Some(header_info) = self.try_get_header_info(rotxn, block_hash)? else {
            return Ok(None);
        };
        let Some(block_info) = self.try_get_block_info(rotxn, block_hash)? else {
            return Ok(None);
        };
        let res = TwoWayPegData {
            header_info,
            block_info,
        };
        Ok(Some(res))
    }

    pub fn get_two_way_peg_data_range(
        &self,
        rotxn: &RoTxn,
        start_block: Option<BlockHash>,
        end_block: BlockHash,
    ) -> Result<Vec<TwoWayPegData>, error::GetTwoWayPegDataRange> {
        let mut res = Vec::new();
        let Some(two_way_peg_data) = self
            .try_get_two_way_peg_data(rotxn, &end_block)
            .map_err(error::GetTwoWayPegDataRange::TryGetTwoWayPegData)?
        else {
            return Err(error::GetTwoWayPegDataRange::EndBlockNotFound { end_block });
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
                    return Err(error::GetTwoWayPegDataRange::StartBlockNotAncestor {
                        start_block,
                        end_block,
                    });
                } else {
                    break;
                }
            }
            let Some(two_way_peg_data) = self
                .try_get_two_way_peg_data(rotxn, &current_block)
                .map_err(error::GetTwoWayPegDataRange::TryGetTwoWayPegData)?
            else {
                return Err(error::GetTwoWayPegDataRange::PreviousBlockNotFound {
                    block: current_block,
                    prev_block,
                });
            };
            prev_block = current_block;
            current_block = two_way_peg_data.header_info.prev_block_hash;
            res.push(two_way_peg_data);
        }
        res.reverse();
        Ok(res)
    }
}
