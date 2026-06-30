use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use bdk_wallet::keys::{DerivableKey as _, ExtendedKey};
use bitcoin::{
    BlockHash, OutPoint, Transaction, TxOut, Txid,
    bip32::{ChildNumber, DerivationPath, Xpriv},
    hashes::Hash as _,
    secp256k1::Secp256k1,
};
use bitcoin_jsonrpsee::client::{GetRawTransactionClient, GetRawTransactionVerbose};
use either::Either;

use super::{
    bip44_coin_type, bip47,
    scan::{ScanContext, ScanCursor, ScanEvent, ScrubOnDrop, scan_tx},
    silent_payments,
};
use crate::wallet::{WalletInner, error};

const PREVOUT_RPC_CONCURRENCY: usize = 16;
const BIP47_RECEIVE_LOOKAHEAD_LIVE: u32 = 20;
const BIP47_RECEIVE_LOOKAHEAD_DEEP_RESCAN: u32 = 1000;

impl WalletInner {
    pub(in crate::wallet) async fn master_xpriv_inner(
        &self,
    ) -> Result<Xpriv, error::ReusablePayments> {
        let mnemonic = {
            let connection = self.self_db.lock().await;
            let read = Self::read_db_mnemonic(&connection)?;
            drop(connection);
            match read {
                Some(Either::Left(m)) => m,
                Some(Either::Right(_)) => return Err(error::ReusablePayments::WalletEncrypted),
                None => return Err(error::ReusablePayments::WalletNotFound),
            }
        };
        let extended: ExtendedKey = mnemonic
            .into_extended_key()
            .map_err(error::ReusablePayments::MnemonicToExtendedKey)?;
        let network: bitcoin::Network = self.validator.network();
        extended
            .into_xprv(network.into())
            .ok_or(error::ReusablePayments::DeriveMasterXpriv)
    }

    pub(in crate::wallet) async fn ensure_wallet_unlocked(
        &self,
    ) -> Result<(), error::ReusablePayments> {
        let connection = self.self_db.lock().await;
        let read = Self::read_db_mnemonic(&connection)?;
        drop(connection);
        match read {
            Some(Either::Left(_)) => Ok(()),
            Some(Either::Right(_)) => Err(error::ReusablePayments::WalletEncrypted),
            None => Err(error::ReusablePayments::WalletNotFound),
        }
    }

    pub(in crate::wallet) async fn bip47_payment_code_inner(
        &self,
        version: bip47::Version,
    ) -> Result<bip47::PaymentCode, error::ReusablePayments> {
        let master = self.master_xpriv_inner().await?;
        let secp = Secp256k1::new();
        let network = self.validator.network();
        let coin = bip44_coin_type(network);
        let account = master.derive_priv(&secp, &bip47::account_path(coin)?)?;
        Ok(bip47::payment_code_from_account(&account, version, &secp))
    }

    pub(in crate::wallet) async fn silent_payment_address_inner(
        &self,
        label: Option<u32>,
    ) -> Result<silent_payments::SilentPaymentAddress, error::ReusablePayments> {
        let master = self.master_xpriv_inner().await?;
        let secp = Secp256k1::new();
        let network = self.validator.network();
        let coin = bip44_coin_type(network);
        let scan_path = silent_payments::sp_path(coin, 0, 1)?;
        let spend_path = silent_payments::sp_path(coin, 0, 0)?;
        let b_scan = master.derive_priv(&secp, &scan_path)?.private_key;
        let b_spend = master.derive_priv(&secp, &spend_path)?.private_key;
        Ok(silent_payments::derive_address(
            &b_scan, &b_spend, label, network, &secp,
        )?)
    }

    pub(in crate::wallet) async fn compute_bip47_send_address(
        &self,
        recipient: &bip47::PaymentCode,
        i: u32,
    ) -> Result<bitcoin::Address, error::ReusablePayments> {
        let master = self.master_xpriv_inner().await?;
        let secp = Secp256k1::new();
        let network = self.validator.network();
        let coin = bip44_coin_type(network);
        let account = master.derive_priv(&secp, &bip47::account_path(coin)?)?;
        Ok(bip47::derive_send_address_from_account(
            &account, recipient, i, network, &secp,
        )?)
    }

