use std::{
    borrow::Cow,
    collections::HashMap,
    future::Future,
    time::{SystemTime, UNIX_EPOCH},
};

use bitcoin::{BlockHash, Transaction, Txid};
use bitcoin_jsonrpsee::client::{GetBlockClient, U8Witness};
use cusf_enforcer_mempool::{
    cusf_block_producer::{
        CoinbaseTxn, CusfBlockProducer, FilledBlockTemplate, InitialBlockTemplate,
        typewit::const_marker::{Bool, BoolWit},
    },
    cusf_enforcer::{ConnectBlockAction, CusfEnforcer, DisconnectBlockAction, TxAcceptAction},
};
use tracing::instrument;

use crate::{
    block_producer::BlockProducer,
    errors::ErrorChain,
    validator::Validator,
    wallet::{Wallet, error},
};

/// Sync wallet to tip.
/// The inner validator is expected to already be synced to the same tip.
/// If present, the block arg must have the same hash as `new_tip_hash`.
async fn sync_wallet_to_tip(
    wallet: &mut Wallet,
    new_tip_hash: BlockHash,
    block: Option<&bitcoin::Block>,
) -> Result<(), error::SyncWalletToTip> {
    let new_tip_height = wallet
        .inner
        .validator()
        .get_header_info(&new_tip_hash)?
        .height;
    tracing::trace!(%new_tip_height);
    // A few checks that need to happen:
    // 1. Is the wallet tip part of the active chain?
    // 2. Does the wallet have all the blocks up until the block we're trying to connect?
    //    If not, we have to iterate over the missing blocks and connect them first.
    let wallet_tip = wallet.inner.get_tip().await?;
    if wallet_tip.hash == new_tip_hash {
        wallet.inner.set_last_synced_now().await;
        return Ok(());
    }
    // If the wallet tip is higher than the block height we need to connect the missing blocks.
    // We have logic for that below. We therefore use max() here to ensure that the loop
    // will run at least once (thereby triggering the logic for missing blocks).
    let expected_blocks =
        std::cmp::max(1, new_tip_height.saturating_sub(wallet_tip.height)) as usize;
    tracing::trace!(
        wallet_tip_height = wallet_tip.height,
        "wallet is about to start processing {expected_blocks} block(s)"
    );
    let block_infos = wallet
        .inner
        .validator()
        .get_block_infos(&new_tip_hash, expected_blocks.saturating_sub(1))?;
    let start = std::time::Instant::now();
    // Have to keep track of the index manually, because we need to be able to retry the current
    // operation if we get a 'try_include_height' error from BDK.
    let mut processed_blocks = 0;
    while processed_blocks < block_infos.len() {
        let (header_info, block_info) = &block_infos[processed_blocks];
        let block_hash = header_info.block_hash;
        let block_height = header_info.height;
        tracing::trace!(
            %block_hash,
            %block_height,
            "wallet is about to process block"
        );
        let block: Cow<bitcoin::Block> = if block_hash == new_tip_hash
            && let Some(block) = block
        {
            Cow::Borrowed(block)
        } else {
            // Fetch the ancestor block if needed
            let fetched_block = wallet
                .inner
                .main_client
                .get_block(block_hash, U8Witness::<0>)
                .await
                .map_err(|err| {
                    let error = error::BitcoinCoreRPC {
                        method: "getblock".to_string(),
                        error: err,
                    };
                    error::SyncWalletToTipInner::GetBlock(error)
                })?
                .0;
            Cow::Owned(fetched_block)
        };
        // The BDK wallet explicitly does NOT allow disconnecting blocks. Instead we're
        // supposed to just connect whatever comes in, and the current tip will be
        // automatically set to the best seen tip. I.e. if a block is invalidated,
        // it will be considered the best tip in the BDK wallet until it is overtaken
        // by another.
        // https://github.com/bitcoindevkit/bdk_wallet/issues/116
        //
        // We're therefore not checking here if the block is connect to the current active
        // chain.
        'connect_block: loop {
            match wallet
                .inner
                .handle_connect_block(&block, block_height, block_info)
                .await?
            {
                Ok(()) => break 'connect_block,
                // Try the recommended fixup - and then go back to the start of the loop
                Err(bdk_chain::local_chain::CannotConnectError { try_include_height }) => {
                    // If we just pass in the recommended include height we iterate forever.
                    // BDK uses a different indexing scheme than we/Core does?
                    let try_include_height = try_include_height + 1;
                    tracing::warn!(
                        "unable to connect block to bdk_chain, trying recommended include height {}",
                        try_include_height
                    );
                    wallet.connect_missing_block(try_include_height).await?;
                }
            }
        }
        processed_blocks += 1;
        // If the wallet tip is equal to the incoming block - 1, we've applied all 'em all
        if processed_blocks == expected_blocks {
            tracing::debug!(
                %block_hash,
                %block_height,
                "wallet finished processing {processed_blocks} block(s) in {}",
                jiff::SignedDuration::try_from(start.elapsed()).unwrap(),
            );
            break;
        }
    }

    wallet.inner.set_last_synced_now().await;

    Ok(())
}

