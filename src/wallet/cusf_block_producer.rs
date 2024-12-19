use std::collections::HashMap;

use bitcoin::{hashes::Hash as _, BlockHash, Transaction, Txid};
use cusf_enforcer_mempool::{
    cusf_block_producer::{
        typewit::const_marker::{Bool, BoolWit},
        CoinbaseTxn, CoinbaseTxouts, CusfBlockProducer, InitialBlockTemplate,
    },
    cusf_enforcer::{ConnectBlockAction, CusfEnforcer},
};

use crate::{
    validator::Validator,
    wallet::{error, Wallet},
};

impl CusfEnforcer for Wallet {
    type SyncError = <Validator as CusfEnforcer>::SyncError;

    async fn sync_to_tip(&mut self, tip: BlockHash) -> std::result::Result<(), Self::SyncError> {
        self.inner.validator.clone().sync_to_tip(tip).await
    }

    type ConnectBlockError = error::ConnectBlockError;

    fn connect_block(
        &mut self,
        block: &bitcoin::Block,
    ) -> Result<ConnectBlockAction, Self::ConnectBlockError> {
        let res = self.inner.validator.clone().connect_block(block)?;
        let block_info = self.inner.validator.get_block_info(&block.block_hash())?;
        let () = self.inner.handle_connect_block(block_info)?;
        Ok(res)
    }

    type DisconnectBlockError = <Validator as CusfEnforcer>::DisconnectBlockError;

    fn disconnect_block(
        &mut self,
        block_hash: BlockHash,
    ) -> std::result::Result<(), Self::DisconnectBlockError> {
        self.inner.validator.clone().disconnect_block(block_hash)
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

    fn initial_block_template<const COINBASE_TXN: bool>(
        &self,
        coinbase_txn_wit: BoolWit<COINBASE_TXN>,
        mut template: InitialBlockTemplate<COINBASE_TXN>,
    ) -> Result<InitialBlockTemplate<COINBASE_TXN>, Self::InitialBlockTemplateError>
    where
        Bool<COINBASE_TXN>: CoinbaseTxn,
    {
        if let BoolWit::True(wit) = coinbase_txn_wit {
            let mainchain_tip = self.validator().get_mainchain_tip()?;
            let wit = wit.map(CoinbaseTxouts);
            let coinbase_txouts: &mut Vec<_> = wit.in_mut().to_right(&mut template.coinbase_txouts);
            coinbase_txouts.extend(self.generate_coinbase_txouts(true, mainchain_tip)?);
        }
        // FIXME: set prefix txns and exclude mempool txs
        Ok(template)
    }

    type SuffixTxsError = error::SuffixTxs;

    fn suffix_txs<const COINBASE_TXN: bool>(
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