    pub(in crate::wallet) async fn compute_bip47_blinded_payload(
        &self,
        recipient: &bip47::PaymentCode,
        version: bip47::Version,
        designated_outpoint: OutPoint,
        designated_spk: bitcoin::ScriptBuf,
    ) -> Result<(Vec<u8>, bip47::PaymentCode), error::ReusablePayments> {
        let (keychain, index) = {
            let wallet_read = self.read_wallet().await?;
            wallet_read
                .derivation_of_spk(designated_spk)
                .ok_or_else(|| {
                    error::ReusablePayments::Rpc(
                        "designated input not in wallet's descriptor".to_string(),
                    )
                })?
        };
        let master = self.master_xpriv_inner().await?;
        let secp = Secp256k1::new();
        let network = self.validator.network();
        let coin = bip44_coin_type(network);

        let sender_acct = master.derive_priv(&secp, &bip47::account_path(coin)?)?;
        let designated_priv =
            ScrubOnDrop::new(descriptor_branch_priv(&master, &secp, keychain, index)?);
        Ok(bip47::build_blinded_payload(
            &sender_acct,
            version,
            &designated_priv,
            recipient,
            designated_outpoint,
            &secp,
        )?)
    }

    /// Build the v3 notification multisig script `OP_1 <A> <F> <G> OP_3 OP_CHECKMULTISIG`.
    pub(in crate::wallet) async fn compute_bip47_v3_notification_script(
        &self,
        recipient: &bip47::PaymentCode,
        ephemeral_priv: &bitcoin::secp256k1::SecretKey,
    ) -> Result<bitcoin::ScriptBuf, error::ReusablePayments> {
        let master = self.master_xpriv_inner().await?;
        let secp = Secp256k1::new();
        let network = self.validator.network();
        let coin = bip44_coin_type(network);
        let sender_acct = master.derive_priv(&secp, &bip47::account_path(coin)?)?;
        Ok(bip47::build_v3_notification_script(
            &sender_acct,
            ephemeral_priv,
            recipient,
            &secp,
        )?)
    }

    pub(in crate::wallet) async fn compute_sp_outputs(
        &self,
        recipients: &[silent_payments::Recipient],
        eligible_input_inputs: &[(OutPoint, bitcoin::ScriptBuf)],
        all_outpoints: &[OutPoint],
    ) -> Result<Vec<bitcoin::TxOut>, error::ReusablePayments> {
        let derivations: Vec<(bdk_wallet::KeychainKind, u32)> = {
            let wallet_read = self.read_wallet().await?;
            eligible_input_inputs
                .iter()
                .map(|(outpoint, spk)| {
                    if !super::silent_payments::is_bip352_eligible_spk(spk) {
                        return Err(error::ReusablePayments::SilentPaymentCrypto(
                            silent_payments::CryptoError::IneligibleInput {
                                outpoint: *outpoint,
                            },
                        ));
                    }
                    wallet_read.derivation_of_spk(spk.clone()).ok_or_else(|| {
                        error::ReusablePayments::Rpc(
                            "sp send: input not in wallet's descriptor".to_string(),
                        )
                    })
                })
                .collect::<Result<_, _>>()?
        };

        let master = self.master_xpriv_inner().await?;
        let secp = Secp256k1::new();

        // EligibleInput scrubs its secret on drop, on all return paths.
        let mut eligible_inputs: Vec<silent_payments::EligibleInput> =
            Vec::with_capacity(eligible_input_inputs.len());
        for ((outpoint, spk), (kind, idx)) in eligible_input_inputs.iter().zip(derivations) {
            let sk = descriptor_branch_priv(&master, &secp, kind, idx)?;
            eligible_inputs.push(silent_payments::EligibleInput::new(
                *outpoint, sk, spk, &secp,
            ));
        }

        Ok(silent_payments::compute_outputs(
            &eligible_inputs,
            all_outpoints,
            recipients,
            &secp,
        )?)
    }

    pub(in crate::wallet) async fn build_scan_context(
        &self,
    ) -> Result<ScanContext, error::ReusablePayments> {
        self.build_scan_context_with_lookahead(BIP47_RECEIVE_LOOKAHEAD_LIVE)
            .await
    }

    pub(in crate::wallet) async fn build_scan_context_for_deep_rescan(
        &self,
    ) -> Result<ScanContext, error::ReusablePayments> {
        self.build_scan_context_with_lookahead(BIP47_RECEIVE_LOOKAHEAD_DEEP_RESCAN)
            .await
    }

