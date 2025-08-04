use std::{borrow::Cow, collections::HashMap, future::Future};

use bitcoin::{BlockHash, Transaction, Txid, hashes::Hash as _};
use bitcoin_jsonrpsee::client::{GetBlockClient, U8Witness};
use cusf_enforcer_mempool::{
    cusf_block_producer::{
        BlockTemplateSuffix, CoinbaseTxn, CoinbaseTxouts, CusfBlockProducer, InitialBlockTemplate,
        typewit::const_marker::{Bool, BoolWit},
    },
    cusf_enforcer::{ConnectBlockAction, CusfEnforcer, TxAcceptAction},
};
use tracing::instrument;

use crate::{
    messages::{CoinbaseBuilder, parse_m8_tx},
    validator::Validator,
    wallet::{Wallet, error},
};

#[derive(Debug, miette::Diagnostic, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Validator(#[from] <Validator as CusfEnforcer>::SyncError),
    #[error(transparent)]
    Wallet(#[from] error::WalletSync),
    #[error(transparent)]
    WalletNotUnlocked(#[from] error::NotUnlocked),
    #[error("Wallet synced to other tip ({wallet_tip}): expected {expected}")]
    WalletTip {
        expected: BlockHash,
        wallet_tip: BlockHash,
    },
    #[error(transparent)]
    FullScan(#[from] error::FullScan),
}

impl CusfEnforcer for Wallet {
    type SyncError = SyncError;

    #[allow(
        clippy::significant_drop_tightening,
        reason = "false positive, sync_write is consumed by commit()"
    )]
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
        let () = self
            .inner
            .validator
            .clone()
            .sync_to_tip(shutdown_signal, tip_hash)
            .await?;
        tracing::debug!(%tip_hash, "Synced validator");
        Ok(())
    }

    type ConnectBlockError = error::ConnectBlock;

    #[instrument(skip_all, fields(block_hash = %block.block_hash()))]
    async fn connect_block(
        &mut self,
        block: &bitcoin::Block,
    ) -> Result<ConnectBlockAction, Self::ConnectBlockError> {
        let start = std::time::Instant::now();

        tracing::trace!("starting block processing");

        // First, connect the block to the validator.
        let res = self.inner.validator.clone().connect_block(block).await?;
        tracing::trace!("validator finished processing block");

        // Important: calling this /before/ the validator has connected the block will fail,
        // as the block header is not yet stored in the validator's database.
        let block_height = self
            .inner
            .validator
            .get_header_info(&block.block_hash())?
            .height;
        tracing::trace!("determined block height: {}", block_height);

        // After the validator has accepted the block, we can handle it in the wallet.
        //
        // A few checks that need to happen:
        // 1. Is the wallet tip part of the active chain?
        // 2. Does the wallet have all the blocks up until the block we're trying to connect?
        //    If not, we have to iterate over the missing blocks and connect them first.

        let wallet_tip = self.inner.get_tip().await?;

        let initial_block_param = block; // Keep the original reference

        // If the wallet tip is higher than the block height we need to connect the missing blocks.
        // We have logic for that below. We therefore use max() here to ensure that the loop
        // will run at least once (thereby triggering the logic for missing blocks).
        let expected_blocks =
            std::cmp::max(1, block_height.saturating_sub(wallet_tip.height)) as usize;

        tracing::trace!(
            wallet_tip_height = wallet_tip.height,
            "wallet is about to start processing {expected_blocks} block(s)"
        );

        let block_info = self.inner.validator.get_block_infos(
            &initial_block_param.block_hash(),
            expected_blocks.saturating_sub(1),
        )?;

        // Have to keep track of the index manually, because we need to be able to retry the current
        // operation if we get a 'try_include_height' error from BDK.
        let mut processed_blocks = 0;

        while processed_blocks < block_info.len() {
            let (header_info, block_info) = &block_info[processed_blocks];
            let block_hash = header_info.block_hash;
            let block_height = header_info.height;

            tracing::trace!(
                block_hash = %block_hash,
                block_height = %block_height,
                "wallet is about to process block"
            );

            // Use Cow to manage the block for this iteration
            let block_for_this_iteration: Cow<'_, bitcoin::Block> =
                if block_hash == initial_block_param.block_hash() {
                    Cow::Borrowed(initial_block_param)
                } else {
                    // Fetch the ancestor block if needed
                    let fetched_block = self
                        .inner
                        .main_client
                        .get_block(block_hash, U8Witness::<0>)
                        .await
                        .map_err(|err| {
                            let error = error::BitcoinCoreRPC {
                                method: "getblock".to_string(),
                                error: err,
                            };
                            error::ConnectBlock::GetBlock(error)
                        })?;
                    Cow::Owned(fetched_block.0) // Cow now owns the fetched block
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

            let () = match self
                .inner
                .handle_connect_block(&block_for_this_iteration, block_height, block_info)
                .await
            {
                Ok(_) => Ok(()),

                // Try the recommended fixup - and then go back to the start of the loop
                Err(error::ConnectBlock::BdkConnect(
                    bdk_chain::local_chain::CannotConnectError { try_include_height },
                )) => {
                    // If we just pass in the recommended include height we iterate forever.
                    // BDK uses a different indexing scheme than we/Core does?
                    let try_include_height = try_include_height + 1;

                    tracing::warn!(
                        "unable to connect block to bdk_chain, trying recommended include height {}",
                        try_include_height
                    );

                    self.connect_missing_block(try_include_height).await?;

                    continue;
                }
                err => err,
            }?;

            processed_blocks += 1;

            // If the wallet tip is equal to the incoming block - 1, we've applied all 'em all
            if processed_blocks == expected_blocks {
                tracing::debug!(
                    block_hash = %block_hash,
                    block_height = %block_height,
                    "wallet finished processing {processed_blocks} block(s) in {:?}",
                    start.elapsed()
                );
                break;
            }
        }

        self.inner.set_last_synced_now().await;

        Ok(res)
    }

    type DisconnectBlockError = <Validator as CusfEnforcer>::DisconnectBlockError;

    async fn disconnect_block(
        &mut self,
        block_hash: BlockHash,
    ) -> std::result::Result<(), Self::DisconnectBlockError> {
        self.inner
            .validator
            .clone()
            .disconnect_block(block_hash)
            .await
        // FIXME: disconnect block for wallet
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
        self.inner.validator.clone().accept_tx(tx, tx_inputs)
    }
}

