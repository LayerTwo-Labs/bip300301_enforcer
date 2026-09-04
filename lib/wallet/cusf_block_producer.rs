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
    cusf_enforcer::{
        ConnectBlockAction, CusfEnforcer, DisconnectBlockAction, SyncToTipError, TxAcceptAction,
    },
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
            self.set_last_synced_now();
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
        let max_block_by_block_replay = self.config.wallet_opts.max_block_by_block_replay;
        let blocks_behind = new_tip_height.saturating_sub(wallet_tip.height);
        let wallet_tip = if blocks_behind <= max_block_by_block_replay {
            wallet_tip
        } else if self.chain_source_client().is_some() {
            tracing::info!(
                wallet_tip_height = wallet_tip.height,
                %new_tip_height,
                %blocks_behind,
                %max_block_by_block_replay,
                "wallet is too far behind to replay block-by-block, \
                 checkpointing chain forward and running a full scan instead"
            );
            // The checkpoint leaves the replay loop below with only the tip
            // block to connect, so the policy maintenance that connecting a
            // block performs never runs for the gap. Apply it for the skipped
            // range first: doing it before the chain moves means a failure
            // here leaves the wallet tip where it is and the whole crossing is
            // retried, rather than the gap being skipped for good.
            let () = self
                .apply_skipped_blocks_policy(new_tip_hash, blocks_behind)
                .await?;
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
                %max_block_by_block_replay,
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

        self.set_last_synced_now();

        Ok(())
    }

    /// Drop the sidechain proposals and withdrawal bundles that were settled
    /// by the `skipped_blocks` blocks ending at (and including) `tip_hash`.
    ///
    /// Every block connected one at a time reaches
    /// [`BlockProducer::apply_connected_block_policy`] via
    /// [`Self::handle_connect_block`], which is what stops a proposal the chain
    /// has already settled from being re-proposed in every later coinbase. The
    /// checkpoint path in [`Self::sync_wallet_to_tip`] connects none of the
    /// blocks it jumps over, so that maintenance has to be applied for the
    /// whole skipped range here instead.
    ///
    /// The validator already holds every `BlockInfo` in the range, so no block
    /// is fetched from the node. The range is walked in chunks: it is bounded
    /// only by the length of the chain, and an initial sync crosses all of it.
    /// The maintenance is deletes alone, so the order blocks are visited in
    /// does not matter, and blocks whose settlements were already dropped are
    /// no-ops.
    async fn apply_skipped_blocks_policy(
        &self,
        tip_hash: BlockHash,
        skipped_blocks: u32,
    ) -> Result<(), error::SyncWalletToTip> {
        // Blocks read from the validator per batch, so that crossing a gap
        // the length of the chain does not hold every one of its block infos
        // in memory at once.
        const CHUNK_BLOCKS: usize = 10_000;
        tracing::debug!(
            %skipped_blocks,
            "applying policy for the blocks skipped by the checkpoint"
        );
        let mut block_hash = tip_hash;
        let mut remaining = skipped_blocks as usize;
        while remaining > 0 {
            let chunk = std::cmp::min(remaining, CHUNK_BLOCKS);
            let block_infos = self
                .validator()
                .get_block_infos(&block_hash, chunk.saturating_sub(1))?;
            for (_header_info, block_info) in &block_infos {
                let () = self
                    .producer
                    .apply_connected_block_policy(block_info)
                    .await
                    .map_err(|err| {
                        error::SyncWalletToTipInner::ApplySkippedBlocksPolicy(
                            error::SqliteError::from(err),
                        )
                    })?;
            }
            // `get_block_infos` stops at the first block it has no info for,
            // which is also the first one that cannot be walked past.
            if block_infos.len() < chunk {
                break;
            }
            let (oldest_header_info, _block_info) = &block_infos[block_infos.len() - 1];
            block_hash = oldest_header_info.prev_block_hash;
            remaining -= block_infos.len();
        }
        Ok(())
    }

    /// Connect missing blocks to the BDK chain. Retries if we get a 'nested'
    /// alert from BDK, about further missing ancestors.
    async fn connect_missing_block(
        &self,
        try_include_height: u32,
    ) -> std::result::Result<(), error::ConnectMissingBlock> {
        use bitcoin_jsonrpsee::{
            MainClient as _,
            client::{GetBlockClient as _, U8Witness},
        };

        struct TryInclude {
            block_height: u32,
            block: Option<bitcoin::Block>,
        }

        // stack of block heights / blocks to connect
        let mut try_includes = vec![TryInclude {
            block_height: try_include_height,
            block: None,
        }];

        while let Some(try_include) = try_includes.last_mut() {
            let TryInclude {
                block_height,
                block,
            } = try_include;
            let block = match block {
                Some(block) => block,
                None => {
                    let block_hash = self
                        .main_client
                        .getblockhash(*block_height as usize)
                        .await
                        .map_err(|err| {
                            error::ConnectMissingBlockInner::GetBlockHash(error::BitcoinCoreRPC {
                                method: "getblockhash".to_string(),
                                error: err,
                            })
                        })?;
                    block.insert(
                        self.main_client
                            .get_block(block_hash, U8Witness::<0>)
                            .await
                            .map_err(|err| {
                                error::ConnectMissingBlockInner::GetBlock(error::BitcoinCoreRPC {
                                    method: "getblock".to_string(),
                                    error: err,
                                })
                            })?
                            .0,
                    )
                }
            };
            let block_hash = block.block_hash();
            let infos = self.validator().get_block_infos(&block_hash, 0)?;
            assert_eq!(infos.len(), 1);
            let (_header_info, block_info) = infos.head;
            tracing::debug!(
                "connecting missing block {} at height {}",
                block_hash,
                block_height,
            );
            match self
                .handle_connect_block(block, *block_height, &block_info)
                .await?
            {
                Ok(()) => {
                    tracing::debug!(
                        "connected missing block {} at height {}",
                        block_hash,
                        block_height
                    );
                    try_includes.pop();
                }
                // We can receive 'nested' alerts from BDK, about further missing ancestors. We therefore
                // recurse, but make sure to only do so if the recommended try_include_height is /below/
                // what we just tried. Otherwise we'll just loop forever.
                Err(
                    err @ bdk_wallet::chain::local_chain::CannotConnectError { try_include_height },
                ) => {
                    if try_include_height < *block_height {
                        // BDK's `try_include_height` can skip past the block's
                        // immediate parent, and retrying at the skipped-to height
                        // connects as a no-op without fixing anything, looping
                        // forever. Step down one height at a time instead. The
                        // reported height is only used (above) to check that we're
                        // still making downward progress.
                        let next_height = *block_height - 1;
                        tracing::debug!("adding missing block at height {} to stack", next_height);
                        try_includes.push(TryInclude {
                            block_height: next_height,
                            block: None,
                        });
                    } else {
                        return Err(error::ConnectMissingBlockInner::BdkConnect {
                            block_height: *block_height,
                            source: err,
                        }
                        .into());
                    }
                }
            };
        }

        Ok(())
    }
}