    async fn build_scan_context_with_lookahead(
        &self,
        lookahead: u32,
    ) -> Result<ScanContext, error::ReusablePayments> {
        // Read everything the context needs from the DB before deriving any
        // secrets so no key material is held across an await point
        let label_ms: Vec<u32> = {
            let connection = self.self_db.lock().await;
            let mut stmt = connection
                .prepare("SELECT m FROM silent_payment_labels WHERE m >= 1")
                .map_err(error::ReusablePayments::db)?;
            let collected: Result<Vec<i64>, _> = stmt
                .query_map([], |row| row.get::<_, i64>(0))
                .and_then(|iter| iter.collect());
            drop(stmt);
            drop(connection);
            collected
                .map_err(error::ReusablePayments::db)?
                .into_iter()
                .filter_map(|m| u32::try_from(m).ok())
                .collect()
        };
        let payers = self.read_inbound_payers().await?;
        let master = self.master_xpriv_inner().await?;

        let secp = Secp256k1::new();
        let network = self.validator.network();
        let coin = bip44_coin_type(network);

        let bip47_acct = master.derive_priv(&secp, &bip47::account_path(coin)?)?;
        let scan_path = silent_payments::sp_path(coin, 0, 1)?;
        let spend_path = silent_payments::sp_path(coin, 0, 0)?;
        let b_scan = master.derive_priv(&secp, &scan_path)?.private_key;
        let b_spend_pub = master
            .derive_priv(&secp, &spend_path)?
            .private_key
            .public_key(&secp);

        Ok(super::scan::build_scan_context(
            &bip47_acct,
            b_scan,
            b_spend_pub,
            &label_ms,
            &payers,
            lookahead,
            network,
            &secp,
        )?)
    }

