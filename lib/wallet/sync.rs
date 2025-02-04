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
        // Don't error out here if the wallet is locked, just skip the sync.
        let wallet_read = match self.read_wallet() {
            Ok(wallet_read) => wallet_read,
            // "Accepted" errors, that aren't really errors in this case.
            Err(error::Read::NotUnlocked(_)) => {
                tracing::trace!("sync: skipping sync due to wallet error");
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        };
        let last_sync_write = self.last_sync.write();
        let request = wallet_read.start_sync_with_revealed_spks();
        drop(wallet_read);

        // I (Torkel) have no idea what's a suitable batch size. ChatGPT tells me
        // 25 is a reasonable default. Too large batch sizes could cause timeouts
        // or overload some Electrum server. We're the only user of our Electrum server,
        // so sending large requests is probably fine.
        const BATCH_SIZE: usize = 100;
        const FETCH_PREV_TXOUTS: bool = false;
        let update = self
            .electrum_client
            .sync(request, BATCH_SIZE, FETCH_PREV_TXOUTS)?;
        // Be a bit smart about the wallet locks, and only acquire the write lock
        // after the sync itself has completed and we're ready the apply
        // it to the wallet.
        let mut wallet_write = self.write_wallet()?;
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

    /// Sync the wallet if the wallet is not locked, commiting changes
    pub(in crate::wallet) fn sync(&self) -> Result<(), error::WalletSync> {
        match self.sync_lock()? {
            Some(sync_write) => {
                let () = sync_write.commit()?;
                Ok(())
            }
            None => Ok(()),
        }
    }
}
