use std::{
    borrow::Cow,
    future::Future,
    time::{SystemTime, UNIX_EPOCH},
};

use bitcoin::{BlockHash, Transaction};
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
    wallet::{Wallet, WalletInner, error},
};

impl WalletInner {
    /// Sync wallet to tip.
    /// The validator is expected to already be synced to the same tip.
    /// If present, the block arg must have the same hash as `new_tip_hash`.
    async fn sync_wallet_to_tip(
        &self,
        new_tip_hash: BlockHash,
        block: Option<&bitcoin::Block>,
    ) -> Result<(), error::SyncWalletToTip> {
        let new_tip_height = self.validator().get_header_info(&new_tip_hash)?.height;
        tracing::trace!(%new_tip_height);
        // A few checks that need to happen:
        // 1. Is the wallet tip part of the active chain?
        // 2. Does the wallet have all the blocks up until the block we're trying to connect?
        //    If not, we have to iterate over the missing blocks and connect them first.
        let wallet_tip = self.get_tip().await?;
        if wallet_tip.hash == new_tip_hash {
            self.set_last_synced_now().await;
            return Ok(());
        }
        // Blocks mined before the wallet's keys existed provably cannot contain
        // wallet transactions, so the block-by-block replay below is pure waste
        // below the wallet's birthday.
        const BIRTHDAY_REORG_MARGIN: u32 = 100;
        let wallet_tip = if let Some(birthday_height) = self.birthday_height().await {
            let skip_below = birthday_height
                .saturating_sub(BIRTHDAY_REORG_MARGIN)
                .min(new_tip_height);

            if wallet_tip.height < skip_below {
                tracing::info!(
                    wallet_tip_height = wallet_tip.height,
                    %birthday_height,
                    %skip_below,
                    %new_tip_height,
                    "fast-forwarding wallet chain to just below its birthday"
                );
                let () = self.fast_forward_chain(Some(skip_below)).await?;
                self.get_tip().await?
            } else {
                wallet_tip
            }
        } else {
            wallet_tip
        };
        // Replaying the gap block-by-block below costs two node RPCs and a wallet
        // persist per block, and each persist gets slower as the local chain grows.
        // That is fine for the handful of blocks a running node falls behind by, but
        // a gap of hundreds of thousands of blocks takes hours. Past this many
        // blocks, advance the local chain with a single checkpoint update built from
        // validator headers and recover any wallet transactions in the skipped range
        // with a full scan against the chain source.
        const MAX_BLOCK_BY_BLOCK_REPLAY: u32 = 2_000;
        let blocks_behind = new_tip_height.saturating_sub(wallet_tip.height);
        let wallet_tip = if blocks_behind <= MAX_BLOCK_BY_BLOCK_REPLAY {
            wallet_tip
        } else if self.chain_source_client().is_some() {
            tracing::info!(
                wallet_tip_height = wallet_tip.height,
                %new_tip_height,
                %blocks_behind,
                "wallet is too far behind to replay block-by-block, \
                 checkpointing chain forward and running a full scan instead"
            );
            // `full_scan` checkpoints the local chain to the validator tip from
            // validator headers, then scans the chain source for transactions
            // against that checkpoint, so it both closes the gap and recovers the
            // wallet transactions the skipped blocks would have contributed.
            let _tip: BlockHash = self.full_scan().await?;
            self.get_tip().await?
        } else {
            tracing::warn!(
                wallet_tip_height = wallet_tip.height,
                %new_tip_height,
                %blocks_behind,
                "wallet is far behind and no chain source is available, \
                 falling back to block-by-block replay. This is very slow. \
                 A reachable electrum or esplora --wallet-sync-source catches \
                 up with a checkpoint and full scan instead"
            );
            wallet_tip
        };
        // If the wallet tip is higher than the block height we need to connect the missing blocks.
        // We have logic for that below. We therefore use max() here to ensure that the loop
        // will run at least once (thereby triggering the logic for missing blocks).
        let expected_blocks =
            std::cmp::max(1, new_tip_height.saturating_sub(wallet_tip.height)) as usize;
        tracing::trace!(
            wallet_tip_height = wallet_tip.height,
            "wallet is about to start processing {expected_blocks} block(s)"
        );
        let block_infos = self
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
                let fetched_block = self
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
                match self
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
                        self.connect_missing_block(try_include_height).await?;
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

        self.set_last_synced_now().await;

        Ok(())
    }
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
        // The validator may have stopped short of `tip_hash`. It
        // stops early the moment it hits a block it rejects Ask it
        // what it actually reached.
        let synced_tip_hash = self.inner.validator().get_mainchain_tip()?;
        tracing::debug!(%tip_hash, %synced_tip_hash, "Synced validator");

        let sync_wallet_to_tip = self.inner.sync_wallet_to_tip(synced_tip_hash, None);
        tokio::pin!(sync_wallet_to_tip);
        match futures::future::select(shutdown_signal, sync_wallet_to_tip).await {
            futures::future::Either::Left(((), _sync_wallet_to_tip)) => {
                Err(Self::SyncError::Shutdown)
            }
            futures::future::Either::Right((res, _)) => {
                let () = res?;
                tracing::debug!(%synced_tip_hash, "Synced wallet");
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
                let () = self
                    .inner
                    .sync_wallet_to_tip(block.block_hash(), Some(block))
                    .await?;
                let mut wallet_write = self.inner.write_wallet().await?;
                // Lock order: wallet before `bdk_db`, see the `bdk_db` field
                // docs
                let mut bdk_db = self.inner.bdk_db.lock().await;
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let _changed: bool = wallet_write
                    .with_mut(|wallet| {
                        wallet
                            .apply_evicted_txs(remove_mempool_txs.iter().map(|txid| (*txid, now)));
                        // Sweep BMM requests the mempool machinery has not
                        // tracked: any unconfirmed request not bidding on
                        // this block can no longer confirm.
                        let _stale = Wallet::evict_stale_bmm_requests(wallet, block.block_hash());
                        // Persist, or a restart reloads the evicted requests
                        // from the wallet DB and re-locks their inputs
                        wallet.persist_async(&mut bdk_db)
                    })
                    .await
                    .map_err(|err| {
                        error::ConnectBlock::PersistBmmEviction(error::SqliteError::from(err))
                    })?;
                drop(bdk_db);
                drop(wallet_write);
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

    fn accept_tx(
        &mut self,
        tx: &Transaction,
    ) -> std::result::Result<TxAcceptAction, Self::AcceptTxError> {
        let res = self.inner.validator().clone().accept_tx(tx)?;
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
                            // Lock order: wallet before `bdk_db`, see the
                            // `bdk_db` field docs
                            let mut wallet_write = match inner.write_wallet().await {
                                Ok(wallet_write) => wallet_write,
                                Err(err) => {
                                    tracing::error!("{:#}", ErrorChain::new(&err));
                                    return;
                                }
                            };
                            let mut bdk_db_lock = inner.bdk_db.lock().await;
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