    async fn read_inbound_payers(
        &self,
    ) -> Result<Vec<(bip47::PaymentCode, u32)>, error::ReusablePayments> {
        let connection = self.self_db.lock().await;
        let mut stmt = connection
            .prepare(
                "SELECT sender_payment_code, next_receive_index \
                 FROM bip47_inbound_payers",
            )
            .map_err(error::ReusablePayments::db)?;
        let rows: Result<Vec<(String, u32)>, _> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
            })
            .and_then(|iter| iter.collect());
        drop(stmt);
        drop(connection);
        Ok(rows
            .map_err(error::ReusablePayments::db)?
            .into_iter()
            .filter_map(|(code, idx)| bip47::PaymentCode::from_str(&code).ok().map(|c| (c, idx)))
            .collect())
    }

    async fn derive_bip47_entries_for_new_payer(
        &self,
        sender: &bip47::PaymentCode,
        lookahead: u32,
    ) -> Result<
        std::collections::HashMap<[u8; 20], super::scan::Bip47ReceiveSource>,
        error::ReusablePayments,
    > {
        let master = self.master_xpriv_inner().await?;
        let secp = Secp256k1::new();
        let network = self.validator.network();
        let coin = bip44_coin_type(network);
        let account = master.derive_priv(&secp, &bip47::account_path(coin)?)?;
        Ok(super::scan::build_bip47_receive_lookup(
            &account,
            &[(sender.clone(), 0)],
            lookahead,
            network,
            &secp,
        )?)
    }

    async fn resolve_block_prevouts(
        &self,
        block: &bitcoin::Block,
    ) -> Result<HashMap<OutPoint, TxOut>, error::ReusablePayments> {
        let mut in_block: HashMap<Txid, &Transaction> = HashMap::new();
        for tx in &block.txdata {
            in_block.insert(tx.compute_txid(), tx);
        }

        let zero_txid = Txid::all_zeros();
        let mut to_fetch: HashSet<Txid> = HashSet::new();
        for tx in &block.txdata {
            for input in &tx.input {
                let prev = input.previous_output.txid;
                if prev == zero_txid || in_block.contains_key(&prev) {
                    continue;
                }
                to_fetch.insert(prev);
            }
        }

        let to_fetch: Vec<Txid> = to_fetch.into_iter().collect();
        use futures::stream::{StreamExt, TryStreamExt};
        let client = &self.main_client;
        let fetched_vec: Vec<(Txid, Transaction)> = futures::stream::iter(to_fetch)
            .map(|txid| async move {
                let hex = client
                    .get_raw_transaction(txid, GetRawTransactionVerbose::<false>, None)
                    .await
                    .map_err(|err| error::ReusablePayments::Rpc(err.to_string()))?;
                let tx: Transaction = bitcoin::consensus::encode::deserialize_hex(&hex)
                    .map_err(|e| error::ReusablePayments::ConsensusDecode(e.to_string()))?;
                Ok::<(Txid, Transaction), error::ReusablePayments>((txid, tx))
            })
            .buffered(PREVOUT_RPC_CONCURRENCY)
            .try_collect()
            .await?;
        let fetched: HashMap<Txid, Transaction> = fetched_vec.into_iter().collect();

        let mut prevouts = HashMap::new();
        for tx in &block.txdata {
            for input in &tx.input {
                let prev = input.previous_output;
                if prev.txid == zero_txid {
                    continue;
                }
                let prev_tx_opt: Option<&Transaction> = fetched
                    .get(&prev.txid)
                    .or_else(|| in_block.get(&prev.txid).copied());
                if let Some(prev_tx) = prev_tx_opt
                    && let Some(txout) = prev_tx.output.get(prev.vout as usize)
                {
                    prevouts.insert(prev, txout.clone());
                }
            }
        }
        Ok(prevouts)
    }

    pub(in crate::wallet) async fn scan_block_for_reusable_payments(
        &self,
        block: &bitcoin::Block,
        height: u32,
    ) -> Result<(), error::ReusablePayments> {
        let _scanner_guard = self
            .scanner_lock
            .acquire()
            .await
            .expect("scanner semaphore is never closed");
        self.scan_block_for_reusable_payments_locked(block, height)
            .await
    }

    pub(in crate::wallet) async fn scan_block_for_reusable_payments_locked(
        &self,
        block: &bitcoin::Block,
        height: u32,
    ) -> Result<(), error::ReusablePayments> {
        let mut ctx = match self.build_scan_context().await {
            Ok(ctx) => ctx,
            Err(error::ReusablePayments::WalletNotFound)
            | Err(error::ReusablePayments::WalletEncrypted) => return Ok(()),
            Err(err) => return Err(err),
        };
        self.scan_block_with_ctx(block, height, &mut ctx).await
    }

    pub(in crate::wallet) async fn scan_block_with_ctx(
        &self,
        block: &bitcoin::Block,
        height: u32,
        ctx: &mut ScanContext,
    ) -> Result<(), error::ReusablePayments> {
        let cursor = match self.load_scan_cursor_if_present().await? {
            Some(c) => c,
            None => return Ok(()),
        };

        if height < cursor.birthday_height {
            return Ok(());
        }

        if let Some(prev_seen) = cursor.last_scanned_block_hash
            && height == cursor.last_scanned_height + 1
            && block.header.prev_blockhash != prev_seen
        {
            tracing::warn!(
                height,
                expected = %prev_seen,
                got = %block.header.prev_blockhash,
                "reusable-payments: reorg detected, rolling back scan cursor",
            );
            self.drop_scan_events_above(cursor.last_scanned_height)
                .await?;
            return Ok(());
        }

        let prevouts = self.resolve_block_prevouts(block).await?;
        let scanned = super::scan::block_to_scanned_txs(block, &prevouts);

        let secp = Secp256k1::new();
        let mut events: Vec<ScanEvent> = Vec::new();
        for tx in &scanned {
            match scan_tx(tx, ctx, &secp) {
                Ok(tx_events) => events.extend(tx_events),
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        txid = %tx.txid,
                        "scan_tx skipping tx (non-fatal crypto error)",
                    );
                }
            }
        }
        let mut new_senders: Vec<bip47::PaymentCode> = Vec::new();
        for event in &events {
            if let ScanEvent::Bip47Notification { sender, .. } = event {
                let code_str = sender.to_string();
                let already_known = ctx
                    .bip47_receive_lookup
                    .values()
                    .any(|s| s.sender_payment_code == code_str);
                if !already_known && !new_senders.contains(sender) {
                    new_senders.push(sender.clone());
                }
            }
        }
        if !new_senders.is_empty() {
            let mut new_entries = std::collections::HashMap::new();
            for sender in &new_senders {
                let entries = self
                    .derive_bip47_entries_for_new_payer(sender, ctx.bip47_lookahead)
                    .await?;
                new_entries.extend(entries);
            }
            let mut recovered: Vec<ScanEvent> = Vec::new();
            for tx in &scanned {
                recovered.extend(super::scan::detect_bip47_receives(tx, &new_entries));
            }
            if !recovered.is_empty() {
                tracing::info!(
                    count = recovered.len(),
                    height,
                    "detected same-block BIP47 payment(s) for newly notified payer(s)",
                );
                events.extend(recovered);
            }
            ctx.bip47_receive_lookup.extend(new_entries);
        }

        let block_hash = block.block_hash();
        self.persist_scan_events(height, events).await?;
        self.save_scan_cursor(ScanCursor {
            birthday_height: cursor.birthday_height,
            last_scanned_height: height,
            last_scanned_block_hash: Some(block_hash),
        })
        .await?;
        Ok(())
    }

    pub(in crate::wallet) async fn ensure_scanner_activated(
        &self,
    ) -> Result<(), error::ReusablePayments> {
        if self.load_scan_cursor_if_present().await?.is_some() {
            return Ok(());
        }

        let tip_height = match self.get_tip().await {
            Ok(tip) => tip.height,
            Err(_) => 0,
        };
        let birthday = tip_height.max(1);

        let connection = self.self_db.lock().await;
        connection
            .execute(
                "INSERT INTO reusable_scan_state \
                 (wallet_id, birthday_height, last_scanned_height) \
                 VALUES (1, ?1, ?2) \
                 ON CONFLICT(wallet_id) DO NOTHING",
                rusqlite::params![birthday, birthday.saturating_sub(1)],
            )
            .map_err(error::ReusablePayments::db)?;
        drop(connection);

        tracing::info!(
            birthday_height = birthday,
            "reusable-payments scanner activated for this wallet"
        );
        Ok(())
    }

    pub(in crate::wallet) async fn load_scan_cursor_if_present(
        &self,
    ) -> Result<Option<ScanCursor>, error::ReusablePayments> {
        let connection = self.self_db.lock().await;
        let row: Option<(u32, u32, Option<Vec<u8>>)> = match connection.query_row(
            "SELECT birthday_height, last_scanned_height, last_scanned_block_hash \
             FROM reusable_scan_state WHERE wallet_id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ) {
            Ok(r) => Some(r),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => {
                drop(connection);
                return Err(error::ReusablePayments::db(e));
            }
        };
        drop(connection);
        Ok(row.map(|(birthday, last, hash_bytes)| ScanCursor {
            birthday_height: birthday,
            last_scanned_height: last,
            last_scanned_block_hash: hash_bytes.and_then(|b| BlockHash::from_slice(&b).ok()),
        }))
    }

    async fn save_scan_cursor(&self, cursor: ScanCursor) -> Result<(), error::ReusablePayments> {
        let map_err = error::ReusablePayments::db;
        let hash_bytes: Option<Vec<u8>> = cursor
            .last_scanned_block_hash
            .map(|h| h.as_byte_array().to_vec());
        let connection = self.self_db.lock().await;
        connection
            .execute(
                "INSERT INTO reusable_scan_state \
                 (wallet_id, birthday_height, last_scanned_height, last_scanned_block_hash) \
                 VALUES (1, ?1, ?2, ?3) \
                 ON CONFLICT(wallet_id) DO UPDATE SET \
                     last_scanned_height = excluded.last_scanned_height, \
                     last_scanned_block_hash = excluded.last_scanned_block_hash",
                rusqlite::params![
                    cursor.birthday_height,
                    cursor.last_scanned_height,
                    hash_bytes,
                ],
            )
            .map_err(map_err)?;
        drop(connection);
        Ok(())
    }

    async fn persist_scan_events(
        &self,
        block_height: u32,
        events: Vec<ScanEvent>,
    ) -> Result<(), error::ReusablePayments> {
        if events.is_empty() {
            return Ok(());
        }
        let map_err = error::ReusablePayments::db;
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let connection = self.self_db.lock().await;
        let tx = connection.unchecked_transaction().map_err(map_err)?;
        for event in events {
            match event {
                ScanEvent::Bip47Notification {
                    sender,
                    notification_txid,
                } => {
                    let code_str = sender.to_string();
                    let version_i64 = sender.version() as i64;
                    tx.execute(
                        "INSERT INTO bip47_inbound_payers \
                         (sender_payment_code, version, notification_txid, \
                          notification_height, next_receive_index, first_seen_unix) \
                         VALUES (?1, ?2, ?3, ?4, 0, ?5) \
                         ON CONFLICT(sender_payment_code) DO NOTHING",
                        rusqlite::params![
                            code_str,
                            version_i64,
                            notification_txid.as_byte_array().to_vec(),
                            block_height,
                            now_unix,
                        ],
                    )
                    .map_err(map_err)?;
                }
                ScanEvent::SilentPaymentReceive { txid, match_ } => {
                    let label_i64: Option<i64> = match_.label.map(|m| m as i64);
                    tx.execute(
                        "INSERT INTO silent_payment_received \
                         (txid, vout, output_pubkey, amount_sats, tweak_k, label_m, height) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                         ON CONFLICT(txid, vout) DO NOTHING",
                        rusqlite::params![
                            txid.as_byte_array().to_vec(),
                            match_.vout,
                            match_.output_xonly.serialize().to_vec(),
                            match_.amount.to_sat() as i64,
                            match_.tweak_k,
                            label_i64,
                            block_height,
                        ],
                    )
                    .map_err(map_err)?;
                }
                ScanEvent::Bip47PaymentReceive {
                    txid,
                    vout,
                    amount,
                    script_pubkey,
                    source,
                } => {
                    let version_i64 = source.version as i64;
                    let script_pubkey = script_pubkey.to_bytes();
                    tx.execute(
                        "INSERT INTO bip47_received \
                         (txid, vout, sender_payment_code, sender_index, version, \
                          amount_sats, script_pubkey, height) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                         ON CONFLICT(txid, vout) DO NOTHING",
                        rusqlite::params![
                            txid.as_byte_array().to_vec(),
                            vout,
                            source.sender_payment_code,
                            source.i,
                            version_i64,
                            amount.to_sat() as i64,
                            script_pubkey,
                            block_height,
                        ],
                    )
                    .map_err(map_err)?;
                    tx.execute(
                        "UPDATE bip47_inbound_payers \
                         SET next_receive_index = MAX(next_receive_index, ?1 + 1) \
                         WHERE sender_payment_code = ?2",
                        rusqlite::params![source.i, source.sender_payment_code],
                    )
                    .map_err(map_err)?;
                }
            }
        }
        tx.commit().map_err(map_err)?;
        drop(connection);
        Ok(())
    }

    pub(in crate::wallet) async fn load_bip47_send_state(
        &self,
        recipient_code: &str,
    ) -> Result<(Option<Txid>, u32), error::ReusablePayments> {
        let connection = self.self_db.lock().await;
        let row: Option<(Option<Vec<u8>>, u32)> = match connection.query_row(
            "SELECT notification_txid, next_send_index \
             FROM bip47_send_state WHERE recipient_payment_code = ?1",
            rusqlite::params![recipient_code],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ) {
            Ok(r) => Some(r),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => {
                drop(connection);
                return Err(error::ReusablePayments::db(e));
            }
        };
        drop(connection);
        Ok(match row {
            None => (None, 0),
            Some((txid_bytes, idx)) => (txid_bytes.and_then(|b| Txid::from_slice(&b).ok()), idx),
        })
    }

    pub(in crate::wallet) async fn persist_bip47_notification(
        &self,
        recipient_code: &str,
        version: super::bip47::Version,
        notification_txid: Txid,
        notification_height: u32,
    ) -> Result<(), error::ReusablePayments> {
        let map_err = error::ReusablePayments::db;
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let version_i64 = version as i64;
        let txid_bytes = notification_txid.as_byte_array().to_vec();
        let connection = self.self_db.lock().await;
        connection
            .execute(
                "INSERT INTO bip47_send_state \
                 (recipient_payment_code, version, notification_txid, \
                  notification_broadcast_at, notification_height, next_send_index) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 0) \
                 ON CONFLICT(recipient_payment_code) DO UPDATE SET \
                     notification_txid = excluded.notification_txid, \
                     notification_broadcast_at = excluded.notification_broadcast_at, \
                     notification_height = excluded.notification_height",
                rusqlite::params![
                    recipient_code,
                    version_i64,
                    txid_bytes,
                    now_unix,
                    notification_height,
                ],
            )
            .map_err(map_err)?;
        drop(connection);
        Ok(())
    }

    pub(in crate::wallet) async fn advance_bip47_next_index(
        &self,
        recipient_code: &str,
        new_index: u32,
    ) -> Result<(), error::ReusablePayments> {
        let connection = self.self_db.lock().await;
        connection
            .execute(
                "UPDATE bip47_send_state SET next_send_index = ?1 \
                 WHERE recipient_payment_code = ?2",
                rusqlite::params![new_index, recipient_code],
            )
            .map_err(error::ReusablePayments::db)?;
        drop(connection);
        Ok(())
    }

    pub(in crate::wallet) async fn rollback_bip47_notification(
        &self,
        recipient_code: &str,
    ) -> Result<(), error::ReusablePayments> {
        let map_err = error::ReusablePayments::db;
        let connection = self.self_db.lock().await;
        connection
            .execute(
                "UPDATE bip47_send_state \
                 SET notification_txid = NULL, notification_height = NULL, \
                     notification_broadcast_at = NULL \
                 WHERE recipient_payment_code = ?1",
                rusqlite::params![recipient_code],
            )
            .map_err(map_err)?;
        drop(connection);
        Ok(())
    }

    pub(in crate::wallet) async fn rollback_bip47_index_advance(
        &self,
        recipient_code: &str,
        restore_to: u32,
    ) -> Result<(), error::ReusablePayments> {
        let connection = self.self_db.lock().await;
        connection
            .execute(
                "UPDATE bip47_send_state SET next_send_index = ?1 \
                 WHERE recipient_payment_code = ?2 AND next_send_index = ?1 + 1",
                rusqlite::params![restore_to, recipient_code],
            )
            .map_err(error::ReusablePayments::db)?;
        drop(connection);
        Ok(())
    }

    pub(in crate::wallet) async fn notification_in_chain_or_mempool(
        &self,
        notif_txid: Txid,
    ) -> Result<bool, error::ReusablePayments> {
        use bitcoin_jsonrpsee::jsonrpsee::core::client::Error as JsonRpcError;
        const RPC_INVALID_ADDRESS_OR_KEY: i32 = -5;
        match self
            .main_client
            .get_raw_transaction(notif_txid, GetRawTransactionVerbose::<false>, None)
            .await
        {
            Ok(_) => Ok(true),
            Err(JsonRpcError::Call(err)) if err.code() == RPC_INVALID_ADDRESS_OR_KEY => Ok(false),
            Err(e) => Err(error::ReusablePayments::Rpc(format!(
                "notification liveness check for {notif_txid}: {e}"
            ))),
        }
    }

    pub(in crate::wallet) async fn drop_scan_events_above(
        &self,
        height: u32,
    ) -> Result<(), error::ReusablePayments> {
        let map_err = error::ReusablePayments::db;
        let connection = self.self_db.lock().await;
        let tx = connection.unchecked_transaction().map_err(map_err)?;
        tx.execute(
            "DELETE FROM silent_payment_received WHERE height >= ?1",
            rusqlite::params![height],
        )
        .map_err(map_err)?;
        tx.execute(
            "DELETE FROM bip47_inbound_payers WHERE notification_height >= ?1",
            rusqlite::params![height],
        )
        .map_err(map_err)?;
        tx.execute(
            "DELETE FROM bip47_received WHERE height >= ?1",
            rusqlite::params![height],
        )
        .map_err(map_err)?;
        tx.execute(
            "UPDATE bip47_inbound_payers \
             SET next_receive_index = COALESCE( \
                 (SELECT MAX(sender_index) + 1 FROM bip47_received \
                  WHERE sender_payment_code = bip47_inbound_payers.sender_payment_code), \
                 0)",
            [],
        )
        .map_err(map_err)?;
        tx.execute(
            "UPDATE reusable_scan_state \
             SET last_scanned_height = ?1, last_scanned_block_hash = NULL \
             WHERE wallet_id = 1",
            rusqlite::params![height.saturating_sub(1)],
        )
        .map_err(map_err)?;
        tx.commit().map_err(map_err)?;
        drop(connection);
        Ok(())
    }
    /// Unspent outputs the wallet owns via reusable payments (BIP47 / silent
    /// payments), which live outside the BDK descriptor. Each carries the data
    /// needed to recover its spend key. Excludes outputs marked spent.
    pub(in crate::wallet) async fn read_reusable_owned_outputs(
        &self,
    ) -> Result<Vec<super::ReusableOwnedOutput>, error::ReusablePayments> {
        use super::{ReusableKeySource, ReusableOwnedOutput};
        let connection = self.self_db.lock().await;
        let mut outputs = Vec::new();

        let mut stmt = connection
            .prepare(
                "SELECT txid, vout, sender_payment_code, sender_index, amount_sats, \
                 script_pubkey, height FROM bip47_received WHERE spent_in_txid IS NULL",
            )
            .map_err(error::ReusablePayments::db)?;
        let bip47_rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, u32>(6)?,
                ))
            })
            .and_then(|iter| iter.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(error::ReusablePayments::db)?;
        drop(stmt);
        for (txid_b, vout, code_str, index, amount, spk, height) in bip47_rows {
            let Ok(txid) = Txid::from_slice(&txid_b) else {
                continue;
            };
            let Ok(sender_code) = bip47::PaymentCode::from_str(&code_str) else {
                continue;
            };
            outputs.push(ReusableOwnedOutput {
                outpoint: OutPoint::new(txid, vout),
                txout: TxOut {
                    value: bitcoin::Amount::from_sat(amount as u64),
                    script_pubkey: bitcoin::ScriptBuf::from_bytes(spk),
                },
                height,
                key_source: ReusableKeySource::Bip47 { sender_code, index },
            });
        }

        let mut stmt = connection
            .prepare(
                "SELECT txid, vout, output_pubkey, amount_sats, tweak_k, label_m, height \
                 FROM silent_payment_received WHERE spent_in_txid IS NULL",
            )
            .map_err(error::ReusablePayments::db)?;
        let sp_rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, u32>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, u32>(6)?,
                ))
            })
            .and_then(|iter| iter.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(error::ReusablePayments::db)?;
        drop(stmt);
        drop(connection);
        for (txid_b, vout, xonly_b, amount, tweak_k, label_m, height) in sp_rows {
            let Ok(txid) = Txid::from_slice(&txid_b) else {
                continue;
            };
            let Ok(xonly) = bitcoin::XOnlyPublicKey::from_slice(&xonly_b) else {
                continue;
            };
            let script_pubkey = bitcoin::ScriptBuf::new_p2tr_tweaked(
                bitcoin::key::TweakedPublicKey::dangerous_assume_tweaked(xonly),
            );
            outputs.push(ReusableOwnedOutput {
                outpoint: OutPoint::new(txid, vout),
                txout: TxOut {
                    value: bitcoin::Amount::from_sat(amount as u64),
                    script_pubkey,
                },
                height,
                key_source: ReusableKeySource::SilentPayment {
                    tweak_k,
                    label: label_m.map(|m| m as u32),
                },
            });
        }
        Ok(outputs)
    }
    /// Recover the private key that spends a reusable-payments output.
    pub(in crate::wallet) async fn recover_reusable_spend_key(
        &self,
        owned: &super::ReusableOwnedOutput,
    ) -> Result<bitcoin::secp256k1::SecretKey, error::ReusablePayments> {
        use super::ReusableKeySource;
        let master = self.master_xpriv_inner().await?;
        let secp = Secp256k1::new();
        let network = self.validator.network();
        let coin = bip44_coin_type(network);
        match &owned.key_source {
            ReusableKeySource::Bip47 { sender_code, index } => {
                let account = master.derive_priv(&secp, &bip47::account_path(coin)?)?;
                Ok(bip47::recover_receive_priv(
                    &account,
                    sender_code,
                    *index,
                    network,
                    &secp,
                )?)
            }
            ReusableKeySource::SilentPayment { tweak_k, label } => {
                use bitcoin_jsonrpsee::{
                    client::MainClient,
                    jsonrpsee::core::{client::ClientT, params::ArrayParams},
                };
                let scan_path = silent_payments::sp_path(coin, 0, 1)?;
                let spend_path = silent_payments::sp_path(coin, 0, 0)?;
                let b_scan = master.derive_priv(&secp, &scan_path)?.private_key;
                let b_spend = master.derive_priv(&secp, &spend_path)?.private_key;

                let map_rpc = |err: bitcoin_jsonrpsee::jsonrpsee::core::client::Error| {
                    error::ReusablePayments::Rpc(err.to_string())
                };
                let block_hash = self
                    .main_client
                    .getblockhash(owned.height as usize)
                    .await
                    .map_err(map_rpc)?;
                let mut params = ArrayParams::new();
                params
                    .insert(block_hash.to_string())
                    .map_err(|e| error::ReusablePayments::Rpc(e.to_string()))?;
                params
                    .insert(0u32)
                    .map_err(|e| error::ReusablePayments::Rpc(e.to_string()))?;
                let hex_str: String = self
                    .main_client
                    .request("getblock", params)
                    .await
                    .map_err(map_rpc)?;
                let bytes = hex::decode(&hex_str)
                    .map_err(|e| error::ReusablePayments::ConsensusDecode(e.to_string()))?;
                let block: bitcoin::Block = bitcoin::consensus::encode::deserialize(&bytes)
                    .map_err(|e| error::ReusablePayments::ConsensusDecode(e.to_string()))?;
                let prevouts = self.resolve_block_prevouts(&block).await?;
                let scanned = super::scan::block_to_scanned_txs(&block, &prevouts);
                let funding = scanned
                    .iter()
                    .find(|t| t.txid == owned.outpoint.txid)
                    .ok_or_else(|| {
                        error::ReusablePayments::Rpc(
                            "silent-payment funding tx not found in its block".to_string(),
                        )
                    })?;
                Ok(super::scan::recover_sp_spending_key(
                    &b_spend, &b_scan, funding, *tweak_k, *label, &secp,
                )?)
            }
        }
    }

    /// Mark a reusable-payments output as spent by `spent_in`.
    pub(in crate::wallet) async fn mark_reusable_spent(
        &self,
        outpoint: OutPoint,
        spent_in: Txid,
    ) -> Result<(), error::ReusablePayments> {
        let map_err = error::ReusablePayments::db;
        let txid_b = outpoint.txid.as_byte_array().to_vec();
        let spent_b = spent_in.as_byte_array().to_vec();
        let connection = self.self_db.lock().await;
        connection
            .execute(
                "UPDATE bip47_received SET spent_in_txid = ?1 WHERE txid = ?2 AND vout = ?3",
                rusqlite::params![spent_b, txid_b, outpoint.vout],
            )
            .map_err(map_err)?;
        connection
            .execute(
                "UPDATE silent_payment_received SET spent_in_txid = ?1 \
                 WHERE txid = ?2 AND vout = ?3",
                rusqlite::params![spent_b, txid_b, outpoint.vout],
            )
            .map_err(map_err)?;
        drop(connection);
        Ok(())
    }
}

/// Private key at the wallet descriptor path `84'/1'/0'/<keychain>/<index>`.
fn descriptor_branch_priv(
    master: &Xpriv,
    secp: &Secp256k1<bitcoin::secp256k1::All>,
    keychain: bdk_wallet::KeychainKind,
    index: u32,
) -> Result<bitcoin::secp256k1::SecretKey, error::ReusablePayments> {
    let branch: u32 = match keychain {
        bdk_wallet::KeychainKind::External => 0,
        bdk_wallet::KeychainKind::Internal => 1,
    };
    let path = DerivationPath::from(
        [
            ChildNumber::from_hardened_idx(84)?,
            ChildNumber::from_hardened_idx(1)?,
            ChildNumber::from_hardened_idx(0)?,
            ChildNumber::from_normal_idx(branch)?,
            ChildNumber::from_normal_idx(index)?,
        ]
        .as_slice(),
    );
    Ok(master.derive_priv(secp, &path)?.private_key)
}
