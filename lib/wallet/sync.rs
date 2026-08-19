//! Wallet synchronization

use std::time::SystemTime;

use bdk_chain::bdk_core;
use bdk_esplora::EsploraAsyncExt as _;
use futures::TryFutureExt;
use tokio::time::Instant;
use tracing::instrument;

use crate::wallet::{
    BdkWallet, ChainSourceClient, Persistence, WalletInner, error,
    locks::FullScanGuard,
    sync_state::SharedSyncState,
    util::{RwLockUpgradableReadGuardSome, RwLockWriteGuardSome},
};

/// Write-locked wallet and database, plus the sync state to stamp on commit.
#[must_use]
pub(in crate::wallet) struct SyncWriteGuard<'a> {
    database: tokio::sync::MutexGuard<'a, Persistence>,
    sync_state: &'a SharedSyncState,
    pub(in crate::wallet) wallet: RwLockWriteGuardSome<'a, BdkWallet>,
}

impl SyncWriteGuard<'_> {
    /// Persist changes from the sync
    #[instrument(skip_all, fields(file = %self.database.file_path.display()))]
    pub(in crate::wallet) async fn commit(mut self) -> Result<(), error::BdkWalletPersist> {
        tracing::trace!("committing wallet DB to file");
        self.wallet
            .with_mut(|wallet| wallet.persist_async(&mut self.database))
            .await
            .map_err(|err| error::BdkWalletPersist {
                file_path: self.database.file_path.clone(),
                source: err,
            })?;
        self.sync_state.mark_synced_now();
        Ok(())
    }
}

const ESPLORA_PARALLEL_REQUESTS: usize = 25;

/// Number of consecutive unused addresses that a full scan must observe before
/// considering a keychain exhausted. Larger than the BIP44 gap limit of 20,
/// since a recovered seed may have been used by a wallet that hands out
/// addresses without requiring them to be used.
const STOP_GAP: usize = 200;

impl WalletInner {
    pub(in crate::wallet) async fn get_tip(&self) -> Result<bdk_core::BlockId, error::NotUnlocked> {
        let wallet = self.read_wallet().await?;

        Ok(wallet.local_chain().tip().block_id())
    }

    #[instrument(skip_all, fields(block_height))]
    pub(in crate::wallet) async fn handle_connect_block(
        &self,
        block: &bitcoin::Block,
        block_height: u32,
        block_info: &crate::types::BlockInfo,
    ) -> Result<Result<(), bdk_chain::local_chain::CannotConnectError>, error::HandleConnectBlock>
    {
        // Acquire a wallet lock immediately, so that it does not update
        // while other dbs are being written to
        let mut wallet_write = self.write_wallet().await?;
        let () = self
            .producer
            .apply_connected_block_policy(block_info)
            .await?;
        let mut database = self.locks.db(&wallet_write).await;
        tracing::trace!("applying block to BDK wallet");

        // `apply_block` mutates the in-memory `LocalChain` immediately, so it must
        // never advance the wallet without the matching `persist_async` also being
        // issued against the applied state. Bind the `persist_async` future inside
        // the same `with_mut` as `apply_block` so the in-memory apply and its
        // persistence are a single, indivisible step under the held wallet and
        // `bdk_db` locks -- there is no point at which the wallet can be advanced in
        // memory but left without a pending persist of that advance. This mirrors
        // how `full_scan` and `accept_tx` couple `apply_*` with `persist_async`.
        let persist = match wallet_write.with_mut(|wallet| {
            wallet
                .apply_block(block, block_height)
                .map(|()| wallet.persist_async(&mut database))
        }) {
            Ok(persist) => persist,
            Err(err) => return Ok(Err(err)),
        };
        let persisted_changed = persist.await.map_err(error::HandleConnectBlock::from)?;

        tracing::trace!(
            "applied block {} in persisted changes to BDK wallet",
            if persisted_changed {
                "resulted"
            } else {
                "did NOT result"
            }
        );
        drop(wallet_write);
        Ok(Ok(()))
    }

    pub(in crate::wallet) fn set_last_synced_now(&self) {
        self.sync_state.mark_synced_now();
    }