impl CusfBlockProducer for Wallet {
    type InitialBlockTemplateError = error::InitialBlockTemplate;

    /// This function is called when the RPC server starts producing a block template.
    /// The flow is something like this:
    /// 1. RPC server (within the `cusf_enforcer_mempool` create) receives request
    /// 2. Fetches the initial block template (this function!)
    /// 3. Processes it further, and spits out to the client
    ///
    /// This function is our "hook" for adding Drivechain coinbase messages to
    /// the about-to-be-generated block.
    async fn initial_block_template<const COINBASE_TXN: bool>(
        &self,
        parent_block_hash: &BlockHash,
        coinbase_txn_wit: BoolWit<COINBASE_TXN>,
        mut template: InitialBlockTemplate<COINBASE_TXN>,
    ) -> Result<InitialBlockTemplate<COINBASE_TXN>, Self::InitialBlockTemplateError>
    where
        Bool<COINBASE_TXN>: CoinbaseTxn,
    {
        if let BoolWit::True(wit) = coinbase_txn_wit {
            tracing::debug!(
                "CUSF block producer: extending initial block template with coinbase TX outputs"
            );

            tracing::debug!(
                "Initial coinbase txouts pre-extension: {:?}",
                template.coinbase_txouts
            );

            let mainchain_tip = self.validator().get_mainchain_tip()?;
            let wit = wit.map(CoinbaseTxouts);
            let coinbase_txouts: &mut Vec<_> = wit.in_mut().to_right(&mut template.coinbase_txouts);

            tracing::debug!(
                "Initial coinbase txouts post-type magic: {:?}",
                coinbase_txouts
            );

            const ACK_ALL_PROPOSALS: bool = true;
            let () = self
                .extend_coinbase_txouts(ACK_ALL_PROPOSALS, mainchain_tip, coinbase_txouts)
                .await?;
            tracing::debug!(
                "Initial coinbase txouts post-extension: {:?}",
                coinbase_txouts
            );
            // Exclude M8 txs with different h*
            {
                let coinbase_builder = CoinbaseBuilder::new(coinbase_txouts)?;
                let coinbase_m7_accepts = coinbase_builder.messages().m7_bmm_accepts();
                let seen_bmm_requests = self
                    .validator()
                    .get_seen_bmm_requests_for_parent_block(*parent_block_hash)?;
                let exclude = {
                    let mut exclude = seen_bmm_requests;
                    exclude.retain(|sidechain_number, txids| {
                        let Some(commitment) = coinbase_m7_accepts.get(sidechain_number) else {
                            return false;
                        };
                        txids.remove(commitment);
                        true
                    });
                    exclude
                        .into_values()
                        .flat_map(|txids| txids.into_values().flatten())
                };
                template.exclude_mempool_txs.extend(exclude);
            }
        }
        // FIXME: set prefix txns and exclude mempool txs
        Ok(template)
    }