impl CusfEnforcer for Wallet {
    type InvalidBlockReason = <Validator as CusfEnforcer>::InvalidBlockReason;
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
    ) -> std::result::Result<(), SyncToTipError<Self::InvalidBlockReason, Self::SyncError>>
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
                    let () = sync_validator_to_tip
                        .await
                        .map_err(|err| err.map_other(Self::SyncError::from))?;
                    return Err(Self::SyncError::Shutdown.into());
                }
                futures::future::Either::Right((res, shutdown_signal)) => {
                    let () = res.map_err(|err| err.map_other(Self::SyncError::from))?;
                    shutdown_signal
                }
            };
        // Sync the wallet to the tip the validator actually reached, rather
        // than assuming it is `tip_hash`.
        let synced_tip_hash = self
            .inner
            .validator()
            .get_mainchain_tip()
            .map_err(|err| SyncToTipError::Other(err.into()))?;
        tracing::debug!(%tip_hash, %synced_tip_hash, "Synced validator");

        let sync_wallet_to_tip = self.inner.sync_wallet_to_tip(synced_tip_hash, None);
        tokio::pin!(sync_wallet_to_tip);
        match futures::future::select(shutdown_signal, sync_wallet_to_tip).await {
            futures::future::Either::Left(((), _sync_wallet_to_tip)) => {
                Err(Self::SyncError::Shutdown.into())
            }
            futures::future::Either::Right((res, _)) => {
                let () = res.map_err(|err| SyncToTipError::Other(err.into()))?;
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
                let mut bdk_db = self.inner.locks.db(&wallet_write).await;
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
                    async move {
                        // Apply the unconfirmed tx and persist, so the spent
                        // inputs survive a restart before the tx confirms.
                        if let Err(err) = inner.apply_unconfirmed_tx(tx).await {
                            tracing::error!("{:#}", ErrorChain::new(&err));
                        }
                    }
                });
            }
            TxAcceptAction::Reject => (),
        }
        Ok(res)
    }

    type ValidateBlockError = <Validator as CusfEnforcer>::ValidateBlockError;

    fn validate_block(
        &self,
        block: &bitcoin::Block,
    ) -> Result<Option<String>, Self::ValidateBlockError> {
        self.inner.validator().validate_block(block)
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
