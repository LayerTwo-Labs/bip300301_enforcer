use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use bdk_wallet::keys::{DerivableKey as _, ExtendedKey};
use bitcoin::{
    BlockHash, OutPoint, Transaction, TxOut, Txid,
    bip32::{ChildNumber, DerivationPath, Xpriv, Xpub},
    hashes::Hash as _,
    secp256k1::Secp256k1,
};
use bitcoin_jsonrpsee::client::{GetRawTransactionClient, GetRawTransactionVerbose};
use either::Either;

use super::{
    bip47,
    scan::{
        ScanContext, ScanCursor, ScanEvent, ScannedInput, ScannedTx, ScrubOnDrop,
        extract_bip47_designated_pubkey, extract_input_pubkey, scan_tx,
    },
    silent_payments, util,
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
        let coin = util::bip44_coin_type(network);
        let account = master.derive_priv(&secp, &bip47_account_path(coin)?)?;
        let account_xpub = Xpub::from_priv(&secp, &account);
        Ok(bip47::PaymentCode::from_xpub(&account_xpub, version))
    }

    pub(in crate::wallet) async fn silent_payment_address_inner(
        &self,
        label: Option<u32>,
    ) -> Result<silent_payments::SilentPaymentAddress, error::ReusablePayments> {
        let master = self.master_xpriv_inner().await?;
        let secp = Secp256k1::new();
        let network = self.validator.network();
        let coin = util::bip44_coin_type(network);
        let scan_path = silent_payments::sp_path(coin, 0, 1)?;
        let spend_path = silent_payments::sp_path(coin, 0, 0)?;
        let b_scan = master.derive_priv(&secp, &scan_path)?.private_key;
        let b_spend = master.derive_priv(&secp, &spend_path)?.private_key;
        match label {
            None => {
                let scan_pub = b_scan.public_key(&secp);
                let spend_pub = b_spend.public_key(&secp);
                Ok(silent_payments::SilentPaymentAddress::base(
                    scan_pub, spend_pub, network,
                ))
            }
            Some(m) => Ok(silent_payments::labeled_address(
                &b_scan, &b_spend, m, network, &secp,
            )?),
        }
    }

    pub(in crate::wallet) async fn compute_bip47_send_address(
        &self,
        recipient: &bip47::PaymentCode,
        i: u32,
    ) -> Result<bitcoin::Address, error::ReusablePayments> {
        let master = self.master_xpriv_inner().await?;
        let secp = Secp256k1::new();
        let network = self.validator.network();
        let coin = util::bip44_coin_type(network);
        let account = master.derive_priv(&secp, &bip47_account_path(coin)?)?;
        let root = match recipient.version() {
            bip47::Version::V1 => account,
            bip47::Version::V3 => bip47::v3_root_xpriv(&account, network, &secp)?,
        };
        let a0 = ScrubOnDrop::new(
            root.derive_priv(&secp, &[ChildNumber::from_normal_idx(0)?])?
                .private_key,
        );
        Ok(bip47::derive_send_address(
            &a0, recipient, i, network, &secp,
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
        let coin = util::bip44_coin_type(network);

        let sender_acct = master.derive_priv(&secp, &bip47_account_path(coin)?)?;
        let sender_acct_xpub = Xpub::from_priv(&secp, &sender_acct);
        let sender_code = bip47::PaymentCode::from_xpub(&sender_acct_xpub, version);

        let designated_priv =
            ScrubOnDrop::new(descriptor_branch_priv(&master, &secp, keychain, index)?);

        let recipient_notif_pub = recipient.notification_pubkey(&secp)?;
        let blinded = bip47::blind(
            &sender_code,
            &designated_priv,
            &recipient_notif_pub,
            designated_outpoint,
            &secp,
        );
        Ok((blinded, sender_code))
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
        let coin = util::bip44_coin_type(network);
        let sender_acct = master.derive_priv(&secp, &bip47_account_path(coin)?)?;
        let sender_code = bip47::PaymentCode::from_xpub(
            &Xpub::from_priv(&secp, &sender_acct),
            bip47::Version::V3,
        );
        let blinded = bip47::blind(
            &sender_code,
            ephemeral_priv,
            &recipient.notification_pubkey(&secp)?,
            OutPoint::null(),
            &secp,
        );
        let g: [u8; 33] = blinded.try_into().expect("v3 blinded payload is 33 bytes");
        let a = ephemeral_priv.public_key(&secp).serialize();
        let f = recipient.identifier();
        Ok(bip47::v3_notification_script(&a, &f, &g))
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
                    if !is_bip352_eligible_spk(spk) {
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
            let sk = if spk.is_p2tr()
                && sk.public_key(&secp).x_only_public_key().1 == bitcoin::secp256k1::Parity::Odd
            {
                sk.negate()
            } else {
                sk
            };
            eligible_inputs.push(silent_payments::EligibleInput {
                outpoint: *outpoint,
                secret: sk,
            });
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
        let label_rows: Vec<i64> = {
            let connection = self.self_db.lock().await;
            let mut stmt = connection
                .prepare("SELECT m FROM silent_payment_labels WHERE m >= 1")
                .map_err(error::ReusablePayments::db)?;
            let collected: Result<Vec<i64>, _> = stmt
                .query_map([], |row| row.get::<_, i64>(0))
                .and_then(|iter| iter.collect());
            drop(stmt);
            drop(connection);
            collected.map_err(error::ReusablePayments::db)?
        };
        let inbound = self.read_inbound_payers().await?;
        let master = self.master_xpriv_inner().await?;

        let secp = Secp256k1::new();
        let network = self.validator.network();
        let coin = util::bip44_coin_type(network);

        let bip47_acct = master.derive_priv(&secp, &bip47_account_path(coin)?)?;
        let v1_notif_priv = bip47_acct
            .derive_priv(&secp, &[ChildNumber::from_normal_idx(0)?])?
            .private_key;

        let v3_master_xpriv = bip47::v3_root_xpriv(&bip47_acct, network, &secp)?;
        let v3_notif_priv = v3_master_xpriv
            .derive_priv(&secp, &[ChildNumber::from_normal_idx(0)?])?
            .private_key;
        let v3_identifier =
            bip47::PaymentCode::from_xpub(&Xpub::from_priv(&secp, &bip47_acct), bip47::Version::V3)
                .identifier();

        let scan_path = silent_payments::sp_path(coin, 0, 1)?;
        let spend_path = silent_payments::sp_path(coin, 0, 0)?;
        let b_scan = master.derive_priv(&secp, &scan_path)?.private_key;
        let b_spend = master.derive_priv(&secp, &spend_path)?.private_key;
        let b_spend_pub = b_spend.public_key(&secp);

        let mut label_set = silent_payments::LabelSet::with_change(&b_scan, &secp)
            .map_err(error::ReusablePayments::SilentPaymentCrypto)?;
        for m in label_rows {
            let m_u32 = u32::try_from(m).unwrap_or(0);
            if m_u32 == 0 {
                continue;
            }
            label_set
                .add(&b_scan, m_u32, &secp)
                .map_err(error::ReusablePayments::SilentPaymentCrypto)?;
        }

        let bip47_receive_lookup =
            Self::build_bip47_receive_lookup(&bip47_acct, inbound, network, &secp, lookahead)?;

        let bip47_data = super::scan::Bip47ScanData {
            v1_notif_priv,
            v3_notif_priv,
            v3_identifier,
            receive_lookup: bip47_receive_lookup,
            lookahead,
        };
        Ok(ScanContext::new(
            bip47_data,
            b_scan,
            b_spend_pub,
            label_set,
            &secp,
        ))
    }

    async fn read_inbound_payers(
        &self,
    ) -> Result<Vec<(String, i64, u32)>, error::ReusablePayments> {
        let connection = self.self_db.lock().await;
        let mut stmt = connection
            .prepare(
                "SELECT sender_payment_code, version, next_receive_index \
                 FROM bip47_inbound_payers",
            )
            .map_err(error::ReusablePayments::db)?;
        let collected: Result<Vec<(String, i64, u32)>, _> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, u32>(2)?,
                ))
            })
            .and_then(|iter| iter.collect());
        drop(stmt);
        drop(connection);
        collected.map_err(error::ReusablePayments::db)
    }

    fn build_bip47_receive_lookup(
        bip47_acct: &Xpriv,
        inbound: Vec<(String, i64, u32)>,
        network: bitcoin::Network,
        secp: &Secp256k1<bitcoin::secp256k1::All>,
        lookahead: u32,
    ) -> Result<
        std::collections::HashMap<[u8; 20], super::scan::Bip47ReceiveSource>,
        error::ReusablePayments,
    > {
        if inbound.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let v3_root = bip47::v3_root_xpriv(bip47_acct, network, secp)?;

        let mut lookup: std::collections::HashMap<[u8; 20], super::scan::Bip47ReceiveSource> =
            std::collections::HashMap::new();

        for (sender_code_str, version_i64, next_receive_index) in inbound {
            let version = match version_i64 {
                1 => bip47::Version::V1,
                3 => bip47::Version::V3,
                _ => continue,
            };
            let sender_code = match bip47::PaymentCode::from_str(&sender_code_str) {
                Ok(pc) => pc,
                Err(_) => continue,
            };
            let root: &Xpriv = match version {
                bip47::Version::V1 => bip47_acct,
                bip47::Version::V3 => &v3_root,
            };
            let entries = super::scan::derive_bip47_lookup_entries(
                root,
                &sender_code,
                next_receive_index,
                lookahead,
                network,
                secp,
            )?;
            lookup.extend(entries);
        }
        Ok(lookup)
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
        let coin = util::bip44_coin_type(network);
        let account = master.derive_priv(&secp, &bip47_account_path(coin)?)?;
        let root = match sender.version() {
            bip47::Version::V1 => account,
            bip47::Version::V3 => bip47::v3_root_xpriv(&account, network, &secp)?,
        };
        Ok(super::scan::derive_bip47_lookup_entries(
            &root, sender, 0, lookahead, network, &secp,
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

    fn block_to_scanned_txs(
        block: &bitcoin::Block,
        prevouts: &HashMap<OutPoint, TxOut>,
    ) -> Vec<ScannedTx> {
        block
            .txdata
            .iter()
            .map(|tx| {
                let mut inputs = Vec::with_capacity(tx.input.len());
                let mut bip47_designated = None;
                let mut spends_segwit_gt_v1 = false;
                for txin in &tx.input {
                    let prev = prevouts.get(&txin.previous_output);
                    let pubkey = prev.and_then(|prev| {
                        extract_input_pubkey(
                            &txin.witness,
                            txin.script_sig.as_script(),
                            prev.script_pubkey.as_script(),
                        )
                    });
                    if bip47_designated.is_none()
                        && let Some(pk) = prev.and_then(|prev| {
                            extract_bip47_designated_pubkey(
                                &txin.witness,
                                txin.script_sig.as_script(),
                                prev.script_pubkey.as_script(),
                            )
                        })
                    {
                        bip47_designated = Some((txin.previous_output, pk));
                    }
                    if let Some(prev) = prev
                        && let Some(version) = prev.script_pubkey.witness_version()
                        && version.to_num() > 1
                    {
                        spends_segwit_gt_v1 = true;
                    }
                    inputs.push(ScannedInput {
                        outpoint: txin.previous_output,
                        pubkey,
                    });
                }

                let mut taproot_outputs = Vec::new();
                let mut op_return_data = Vec::new();
                let mut p2pkh_outputs = Vec::new();
                let mut p2wpkh_outputs = Vec::new();
                let mut p2pk_outputs = Vec::new();
                let mut bare_multisig_1of3 = Vec::new();
                for (vout, txout) in tx.output.iter().enumerate() {
                    let script = txout.script_pubkey.as_script();
                    if let Some(keys) = bip47::parse_1of3_multisig(script) {
                        bare_multisig_1of3.push((vout as u32, keys));
                        continue;
                    }
                    if script.is_p2tr() {
                        let bytes = script.as_bytes();
                        if bytes.len() == 34
                            && let Ok(xonly) = bitcoin::XOnlyPublicKey::from_slice(&bytes[2..])
                        {
                            taproot_outputs.push((vout as u32, xonly, txout.value));
                        }
                    } else if script.is_op_return() {
                        for ins in script.instructions().flatten() {
                            // Only 80-byte payloads can be v1 notifications.
                            if let bitcoin::script::Instruction::PushBytes(b) = ins
                                && b.as_bytes().len() == 80
                            {
                                op_return_data.push(b.as_bytes().to_vec());
                            }
                        }
                    } else if script.is_p2pkh() {
                        let bytes = script.as_bytes();
                        if bytes.len() == 25 {
                            let mut h = [0u8; 20];
                            h.copy_from_slice(&bytes[3..23]);
                            p2pkh_outputs.push((vout as u32, h, txout.value));
                        }
                    } else if script.is_p2wpkh() {
                        let bytes = script.as_bytes();
                        if bytes.len() == 22 {
                            let mut h = [0u8; 20];
                            h.copy_from_slice(&bytes[2..22]);
                            p2wpkh_outputs.push((vout as u32, h, txout.value));
                        }
                    } else {
                        // Compressed-key P2PK: <33-byte key> OP_CHECKSIG.
                        let bytes = script.as_bytes();
                        if bytes.len() == 35 && bytes[0] == 0x21 && bytes[34] == 0xac {
                            let mut key = [0u8; 33];
                            key.copy_from_slice(&bytes[1..34]);
                            p2pk_outputs.push((vout as u32, key, txout.value));
                        }
                    }
                }

                ScannedTx {
                    txid: tx.compute_txid(),
                    inputs,
                    bip47_designated,
                    spends_segwit_gt_v1,
                    taproot_outputs,
                    op_return_data,
                    p2pkh_outputs,
                    p2wpkh_outputs,
                    p2pk_outputs,
                    bare_multisig_1of3,
                }
            })
            .collect()
    }

    pub(in crate::wallet) async fn scan_block_for_reusable_payments(
        &self,
        block: &bitcoin::Block,
        height: u32,
    ) -> Result<(), error::ReusablePayments> {
        let _scanner_guard = self.scanner_lock.lock().await;
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
        let scanned = Self::block_to_scanned_txs(block, &prevouts);

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
}

fn bip47_account_path(coin: u32) -> Result<DerivationPath, error::ReusablePayments> {
    let components = [
        ChildNumber::from_hardened_idx(47)?,
        ChildNumber::from_hardened_idx(coin)?,
        ChildNumber::from_hardened_idx(0)?,
    ];
    Ok(DerivationPath::from(components.as_slice()))
}

pub fn is_bip352_eligible_spk(spk: &bitcoin::Script) -> bool {
    spk.is_p2tr() || spk.is_p2wpkh() || spk.is_p2pkh()
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
