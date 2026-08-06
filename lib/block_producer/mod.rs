use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

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
    types::{BlindedM6, M6id, PendingM6idInfo, SidechainNumber, WithdrawalBundleEventKind},
    validator::Validator,
};

mod coinbase;
pub mod db;
pub mod error;
mod mine;

pub use self::db::Db;

/// Bundle proposals for a sidechain, joined with the validator's view of how
/// each one is doing (vote count etc).
pub(crate) type BundleProposals = Vec<(M6id, BlindedM6<'static>, Option<PendingM6idInfo>)>;

struct Inner {
    validator: Validator,
    db: Db,
    main_client: bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
    gbt_client: bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
    config: crate::cli::Config,
    // Always Some(_) on signets
    signet_challenge: Option<bitcoin::ScriptBuf>,
    /// Error from the most recent failed block template build, cleared on
    /// success. The GBT server reports template failures to its JSON-RPC client
    /// in a field that `bitcoin-cli` (and thus the signet miner's stderr) drops,
    /// so `GenerateToAddress` attaches this to its own error to surface the
    /// root cause.
    last_gbt_error: parking_lot::RwLock<Option<String>>,
    /// Limits `GenerateToAddress` to one concurrent call at a time.
    generate_blocks_semaphore: Arc<tokio::sync::Semaphore>,
}

#[derive(Clone)]
pub struct BlockProducer {
    inner: Arc<Inner>,
}

impl BlockProducer {
    pub fn new(
        data_dir: &std::path::Path,
        validator: Validator,
        main_client: bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
        gbt_client: bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient,
        config: crate::cli::Config,
        signet_challenge: Option<bitcoin::ScriptBuf>,
    ) -> Result<Self, error::InitDbConnection> {
        let db = Db::new(data_dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                validator,
                db,
                main_client,
                gbt_client,
                config,
                signet_challenge,
                last_gbt_error: parking_lot::RwLock::new(None),
                generate_blocks_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            }),
        })
    }

    pub fn validator(&self) -> &Validator {
        &self.inner.validator
    }

    /// JSON-RPC client for the Bitcoin Core node.
    pub(crate) fn main_client(&self) -> &bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient {
        &self.inner.main_client
    }

    pub(crate) fn gbt_client(&self) -> &bitcoin_jsonrpsee::jsonrpsee::http_client::HttpClient {
        &self.inner.gbt_client
    }

    pub(crate) fn config(&self) -> &crate::cli::Config {
        &self.inner.config
    }

    pub(crate) fn signet_challenge(&self) -> Option<&bitcoin::Script> {
        self.inner.signet_challenge.as_deref()
    }

    pub fn generate_blocks_semaphore(&self) -> &Arc<tokio::sync::Semaphore> {
        &self.inner.generate_blocks_semaphore
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

    /// Sink a dead bid's fee to the floor in bitcoind's mempool.
    ///
    /// A bid that lost its auction can never be mined -- `handle_m8` rejects
    /// any block carrying an M8 whose `prev_mainchain_block_hash` isn't that
    /// block's actual parent -- but nothing removes it from the mempool,
    /// where it goes on holding its inputs. Core weighs *modified* fees when
    /// judging whether a replacement pays enough, so sinking this one is what
    /// lets the next bid reuse those inputs instead of having to outbid a
    /// transaction that cannot win. Without it, a client bidding a fixed
    /// amount is locked out from its first loss onwards.
    ///
    /// `cusf-enforcer-mempool`'s sync task does this for the validator's
    /// seen-bid table, which only `accept_tx` fills, so that one is empty
    /// whenever we run without a mempool. Our own tracked bids are recorded
    /// at broadcast time, in every mode; deltas from the two paths stack.
    ///
    /// Best-effort: an unreachable bitcoind must not fail block connection.
    async fn deprioritize_lost_bids(&self, lost: &HashSet<Txid>) {
        use bitcoin_jsonrpsee::client::MainClient as _;
        const NEGATIVE_MAX_SATS: i64 = -(21_000_000 * 100_000_000);
        for txid in lost {
            match self
                .inner
                .main_client
                .prioritize_transaction(*txid, NEGATIVE_MAX_SATS)
                .await
            {
                Ok(_) => tracing::debug!(%txid, "deprioritized bid that lost its auction"),
                Err(err) => tracing::warn!(
                    %txid,
                    "failed to deprioritize losing bid: {:#}",
                    ErrorChain::new(&err),
                ),
            }
        }
    }

    /// Policy-table maintenance for a block the validator just accepted: drop the
    /// sidechain proposals and withdrawal bundles that this block settled, so we
    /// stop re-proposing them in later coinbases.
    ///
    /// Returns the tracked bids this block killed: they targeted the slot it
    /// just filled, and it did not include them. Their rows stay -- a later
    /// bid replaces those transactions -- but a caller with a BDK wallet must
    /// evict them, or BDK goes on treating their inputs as spent.
    pub(crate) async fn apply_connected_block_policy(
        &self,
        block: &bitcoin::Block,
        block_info: &crate::types::BlockInfo,
    ) -> Result<HashSet<Txid>, rusqlite::Error> {
        // Sort the tracked bids by what this block did to each. Skipped when
        // nothing is tracked, so a non-bidding node does no per-tx hashing.
        let tracked_bmm_bids = self.inner.db.tracked_bmm_bids().await?;
        let mut lost: HashSet<Txid> = HashSet::new();
        if !tracked_bmm_bids.is_empty() {
            let block_txids: HashSet<Txid> =
                block.txdata.iter().map(|tx| tx.compute_txid()).collect();
            let mut won: Vec<Txid> = Vec::new();
            for (txid, prev_block_hash) in tracked_bmm_bids {
                if block_txids.contains(&txid) {
                    won.push(txid);
                } else if prev_block_hash == block.header.prev_blockhash {
                    // Bid for the slot this block just filled, and not in it.
                    // Bids for older slots already lost, and were sunk then.
                    lost.insert(txid);
                }
            }
            let deleted = self
                .inner
                .db
                .delete_won_bmm_requests(&block.block_hash(), &won)
                .await?;
            if deleted > 0 {
                tracing::debug!(
                    block_hash = %block.block_hash(),
                    "settled {deleted} BMM request(s) confirmed by this block",
                );
            }
            let () = self.deprioritize_lost_bids(&lost).await;
        }
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
        let () = self
            .inner
            .db
            .delete_pending_sidechain_proposals(sidechain_proposal_ids)
            .await?;
        Ok(lost)
    }

    /// Restore the tracked bids of every block a reorg has left behind.
    ///
    /// `disconnect_block` only covers reorgs seen live. One resolved while the
    /// enforcer is down never reaches it -- the validator walks back to the
    /// last common ancestor inside its own sync -- so the row stays stranded
    /// in `bmm_requests_undo` while bitcoind puts the bid's transaction back
    /// in its mempool: untracked, so nothing replaces or deprioritizes it, and
    /// the next bid becomes a second live M8 for the sidechain. Running after
    /// every sync covers reorgs of any depth, seen or unseen, and repairs
    /// databases already carrying stranded rows.
    ///
    /// Policy SQLite is best-effort here as on disconnect: failures are logged
    /// rather than failing the sync.
    async fn restore_disconnected_bmm_requests(&self) {
        let block_hashes = match self.inner.db.bmm_requests_undo_block_hashes().await {
            Ok(block_hashes) => block_hashes,
            Err(err) => {
                tracing::error!(
                    "failed to read restorable BMM requests: {:#}",
                    ErrorChain::new(&err),
                );
                return;
            }
        };
        for block_hash in block_hashes {
            match self.inner.validator.is_on_active_chain(&block_hash) {
                Ok(true) => continue,
                Ok(false) => (),
                Err(err) => {
                    tracing::error!(
                        %block_hash,
                        "failed to check whether a block is on the active chain: {:#}",
                        ErrorChain::new(&err),
                    );
                    continue;
                }
            }
            if let Err(err) = self.inner.db.restore_bmm_requests(&block_hash).await {
                tracing::error!(
                    %block_hash,
                    "failed to restore BMM requests of a disconnected block: {:#}",
                    ErrorChain::new(&err),
                );
            }
        }
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
        let () = self
            .inner
            .validator
            .clone()
            .sync_to_tip(shutdown_signal, tip_hash)
            .await?;
        let () = self.restore_disconnected_bmm_requests().await;
        Ok(())
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
                // Losing bids are dropped: a producer without a wallet has no
                // BDK view to evict them from, and the deprioritization
                // already happened inside.
                let _lost = self.apply_connected_block_policy(block, block_info).await?;
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
        let BoolWit::True(wit) = coinbase_txn_wit else {
            return Err(error::InitialBlockTemplateInner::NoCoinbaseTxn.into());
        };
        tracing::debug!(
            "CUSF block producer: extending initial block template with coinbase TX outputs"
        );

        tracing::debug!(
            "Initial coinbase txouts pre-extension: {:?}",
            template.coinbase_txouts
        );

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
            .extend_coinbase_txouts(ack_all_proposals, *parent_block_hash, coinbase_txouts)
            .await?;
        tracing::debug!(
            "Initial coinbase txouts post-extension: {:?}",
            coinbase_txouts
        );
        // Settle each sidechain's auction here, before selection, and exclude
        // the bids that lost it. Left to the mempool, the winner would be
        // whichever bid has the highest ancestor fee *rate*, because that is
        // the order `propose_txs` walks and it drops the rest as conflicts --
        // so a compact 9,000-sat bid would beat a padded 10,000-sat one, and
        // direct mining, which weighs the bids themselves, would disagree
        // about the same mempool.
        {
            let seen_bmm_requests = self
                .validator()
                .get_seen_bmm_requests_for_parent_block(*parent_block_hash)?;
            let fees = self.validator().get_seen_bmm_request_fees(
                seen_bmm_requests
                    .values()
                    .flat_map(|by_commitment| by_commitment.values().flatten().copied()),
            )?;
            for (sidechain_number, by_commitment) in &seen_bmm_requests {
                let candidates: Option<Vec<_>> = by_commitment
                    .iter()
                    .flat_map(|(commitment, txids)| {
                        txids.iter().map(move |txid| (commitment, txid))
                    })
                    .map(|(commitment, txid)| {
                        let fee = bitcoin::Amount::from_sat(*fees.get(txid)?);
                        Some((*sidechain_number, *commitment, *txid, fee))
                    })
                    .collect();
                // A slot holding a bid we cannot price is left alone:
                // excluding its rivals would award the slot on no evidence.
                let winner = candidates
                    .and_then(|candidates| {
                        coinbase::bmm_auction_winners(candidates).remove(sidechain_number)
                    })
                    .or_else(|| {
                        tracing::warn!(%sidechain_number, "BMM auction not settled: a bid's fee is unknown");
                        None
                    });
                // Kept by txid, not by commitment: BIP301 allows one M8 per
                // slot, and a duplicate carrying the winner's `(S, H)` is
                // still a second one.
                let Some((_commitment, winner_txid)) = winner else {
                    continue;
                };
                template.exclude_mempool_txs.extend(
                    by_commitment
                        .values()
                        .flatten()
                        .copied()
                        .filter(|txid| *txid != winner_txid),
                );
            }
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
                .extend(fake_suffix_txs.into_iter().map(|(tx, _fee)| {
                    initial_block_template::SuffixTxsItem::Reserved {
                        weight: tx.weight(),
                    }
                }));
        }
        Ok(())
    }

    async fn block_template_suffix_inner<const COINBASE_TXN: bool>(
        &self,
        parent_block_hash: &BlockHash,
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
                // Accept the requests that made it into the template with
                // their slot still free -- this is where a bid from another
                // wallet earns its M7. The tx set is fixed by now, so the
                // rule's other verdicts are the exclusion pass's to enforce.
                //
                // Which transaction claimed each slot is tracked here rather
                // than read back off the coinbase: a duplicate carrying the
                // same `(S, H)` would match the M7 already written and be
                // waved through as the same request.
                let mut accepted_by_slot: HashMap<SidechainNumber, Txid> = HashMap::new();
                for (tx, _) in template.prefix_txs {
                    if let Some(bmm_request) = parse_m8_tx(tx) {
                        let txid = tx.compute_txid();
                        let claimed = accepted_by_slot.get(&bmm_request.sidechain_number).copied();
                        if coinbase::classify_bmm_request(
                            claimed,
                            parent_block_hash,
                            txid,
                            &bmm_request,
                        ) == coinbase::BmmRequestAction::Accept
                        {
                            accepted_by_slot.insert(bmm_request.sidechain_number, txid);
                            coinbase_builder.bmm_accept(
                                bmm_request.sidechain_number,
                                bmm_request.sidechain_block_hash,
                            )?;
                        }
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
                template.suffix_txs.extend(suffix_txs);
                Ok(())
            }
            BoolWit::False(_wit) => Ok(()),
        }
    }
}
