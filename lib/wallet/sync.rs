//! Wallet synchronization

use std::time::SystemTime;

use async_lock::RwLockWriteGuard;
use bdk_electrum::electrum_client::ElectrumApi;
use bdk_esplora::EsploraAsyncExt as _;
use either::Either;
use miette::{miette, IntoDiagnostic};
use tokio::time::Instant;

use crate::{
    types::WithdrawalBundleEventKind,
    wallet::{
        error,
        util::{RwLockUpgradableReadGuardSome, RwLockWriteGuardSome},
        BdkWallet, Persistence, PersistenceError, WalletInner,
    },
};

/// Write-locked last_sync, wallet, and database
#[must_use]
pub(in crate::wallet) struct SyncWriteGuard<'a> {
    database: tokio::sync::MutexGuard<'a, Persistence>,
    last_sync: RwLockWriteGuard<'a, Option<SystemTime>>,
    pub(in crate::wallet) wallet: RwLockWriteGuardSome<'a, BdkWallet>,
}

impl SyncWriteGuard<'_> {
    /// Persist changes from the sync
    pub(in crate::wallet) async fn commit(mut self) -> Result<(), PersistenceError> {
        self.wallet
            .with_mut(|wallet| wallet.persist_async(&mut self.database))
            .await?;
        *self.last_sync = Some(SystemTime::now());
        Ok(())
    }
}

const ESPLORA_PARALLEL_REQUESTS: usize = 25;

impl WalletInner {
    pub(in crate::wallet) async fn handle_connect_block(
        &self,
        block: &bitcoin::Block,
        block_height: u32,
        block_info: crate::types::BlockInfo,
    ) -> Result<(), error::ConnectBlock> {
        // Acquire a wallet lock immediately, so that it does not update
        // while other dbs are being written to
        let mut wallet_write = self.write_wallet().await?;
        let finalized_withdrawal_bundles =
            block_info
                .withdrawal_bundle_events()
                .filter_map(|event| match event.kind {
                    WithdrawalBundleEventKind::Failed
                    | WithdrawalBundleEventKind::Succeeded {
                        sequence_number: _,
                        transaction: _,
                    } => Some((event.sidechain_id, event.m6id)),
                    WithdrawalBundleEventKind::Submitted => None,
                });
        let () = self
            .delete_bundle_proposals(finalized_withdrawal_bundles)
            .await?;
        let sidechain_proposal_ids = block_info
            .sidechain_proposals()
            .map(|(_vout, proposal)| proposal.compute_id());
        let () = self
            .delete_pending_sidechain_proposals(sidechain_proposal_ids)
            .await?;
        let mut database = self.bdk_db.lock().await;
        let () = wallet_write.with_mut(|wallet| wallet.apply_block(block, block_height))?;
        let _: bool = wallet_write
            .with_mut(|wallet| wallet.persist_async(&mut database))
            .await
            .map_err(error::ConnectBlock::from)?;
        drop(wallet_write);
        Ok(())
    }