impl CusfEnforcer for Wallet {
    type SyncError = error::InitialSync;

    #[instrument(skip_all, fields(tip_hash))]
    // TODO: this is confusing. This function is called multiple times? I want an easy
    // way to run a initial full scan after the validator has synced to the tip.
    // It seems to me (Torkel)that the CUSF enforcer mempool library exposes hooks for
    // this in a sub-optimal way.
    async fn sync_to_tip<Signal>(
        &mut self,
        shutdown_signal: Signal,
        tip_hash: BlockHash,
    ) -> std::result::Result<(), Self::SyncError>
    where
        Signal: Future<Output = ()> + Send,
    {
        let cancellation_token = tokio_util::sync::CancellationToken::new();
        tokio::pin!(shutdown_signal);
        let sync_validator_to_tip = {
            let cancellation_token = cancellation_token.clone();
            let mut validator = self.inner.validator().clone();
            async move {
                validator
                    .sync_to_tip(cancellation_token.cancelled_owned(), tip_hash)
                    .await
            }
        };
        tokio::pin!(sync_validator_to_tip);
        let shutdown_signal =
            match futures::future::select(shutdown_signal, sync_validator_to_tip).await {
                futures::future::Either::Left(((), sync_validator_to_tip)) => {
                    cancellation_token.cancel();
                    let () = sync_validator_to_tip.await?;
                    return Err(Self::SyncError::Shutdown);
                }
                futures::future::Either::Right((res, shutdown_signal)) => {
                    let () = res?;
                    shutdown_signal
                }
            };
        tracing::debug!(%tip_hash, "Synced validator");

        let sync_wallet_to_tip = sync_wallet_to_tip(self, tip_hash, None);
        tokio::pin!(sync_wallet_to_tip);
        match futures::future::select(shutdown_signal, sync_wallet_to_tip).await {
            futures::future::Either::Left(((), _sync_wallet_to_tip)) => {
                Err(Self::SyncError::Shutdown)
            }
            futures::future::Either::Right((res, _)) => {
                let () = res?;
                tracing::debug!(%tip_hash, "Synced wallet");
                Ok(())
            }
        }
    }

    type ConnectBlockError = error::ConnectBlock;

    #[instrument(skip_all, fields(block_hash = %block.block_hash()))]
    async fn connect_block(
        &mut self,
        block: &bitcoin::Block,
    ) -> Result<ConnectBlockAction, Self::ConnectBlockError> {
        tracing::trace!("starting block processing");
        // Validator step only. The producer's policy-table maintenance runs in
        // `handle_connect_block` instead, so that it happens under the BDK write
        // lock.
        let res = self
            .inner
            .producer
            .clone()
            .connect_block_validator(block)
            .await?;
        tracing::trace!("validator finished processing block");
        // Skip wallet sync if the validator rejected the block. The validator
        // aborts the child rwtxn on `Reject`, so block info is not persisted —
        // `sync_wallet_to_tip` would fail in `get_block_infos` and bubble an
        // error up that prevents the standalone driver from issuing
        // `invalidateblock` to bitcoind.
        match &res {
            ConnectBlockAction::Accept { remove_mempool_txs } => {
                let () = sync_wallet_to_tip(self, block.block_hash(), Some(block)).await?;
                let mut wallet_write = self.inner.write_wallet().await?;
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                wallet_write.with_mut(|wallet| {
                    wallet.apply_evicted_txs(remove_mempool_txs.iter().map(|txid| (*txid, now)))
                })
            }
            ConnectBlockAction::Reject => (),
        }
        Ok(res)
    }

