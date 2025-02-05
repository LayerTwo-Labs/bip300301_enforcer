//! Wallet synchronization

use std::time::SystemTime;

use bdk_wallet::{file_store::Store, ChangeSet, FileStoreError};
use parking_lot::{MappedRwLockWriteGuard, MutexGuard, RwLockWriteGuard};

use crate::{
    types::WithdrawalBundleEventKind,
    wallet::{error, BdkWallet, WalletInner},
};

/// Write-locked last_sync, wallet, and database
#[must_use]
pub(in crate::wallet) struct SyncWriteGuard<'a> {
    database: MutexGuard<'a, Store<ChangeSet>>,
    last_sync: RwLockWriteGuard<'a, Option<SystemTime>>,
    pub(in crate::wallet) wallet: MappedRwLockWriteGuard<'a, BdkWallet>,
}

impl SyncWriteGuard<'_> {
    /// Persist changes from the sync
    pub(in crate::wallet) fn commit(mut self) -> Result<(), FileStoreError> {
        self.wallet.persist(&mut self.database)?;
        *self.last_sync = Some(SystemTime::now());
        Ok(())
    }
}

impl WalletInner {
    pub(in crate::wallet) fn handle_connect_block(
        &self,
        block: &bitcoin::Block,
        block_height: u32,
        block_info: crate::types::BlockInfo,
    ) -> Result<(), error::ConnectBlock> {
        // Acquire a wallet lock immediately, so that it does not update
        // while other dbs are being written to
        let mut wallet_write = self.write_wallet()?;
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
        let () = self.delete_bundle_proposals(finalized_withdrawal_bundles)?;
        let sidechain_proposal_ids = block_info
            .sidechain_proposals()
            .map(|(_vout, proposal)| proposal.compute_id());
        let () = self.delete_pending_sidechain_proposals(sidechain_proposal_ids)?;
        wallet_write.apply_block(block, block_height)?;
        let mut database = self.bitcoin_db.lock();
        wallet_write.persist(&mut database)?;
        drop(wallet_write);
        Ok(())
    }

    /// Sync the wallet, returning a write guard on last_sync, wallet, and database
    /// if wallet was not locked.
    /// Does not commit changes.
    pub(in crate::wallet) fn sync_lock(&self) -> Result<Option<SyncWriteGuard>, error::WalletSync> {
        let start = SystemTime::now();
        tracing::trace!("starting wallet sync");
        // Hold a write lock for the duration of the sync, to prevent other
        // updates to the wallet between fetching an update via electrum
        // client, and applying the update.
        // Don't error out here if the wallet is locked, just skip the sync.
        let mut wallet_write = match self.write_wallet() {
            Ok(wallet_write) => wallet_write,
            // "Accepted" errors, that aren't really errors in this case.
            Err(error::Write::NotUnlocked(_)) => {
                tracing::trace!("sync: skipping sync due to wallet error");
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        };
        tracing::trace!("Acquired write lock on wallet");
        let last_sync_write = self.last_sync.write();
        let request = wallet_write.start_sync_with_revealed_spks().build();

        tracing::trace!(
            spks = request.progress().spks_remaining,
            txids = request.progress().txids_remaining,
            outpoints = request.progress().outpoints_remaining,
            "Requesting sync via electrum client"
        );
        const BATCH_SIZE: usize = 5;
        const FETCH_PREV_TXOUTS: bool = false;
        let update = self
            .electrum_client
            .sync(request, BATCH_SIZE, FETCH_PREV_TXOUTS)?;
        tracing::trace!("Fetched update from electrum client, applying update");
        wallet_write.apply_update(update)?;
        tracing::debug!(
            "wallet sync complete in {:?}",
            start.elapsed().unwrap_or_default(),
        );
        Ok(Some(SyncWriteGuard {
            database: self.bitcoin_db.lock(),
            last_sync: last_sync_write,
            wallet: wallet_write,
        }))
    }

    /// Sync the wallet if the wallet is not locked, committing changes
    pub(in crate::wallet) fn sync(&self) -> Result<(), error::WalletSync> {
        match self.sync_lock()? {
            Some(sync_write) => {
                tracing::trace!("obtained sync lock, committing changes");
                let () = sync_write.commit()?;
                Ok(())
            }
            None => {
                tracing::trace!("no sync lock, skipping commit");
                Ok(())
            }
        }
    }
}
