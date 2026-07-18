use std::{collections::HashMap, sync::Arc};

use bitcoin::{BlockHash, Transaction, Txid, hashes::Hash as _};
use cusf_enforcer_mempool::{
    cusf_block_producer::{
        CoinbaseTxn, CoinbaseTxouts, CusfBlockProducer, FilledBlockTemplate, InitialBlockTemplate,
        initial_block_template,
        typewit::const_marker::{Bool, BoolWit},
    },
    cusf_enforcer::{ConnectBlockAction, CusfEnforcer, DisconnectBlockAction, TxAcceptAction},
};
use tracing::instrument;

use crate::{
    errors::ErrorChain,
    messages::{CoinbaseBuilder, parse_m8_tx},
    types::{BlindedM6, M6id, PendingM6idInfo, WithdrawalBundleEventKind},
    validator::Validator,
};

mod coinbase;
pub mod db;
pub mod error;

pub use self::db::Db;

/// Bundle proposals for a sidechain, joined with the validator's view of how
/// each one is doing (vote count etc).
pub(crate) type BundleProposals = Vec<(M6id, BlindedM6<'static>, Option<PendingM6idInfo>)>;

struct Inner {
    validator: Validator,
    db: Db,
    /// Error from the most recent failed block template build, cleared on
    /// success. The GBT server reports template failures to its JSON-RPC client
    /// in a field that `bitcoin-cli` (and thus the signet miner's stderr) drops,
    /// so GenerateBlocks attaches this to its own error to surface the root
    /// cause.
    last_gbt_error: parking_lot::RwLock<Option<String>>,
}

#[derive(Clone)]
pub struct BlockProducer {
    inner: Arc<Inner>,
}

impl BlockProducer {
    pub fn new(
        data_dir: &std::path::Path,
        validator: Validator,
    ) -> Result<Self, error::InitDbConnection> {
        let db = Db::new(data_dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                validator,
                db,
                last_gbt_error: parking_lot::RwLock::new(None),
            }),
        })
    }

    pub fn validator(&self) -> &Validator {
        &self.inner.validator
    }

    /// The drivechain policy store. The producer reads and writes policy only:
    /// it never touches the legacy `wallet_seeds` slot that pre-split
    /// deployments carry in the same `db.sqlite`.
    pub fn db(&self) -> &Db {
        &self.inner.db
    }

    pub fn last_gbt_error(&self) -> Option<String> {
        self.inner.last_gbt_error.read().clone()
    }

    fn record_gbt_result<T, Err>(&self, res: &Result<T, Err>)
    where
        Err: std::error::Error,
    {
        *self.inner.last_gbt_error.write() = res
            .as_ref()
            .err()
            .map(|err| format!("{:#}", ErrorChain::new(err)));
    }

    /// Connect a block to the validator only, without touching policy state.
    ///
    /// Split out from [`CusfEnforcer::connect_block`] so the wallet can hold the
    /// BDK write lock across [`Self::apply_connected_block_policy`].
    pub(crate) async fn connect_block_validator(
        &mut self,
        block: &bitcoin::Block,
    ) -> Result<ConnectBlockAction, <Validator as CusfEnforcer>::ConnectBlockError> {
        self.inner.validator.clone().connect_block(block).await
    }

    /// Policy-table maintenance for a block the validator just accepted: drop the
    /// sidechain proposals and withdrawal bundles that this block settled, so we
    /// stop re-proposing them in later coinbases.
    pub(crate) async fn apply_connected_block_policy(
        &self,
        block_info: &crate::types::BlockInfo,
    ) -> Result<(), rusqlite::Error> {
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
            .inner
            .db
            .delete_bundle_proposals(finalized_withdrawal_bundles)
            .await?;
        let sidechain_proposal_ids = block_info
            .sidechain_proposals()
            .map(|(_vout, proposal)| proposal.compute_id());
        self.inner
            .db
            .delete_pending_sidechain_proposals(sidechain_proposal_ids)
            .await
    }
}

impl CusfEnforcer for BlockProducer {
    type SyncError = <Validator as CusfEnforcer>::SyncError;

    async fn sync_to_tip<Signal>(
        &mut self,
        shutdown_signal: Signal,
        tip_hash: BlockHash,
    ) -> Result<(), Self::SyncError>
    where
        Signal: std::future::Future<Output = ()> + Send,
    {
        self.inner
            .validator
            .clone()
            .sync_to_tip(shutdown_signal, tip_hash)
            .await
    }

    type ConnectBlockError = error::ConnectBlock;

    #[instrument(skip_all, fields(block_hash = %block.block_hash()))]
    async fn connect_block(
        &mut self,
        block: &bitcoin::Block,
    ) -> Result<ConnectBlockAction, Self::ConnectBlockError> {
        let res = self.connect_block_validator(block).await?;
        // Skip policy maintenance if the validator rejected the block: it aborts
        // the child rwtxn on `Reject`, so the block's info was never persisted.
        if let ConnectBlockAction::Accept { .. } = &res {
            let block_hash = block.block_hash();
            let block_infos = self.inner.validator.get_block_infos(&block_hash, 0)?;
            for (_header_info, block_info) in &block_infos {
                let () = self.apply_connected_block_policy(block_info).await?;
            }
        }
        Ok(res)
    }

    type DisconnectBlockError = <Validator as CusfEnforcer>::DisconnectBlockError;

