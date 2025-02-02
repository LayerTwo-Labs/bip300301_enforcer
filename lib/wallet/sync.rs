//! Wallet synchronization

use std::time::SystemTime;

use crate::{
    types::WithdrawalBundleEventKind,
    wallet::{error, WalletInner},
};

impl WalletInner {
    pub(in crate::wallet) fn handle_connect_block(
        &self,
        block: &bitcoin::Block,
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
        wallet_write.apply_block(block, block.bip34_block_height()? as u32)?;
        let mut database = self.bitcoin_db.lock();
        wallet_write.persist(&mut database)?;
        drop(wallet_write);
        Ok(())
    }

    pub(in crate::wallet) fn sync(&self) -> Result<(), error::WalletSync> {
        let start = SystemTime::now();
        tracing::trace!("starting wallet sync");

        // Don't error out here if the wallet is locked, just skip the sync.
        let wallet_read = match self.read_wallet() {
            Ok(wallet_read) => wallet_read,

            // "Accepted" errors, that aren't really errors in this case.
            Err(error::Read::NotUnlocked(_)) => {
                tracing::trace!("sync: skipping sync due to wallet error");
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };

        let mut last_sync_write = self.last_sync.write();
        let request = wallet_read.start_sync_with_revealed_spks();
        drop(wallet_read);

        const BATCH_SIZE: usize = 5;
        const FETCH_PREV_TXOUTS: bool = false;

        let update = self
            .electrum_client
            .sync(request, BATCH_SIZE, FETCH_PREV_TXOUTS)?;

        // Be a bit smart about the wallet locks, and only acquire the write lock
        // after the sync itself has completed and we're ready the apply
        // it to the wallet.
        let mut wallet_write = self.write_wallet()?;
        wallet_write.apply_update(update)?;

        let mut database = self.bitcoin_db.lock();
        wallet_write.persist(&mut database)?;

        tracing::debug!(
            "wallet sync complete in {:?}",
            start.elapsed().unwrap_or_default(),
        );

        *last_sync_write = Some(SystemTime::now());
        drop(last_sync_write);
        drop(wallet_write);
        Ok(())
    }
}
