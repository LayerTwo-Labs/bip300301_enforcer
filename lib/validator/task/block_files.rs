use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
};

use bitcoin::{Block, BlockHash, Network, hashes::Hash};
use sneed::RwTxn;
use tokio_util::sync::CancellationToken;

use crate::{
    types::Event,
    validator::{
        dbs::Dbs,
        parse_block_files,
        task::{error, handle_block_batch},
    },
};

/// Simple LRU-style cache for temporarily storing blocks read from disk
/// that don't match the current expected block hash.
struct BlockCache {
    blocks: HashMap<BlockHash, Block>,
    insertion_order: VecDeque<BlockHash>,
    max_size: usize,
}

impl BlockCache {
    fn new(max_size: usize) -> Self {
        Self {
            blocks: HashMap::new(),
            insertion_order: VecDeque::new(),
            max_size,
        }
    }

    fn insert(&mut self, block_hash: BlockHash, block: Block) {
        // If block already exists, don't insert again
        if self.blocks.contains_key(&block_hash) {
            return;
        }

        // Evict oldest block if at capacity
        while self.blocks.len() >= self.max_size {
            if let Some(oldest_hash) = self.insertion_order.pop_front() {
                self.blocks.remove(&oldest_hash);
                tracing::trace!("Evicted block `{}` from cache", oldest_hash);
            }
        }

        // Insert new block
        self.blocks.insert(block_hash, block);
        self.insertion_order.push_back(block_hash);
        tracing::trace!(
            "Cached block `{}` (cache size: {})",
            block_hash,
            self.blocks.len()
        );
    }

    fn remove(&mut self, block_hash: &BlockHash) -> Option<Block> {
        if let Some(block) = self.blocks.remove(block_hash) {
            // Remove from insertion order
            if let Some(pos) = self.insertion_order.iter().position(|h| h == block_hash) {
                self.insertion_order.remove(pos);
            }
            tracing::trace!("Cache hit for block `{}`", block_hash,);
            Some(block)
        } else {
            None
        }
    }

    fn len(&self) -> usize {
        self.blocks.len()
    }
}

/// Configuration constants for block file parsing with caching
const MAX_BLOCK_CACHE_SIZE: usize = 5000;
const MAX_ITERATIONS_WITHOUT_MATCH: usize = 5000;
const BLOCKS_DIR_CONNECT_BATCH_SIZE: usize = 2000;

/// Check cache for subsequent expected blocks and process them if found.
/// This function iteratively looks for the next expected block in the cache
/// and processes it until no more consecutive blocks are found.
fn process_cached_blocks(
    dbs: &Dbs,
    event_tx: &async_broadcast::Sender<Event>,
    block_cache: &mut BlockCache,
    missing_blocks: &mut Vec<BlockHash>,
    pending_blocks: &mut Vec<Block>,
    total_handled_blocks: &mut usize,
) -> Result<(), error::Sync> {
    while let Some(&expected_block_hash) = missing_blocks.last() {
        if let Some(cached_block) = block_cache.remove(&expected_block_hash) {
            // Found next expected block in cache
            pending_blocks.push(cached_block);
            missing_blocks.pop();

            // Check if we should process batch
            if pending_blocks.len() >= BLOCKS_DIR_CONNECT_BATCH_SIZE || missing_blocks.is_empty() {
                let mut rwtxn: RwTxn<'_> = dbs.write_txn()?;
                handle_block_batch(dbs, &mut rwtxn, pending_blocks, event_tx)?;
                rwtxn.commit()?;

                *total_handled_blocks += pending_blocks.len();
                pending_blocks.clear();
            }
        } else {
            // Next expected block not in cache, break out
            break;
        }
    }

    Ok(())
}