    /// The wallet's recorded birthday height, if any. Read errors are
    /// downgraded to `None`. The fallback is merely a slower (full) sync.
    pub(in crate::wallet) async fn birthday_height(&self) -> Option<u32> {
        match self.seed_store.read_birthday_height().await {
            Ok(birthday_height) => birthday_height,
            Err(err) => {
                tracing::warn!(
                    "failed to read wallet birthday; syncing from genesis: {:#}",
                    crate::errors::ErrorChain::new(&err)
                );
                None
            }
        }
    }

    /// Fast-forward the wallet's local chain up to `up_to_height` (or the
    /// validator tip) by applying one checkpoint update built from validator
    /// headers, instead of connecting every missing block individually.
    pub(in crate::wallet) async fn fast_forward_chain(
        &self,
        up_to_height: Option<u32>,
    ) -> Result<(), error::FullScan> {
        let start = Instant::now();
        let mut wallet_write = self.write_wallet().await?;
        let mut bdk_db = self.locks.db(&wallet_write).await;

        let checkpoint = {
            let local_chain = wallet_write.local_chain();
            self.get_chain_checkpoint(local_chain, up_to_height).await?
        };
        let tip_height = checkpoint.height();

        let update = bdk_wallet::Update {
            chain: Some(checkpoint),
            ..Default::default()
        };
        wallet_write
            .with_mut(|wallet| {
                wallet
                    .apply_update(update)
                    .map(|()| wallet.persist_async(&mut bdk_db))
            })
            .map_err(error::FullScan::CannotConnect)?
            .await
            .map_err(|err| error::FullScan::PersistWallet(error::SqliteError::from(err)))?;

        drop(wallet_write);
        tracing::info!(
            %tip_height,
            "fast-forwarded wallet chain to tip in {:?}",
            start.elapsed()
        );
        Ok(())
    }
    /// Sync the wallet, returning a write guard on last_sync, wallet, and database
    /// if wallet was not locked.
    /// Does not commit changes.
    pub(in crate::wallet) async fn sync_lock(
        &self,
    ) -> Result<Option<SyncWriteGuard<'_>>, error::WalletSync> {
        let start = SystemTime::now();
        tracing::trace!("starting wallet sync");
        // Hold an upgradable lock for the duration of the sync, to prevent other
        // updates to the wallet between fetching an update via the chain source,
        // and applying the update.
        // Don't error out here if the wallet is locked, just skip the sync.
        let wallet_read = {
            match self.read_wallet_upgradable().await {
                Ok(wallet_read) => wallet_read,
                // "Accepted" errors, that aren't really errors in this case.
                Err(error::NotUnlocked) => {
                    tracing::trace!("skipping sync due to wallet error");
                    return Ok(None);
                }
            }
        };
        tracing::trace!("acquired upgradable read lock on wallet");
        let request = wallet_read.start_sync_with_revealed_spks().build();

        tracing::trace!(
            spks = request.progress().spks_remaining,
            txids = request.progress().txids_remaining,
            outpoints = request.progress().outpoints_remaining,
            "Requesting sync via chain source"
        );
        let Some(chain_source_client) = self.chain_source_client() else {
            tracing::info!("no sync source client available, aborting sync");
            return Ok(None);
        };
        let (chain_source, update) = match chain_source_client.as_ref() {
            ChainSourceClient::Electrum(electrum_client) => {
                const BATCH_SIZE: usize = 5;
                const FETCH_PREV_TXOUTS: bool = false;
                let result = electrum_client.sync(request, BATCH_SIZE, FETCH_PREV_TXOUTS);
                ("electrum", self.record_sync_backend_result(result)?)
            }
            ChainSourceClient::Esplora(esplora_client) => {
                let result = esplora_client
                    .sync(request, ESPLORA_PARALLEL_REQUESTS)
                    .await;
                ("esplora", self.record_sync_backend_result(result)?)
            }
        };
        tracing::trace!("Fetched update from {chain_source}");
        if let Some(chain_update) = update.chain_update {
            // The wallet chain should never be updated here.
            // Sync should only ever update txs
            tracing::debug!(
                checkpoint_block_hash = %chain_update.hash(),
                checkpoint_height = chain_update.height(),
                "Aborting wallet sync to new checkpoint",
            );
            return Ok(None);
        }

