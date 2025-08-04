use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use bitcoin::{BlockHash, Txid, Work, block::Header, hashes::Hash as _};
use fallible_iterator::FallibleIterator;
use heed_types::SerdeBincode;
use nonempty::NonEmpty;
use sneed::{DatabaseDup, DatabaseUnique, Env, RoDatabaseUnique, RoTxn, RwTxn, db, env};
use tracing::instrument;

use crate::types::{
    BlockEvent, BlockInfo, BmmCommitment, BmmCommitments, HeaderInfo, SidechainNumber,
    TwoWayPegData,
};

pub mod error {
    use bitcoin::BlockHash;
    use sneed::db;
    use thiserror::Error;
    use transitive::Transitive;

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

    #[derive(Debug, Error, Transitive)]
    #[transitive(from(db::error::Put, db::Error), from(db::error::TryGet, db::Error))]
    pub(in crate::validator::dbs::block_hashes) enum PutBlockInfoInner {
        #[error(transparent)]
        Db(Box<db::Error>),
        #[error(transparent)]
        MissingHeader(#[from] MissingHeader),
        #[error(transparent)]
        MissingParent(#[from] MissingParent),
    }

    impl From<db::Error> for PutBlockInfoInner {
        fn from(err: db::Error) -> Self {
            Self::Db(Box::new(err))
        }
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

    #[derive(Debug, Error, Transitive)]
    #[transitive(from(db::error::Get, db::Error), from(db::error::TryGet, db::Error))]
    pub(crate) enum LastCommonAncestor {
        #[error(transparent)]
        Db(#[from] db::Error),
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
    pub(crate) enum GetHeaderInfo {
        #[error(transparent)]
        Db(#[from] db::Error),
        #[error(transparent)]
        MissingHeader(#[from] MissingHeader),
    }

    #[derive(Debug, Error)]
    pub(crate) enum GetBlockInfo {
        #[error(transparent)]
        Db(#[from] db::Error),
        #[error("Missing block info for block hash `{block_hash}`")]
        MissingValue { block_hash: BlockHash },
    }

    #[derive(Debug, Error)]
    pub(crate) enum GetTwoWayPegDataRange {
        #[error(transparent)]
        Db(#[from] db::Error),
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
    }
}

#[derive(Clone)]
pub struct BlockHashDbs {
    // All ancestors for each block MUST exist in this DB.
    // All keys in this DB MUST also exist in ALL other DBs.
    bmm_commitments: DatabaseUnique<SerdeBincode<BlockHash>, SerdeBincode<BmmCommitments>>,
    // All ancestors for each block MUST exist in this DB.
    // All keys in this DB MUST also exist in ALL other DBs.
    coinbase_txid: DatabaseUnique<SerdeBincode<BlockHash>, SerdeBincode<Txid>>,
    // All ancestors for each block MUST exist in this DB.
    // All keys in this DB MUST also exist in ALL other DBs.
    cumulative_work: DatabaseUnique<SerdeBincode<BlockHash>, SerdeBincode<Work>>,
    // All ancestors for each block MUST exist in this DB.
    // All keys in this DB MUST also exist in ALL other DBs.
    events: DatabaseUnique<SerdeBincode<BlockHash>, SerdeBincode<Vec<BlockEvent>>>,
    // All keys in this DB MUST also exist in `height`
    header: DatabaseUnique<SerdeBincode<BlockHash>, SerdeBincode<Header>>,
    // All keys in this DB MUST also exist in `header` as keys AND/OR
    // `prev_blockhash` in a value
    height: DatabaseUnique<SerdeBincode<BlockHash>, SerdeBincode<u32>>,
    // Used for determining conflicts for mempool txs.
    // Maps block hash and sidechain number to a set of txids and their h*
    // commitments.
    #[allow(clippy::type_complexity)]
    seen_bmm_request_txs: DatabaseDup<
        SerdeBincode<(BlockHash, SidechainNumber)>,
        SerdeBincode<(Txid, BmmCommitment)>,
    >,
}

impl BlockHashDbs {
    pub const NUM_DBS: u32 = 7;

    pub(super) fn new(env: &Env, rwtxn: &mut RwTxn) -> Result<Self, env::error::CreateDb> {
        let bmm_commitments = DatabaseUnique::create(env, rwtxn, "block_hash_to_bmm_commitments")?;
        let coinbase_txid = DatabaseUnique::create(env, rwtxn, "block_hash_to_coinbase_txid")?;
        let cumulative_work = DatabaseUnique::create(env, rwtxn, "block_hash_to_cumulative_work")?;
        let events = DatabaseUnique::create(env, rwtxn, "block_hash_to_events")?;
        let header = DatabaseUnique::create(env, rwtxn, "block_hash_to_header")?;
        let height = DatabaseUnique::create(env, rwtxn, "block_hash_to_height")?;
        let seen_bmm_request_txs = DatabaseDup::create(env, rwtxn, "seen_bmm_request_txs")?;
        Ok(Self {
            bmm_commitments,
            coinbase_txid,
            cumulative_work,
            events,
            header,
            height,
            seen_bmm_request_txs,
        })
    }

    pub fn bmm_commitments(
        &self,
    ) -> RoDatabaseUnique<SerdeBincode<BlockHash>, SerdeBincode<BmmCommitments>> {
        (*self.bmm_commitments).clone()
    }

    pub fn cumulative_work(&self) -> RoDatabaseUnique<SerdeBincode<BlockHash>, SerdeBincode<Work>> {
        (*self.cumulative_work).clone()
    }

    pub fn height(&self) -> RoDatabaseUnique<SerdeBincode<BlockHash>, SerdeBincode<u32>> {
        (*self.height).clone()
    }

    /// Check if the database contains the provided header
    pub fn contains_header(
        &self,
        rotxn: &RoTxn,
        block_hash: &BlockHash,
    ) -> Result<bool, db::error::TryGet> {
        self.header.contains_key(rotxn, block_hash)
    }

    /// Store info for a single header
    #[instrument(skip_all)]
    pub fn put_headers(
        &self,
        rwtxn: &mut RwTxn,
        headers: &[(Header, u32)],
    ) -> Result<(), db::error::Put> {
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
    ) -> impl FallibleIterator<Item = (BlockHash, Header), Error = db::error::TryGet> + 'a {
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
    ) -> Result<Option<HeaderInfo>, db::Error> {
        let Some(header) = self.header.try_get(rotxn, block_hash)? else {
            return Ok(None);
        };
        assert_eq!(header.block_hash(), *block_hash);
        let Some(height) = self.height.try_get(rotxn, block_hash)? else {
            let err = db::error::inconsistent::Xor::new(
                block_hash,
                db::error::inconsistent::ByKey(&*self.header),
                db::error::inconsistent::ByKey(&*self.height),
            );
            return Err(db::Error::Inconsistent(err.into()));
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
    ) -> Result<Option<NonEmpty<HeaderInfo>>, db::Error> {
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
    ) -> Result<Option<BlockInfo>, db::Error> {
        let Some(bmm_commitments) = self.bmm_commitments.try_get(rotxn, block_hash)? else {
            return Ok(None);
        };
        let Some(coinbase_txid) = self.coinbase_txid.try_get(rotxn, block_hash)? else {
            let err = db::error::inconsistent::Xor::new(
                block_hash,
                db::error::inconsistent::ByKey(&*self.bmm_commitments),
                db::error::inconsistent::ByKey(&*self.coinbase_txid),
            );
            return Err(db::Error::Inconsistent(err.into()));
        };
        let Some(events) = self.events.try_get(rotxn, block_hash)? else {
            let err = db::error::inconsistent::Xor::new(
                block_hash,
                db::error::inconsistent::ByKey(&*self.bmm_commitments),
                db::error::inconsistent::ByKey(&*self.events),
            );
            return Err(db::Error::Inconsistent(err.into()));
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
    ) -> Result<Option<TwoWayPegData>, db::Error> {
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
        let Some(two_way_peg_data) = self.try_get_two_way_peg_data(rotxn, &end_block)? else {
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
            let Some(two_way_peg_data) = self.try_get_two_way_peg_data(rotxn, &current_block)?
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

    /// Get seen BMM requests for a given parent block hash and sidechain slot.
    /// Returns a map of h* commitments to txids.
    pub fn get_seen_bmm_requests(
        &self,
        rotxn: &RoTxn,
        parent_block_hash: BlockHash,
        sidechain_slot: SidechainNumber,
    ) -> Result<HashMap<BmmCommitment, HashSet<Txid>>, db::Error> {
        self.seen_bmm_request_txs
            .get(rotxn, &(parent_block_hash, sidechain_slot))?
            .fold(
                HashMap::<_, HashSet<_>>::new(),
                |mut res, (txid, commitment)| {
                    res.entry(commitment).or_default().insert(txid);
                    Ok(res)
                },
            )
            .map_err(db::Error::from)
    }

    /// Get seen BMM requests for a given parent block hash/
    /// Returns a map of sidechain numbers to h* commitments to txids.
    pub fn get_seen_bmm_requests_for_parent_block(
        &self,
        rotxn: &RoTxn,
        parent_block_hash: BlockHash,
    ) -> Result<HashMap<SidechainNumber, HashMap<BmmCommitment, HashSet<Txid>>>, db::error::Range>
    {
        self.seen_bmm_request_txs
            .range_through_duplicate_values(
                rotxn,
                &((parent_block_hash, SidechainNumber::MIN)
                    ..=(parent_block_hash, SidechainNumber::MAX)),
            )?
            .fold(
                HashMap::<_, HashMap<_, HashSet<_>>>::new(),
                |mut res, ((_parent_block_hash, sidechain_slot), (txid, commitment))| {
                    res.entry(sidechain_slot)
                        .or_default()
                        .entry(commitment)
                        .or_default()
                        .insert(txid);
                    Ok(res)
                },
            )
            .map_err(db::error::Range::from)
    }

    pub fn put_seen_bmm_request(
        &self,
        rwtxn: &mut RwTxn,
        parent_block_hash: BlockHash,
        sidechain_slot: SidechainNumber,
        txid: Txid,
        commitment: BmmCommitment,
    ) -> Result<(), db::error::Put> {
        self.seen_bmm_request_txs.put(
            rwtxn,
            &(parent_block_hash, sidechain_slot),
            &(txid, commitment),
        )
    }
}