    type DisconnectBlockError = <Validator as CusfEnforcer>::DisconnectBlockError;

    async fn disconnect_block(
        &mut self,
        block_hash: BlockHash,
    ) -> std::result::Result<DisconnectBlockAction, Self::DisconnectBlockError> {
        // We're NOT disconnecting blocks for the BDK wallet. This concept doesn't exist
        // in BDK. Instead, we're supposed to just connect whatever comes in, and the current tip
        // will be automatically set to the best seen tip. I.e. if a block is invalidated,
        // it will be considered the best tip in the BDK wallet until it is overtaken
        // by another.
        // https://github.com/bitcoindevkit/bdk_wallet/issues/116
        self.inner
            .producer
            .clone()
            .disconnect_block(block_hash)
            .await
    }

    type AcceptTxError = <Validator as CusfEnforcer>::AcceptTxError;

    fn accept_tx<TxRef>(
        &mut self,
        tx: &Transaction,
        tx_inputs: &HashMap<Txid, TxRef>,
    ) -> std::result::Result<TxAcceptAction, Self::AcceptTxError>
    where
        TxRef: std::borrow::Borrow<Transaction>,
    {
        let res = self.inner.validator().clone().accept_tx(tx, tx_inputs)?;
        match res {
            TxAcceptAction::Accept { .. } => {
                // TODO: Ideally we could push these updates to a channel, and
                // a wallet task could apply the updates
                tokio::spawn({
                    let inner = self.inner.clone();
                    let tx = tx.clone();
                    || async move {
                        // Apply the unconfirmed tx and persist, so the spent
                        // inputs survive a restart before the tx confirms. The
                        // locks are scoped so they are released before logging.
                        let persist_res = {
                            let mut bdk_db_lock = inner.bdk_db.lock().await;
                            let mut wallet_write = match inner.write_wallet().await {
                                Ok(wallet_write) => wallet_write,
                                Err(err) => {
                                    tracing::error!("{:#}", ErrorChain::new(&err));
                                    return;
                                }
                            };
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs();
                            wallet_write
                                .with_mut(|wallet| {
                                    wallet.apply_unconfirmed_txs([(tx, now)]);
                                    wallet.persist_async(&mut bdk_db_lock)
                                })
                                .await
                        };
                        if let Err(err) = persist_res {
                            tracing::error!("{:#}", ErrorChain::new(&err));
                        }
                    }
                }());
            }
            TxAcceptAction::Reject => (),
        }
        Ok(res)
    }
}

impl CusfBlockProducer for Wallet {
    type InitialBlockTemplateError =
        <BlockProducer as CusfBlockProducer>::InitialBlockTemplateError;

    async fn initial_block_template<const COINBASE_TXN: bool>(
        &self,
        parent_block_hash: &BlockHash,
        coinbase_txn_wit: BoolWit<COINBASE_TXN>,
        template: &mut InitialBlockTemplate<COINBASE_TXN>,
    ) -> Result<(), Self::InitialBlockTemplateError>
    where
        Bool<COINBASE_TXN>: CoinbaseTxn,
    {
        self.inner
            .producer
            .initial_block_template(parent_block_hash, coinbase_txn_wit, template)
            .await
    }

    type FinalizeBlockTemplateError =
        <BlockProducer as CusfBlockProducer>::FinalizeBlockTemplateError;

    async fn finalize_block_template<const COINBASE_TXN: bool>(
        &self,
        parent_block_hash: &BlockHash,
        coinbase_txn_wit: BoolWit<COINBASE_TXN>,
        template: &mut FilledBlockTemplate<COINBASE_TXN>,
    ) -> Result<(), Self::FinalizeBlockTemplateError>
    where
        Bool<COINBASE_TXN>: CoinbaseTxn,
    {
        self.inner
            .producer
            .finalize_block_template(parent_block_hash, coinbase_txn_wit, template)
            .await
    }
}