        tracing::trace!("applying update");
        // Upgrade wallet lock
        let mut wallet_write = RwLockUpgradableReadGuardSome::upgrade(wallet_read).await;
        wallet_write.with_mut(|wallet| wallet.apply_update(update))?;
        // The update re-adopts any stale BMM request that still sits in the
        // chain source's mempool view, re-locking its inputs. Evict again
        // before the update is committed.
        if let Ok(mainchain_tip) = self.validator().get_mainchain_tip() {
            let _stale = wallet_write
                .with_mut(|wallet| super::Wallet::evict_stale_bmm_requests(wallet, mainchain_tip));
        }
        tracing::debug!(
            "wallet sync complete in {:?}",
            start.elapsed().unwrap_or_default(),
        );
        let database = self.locks.db(&wallet_write).await;
        Ok(Some(SyncWriteGuard {
            database,
            sync_state: &self.sync_state,
            wallet: wallet_write,
        }))
    }

    async fn get_chain_checkpoint(
        &self,
        local_chain: &bdk_chain::local_chain::LocalChain,
        up_to_height: Option<u32>,
    ) -> miette::Result<bdk_chain::CheckPoint, error::FullScan> {
        let start = Instant::now();
        let mut headers = self
            .validator()
            .list_headers(local_chain.tip().height())
            .map_err(error::FullScan::ListHeaders)?;
        if let Some(up_to_height) = up_to_height {
            headers.retain(|(height, _)| *height <= up_to_height);
        }

        tracing::debug!(
            "listed {} headers since height {} in {:?}: {} -> {}",
            headers.len(),
            local_chain.tip().height(),
            start.elapsed(),
            headers
                .first()
                .map(|(height, hash)| format!("{height}:{hash}"))
                .unwrap_or("nil".to_string()),
            headers
                .last()
                .map(|(height, hash)| format!("{height}:{hash}"))
                .unwrap_or("nil".to_string()),
        );

        // Extend the wallet's existing checkpoint rather than building a fresh
        // chain that starts at its tip. A sync inserts a `BlockId` for every
        // transaction it finds, and `CheckPoint::insert` panics ("will break
        // before genesis block") if an insert lands below the lowest checkpoint
        // it was handed. A chain rebuilt from the tip has no history below it,
        // so the wallet's own older transactions are exactly what blows it up.
        let tip = local_chain.tip();
        let tip_height = tip.height();
        let block_ids = headers
            .into_iter()
            // Anything at or below the tip is already covered by the checkpoint
            // being extended, and `extend` requires strictly ascending heights.
            .filter(|(height, _)| *height > tip_height)
            .map(|(height, hash)| bdk_chain::BlockId { height, hash });

        let checkpoint = tip.extend(block_ids).map_err(|last_successful_header| {
            error::FullScan::CreateCheckPointFromHeaders {
                last_successful_header: Some(last_successful_header),
            }
        })?;
        Ok(checkpoint)
    }

    /// Full scan the wallet, waiting for any scan already in flight to
    /// finish first. For callers that must not be turned away, such as the
    /// gap path in `sync_wallet_to_tip`.
    pub(in crate::wallet) async fn full_scan(
        &self,
    ) -> miette::Result<bdk_wallet::bitcoin::BlockHash, error::FullScan> {
        let scan_slot = self.locks.full_scan().await;
        self.full_scan_guarded(scan_slot).await
    }

    /// Full scan the wallet, failing with [`error::FullScan::ScanInProgress`]
    /// if a scan is already running. For on-demand callers: a scan holds the
    /// wallet across minutes of network I/O and the enforcer's block
    /// connection path needs that wallet, so queueing scans would let a
    /// caller keep blocks from being connected for as long as it keeps
    /// asking.
    pub(in crate::wallet) async fn try_full_scan(
        &self,
    ) -> miette::Result<bdk_wallet::bitcoin::BlockHash, error::FullScan> {
        let scan_slot = self
            .locks
            .try_full_scan()
            .ok_or(error::FullScan::ScanInProgress)?;
        self.full_scan_guarded(scan_slot).await
    }

    #[expect(clippy::significant_drop_tightening, reason = "false positive")]
    async fn full_scan_guarded(
        &self,
        _scan_slot: FullScanGuard<'_>,
    ) -> miette::Result<bdk_wallet::bitcoin::BlockHash, error::FullScan> {
        tracing::info!("starting wallet full scan");

        // Errors when no chain source client is available: disabled by
        // config, or the backend has not been reached yet. The caller checks
        // availability first and falls back to block-by-block replay
        // otherwise, so this is a backstop rather than a wait.
        let chain_source_client =
            self.chain_source_client()
                .ok_or(error::FullScan::InvalidSyncSource {
                    sync_source: self.config.wallet_opts.sync_source,
                })?;

        // Applying the scan requires a mainchain tip to judge stale BMM
        // requests against (see below). Check for one before spending minutes
        // on a scan that cannot be persisted. The tip advances during the
        // scan, so the value used there is re-read rather than carried down.
        let _: bitcoin::BlockHash = self
            .validator()
            .try_get_mainchain_tip()
            .map_err(error::FullScan::TryGetMainchainTip)?
            .ok_or(error::FullScan::NoMainchainTip)?;

        let mut start = SystemTime::now();

        let wallet_read = self
            .read_wallet_upgradable()
            .await
            .map_err(error::FullScan::WalletNotUnlocked)?;

        let checkpoint = {
            let local_chain = wallet_read.local_chain();
            self.get_chain_checkpoint(local_chain, None).await?
        };
        // Scan every keychain from index 0, stopping only once `STOP_GAP`
        // consecutive unused addresses have been seen. Addresses are not
        // necessarily used in index order, so searching for the first unused
        // index, and revealing/syncing only up to it, misses addresses that are
        // funded after a gap.
        // Keeping already revealed SPKs, wallet txids and wallet UTXOs up to
        // date remains the job of `sync`.
        let request = wallet_read.start_full_scan().chain_tip(checkpoint);

        let update = {
            let result = match chain_source_client.as_ref() {
                ChainSourceClient::Electrum(electrum_client) => {
                    const BATCH_SIZE: usize = 100;
                    const FETCH_PREV_TXOUTS: bool = true;
                    electrum_client
                        .full_scan(request, STOP_GAP, BATCH_SIZE, FETCH_PREV_TXOUTS)
                        .map_err(error::ChainSourceClient::Electrum)
                }

                ChainSourceClient::Esplora(esplora_client) => {
                    esplora_client
                        .full_scan(request, STOP_GAP, ESPLORA_PARALLEL_REQUESTS)
                        .map_err(|err| error::ChainSourceClient::Esplora(*err))
                        .await
                }
            };
            self.record_sync_backend_result(result)?
        };

        tracing::info!(
            "wallet full scan complete in {:?}, last active indexes: {:?}",
            start.elapsed().unwrap_or_default(),
            update.last_active_indices,
        );

        start = SystemTime::now();

        // Applying the update reveals addresses up to the last active index of
        // each keychain, so that the persist below records which index we're at.
        let mut wallet_write = RwLockUpgradableReadGuardSome::upgrade(wallet_read).await;
        let mut bdk_db = self.locks.db(&wallet_write).await;

        // A full scan re-adopts stale BMM requests still in the chain
        // source's mempool view. Evict them again before persisting.
        // Without a tip there is nothing to judge staleness against, and
        // persisting the update anyway would commit the re-adopted requests
        // and re-lock their inputs: fail instead of skipping the eviction.
        let mainchain_tip = self
            .validator()
            .try_get_mainchain_tip()
            .map_err(error::FullScan::TryGetMainchainTip)?
            .ok_or(error::FullScan::NoMainchainTip)?;
        wallet_write
            .with_mut(|wallet| {
                wallet.apply_update(update).map(|_| {
                    let _stale = super::Wallet::evict_stale_bmm_requests(wallet, mainchain_tip);
                    wallet.persist_async(&mut bdk_db)
                })
            })
            .map_err(error::FullScan::CannotConnect)?
            .await
            .map_err(|err| error::FullScan::PersistWallet(error::SqliteError::from(err)))?;

        let tip = wallet_write.local_chain().tip().hash();

        drop(wallet_write);

        tracing::info!(
            "wallet full scan result persisted in {:?}",
            start.elapsed().unwrap_or_default(),
        );

        Ok(tip)
    }

    /// Sync the wallet if the wallet is not locked, committing changes
    pub(in crate::wallet) async fn sync(&self) -> Result<(), error::WalletSync> {
        match self.sync_lock().await? {
            Some(sync_write) => {
                let start = Instant::now();
                tracing::trace!("obtained sync lock, committing changes");
                let () = sync_write.commit().await?;
                tracing::trace!("sync lock commit complete in {:?}", start.elapsed());
                Ok(())
            }
            None => {
                tracing::trace!("no sync lock, skipping commit");
                Ok(())
            }
        }
    }
}