    type SuffixTxsError = error::SuffixTxs;

    async fn block_template_suffix<const COINBASE_TXN: bool>(
        &self,
        _parent_block_hash: &BlockHash,
        coinbase_txn_wit: BoolWit<COINBASE_TXN>,
        template: &InitialBlockTemplate<COINBASE_TXN>,
    ) -> Result<BlockTemplateSuffix<COINBASE_TXN>, Self::SuffixTxsError>
    where
        Bool<COINBASE_TXN>: CoinbaseTxn,
    {
        match coinbase_txn_wit {
            BoolWit::True(wit) => {
                let (tip, height) = match self.validator().try_get_mainchain_tip()? {
                    Some(tip) => {
                        let tip_height = self.validator().get_header_info(&tip)?.height;
                        (tip, tip_height + 1)
                    }
                    None => (BlockHash::all_zeros(), 0),
                };
                const WITNESS_RESERVED_VALUE: [u8; 32] = [0; 32];
                let bip34_height_script = bitcoin::blockdata::script::Builder::new()
                    .push_int(height as i64)
                    .push_opcode(bitcoin::opcodes::OP_0)
                    .into_script();
                let mut coinbase_txouts = wit
                    .map(cusf_enforcer_mempool::cusf_block_producer::CoinbaseTxouts)
                    .in_ref()
                    .to_right(&template.coinbase_txouts)
                    .clone();
                let mut coinbase_builder = CoinbaseBuilder::new(&mut coinbase_txouts)?;
                for (tx, _) in &template.prefix_txs {
                    if let Some(bmm_request) = parse_m8_tx(tx)
                        && coinbase_builder
                            .messages()
                            .m7_bmm_accept_slot_vout(&bmm_request.sidechain_number)
                            .is_none()
                    {
                        coinbase_builder.bmm_accept(
                            bmm_request.sidechain_number,
                            bmm_request.sidechain_block_hash,
                        )?;
                    }
                }
                let coinbase_txouts_suffix = coinbase_builder
                    .build_extension()
                    .map_err(error::SuffixTxsInner::GenerateSuffixCoinbaseTxouts)?;
                coinbase_txouts.extend(coinbase_txouts_suffix.iter().cloned());
                let coinbase_tx = Transaction {
                    version: bitcoin::transaction::Version::TWO,
                    lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
                    input: vec![bitcoin::TxIn {
                        previous_output: bitcoin::OutPoint {
                            txid: Txid::all_zeros(),
                            vout: 0xFFFF_FFFF,
                        },
                        sequence: bitcoin::Sequence::MAX,
                        witness: bitcoin::Witness::from_slice(&[WITNESS_RESERVED_VALUE]),
                        script_sig: bip34_height_script,
                    }],
                    output: coinbase_txouts,
                };
                let merkle_root = {
                    let hashes = std::iter::once(coinbase_tx.compute_txid().to_raw_hash()).chain(
                        template
                            .prefix_txs
                            .iter()
                            .map(|(tx, _)| tx.compute_txid().to_raw_hash()),
                    );
                    bitcoin::merkle_tree::calculate_root(hashes)
                        .map(bitcoin::TxMerkleNode::from_raw_hash)
                        .unwrap()
                };
                let time = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as u32;
                let header = bitcoin::block::Header {
                    version: bitcoin::block::Version::TWO,
                    prev_blockhash: tip,
                    merkle_root,
                    time,
                    bits: bitcoin::Target::MAX.to_compact_lossy(),
                    nonce: 0,
                };
                let block = bitcoin::Block {
                    header,
                    txdata: std::iter::once(coinbase_tx)
                        .chain(template.prefix_txs.iter().map(|(tx, _)| tx.clone()))
                        .collect(),
                };
                let ctips = crate::validator::cusf_enforcer::get_ctips_after(
                    &self.inner.validator,
                    &block,
                )?
                .ok_or(error::SuffixTxsInner::InitialBlockTemplate)?;
                let suffix_txs = self
                    .generate_suffix_txs(&ctips)
                    .await?
                    .into_iter()
                    .map(|tx| (tx, bitcoin::Amount::ZERO))
                    .collect();
                Ok(BlockTemplateSuffix {
                    coinbase_txouts: wit
                        .map(cusf_enforcer_mempool::cusf_block_producer::CoinbaseTxouts)
                        .to_left(coinbase_txouts_suffix),
                    txs: suffix_txs,
                })
            }
            BoolWit::False(wit) => Ok(BlockTemplateSuffix {
                coinbase_txouts: wit
                    .map(cusf_enforcer_mempool::cusf_block_producer::CoinbaseTxouts)
                    .to_left(()),
                txs: Vec::new(),
            }),
        }
    }
}
