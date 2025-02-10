use std::collections::HashMap;

use bitcoin::{hashes::Hash as _, BlockHash, Transaction, Txid};
use cusf_enforcer_mempool::{
    cusf_block_producer::{
        typewit::const_marker::{Bool, BoolWit},
        CoinbaseTxn, CoinbaseTxouts, CusfBlockProducer, InitialBlockTemplate,
    },
    cusf_enforcer::{ConnectBlockAction, CusfEnforcer},
};
use tracing::instrument;

use crate::{
    validator::Validator,
    wallet::{error, Wallet},
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
}

impl CusfEnforcer for Wallet {
    type SyncError = SyncError;

    #[allow(
        clippy::significant_drop_tightening,
        reason = "false positive, sync_write is consumed by commit()"
    )]
    #[instrument(skip_all, fields(tip_hash))]
    async fn sync_to_tip(
        &mut self,
        tip_hash: BlockHash,
    ) -> std::result::Result<(), Self::SyncError> {
        let () = self.inner.validator.clone().sync_to_tip(tip_hash).await?;
        tracing::debug!(%tip_hash, "Synced validator, syncing wallet..");
        let sync_write = self
            .inner
            .sync_lock()
            .await?
            .ok_or_else(|| Self::SyncError::WalletNotUnlocked(error::NotUnlocked))?;
        let wallet_tip = sync_write.wallet.local_chain().tip().hash();
        if tip_hash == wallet_tip {
            let () = sync_write.commit().map_err(error::WalletSync::from)?;
            if !self.inner.config.wallet_opts.skip_periodic_sync {
                let mut start_tx_lock = self.task.start_tx.lock();
                if let Some(start_tx) = start_tx_lock.take() {
                    start_tx.send(()).unwrap_or_else(|_| {
                        tracing::error!("Failed to send start signal to wallet task")
                    });
                }
            }
            Ok(())
        } else {
            Err(Self::SyncError::WalletTip {
                expected: tip_hash,
                wallet_tip,
            })
        }
    }

    type ConnectBlockError = error::ConnectBlock;

    async fn connect_block(
        &mut self,
        block: &bitcoin::Block,
    ) -> Result<ConnectBlockAction, Self::ConnectBlockError> {
        let block_hash = block.block_hash();
        tracing::info!(
            %block_hash,
            "CUSF block producer: connecting block"
        );
        let res = self.inner.validator.clone().connect_block(block).await?;
        let block_height = self.inner.validator.get_header_info(&block_hash)?.height;
        let block_info = self.inner.validator.get_block_info(&block.block_hash())?;
        let () = self
            .inner
            .handle_connect_block(block, block_height, block_info)
            .await?;
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
    ) -> std::result::Result<bool, Self::AcceptTxError>
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
            coinbase_txouts
                .extend(self.generate_coinbase_txouts(ACK_ALL_PROPOSALS, mainchain_tip)?);

            tracing::debug!(
                "Initial coinbase txouts post-extension: {:?}",
                coinbase_txouts
            );
        }
        // FIXME: set prefix txns and exclude mempool txs
        Ok(template)
    }

    type SuffixTxsError = error::SuffixTxs;

    async fn suffix_txs<const COINBASE_TXN: bool>(
        &self,
        coinbase_txn_wit: BoolWit<COINBASE_TXN>,
        template: &InitialBlockTemplate<COINBASE_TXN>,
    ) -> Result<Vec<(Transaction, bitcoin::Amount)>, Self::SuffixTxsError>
    where
        Bool<COINBASE_TXN>: CoinbaseTxn,
    {
        if let BoolWit::True(wit) = coinbase_txn_wit {
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
                output: wit
                    .map(cusf_enforcer_mempool::cusf_block_producer::CoinbaseTxouts)
                    .in_ref()
                    .to_right(&template.coinbase_txouts)
                    .clone(),
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
            let ctips =
                crate::validator::cusf_enforcer::get_ctips_after(&self.inner.validator, &block)?
                    .ok_or(error::SuffixTxsInner::InitialBlockTemplate)?;
            let res = self
                .generate_suffix_txs(&ctips)?
                .into_iter()
                .map(|tx| (tx, bitcoin::Amount::ZERO))
                .collect();
            Ok(res)
        } else {
            Ok(Vec::new())
        }
    }
}