    async fn disconnect_block(
        &mut self,
        block_hash: BlockHash,
    ) -> Result<DisconnectBlockAction, Self::DisconnectBlockError> {
        let res = self
            .inner
            .validator
            .clone()
            .disconnect_block(block_hash)
            .await?;

        // Restore the `bmm_requests` rows that were consumed (deleted) when this
        // block was generated, so the operator can re-emit the BMM accepts
        // against the new mainchain tip. Policy SQLite state is best-effort on
        // disconnect, so a restore failure is logged rather than aborting.
        if let Err(err) = self.inner.db.restore_bmm_requests(&block_hash).await {
            tracing::error!(
                %block_hash,
                "failed to restore BMM requests on block disconnect: {:#}",
                ErrorChain::new(&err),
            );
        }

        // Aside from `bmm_requests` (restored above), no disconnect logic is
        // applied to the rest of the policy DB. Those tables are wiped upon
        // generating a new block, so sidechain proposals etc. must be re-created
        // if a block that brought one into existence is disconnected.
        Ok(res)
    }

    type AcceptTxError = <Validator as CusfEnforcer>::AcceptTxError;

    fn accept_tx<TxRef>(
        &mut self,
        tx: &Transaction,
        tx_inputs: &HashMap<Txid, TxRef>,
    ) -> Result<TxAcceptAction, Self::AcceptTxError>
    where
        TxRef: std::borrow::Borrow<Transaction>,
    {
        self.inner.validator.clone().accept_tx(tx, tx_inputs)
    }
}

impl CusfBlockProducer for BlockProducer {
    type InitialBlockTemplateError = error::InitialBlockTemplate;

    /// Called when the RPC server starts producing a block template:
    /// 1. the RPC server (in `cusf_enforcer_mempool`) receives the request,
    /// 2. it fetches the initial block template (this function),
    /// 3. it processes that further and returns it to the client.
    ///
    /// This is the hook for adding drivechain coinbase messages to the
    /// about-to-be-generated block.
    async fn initial_block_template<const COINBASE_TXN: bool>(
        &self,
        parent_block_hash: &BlockHash,
        coinbase_txn_wit: BoolWit<COINBASE_TXN>,
        template: &mut InitialBlockTemplate<COINBASE_TXN>,
    ) -> Result<(), Self::InitialBlockTemplateError>
    where
        Bool<COINBASE_TXN>: CoinbaseTxn,
    {
        let res = self
            .initial_block_template_inner(parent_block_hash, coinbase_txn_wit, template)
            .await;
        self.record_gbt_result(&res);
        res
    }

    type FinalizeBlockTemplateError = error::FinalizeBlockTemplate;

    async fn finalize_block_template<const COINBASE_TXN: bool>(
        &self,
        parent_block_hash: &BlockHash,
        coinbase_txn_wit: BoolWit<COINBASE_TXN>,
        template: &mut FilledBlockTemplate<COINBASE_TXN>,
    ) -> Result<(), Self::FinalizeBlockTemplateError>
    where
        Bool<COINBASE_TXN>: CoinbaseTxn,
    {
        let res = self
            .block_template_suffix_inner(parent_block_hash, coinbase_txn_wit, template)
            .await;
        self.record_gbt_result(&res);
        res
    }
}

impl BlockProducer {
    async fn initial_block_template_inner<const COINBASE_TXN: bool>(
        &self,
        parent_block_hash: &BlockHash,
        coinbase_txn_wit: BoolWit<COINBASE_TXN>,
        template: &mut InitialBlockTemplate<COINBASE_TXN>,
    ) -> Result<(), error::InitialBlockTemplate>
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

            let ack_all_proposals = self
                .db()
                .get_ack_all_proposals()
                .await
                .map_err(error::InitialBlockTemplateInner::GetAckAllProposals)?;
            let () = self
                .extend_coinbase_txouts(ack_all_proposals, mainchain_tip, coinbase_txouts)
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
            // Reserve suffix txs
            {
                let fake_ctips = HashMap::from_iter((0..=u8::MAX).map(|slot_number| {
                    let fake_ctip = crate::types::Ctip {
                        outpoint: bitcoin::OutPoint {
                            txid: bitcoin::Txid::from_byte_array([slot_number; 32]),
                            vout: 0,
                        },
                        value: bitcoin::Amount::MAX_MONEY,
                    };
                    (slot_number.into(), fake_ctip)
                }));
                let fake_suffix_txs = self.generate_suffix_txs(&fake_ctips).await?;
                template
                    .suffix_txs
                    .extend(fake_suffix_txs.into_iter().map(|tx| {
                        initial_block_template::SuffixTxsItem::Reserved {
                            weight: tx.weight(),
                        }
                    }));
            }
        }
        // FIXME: set prefix txns and exclude mempool txs
        Ok(())
    }

    async fn block_template_suffix_inner<const COINBASE_TXN: bool>(
        &self,
        _parent_block_hash: &BlockHash,
        coinbase_txn_wit: BoolWit<COINBASE_TXN>,
        template: &mut FilledBlockTemplate<COINBASE_TXN>,
    ) -> Result<(), error::FinalizeBlockTemplate>
    where
        Bool<COINBASE_TXN>: CoinbaseTxn,
    {
        let template = template.as_mut();
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
                let coinbase_txouts = wit
                    .map(CoinbaseTxouts)
                    .in_mut()
                    .to_right(template.coinbase_txouts);
                let mut coinbase_builder = CoinbaseBuilder::new(coinbase_txouts)?;
                for (tx, _) in template.prefix_txs {
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
                    .map_err(error::FinalizeBlockTemplateInner::GenerateSuffixCoinbaseTxouts)?;
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
                    output: coinbase_txouts.clone(),
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
                .map_err(|reason| {
                    error::FinalizeBlockTemplateInner::InitialBlockTemplate { reason }
                })?;
                let suffix_txs = self.generate_suffix_txs(&ctips).await?;
                template
                    .suffix_txs
                    .extend(suffix_txs.into_iter().map(|tx| (tx, bitcoin::Amount::ZERO)));
                Ok(())
            }
            BoolWit::False(_wit) => Ok(()),
        }
    }
}