    /// Sync the wallet, returning a write guard on last_sync, wallet, and database
    /// if wallet was not locked.
    /// Does not commit changes.
    #[allow(clippy::significant_drop_in_scrutinee, reason = "false positive")]
    pub(in crate::wallet) async fn sync_lock(
        &self,
    ) -> Result<Option<SyncWriteGuard>, error::WalletSync> {
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
                    tracing::trace!("sync: skipping sync due to wallet error");
                    return Ok(None);
                }
            }
        };
        tracing::trace!("Acquired upgradable read lock on wallet");
        let last_sync_write = self.last_sync.write().await;
        let request = wallet_read.start_sync_with_revealed_spks().build();

        tracing::trace!(
            spks = request.progress().spks_remaining,
            txids = request.progress().txids_remaining,
            outpoints = request.progress().outpoints_remaining,
            "Requesting sync via chain source"
        );
        let (source, update) = match &self.chain_source {
            Either::Left(electrum_client) => {
                const BATCH_SIZE: usize = 5;
                const FETCH_PREV_TXOUTS: bool = false;
                (
                    "electrum",
                    electrum_client.sync(request, BATCH_SIZE, FETCH_PREV_TXOUTS)?,
                )
            }

            Either::Right(esplora_client) => (
                "esplora",
                esplora_client
                    .sync(request, ESPLORA_PARALLEL_REQUESTS)
                    .await?,
            ),
        };
        tracing::trace!("Fetched update from {source}, applying update");
        // Upgrade wallet lock
        let mut wallet_write = RwLockUpgradableReadGuardSome::upgrade(wallet_read).await;
        wallet_write.with_mut(|wallet| wallet.apply_update(update))?;
        tracing::debug!(
            "wallet sync complete in {:?}",
            start.elapsed().unwrap_or_default(),
        );
        Ok(Some(SyncWriteGuard {
            database: self.bdk_db.lock().await,
            last_sync: last_sync_write,
            wallet: wallet_write,
        }))
    }

    async fn address_has_txs(&self, address: &bitcoin::Address) -> miette::Result<bool> {
        let res = match &self.chain_source {
            Either::Left(electrum_client) => electrum_client
                .inner
                .script_get_history(&address.script_pubkey())
                .map(|txs| !txs.is_empty())
                .map_err(|err| miette!(err)),

            Either::Right(esplora_client) => esplora_client
                .get_address_txs(address, None)
                .await
                .map(|txs| !txs.is_empty())
                .map_err(|err| miette!(err)),
        };
        res.map_err(|err| miette!("failed to get address txs for `{address}`: {err:#}`"))
    }

    // TODO: is this actually correct? Need help from the Rust grownups!
    #[allow(clippy::significant_drop_tightening, reason = "false positive")]
    pub(in crate::wallet) async fn full_scan(&self) -> miette::Result<()> {
        tracing::info!("starting wallet full scan");

        let mut start = SystemTime::now();

        let wallet_read = self.read_wallet_upgradable().await?;
        let mut reveal_map = std::collections::HashMap::new();

        for (keychain, _) in wallet_read.spk_index().keychains() {
            let mut last_used_index = 0;
            let step = 1000;

            // First find upper bound by incrementing by 1000 until we find unused
            loop {
                let address = wallet_read.peek_address(keychain, last_used_index);
                let has_txs = self.address_has_txs(&address).await?;

                if !has_txs {
                    break;
                }
                last_used_index += step;
            }

            // Now binary search between last_used_index - step and last_used_index
            let mut high = last_used_index;
            let mut low = last_used_index.saturating_sub(step);

            while low < high {
                let mid = low + (high - low) / 2;
                let address = wallet_read.peek_address(keychain, mid);
                let has_txs = self.address_has_txs(&address).await?;

                if !has_txs {
                    high = mid;
                } else {
                    low = mid + 1;
                }
            }

            tracing::info!(
                "Found last used address at index {} for keychain {:?}: {} (next: {})",
                low.saturating_sub(1),
                keychain,
                wallet_read.peek_address(keychain, low.saturating_sub(1)),
                wallet_read.peek_address(keychain, low)
            );

            reveal_map.insert(keychain, low);
        }

        // Now upgrade to write lock and reveal all addresses
        let mut wallet_write = RwLockUpgradableReadGuardSome::upgrade(wallet_read).await;

        for (keychain, index) in reveal_map {
            // Reveal the addresses, so that when we persist later the wallet
            // will know which index we're at.
            let _addresses =
                wallet_write.with_mut(|wallet| wallet.reveal_addresses_to(keychain, index));
        }

        // TODO: even a simple revealed SPK scan results in long-running anchor persistence jobs. Is it
        // possible to pre-populate this?
        let request = wallet_write.start_sync_with_revealed_spks();

        let update = match &self.chain_source {
            Either::Left(electrum_client) => {
                const BATCH_SIZE: usize = 100;
                const FETCH_PREV_TXOUTS: bool = true;
                electrum_client
                    .sync(request, BATCH_SIZE, FETCH_PREV_TXOUTS)
                    .into_diagnostic()?
            }

            Either::Right(esplora_client) => esplora_client
                .sync(request, ESPLORA_PARALLEL_REQUESTS)
                .await
                .into_diagnostic()?,
        };

        tracing::info!(
            "wallet full scan complete in {:?}",
            start.elapsed().unwrap_or_default(),
        );

        start = SystemTime::now();

        let mut bdk_db = self.bitcoin_db.lock().await;

        wallet_write
            .with_mut(|wallet| {
                wallet
                    .apply_update(update)
                    .map(|_| wallet.persist(&mut bdk_db))
            })
            .into_diagnostic()?
            .map_err(|err| miette!("failed to persist wallet: {err:#}"))?;
        drop(wallet_write);

        tracing::info!(
            "wallet full scan result persisted in {:?}",
            start.elapsed().unwrap_or_default(),
        );

        Ok(())
    }

    /// Sync the wallet if the wallet is not locked, committing changes
    #[allow(clippy::significant_drop_in_scrutinee, reason = "false positive")]
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