/// Sync blocks by reading from the raw block files. The idea is to mutate the missing
/// blocks vector, such that once this function returns the JSON-RPC sync logic can
/// finish off the job. Syncing from the block directory is not expected to work all
/// until the end due to Core having blocks it hasn't flushed to disk.
///
/// Handles non-sequential block layout by temporarily caching unexpected blocks
/// and checking the cache when expected blocks are found.
#[tracing::instrument(skip_all)]
pub fn sync_from_directory(
    dbs: &Dbs,
    event_tx: &async_broadcast::Sender<Event>,
    missing_blocks: &mut Vec<BlockHash>,
    main_blocks_dir: PathBuf,
    network: Network,
    cancel: CancellationToken,
) -> Result<u32, error::Sync> {
    let first_missing_block = *missing_blocks.last().expect("missing blocks is empty");

    let index_path = main_blocks_dir.join("index");
    tracing::debug!(
        "fetching `{first_missing_block}` from block index at {}",
        index_path.display()
    );

    let mut parser = parse_block_files::BlockDirectoryParser::new(main_blocks_dir, network)?;

    let is_genesis_block =
        first_missing_block.to_raw_hash().as_byte_array() == network.chain_hash().as_bytes();

    // There's no point in fetching the block index for the genesis block, it's always at file number 0.
    if !is_genesis_block {
        let block_index = parse_block_files::fetch_block_index(index_path, first_missing_block)?;

        let block_index_file_number = block_index
            .file_number
            .ok_or(parse_block_files::FetchBlockIndexError::MissingFileNumber)?;

        let block_index_data_pos = block_index
            .adjusted_data_pos()
            .ok_or(parse_block_files::FetchBlockIndexError::MissingDataPos)?;

        // Only blocks which aren't fully validated don't have file numbers
        parser.set_file_number(block_index_file_number);
        parser
            .set_offset(block_index_data_pos)
            .map_err(error::Sync::BlockFileParserSetOffset)?;
    }

    tracing::debug!(
        "starting block file parser at file number {}",
        parser.file_number()
    );

    let mut total_handled_blocks = 0_usize;
    let mut has_found_start = false;
    let mut block_cache = BlockCache::new(MAX_BLOCK_CACHE_SIZE);
    let mut iterations_without_match = 0_usize;

    let mut pending_blocks: Vec<Block> = vec![];

    let target_end_height = dbs.block_hashes.height().get(
        &dbs.read_txn()?,
        missing_blocks.first().expect("missing blocks is empty"),
    )?;

    tracing::debug!("identified target end height: {target_end_height}");

    tracing::info!(
        "Starting block sync from files with cache (max size: {}, max iterations without match: {})",
        MAX_BLOCK_CACHE_SIZE,
        MAX_ITERATIONS_WITHOUT_MATCH
    );

    let mut iteration_count = 0;
    loop {
        // Check for shutdown every 100 iterations to avoid too much overhead
        if iteration_count % 100 == 0 && cancel.is_cancelled() {
            tracing::info!("Block file sync interrupted during processing");
            // Process any remaining blocks in the current batch before aborting
            if !pending_blocks.is_empty() {
                tracing::debug!(
                    "syncing pending batch of {} blocks before shutdown",
                    pending_blocks.len()
                );
                let mut rwtxn = dbs.write_txn()?;
                handle_block_batch(dbs, &mut rwtxn, &pending_blocks, event_tx)?;
                rwtxn.commit()?;
            }
            return Err(error::Sync::Shutdown);
        }
        iteration_count += 1;

        let block = match parser.next() {
            Some(Ok(block)) => block,
            Some(Err(e)) => return Err(e.into()),
            None => break,
        };

        if !has_found_start {
            if block.header.block_hash() != first_missing_block {
                // Cache this block as it might be needed later - it could be one of children of
                // the block we're looking for
                let full_block = Block {
                    header: block.header,
                    txdata: block.parse_tx_data()?,
                };
                block_cache.insert(block.header.block_hash(), full_block);

                tracing::debug!(
                    "Expected block `{}` but got `{}` from file at byte offset {}, caching and continuing",
                    first_missing_block,
                    block.header.block_hash(),
                    block.offset
                );
                continue;
            }
            tracing::debug!(
                "Found first missing block at file number {} and byte offset {}: `{}`",
                parser.file_number(),
                block.offset,
                block.header.block_hash()
            );
            has_found_start = true;
        }

        let expected_block_hash = *missing_blocks.last().expect("missing blocks is empty");

        let current_block_hash = block.header.block_hash();

        if current_block_hash == expected_block_hash {
            // Found the expected block!
            tracing::trace!(
                "Found expected block `{}` at file offset {}",
                expected_block_hash,
                block.offset
            );

            iterations_without_match = 0;

            // Process the expected block
            pending_blocks.push(Block {
                header: block.header,
                txdata: block.parse_tx_data()?,
            });
            missing_blocks.pop();

            // Check if we should process the current batch
            if pending_blocks.len() >= BLOCKS_DIR_CONNECT_BATCH_SIZE || missing_blocks.is_empty() {
                let mut rwtxn = dbs.write_txn()?;
                handle_block_batch(dbs, &mut rwtxn, &pending_blocks, event_tx)?;
                rwtxn.commit()?;

                total_handled_blocks += pending_blocks.len();
                pending_blocks.clear();
            }

            // Check cache for subsequent expected blocks
            process_cached_blocks(
                dbs,
                event_tx,
                &mut block_cache,
                missing_blocks,
                &mut pending_blocks,
                &mut total_handled_blocks,
            )?;
        } else {
            // Block doesn't match expected - cache it
            let full_block = Block {
                header: block.header,
                txdata: block.parse_tx_data()?,
            };

            block_cache.insert(current_block_hash, full_block);
            iterations_without_match += 1;

            // Check abort conditions
            if iterations_without_match >= MAX_ITERATIONS_WITHOUT_MATCH {
                tracing::warn!(
                    "Reached maximum iterations ({}) without finding expected block ({}), aborting file sync",
                    iterations_without_match,
                    expected_block_hash,
                );

                // Process any remaining blocks in the current batch before aborting
                if !pending_blocks.is_empty() {
                    tracing::debug!(
                        "syncing pending batch of {} blocks before aborting blocks dir sync",
                        pending_blocks.len()
                    );
                    let mut rwtxn = dbs.write_txn()?;
                    handle_block_batch(dbs, &mut rwtxn, &pending_blocks, event_tx)?;
                    rwtxn.commit()?;

                    total_handled_blocks += pending_blocks.len();
                    pending_blocks.clear(); // Clear to avoid double processing
                }

                break;
            }

            if iterations_without_match % 1000 == 0 {
                tracing::debug!(
                    "Processed {} blocks without finding expected `{}`, cache size: {}, file position #{}/{}",
                    iterations_without_match,
                    expected_block_hash,
                    block_cache.len(),
                    parser.file_number(),
                    block.offset,
                );
            }
        }
    }

    // Process any remaining blocks in the pending batch
    if !pending_blocks.is_empty() {
        tracing::debug!(
            "handling final batch of {} blocks at end of file sync",
            pending_blocks.len()
        );
        let mut rwtxn = dbs.write_txn()?;
        handle_block_batch(dbs, &mut rwtxn, &pending_blocks, event_tx)?;
        rwtxn.commit()?;

        total_handled_blocks += pending_blocks.len();
    }

    tracing::info!(
        "Completed block sync from files: {} blocks processed, {} blocks remaining, cache size: {}",
        total_handled_blocks,
        missing_blocks.len(),
        block_cache.len()
    );

    Ok(total_handled_blocks as u32)
}
