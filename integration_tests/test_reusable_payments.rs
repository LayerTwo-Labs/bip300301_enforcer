//! End-to-end coverage of the reusable-payments wallet integration (BIP47
//! payment codes v1/v3 + BIP352 silent payments).
//!
//! The enforcer wallet ("Bob") is created from a fixed mnemonic, so the test
//! can derive Bob's keys independently and cross-check every address the gRPC
//! surface hands out. The test itself plays the counterparty ("Alice") using
//! the `reusable-payments` crate directly: it crafts notification/payment
//! transactions for the enforcer to *receive*, and scans blocks with the
//! crate's scanner to verify transactions the enforcer *sends*. Because Alice
//! is driven by independent keys, sender/receiver role mix-ups in the
//! enforcer's glue code cannot cancel out the way they would in a self-send
//! test.
//!
//! Covered end to end:
//! * BIP47 v1 + v3 inbound: notification detection, payment detection at
//!   non-contiguous indices (lookahead window), per-payer accounting
//! * BIP47 v1 + v3 outbound: notification reuse across sends, sender index
//!   advancement, payloads unblind with the crate as the recipient
//! * BIP352 inbound + outbound: multi-input key aggregation, multi-recipient
//!   sends, label attribution (base / labeled), spend-key recovery
//! * Spending received outputs (mixed descriptor + BIP47 P2PKH/P2WPKH +
//!   taproot key-path inputs in one transaction) — confirmed by bitcoind,
//!   which consensus-validates every derivation in the chain
//! * Rescans: a below-birthday rescan finds pre-activation payments, and
//!   spent outputs stay spent across a rescan
//! * Reorg: a reorged-out receive disappears from balance and listings

use std::collections::HashMap;

use bip300301_enforcer_lib::proto::{
    self,
    common::ReverseHex,
    mainchain::{
        Bip47Version, CreateNewAddressRequest, CreateSilentPaymentLabelRequest, GetBalanceRequest,
        GetBip47PaymentCodeRequest, GetReusableScanStatusRequest, GetSilentPaymentAddressRequest,
        ListBip47InboundPayersRequest, ListSilentPaymentLabelsRequest,
        ListSilentPaymentReceivesRequest, ListUnspentOutputsRequest, RescanReusablePaymentsRequest,
        SendToBip47PaymentCodeRequest, SendToBip47PaymentCodeResponse, SendToSilentPaymentRequest,
        SendToSilentPaymentResponse, SendTransactionRequest, SendTransactionResponse,
        send_to_silent_payment_request, send_transaction_request,
    },
};
use bitcoin::{
    Amount, Block, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness,
    bip32::Xpriv,
    hashes::Hash as _,
    key::CompressedPublicKey,
    psbt::Psbt,
    script::PushBytesBuf,
    secp256k1::{All, Secp256k1, SecretKey},
    transaction::Version as TxVersion,
};
use buffa::MessageField;
use futures::channel::mpsc;
use reusable_payments::{bip44_coin_type, bip47, scan, silent_payments, spend};

use crate::{
    integration_test::wait_for_wallet_sync,
    setup::{Mode, PostSetup, PreSetup, SetupOpts},
};

/// Fixed mnemonic for the enforcer wallet, so the test can independently
/// derive the wallet's payment codes, silent-payment keys, and spend keys.
const BOB_MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

const FEE: Amount = Amount::from_sat(5_000);

// Inbound (Alice -> Bob) amounts.
const PRE_ACTIVATION_SP_SATS: u64 = 111_111;
const V1_PAYMENT_0_SATS: u64 = 250_000;
const V1_PAYMENT_5_SATS: u64 = 260_000;
const V3_PAYMENT_0_SATS: u64 = 150_000;
const SP_BASE_SATS: u64 = 300_000;
const SP_LABELED_SATS: u64 = 200_000;
const REORG_SP_SATS: u64 = 500_000;

// Outbound (Bob -> Alice) amounts.
const OUT_V1_0_SATS: u64 = 400_000;
const OUT_V1_1_SATS: u64 = 410_000;
const OUT_V3_0_SATS: u64 = 420_000;
const OUT_SP_BASE_SATS: u64 = 350_000;
const OUT_SP_LABELED_SATS: u64 = 360_000;

/// Alice's silent-payment label, for outbound label attribution.
const ALICE_SP_LABEL: u32 = 7;

fn secp() -> Secp256k1<All> {
    Secp256k1::new()
}

/// Bob's key material, derived from [`BOB_MNEMONIC`] exactly the way the
/// enforcer derives it, so gRPC responses can be checked against first
/// principles.
struct BobKeys {
    v1_code: bip47::PaymentCode,
    v3_code: bip47::PaymentCode,
    b_scan: SecretKey,
    b_spend: SecretKey,
}

impl BobKeys {
    fn derive() -> anyhow::Result<Self> {
        let secp = secp();
        let mnemonic = bdk_wallet::keys::bip39::Mnemonic::parse_in_normalized(
            bdk_wallet::keys::bip39::Language::English,
            BOB_MNEMONIC,
        )?;
        let seed = mnemonic.to_seed("");
        let master = Xpriv::new_master(bitcoin::NetworkKind::Test, &seed)?;
        let coin = bip44_coin_type(Network::Regtest);
        let bip47_account = master.derive_priv(&secp, &bip47::account_path(coin)?)?;
        let v1_code = bip47::payment_code_from_account(&bip47_account, bip47::Version::V1, &secp);
        let v3_code = bip47::payment_code_from_account(&bip47_account, bip47::Version::V3, &secp);
        let b_scan = master
            .derive_priv(&secp, &silent_payments::sp_path(coin, 0, 1)?)?
            .private_key;
        let b_spend = master
            .derive_priv(&secp, &silent_payments::sp_path(coin, 0, 0)?)?
            .private_key;
        Ok(Self {
            v1_code,
            v3_code,
            b_scan,
            b_spend,
        })
    }

    fn sp_address(&self, label: Option<u32>) -> anyhow::Result<String> {
        let addr = silent_payments::derive_address(
            &self.b_scan,
            &self.b_spend,
            label,
            Network::Regtest,
            &secp(),
        )?;
        Ok(addr.to_string())
    }
}

/// The test-side counterparty: a miniature wallet with a single P2WPKH key
/// driving crate-level BIP47/BIP352 sends, plus the key material to *receive*
/// with the crate's scanner.
struct Alice {
    bip47_account: Xpriv,
    v1_code: bip47::PaymentCode,
    v3_code: bip47::PaymentCode,
    b_scan: SecretKey,
    b_spend: SecretKey,
    wallet_key: SecretKey,
    utxos: Vec<(OutPoint, TxOut)>,
}

impl Alice {
    fn derive() -> anyhow::Result<Self> {
        let secp = secp();
        let master = Xpriv::new_master(bitcoin::NetworkKind::Test, &[0xAA; 64])?;
        let coin = bip44_coin_type(Network::Regtest);
        let bip47_account = master.derive_priv(&secp, &bip47::account_path(coin)?)?;
        let v1_code = bip47::payment_code_from_account(&bip47_account, bip47::Version::V1, &secp);
        let v3_code = bip47::payment_code_from_account(&bip47_account, bip47::Version::V3, &secp);
        let b_scan = master
            .derive_priv(&secp, &silent_payments::sp_path(coin, 0, 1)?)?
            .private_key;
        let b_spend = master
            .derive_priv(&secp, &silent_payments::sp_path(coin, 0, 0)?)?
            .private_key;
        let wallet_key = SecretKey::from_slice(&[0x77; 32])?;
        Ok(Self {
            bip47_account,
            v1_code,
            v3_code,
            b_scan,
            b_spend,
            wallet_key,
            utxos: Vec::new(),
        })
    }

    fn spk(&self) -> ScriptBuf {
        self.address().script_pubkey()
    }

    fn address(&self) -> bitcoin::Address {
        let pubkey = CompressedPublicKey(self.wallet_key.public_key(&secp()));
        bitcoin::Address::p2wpkh(&pubkey, Network::Regtest)
    }

    /// Build and sign a transaction spending the given UTXOs to `outputs`
    /// plus a change output back to Alice. Does not touch the UTXO set.
    fn build_tx(
        &self,
        inputs: &[(OutPoint, TxOut)],
        mut outputs: Vec<TxOut>,
        fee: Amount,
    ) -> anyhow::Result<Transaction> {
        let in_value: Amount = inputs.iter().map(|(_, txo)| txo.value).sum();
        let out_value: Amount = outputs.iter().map(|txo| txo.value).sum();
        let change = in_value
            .checked_sub(out_value)
            .and_then(|value| value.checked_sub(fee))
            .ok_or_else(|| anyhow::anyhow!("alice tx underfunded"))?;
        anyhow::ensure!(
            change >= Amount::from_sat(10_000),
            "alice change {change} too small; fund more"
        );
        outputs.push(TxOut {
            value: change,
            script_pubkey: self.spk(),
        });
        let unsigned = Transaction {
            version: TxVersion::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: inputs
                .iter()
                .map(|(outpoint, _)| TxIn {
                    previous_output: *outpoint,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::default(),
                })
                .collect(),
            output: outputs,
        };
        let mut psbt = Psbt::from_unsigned_tx(unsigned)?;
        for (i, (_, prevout)) in inputs.iter().enumerate() {
            psbt.inputs[i].witness_utxo = Some(prevout.clone());
        }
        // Sign with the crate's PSBT signer — the same code path the enforcer
        // uses for reusable inputs, exercised here for plain P2WPKH.
        let keys: Vec<(OutPoint, SecretKey)> = inputs
            .iter()
            .map(|(outpoint, _)| (*outpoint, self.wallet_key))
            .collect();
        spend::sign_psbt_inputs(&mut psbt, &keys, &secp())?;
        Ok(psbt.extract_tx()?)
    }

    /// Build, sign, and account for a transaction: spends the first
    /// `spend_count` tracked UTXOs and re-tracks the change output.
    fn spend(
        &mut self,
        spend_count: usize,
        outputs: Vec<TxOut>,
        fee: Amount,
    ) -> anyhow::Result<Transaction> {
        anyhow::ensure!(self.utxos.len() >= spend_count, "not enough alice utxos");
        let inputs: Vec<(OutPoint, TxOut)> = self.utxos.drain(..spend_count).collect();
        let n_payload_outputs = outputs.len();
        let tx = self.build_tx(&inputs, outputs, fee)?;
        let txid = tx.compute_txid();
        let change_vout = n_payload_outputs as u32;
        self.utxos.push((
            OutPoint::new(txid, change_vout),
            tx.output[change_vout as usize].clone(),
        ));
        Ok(tx)
    }

    /// Compute BIP352 outputs for `recipients` funded by the first
    /// `spend_count` tracked UTXOs, then build the transaction.
    fn spend_silent_payment(
        &mut self,
        spend_count: usize,
        recipients: &[(silent_payments::SilentPaymentAddress, Amount)],
        fee: Amount,
    ) -> anyhow::Result<Transaction> {
        anyhow::ensure!(self.utxos.len() >= spend_count, "not enough alice utxos");
        let inputs: Vec<(OutPoint, TxOut)> = self.utxos[..spend_count].to_vec();
        let outputs = self.compute_sp_outputs(&inputs, recipients)?;
        self.spend(spend_count, outputs, fee)
    }

    fn compute_sp_outputs(
        &self,
        inputs: &[(OutPoint, TxOut)],
        recipients: &[(silent_payments::SilentPaymentAddress, Amount)],
    ) -> anyhow::Result<Vec<TxOut>> {
        let secp = secp();
        let eligible: Vec<silent_payments::EligibleInput> = inputs
            .iter()
            .map(|(outpoint, prevout)| {
                silent_payments::EligibleInput::new(
                    *outpoint,
                    self.wallet_key,
                    prevout.script_pubkey.as_script(),
                    &secp,
                )
            })
            .collect();
        let all_outpoints: Vec<OutPoint> = inputs.iter().map(|(outpoint, _)| *outpoint).collect();
        Ok(silent_payments::compute_outputs(
            &eligible,
            &all_outpoints,
            recipients,
            &secp,
        )?)
    }

    /// Scan a block the way a receiving wallet would, using the crate's
    /// scanner with Alice's keys. `payers` seeds the BIP47 receive lookup;
    /// senders discovered from notifications in this block are folded in for
    /// a second pass, mirroring same-block notification+payment handling.
    fn scan_block(
        &self,
        block: &Block,
        prevouts: &HashMap<OutPoint, TxOut>,
        payers: &[(bip47::PaymentCode, u32)],
    ) -> anyhow::Result<Vec<scan::ScanEvent>> {
        let secp = secp();
        let ctx = scan::build_scan_context(
            &self.bip47_account,
            self.b_scan,
            self.b_spend.public_key(&secp),
            &[ALICE_SP_LABEL],
            payers,
            20,
            Network::Regtest,
            &secp,
        )?;
        let scanned = scan::block_to_scanned_txs(block, prevouts);
        let mut events = Vec::new();
        for tx in &scanned {
            events.extend(scan::scan_tx(tx, &ctx, &secp)?);
        }
        let discovered: Vec<(bip47::PaymentCode, u32)> = events
            .iter()
            .filter_map(|event| match event {
                scan::ScanEvent::Bip47Notification { sender, .. } => Some((sender.clone(), 0)),
                _ => None,
            })
            .collect();
        if !discovered.is_empty() {
            let lookup = scan::build_bip47_receive_lookup(
                &self.bip47_account,
                &discovered,
                20,
                Network::Regtest,
                &secp,
            )?;
            for tx in &scanned {
                events.extend(scan::detect_bip47_receives(tx, &lookup));
            }
        }
        Ok(events)
    }
}

async fn bitcoin_cli(post_setup: &PostSetup, args: Vec<String>) -> anyhow::Result<String> {
    use bip300301_enforcer_lib::bins::CommandExt as _;
    let command = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty bitcoin-cli args"))?
        .clone();
    let res = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], command, args[1..].to_vec())
        .run_utf8()
        .await?;
    Ok(res)
}

async fn broadcast(post_setup: &PostSetup, tx: &Transaction) -> anyhow::Result<Txid> {
    let hex = bitcoin::consensus::encode::serialize_hex(tx);
    let txid = bitcoin_cli(post_setup, vec!["sendrawtransaction".to_owned(), hex]).await?;
    Ok(txid.trim().parse()?)
}

async fn block_count(post_setup: &PostSetup) -> anyhow::Result<u32> {
    Ok(bitcoin_cli(post_setup, vec!["getblockcount".to_owned()])
        .await?
        .trim()
        .parse()?)
}

/// Mine `blocks` to Bitcoin Core's own wallet and wait for the enforcer
/// wallet to apply them.
async fn mine(post_setup: &mut PostSetup, blocks: u32) -> anyhow::Result<()> {
    let mining_address = post_setup.mining_address.to_string();
    bitcoin_cli(
        post_setup,
        vec![
            "generatetoaddress".to_owned(),
            blocks.to_string(),
            mining_address,
        ],
    )
    .await?;
    wait_for_wallet_sync(post_setup).await
}

/// Wait until the reusable-payments scanner has processed up to `target`.
async fn wait_for_scan(post_setup: &mut PostSetup, target: u32) -> anyhow::Result<()> {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let status = post_setup
            .wallet_service_client
            .get_reusable_scan_status(GetReusableScanStatusRequest::default())
            .await?
            .into_owned();
        if status.last_scanned_height >= target {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "reusable-payments scanner did not reach height {target} within {TIMEOUT:?} \
             (stuck at {})",
            status.last_scanned_height,
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// Mine one block, wait for wallet sync and the reusable-payments scanner to
/// process it, and return the new tip height.
async fn mine_and_scan(post_setup: &mut PostSetup) -> anyhow::Result<u32> {
    mine(post_setup, 1).await?;
    let tip = block_count(post_setup).await?;
    wait_for_scan(post_setup, tip).await?;
    Ok(tip)
}

async fn fetch_block_at(post_setup: &PostSetup, height: u32) -> anyhow::Result<Block> {
    let hash = bitcoin_cli(
        post_setup,
        vec!["getblockhash".to_owned(), height.to_string()],
    )
    .await?;
    let hex = bitcoin_cli(
        post_setup,
        vec![
            "getblock".to_owned(),
            hash.trim().to_owned(),
            "0".to_owned(),
        ],
    )
    .await?;
    Ok(bitcoin::consensus::encode::deserialize_hex(hex.trim())?)
}

async fn fetch_transaction(post_setup: &PostSetup, txid: Txid) -> anyhow::Result<Transaction> {
    let hex = bitcoin_cli(
        post_setup,
        vec!["getrawtransaction".to_owned(), txid.to_string()],
    )
    .await?;
    Ok(bitcoin::consensus::encode::deserialize_hex(hex.trim())?)
}

/// Resolve every prevout referenced by `block` (needed by the crate scanner),
/// the same job the enforcer does over RPC.
async fn resolve_prevouts(
    post_setup: &PostSetup,
    block: &Block,
) -> anyhow::Result<HashMap<OutPoint, TxOut>> {
    let mut in_block: HashMap<Txid, &Transaction> = HashMap::new();
    for tx in &block.txdata {
        in_block.insert(tx.compute_txid(), tx);
    }
    let mut prevouts = HashMap::new();
    for tx in &block.txdata {
        for input in &tx.input {
            let prev = input.previous_output;
            if prev.txid == Txid::all_zeros() {
                continue;
            }
            let prev_tx = match in_block.get(&prev.txid) {
                Some(tx) => (*tx).clone(),
                None => fetch_transaction(post_setup, prev.txid).await?,
            };
            if let Some(txout) = prev_tx.output.get(prev.vout as usize) {
                prevouts.insert(prev, txout.clone());
            }
        }
    }
    Ok(prevouts)
}

/// Fund Alice with `count` P2WPKH UTXOs of `sats` each from Bitcoin Core's
/// wallet, then mine a block to confirm them.
async fn fund_alice(
    post_setup: &mut PostSetup,
    alice: &mut Alice,
    count: usize,
    sats: u64,
) -> anyhow::Result<()> {
    use bip300301_enforcer_lib::bins::CommandExt as _;
    let address = alice.address().to_string();
    let amount = Amount::from_sat(sats);
    for _ in 0..count {
        // `-named` + explicit fee_rate: the regtest node has no fallback fee.
        let txid: Txid = post_setup
            .bitcoin_cli
            .command::<String, _, _, _, _>(
                ["-named".to_owned()],
                "sendtoaddress",
                [
                    format!("address={address}"),
                    format!(
                        "amount={}",
                        amount.to_string_in(bitcoin::Denomination::Bitcoin)
                    ),
                    "fee_rate=25".to_owned(),
                ],
            )
            .run_utf8()
            .await?
            .trim()
            .parse()?;
        let tx = fetch_transaction(post_setup, txid).await?;
        let vout = tx
            .output
            .iter()
            .position(|txo| txo.script_pubkey == alice.spk())
            .ok_or_else(|| anyhow::anyhow!("funding tx does not pay alice"))?;
        alice
            .utxos
            .push((OutPoint::new(txid, vout as u32), tx.output[vout].clone()));
    }
    mine(post_setup, 1).await
}

struct InboundPayer {
    payment_code: String,
    next_receive_index: u32,
    total_received_sats: u64,
}

async fn list_inbound_payers(post_setup: &mut PostSetup) -> anyhow::Result<Vec<InboundPayer>> {
    let resp = post_setup
        .wallet_service_client
        .list_bip47_inbound_payers(ListBip47InboundPayersRequest::default())
        .await?
        .into_owned();
    Ok(resp
        .payers
        .into_iter()
        .map(|payer| InboundPayer {
            payment_code: payer.payment_code,
            next_receive_index: payer.next_receive_index,
            total_received_sats: payer.total_received_sats,
        })
        .collect())
}

#[derive(Debug)]
struct SpReceive {
    outpoint: OutPoint,
    amount_sats: u64,
    label_m: Option<u32>,
    label_name: Option<String>,
    height: u32,
    spent_in_txid: Option<Txid>,
}

async fn list_sp_receives(post_setup: &mut PostSetup) -> anyhow::Result<Vec<SpReceive>> {
    let resp = post_setup
        .wallet_service_client
        .list_silent_payment_receives(ListSilentPaymentReceivesRequest::default())
        .await?
        .into_owned();
    resp.items
        .into_iter()
        .map(|item| {
            let txid = item
                .txid
                .into_option()
                .ok_or_else(|| anyhow::anyhow!("sp receive missing txid"))?
                .decode::<proto::mainchain::SilentPaymentReceive, _>("txid")?;
            let spent_in_txid = item
                .spent_in_txid
                .into_option()
                .map(|hex| hex.decode::<proto::mainchain::SilentPaymentReceive, _>("spent_in_txid"))
                .transpose()?;
            Ok(SpReceive {
                outpoint: OutPoint::new(txid, item.vout),
                amount_sats: item.amount_sats,
                label_m: item.label_m,
                label_name: item.label_name,
                height: item.height,
                spent_in_txid,
            })
        })
        .collect()
}

async fn wallet_balance_sats(post_setup: &mut PostSetup) -> anyhow::Result<u64> {
    let resp = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    Ok(resp.confirmed_sats)
}

async fn list_unspent_outpoints(
    post_setup: &mut PostSetup,
) -> anyhow::Result<HashMap<OutPoint, u64>> {
    let resp = post_setup
        .wallet_service_client
        .list_unspent_outputs(ListUnspentOutputsRequest::default())
        .await?
        .into_owned();
    resp.outputs
        .into_iter()
        .map(|output| {
            let txid = output
                .txid
                .into_option()
                .ok_or_else(|| anyhow::anyhow!("unspent output missing txid"))?
                .decode::<proto::mainchain::list_unspent_outputs_response::Output, _>("txid")?;
            Ok((OutPoint::new(txid, output.vout), output.value_sats))
        })
        .collect()
}

fn find_op_return(tx: &Transaction) -> Option<&[u8]> {
    tx.output.iter().find_map(|txo| {
        if !txo.script_pubkey.is_op_return() {
            return None;
        }
        let mut instructions = txo.script_pubkey.instructions();
        let _op_return = instructions.next();
        match instructions.next() {
            Some(Ok(bitcoin::script::Instruction::PushBytes(bytes))) => Some(bytes.as_bytes()),
            _ => None,
        }
    })
}

/// Phase A: pay Bob's (offline-derived) base silent-payment address *before*
/// the wallet has ever activated reusable-payments scanning. The payment must
/// stay invisible to live scanning and become visible after a below-birthday
/// rescan.
async fn phase_pre_activation_send(
    post_setup: &mut PostSetup,
    alice: &mut Alice,
    bob: &BobKeys,
) -> anyhow::Result<(OutPoint, u32)> {
    tracing::info!("Phase A: silent payment to Bob before scanner activation");
    let bob_base = silent_payments::SilentPaymentAddress::parse_for_network(
        &bob.sp_address(None)?,
        Network::Regtest,
    )?;
    let tx = alice.spend_silent_payment(
        1,
        &[(bob_base, Amount::from_sat(PRE_ACTIVATION_SP_SATS))],
        FEE,
    )?;
    let txid = broadcast(post_setup, &tx).await?;
    mine(post_setup, 1).await?;
    let payment_height = block_count(post_setup).await?;
    // One more block so the payment sits strictly below the activation
    // birthday (birthday == chain tip at the first reusable-payments RPC).
    mine(post_setup, 1).await?;
    Ok((OutPoint::new(txid, 0), payment_height))
}

/// Phase B: activate the scanner via the address RPCs and check every
/// returned address/code against independent derivations from Bob's mnemonic.
/// Returns Bob's base silent-payment address.
async fn phase_activation_and_address_cross_checks(
    post_setup: &mut PostSetup,
    bob: &BobKeys,
) -> anyhow::Result<String> {
    tracing::info!("Phase B: scanner activation + address cross-checks");
    let birthday_tip = block_count(post_setup).await?;

    let v1 = post_setup
        .wallet_service_client
        .get_bip47_payment_code(GetBip47PaymentCodeRequest {
            version: Bip47Version::V1.into(),
        })
        .await?
        .into_owned();
    anyhow::ensure!(
        v1.payment_code == bob.v1_code.to_string(),
        "gRPC v1 payment code {} != derived {}",
        v1.payment_code,
        bob.v1_code,
    );
    let expected_notif_addr =
        bip47::notification_address(&bob.v1_code, Network::Regtest, &secp())?.to_string();
    anyhow::ensure!(
        v1.notification_address == expected_notif_addr,
        "gRPC notification address {} != derived {expected_notif_addr}",
        v1.notification_address,
    );

    let v3 = post_setup
        .wallet_service_client
        .get_bip47_payment_code(GetBip47PaymentCodeRequest {
            version: Bip47Version::V3.into(),
        })
        .await?
        .into_owned();
    anyhow::ensure!(
        v3.payment_code == bob.v3_code.to_string(),
        "gRPC v3 payment code {} != derived {}",
        v3.payment_code,
        bob.v3_code,
    );

    let base = post_setup
        .wallet_service_client
        .get_silent_payment_address(GetSilentPaymentAddressRequest { label: None })
        .await?
        .into_owned();
    anyhow::ensure!(
        base.address == bob.sp_address(None)?,
        "gRPC base sp address {} != derived",
        base.address,
    );

    let label = post_setup
        .wallet_service_client
        .create_silent_payment_label(CreateSilentPaymentLabelRequest {
            name: "integration".to_owned(),
        })
        .await?
        .into_owned();
    anyhow::ensure!(label.label_m == 1, "first label should be m=1");
    anyhow::ensure!(
        label.labeled_address == bob.sp_address(Some(1))?,
        "gRPC labeled sp address {} != derived",
        label.labeled_address,
    );
    let labels = post_setup
        .wallet_service_client
        .list_silent_payment_labels(ListSilentPaymentLabelsRequest::default())
        .await?
        .into_owned();
    anyhow::ensure!(
        labels.labels.len() == 1
            && labels.labels[0].m == 1
            && labels.labels[0].name == "integration",
        "unexpected label listing: {:?}",
        labels.labels,
    );

    // A mainnet address must be rejected on regtest.
    let mainnet_addr = "sp1qqgste7k9hx0qftg6qmwlkqtwuy6cycyavzmzj85c6qdfhjdpdjtdgqjuexzk6murw56suy3e0rd2cgqvycxttddwsvgxe2usfpxumr70xc9pkqwv";
    let mainnet_send = post_setup
        .wallet_service_client
        .send_to_silent_payment(SendToSilentPaymentRequest {
            recipients: vec![send_to_silent_payment_request::Recipient {
                sp_address: mainnet_addr.to_owned(),
                amount_sats: 1_000,
            }],
            fee_sat_per_vbyte: 2,
        })
        .await;
    anyhow::ensure!(
        mainnet_send.is_err(),
        "sending to a mainnet sp address must fail on regtest",
    );

    let status = post_setup
        .wallet_service_client
        .get_reusable_scan_status(GetReusableScanStatusRequest::default())
        .await?
        .into_owned();
    anyhow::ensure!(
        status.birthday_height == birthday_tip,
        "birthday {} != activation tip {birthday_tip}",
        status.birthday_height,
    );

    // The pre-activation payment must be invisible to live scanning.
    let receives = list_sp_receives(post_setup).await?;
    anyhow::ensure!(
        receives.is_empty(),
        "pre-activation payment should not be visible before rescan",
    );
    Ok(base.address)
}

/// Phase C: BIP47 v1 inbound. Alice notifies Bob, then pays at index 0 and at
/// index 5 (skipping 1–4 to exercise the receive lookahead window). Returns
/// the two payment outpoints.
async fn phase_bip47_v1_inbound(
    post_setup: &mut PostSetup,
    alice: &mut Alice,
    bob: &BobKeys,
) -> anyhow::Result<Vec<OutPoint>> {
    tracing::info!("Phase C: BIP47 v1 inbound (notification + payments)");
    let secp = secp();
    let balance_before = wallet_balance_sats(post_setup).await?;

    // Notification: the designated input is Alice's first UTXO.
    let (designated_outpoint, _) = alice.utxos[0];
    let (payload, alice_code) = bip47::build_blinded_payload(
        &alice.bip47_account,
        bip47::Version::V1,
        &alice.wallet_key,
        &bob.v1_code,
        designated_outpoint,
        &secp,
    )?;
    anyhow::ensure!(alice_code == alice.v1_code);
    let notif_address = bip47::notification_address(&bob.v1_code, Network::Regtest, &secp)?;
    let notif_tx = alice.spend(
        1,
        vec![
            TxOut {
                value: Amount::from_sat(546),
                script_pubkey: notif_address.script_pubkey(),
            },
            TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::new_op_return(&PushBytesBuf::try_from(payload)?),
            },
        ],
        FEE,
    )?;
    broadcast(post_setup, &notif_tx).await?;
    mine_and_scan(post_setup).await?;

    let payers = list_inbound_payers(post_setup).await?;
    anyhow::ensure!(
        payers.len() == 1
            && payers[0].payment_code == alice.v1_code.to_string()
            && payers[0].next_receive_index == 0
            && payers[0].total_received_sats == 0,
        "unexpected inbound payers after v1 notification",
    );
    // The 546-sat notification output is not a payment; the balance must not
    // count it.
    anyhow::ensure!(
        wallet_balance_sats(post_setup).await? == balance_before,
        "v1 notification must not change the wallet balance",
    );

    // Payment at index 0.
    let send_addr_0 = bip47::derive_send_address_from_account(
        &alice.bip47_account,
        &bob.v1_code,
        0,
        Network::Regtest,
        &secp,
    )?;
    anyhow::ensure!(send_addr_0.script_pubkey().is_p2pkh(), "v1 must pay P2PKH");
    let pay0 = alice.spend(
        1,
        vec![TxOut {
            value: Amount::from_sat(V1_PAYMENT_0_SATS),
            script_pubkey: send_addr_0.script_pubkey(),
        }],
        FEE,
    )?;
    let pay0_outpoint = OutPoint::new(broadcast(post_setup, &pay0).await?, 0);
    mine_and_scan(post_setup).await?;

    let payers = list_inbound_payers(post_setup).await?;
    anyhow::ensure!(
        payers.len() == 1
            && payers[0].next_receive_index == 1
            && payers[0].total_received_sats == V1_PAYMENT_0_SATS,
        "payment at index 0 not accounted",
    );
    anyhow::ensure!(
        wallet_balance_sats(post_setup).await? == balance_before + V1_PAYMENT_0_SATS,
        "balance must include the v1 payment",
    );
    anyhow::ensure!(
        list_unspent_outpoints(post_setup)
            .await?
            .contains_key(&pay0_outpoint),
        "v1 payment UTXO missing from ListUnspentOutputs",
    );

    // Payment at index 5 (indices 1–4 skipped) must land inside the
    // 20-address lookahead window.
    let send_addr_5 = bip47::derive_send_address_from_account(
        &alice.bip47_account,
        &bob.v1_code,
        5,
        Network::Regtest,
        &secp,
    )?;
    let pay5 = alice.spend(
        1,
        vec![TxOut {
            value: Amount::from_sat(V1_PAYMENT_5_SATS),
            script_pubkey: send_addr_5.script_pubkey(),
        }],
        FEE,
    )?;
    let pay5_outpoint = OutPoint::new(broadcast(post_setup, &pay5).await?, 0);
    mine_and_scan(post_setup).await?;

    let payers = list_inbound_payers(post_setup).await?;
    anyhow::ensure!(
        payers.len() == 1
            && payers[0].next_receive_index == 6
            && payers[0].total_received_sats == V1_PAYMENT_0_SATS + V1_PAYMENT_5_SATS,
        "payment at index 5 not accounted (lookahead window)",
    );
    Ok(vec![pay0_outpoint, pay5_outpoint])
}

/// Phase D: BIP47 v3 inbound. The notification is a bare 1-of-3 multisig
/// output; the payment pays P2WPKH. Returns the payment outpoint.
async fn phase_bip47_v3_inbound(
    post_setup: &mut PostSetup,
    alice: &mut Alice,
    bob: &BobKeys,
) -> anyhow::Result<OutPoint> {
    tracing::info!("Phase D: BIP47 v3 inbound (multisig notification + payment)");
    let secp = secp();

    let ephemeral_priv = SecretKey::from_slice(&[0x33; 32])?;
    let notif_script = bip47::build_v3_notification_script(
        &alice.bip47_account,
        &ephemeral_priv,
        &bob.v3_code,
        &secp,
    )?;
    let notif_tx = alice.spend(
        1,
        vec![TxOut {
            value: Amount::from_sat(1_000),
            script_pubkey: notif_script,
        }],
        FEE,
    )?;
    broadcast(post_setup, &notif_tx).await?;
    mine_and_scan(post_setup).await?;

    let payers = list_inbound_payers(post_setup).await?;
    anyhow::ensure!(
        payers.len() == 2,
        "expected v1 + v3 inbound payers, got {}",
        payers.len(),
    );
    anyhow::ensure!(
        payers
            .iter()
            .any(|p| p.payment_code == alice.v3_code.to_string() && p.next_receive_index == 0),
        "v3 notification not detected",
    );

    let send_addr = bip47::derive_send_address_from_account(
        &alice.bip47_account,
        &bob.v3_code,
        0,
        Network::Regtest,
        &secp,
    )?;
    anyhow::ensure!(send_addr.script_pubkey().is_p2wpkh(), "v3 must pay P2WPKH");
    let pay = alice.spend(
        1,
        vec![TxOut {
            value: Amount::from_sat(V3_PAYMENT_0_SATS),
            script_pubkey: send_addr.script_pubkey(),
        }],
        FEE,
    )?;
    let pay_outpoint = OutPoint::new(broadcast(post_setup, &pay).await?, 0);
    mine_and_scan(post_setup).await?;

    let payers = list_inbound_payers(post_setup).await?;
    let v3_payer = payers
        .iter()
        .find(|p| p.payment_code == alice.v3_code.to_string())
        .ok_or_else(|| anyhow::anyhow!("v3 payer vanished"))?;
    anyhow::ensure!(
        v3_payer.next_receive_index == 1 && v3_payer.total_received_sats == V3_PAYMENT_0_SATS,
        "v3 payment not accounted",
    );
    Ok(pay_outpoint)
}

/// Phase E: BIP352 inbound with two inputs (key aggregation) and two
/// recipients in one transaction (base + labeled address).
async fn phase_silent_payments_inbound(
    post_setup: &mut PostSetup,
    alice: &mut Alice,
    bob_base_address: &str,
) -> anyhow::Result<()> {
    tracing::info!("Phase E: BIP352 inbound (multi-input, base + labeled)");
    let base = silent_payments::SilentPaymentAddress::parse_for_network(
        bob_base_address,
        Network::Regtest,
    )?;
    let labeled_str = post_setup
        .wallet_service_client
        .get_silent_payment_address(GetSilentPaymentAddressRequest { label: Some(1) })
        .await?
        .into_owned()
        .address;
    let labeled =
        silent_payments::SilentPaymentAddress::parse_for_network(&labeled_str, Network::Regtest)?;

    let sp_tx = alice.spend_silent_payment(
        2,
        &[
            (base, Amount::from_sat(SP_BASE_SATS)),
            (labeled, Amount::from_sat(SP_LABELED_SATS)),
        ],
        FEE,
    )?;
    let sp_txid = broadcast(post_setup, &sp_tx).await?;
    let height = mine_and_scan(post_setup).await?;

    let receives = list_sp_receives(post_setup).await?;
    anyhow::ensure!(
        receives.len() == 2,
        "expected 2 sp receives, got {}",
        receives.len(),
    );
    let base_receive = receives
        .iter()
        .find(|r| r.label_m.is_none())
        .ok_or_else(|| anyhow::anyhow!("base sp receive missing"))?;
    anyhow::ensure!(
        base_receive.outpoint.txid == sp_txid
            && base_receive.amount_sats == SP_BASE_SATS
            && base_receive.height == height,
        "base sp receive mismatch: {base_receive:?}",
    );
    let labeled_receive = receives
        .iter()
        .find(|r| r.label_m == Some(1))
        .ok_or_else(|| anyhow::anyhow!("labeled sp receive missing"))?;
    anyhow::ensure!(
        labeled_receive.amount_sats == SP_LABELED_SATS
            && labeled_receive.label_name.as_deref() == Some("integration"),
        "labeled sp receive mismatch: {labeled_receive:?}",
    );
    Ok(())
}

/// Phase F: outbound sends from the enforcer, verified by scanning with the
/// crate as Alice.
async fn phase_outbound(
    post_setup: &mut PostSetup,
    alice: &Alice,
    bob: &BobKeys,
) -> anyhow::Result<()> {
    tracing::info!("Phase F: outbound sends (v1 x2, v3, silent payments)");
    let secp = secp();

    // First v1 send: must broadcast a notification and pay index 0.
    let send0 = post_setup
        .wallet_service_client
        .send_to_bip47_payment_code(SendToBip47PaymentCodeRequest {
            payment_code: alice.v1_code.to_string(),
            amount_sats: OUT_V1_0_SATS,
            fee_sat_per_vbyte: 2,
        })
        .await?
        .into_owned();
    anyhow::ensure!(send0.sender_index == 0, "first v1 send must use index 0");
    let notif_txid = send0
        .notification_txid
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("first v1 send must broadcast a notification"))?
        .decode::<SendToBip47PaymentCodeResponse, _>("notification_txid")?;
    let height = mine_and_scan(post_setup).await?;
    let block = fetch_block_at(post_setup, height).await?;
    let prevouts = resolve_prevouts(post_setup, &block).await?;

    // The notification transaction must pay Alice's notification address 546
    // sats and carry an 80-byte OP_RETURN payload.
    let alice_notif_spk =
        bip47::notification_address(&alice.v1_code, Network::Regtest, &secp)?.script_pubkey();
    let notif_tx = block
        .txdata
        .iter()
        .find(|tx| tx.compute_txid() == notif_txid)
        .ok_or_else(|| anyhow::anyhow!("notification tx not in block"))?;
    anyhow::ensure!(
        notif_tx
            .output
            .iter()
            .any(|txo| txo.script_pubkey == alice_notif_spk && txo.value == Amount::from_sat(546)),
        "notification tx must pay Alice's notification address 546 sats",
    );
    anyhow::ensure!(
        find_op_return(notif_tx).is_some_and(|payload| payload.len() == 80),
        "notification tx must carry an 80-byte OP_RETURN payload",
    );

    // Scanning as Alice must surface the notification (unblinding to Bob's
    // payment code) and the payment at index 0.
    let events = alice.scan_block(&block, &prevouts, &[])?;
    anyhow::ensure!(
        events.iter().any(|event| matches!(
            event,
            scan::ScanEvent::Bip47Notification { sender, .. } if *sender == bob.v1_code
        )),
        "Alice could not unblind Bob's v1 notification",
    );
    let v1_receives: Vec<(u32, u64)> = events
        .iter()
        .filter_map(|event| match event {
            scan::ScanEvent::Bip47PaymentReceive { amount, source, .. }
                if source.sender_payment_code == bob.v1_code.to_string() =>
            {
                Some((source.i, amount.to_sat()))
            }
            _ => None,
        })
        .collect();
    anyhow::ensure!(
        v1_receives == vec![(0, OUT_V1_0_SATS)],
        "expected v1 payment at index 0, got {v1_receives:?}",
    );

    // Second v1 send: the notification must be reused, the index advances.
    let send1 = post_setup
        .wallet_service_client
        .send_to_bip47_payment_code(SendToBip47PaymentCodeRequest {
            payment_code: alice.v1_code.to_string(),
            amount_sats: OUT_V1_1_SATS,
            fee_sat_per_vbyte: 2,
        })
        .await?
        .into_owned();
    anyhow::ensure!(
        send1.notification_txid.into_option().is_none(),
        "second v1 send must not re-broadcast the notification",
    );
    anyhow::ensure!(send1.sender_index == 1, "second v1 send must use index 1");
    let height = mine_and_scan(post_setup).await?;
    let block = fetch_block_at(post_setup, height).await?;
    let prevouts = resolve_prevouts(post_setup, &block).await?;
    let events = alice.scan_block(&block, &prevouts, &[(bob.v1_code.clone(), 0)])?;
    anyhow::ensure!(
        events.iter().any(|event| matches!(
            event,
            scan::ScanEvent::Bip47PaymentReceive { amount, source, .. }
                if source.i == 1 && amount.to_sat() == OUT_V1_1_SATS
        )),
        "Alice did not detect the second v1 payment at index 1",
    );

    // v3 send: bare-multisig notification + P2WPKH payment.
    let send_v3 = post_setup
        .wallet_service_client
        .send_to_bip47_payment_code(SendToBip47PaymentCodeRequest {
            payment_code: alice.v3_code.to_string(),
            amount_sats: OUT_V3_0_SATS,
            fee_sat_per_vbyte: 2,
        })
        .await?
        .into_owned();
    anyhow::ensure!(send_v3.sender_index == 0, "first v3 send must use index 0");
    let height = mine_and_scan(post_setup).await?;
    let block = fetch_block_at(post_setup, height).await?;
    let prevouts = resolve_prevouts(post_setup, &block).await?;
    let events = alice.scan_block(&block, &prevouts, &[])?;
    anyhow::ensure!(
        events.iter().any(|event| matches!(
            event,
            scan::ScanEvent::Bip47Notification { sender, .. } if *sender == bob.v3_code
        )),
        "Alice could not unblind Bob's v3 notification",
    );
    anyhow::ensure!(
        events.iter().any(|event| matches!(
            event,
            scan::ScanEvent::Bip47PaymentReceive { amount, source, .. }
                if source.sender_payment_code == bob.v3_code.to_string()
                    && source.i == 0
                    && amount.to_sat() == OUT_V3_0_SATS
        )),
        "Alice did not detect Bob's v3 payment",
    );

    // Silent payment to Alice's base and labeled addresses in one call.
    let alice_base = silent_payments::derive_address(
        &alice.b_scan,
        &alice.b_spend,
        None,
        Network::Regtest,
        &secp,
    )?;
    let alice_labeled = silent_payments::derive_address(
        &alice.b_scan,
        &alice.b_spend,
        Some(ALICE_SP_LABEL),
        Network::Regtest,
        &secp,
    )?;
    let send_sp = post_setup
        .wallet_service_client
        .send_to_silent_payment(SendToSilentPaymentRequest {
            recipients: vec![
                send_to_silent_payment_request::Recipient {
                    sp_address: alice_base.to_string(),
                    amount_sats: OUT_SP_BASE_SATS,
                },
                send_to_silent_payment_request::Recipient {
                    sp_address: alice_labeled.to_string(),
                    amount_sats: OUT_SP_LABELED_SATS,
                },
            ],
            fee_sat_per_vbyte: 2,
        })
        .await?
        .into_owned();
    let sp_txid = send_sp
        .txid
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("sp send missing txid"))?
        .decode::<SendToSilentPaymentResponse, _>("txid")?;
    let height = mine_and_scan(post_setup).await?;
    let block = fetch_block_at(post_setup, height).await?;
    let prevouts = resolve_prevouts(post_setup, &block).await?;
    let events = alice.scan_block(&block, &prevouts, &[])?;
    let mut sp_matches: Vec<(Option<u32>, u64)> = events
        .iter()
        .filter_map(|event| match event {
            scan::ScanEvent::SilentPaymentReceive { txid, match_ } if *txid == sp_txid => {
                Some((match_.label, match_.amount.to_sat()))
            }
            _ => None,
        })
        .collect();
    sp_matches.sort();
    anyhow::ensure!(
        sp_matches
            == vec![
                (None, OUT_SP_BASE_SATS),
                (Some(ALICE_SP_LABEL), OUT_SP_LABELED_SATS),
            ],
        "Alice's sp scan mismatch: {sp_matches:?}",
    );

    // Recover the spending keys for both outputs and check they match the
    // on-chain taproot output keys — proves spendability without spending.
    let scanned = scan::block_to_scanned_txs(&block, &prevouts);
    let funding = scanned
        .iter()
        .find(|tx| tx.txid == sp_txid)
        .ok_or_else(|| anyhow::anyhow!("sp tx not in scanned block"))?;
    for event in &events {
        if let scan::ScanEvent::SilentPaymentReceive { txid, match_ } = event
            && *txid == sp_txid
        {
            let recovered = scan::recover_sp_spending_key(
                &alice.b_spend,
                &alice.b_scan,
                funding,
                match_.tweak_k,
                match_.label,
                &secp,
            )?;
            let (recovered_xonly, _) = recovered.public_key(&secp).x_only_public_key();
            anyhow::ensure!(
                recovered_xonly == match_.output_xonly,
                "recovered sp key does not match the output key",
            );
        }
    }
    Ok(())
}

/// Phase G: rescan from below the birthday (must find the pre-activation
/// payment), then spend every reusable output plus descriptor funds in one
/// transaction, then rescan again (spent outputs must stay spent).
async fn phase_rescan_and_spend_back(
    post_setup: &mut PostSetup,
    alice: &Alice,
    bip47_outpoints: &[OutPoint],
    pre_activation: (OutPoint, u32),
) -> anyhow::Result<()> {
    let (pre_outpoint, pre_height) = pre_activation;
    tracing::info!("Phase G: below-birthday rescan, spend-back, rescan again");

    // Snapshot state that the rescan must reproduce.
    let payers_before = list_inbound_payers(post_setup).await?;
    let receives_before = list_sp_receives(post_setup).await?;
    let balance_before = wallet_balance_sats(post_setup).await?;

    let rescan = post_setup
        .wallet_service_client
        .rescan_reusable_payments(RescanReusablePaymentsRequest {
            from_height: pre_height,
        })
        .await?
        .into_owned();
    anyhow::ensure!(rescan.scheduled_from_height == pre_height);

    // The pre-activation payment must now be visible...
    let receives = list_sp_receives(post_setup).await?;
    anyhow::ensure!(
        receives.iter().any(|r| r.outpoint == pre_outpoint
            && r.amount_sats == PRE_ACTIVATION_SP_SATS
            && r.height == pre_height),
        "below-birthday rescan did not find the pre-activation payment",
    );
    anyhow::ensure!(
        receives.len() == receives_before.len() + 1,
        "rescan must add exactly the pre-activation receive",
    );
    // ...and everything else must be unchanged (rescan idempotence).
    let payers = list_inbound_payers(post_setup).await?;
    anyhow::ensure!(
        payers.len() == payers_before.len()
            && payers_before.iter().all(|before| payers.iter().any(|now| {
                now.payment_code == before.payment_code
                    && now.next_receive_index == before.next_receive_index
                    && now.total_received_sats == before.total_received_sats
            })),
        "inbound payers changed across rescan",
    );
    anyhow::ensure!(
        wallet_balance_sats(post_setup).await? == balance_before + PRE_ACTIVATION_SP_SATS,
        "balance must gain exactly the recovered pre-activation payment",
    );

    // Spend back everything reusable — the two v1 P2PKH outputs, the v3
    // P2WPKH output, and the three silent-payment taproot outputs — mixed
    // with descriptor funds in a single transaction. Confirmation by bitcoind
    // consensus-validates every recovered key and signature.
    let sp_outpoints: Vec<OutPoint> = receives.iter().map(|r| r.outpoint).collect();
    let all_reusable: Vec<OutPoint> = bip47_outpoints
        .iter()
        .chain(sp_outpoints.iter())
        .copied()
        .collect();
    anyhow::ensure!(
        all_reusable.len() == 6,
        "expected 6 reusable outputs to spend, got {}",
        all_reusable.len(),
    );
    let unspent = list_unspent_outpoints(post_setup).await?;
    for outpoint in &all_reusable {
        anyhow::ensure!(
            unspent.contains_key(outpoint),
            "reusable output {outpoint} missing from ListUnspentOutputs",
        );
    }
    // Pinning UTXOs restricts selection to exactly the pinned set (the wallet
    // does not top up automatically), so pin Bob's largest descriptor UTXO
    // alongside the reusable ones to get a mixed descriptor + foreign spend.
    let descriptor_outpoint = unspent
        .iter()
        .filter(|(outpoint, _)| !all_reusable.contains(outpoint))
        .max_by_key(|(_, sats)| **sats)
        .map(|(outpoint, _)| *outpoint)
        .ok_or_else(|| anyhow::anyhow!("no descriptor UTXO to mix into the spend-back"))?;

    let balance_before_spend = wallet_balance_sats(post_setup).await?;
    let reusable_sats: u64 = PRE_ACTIVATION_SP_SATS
        + V1_PAYMENT_0_SATS
        + V1_PAYMENT_5_SATS
        + V3_PAYMENT_0_SATS
        + SP_BASE_SATS
        + SP_LABELED_SATS;
    // More than the reusable outputs hold, so the descriptor input is
    // genuinely required to fund the transaction.
    let spend_back_sats: u64 = reusable_sats + 100_000;
    let send = post_setup
        .wallet_service_client
        .send_transaction(SendTransactionRequest {
            destinations: [(alice.address().to_string(), spend_back_sats)]
                .into_iter()
                .collect(),
            required_utxos: all_reusable
                .iter()
                .chain(std::iter::once(&descriptor_outpoint))
                .map(|outpoint| send_transaction_request::RequiredUtxo {
                    txid: MessageField::some(ReverseHex::encode(&outpoint.txid)),
                    vout: outpoint.vout,
                })
                .collect(),
            ..Default::default()
        })
        .await?
        .into_owned();
    let spend_txid: Txid = send
        .txid
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("spend-back missing txid"))?
        .decode::<SendTransactionResponse, _>("txid")?;
    mine_and_scan(post_setup).await?;

    // bitcoind mined it: every reusable input signature was consensus-valid.
    let verbose_tx = bitcoin_cli(
        post_setup,
        vec![
            "getrawtransaction".to_owned(),
            spend_txid.to_string(),
            "true".to_owned(),
        ],
    )
    .await?;
    let verbose: serde_json::Value = serde_json::from_str(&verbose_tx)?;
    anyhow::ensure!(
        verbose["confirmations"].as_u64() == Some(1),
        "spend-back tx not confirmed",
    );

    let receives = list_sp_receives(post_setup).await?;
    anyhow::ensure!(
        receives.iter().all(|r| r.spent_in_txid == Some(spend_txid)),
        "all sp receives must be marked spent by the spend-back tx",
    );
    let unspent_after = list_unspent_outpoints(post_setup).await?;
    for outpoint in &all_reusable {
        anyhow::ensure!(
            !unspent_after.contains_key(outpoint),
            "spent reusable output {outpoint} still in ListUnspentOutputs",
        );
    }
    let balance_after_spend = wallet_balance_sats(post_setup).await?;
    anyhow::ensure!(
        balance_after_spend < balance_before_spend,
        "balance must decrease after the spend-back",
    );

    // Rescanning must not resurrect the spent outputs.
    post_setup
        .wallet_service_client
        .rescan_reusable_payments(RescanReusablePaymentsRequest {
            from_height: pre_height,
        })
        .await?;
    let receives = list_sp_receives(post_setup).await?;
    anyhow::ensure!(
        receives.iter().all(|r| r.spent_in_txid == Some(spend_txid)),
        "rescan resurrected spent silent-payment outputs",
    );
    let unspent_after_rescan = list_unspent_outpoints(post_setup).await?;
    for outpoint in &all_reusable {
        anyhow::ensure!(
            !unspent_after_rescan.contains_key(outpoint),
            "rescan resurrected spent reusable output {outpoint}",
        );
    }
    anyhow::ensure!(
        wallet_balance_sats(post_setup).await? == balance_after_spend,
        "rescan changed the balance",
    );
    Ok(())
}

/// Phase H: a confirmed silent-payment receive is reorged out (its funding
/// input is double-spent on the replacement chain); the receive must
/// disappear from listings and balance.
async fn phase_reorg(
    post_setup: &mut PostSetup,
    alice: &mut Alice,
    bob_base_address: &str,
) -> anyhow::Result<()> {
    tracing::info!("Phase H: reorged-out receive must disappear");
    let base = silent_payments::SilentPaymentAddress::parse_for_network(
        bob_base_address,
        Network::Regtest,
    )?;

    let balance_before = wallet_balance_sats(post_setup).await?;
    let receives_before = list_sp_receives(post_setup).await?.len();

    // Build the payment and a conflicting replacement spending the same
    // input. Bookkeep manually — exactly one of the two confirms.
    let funding = alice.utxos.remove(0);
    let sp_outputs = alice.compute_sp_outputs(
        std::slice::from_ref(&funding),
        &[(base, Amount::from_sat(REORG_SP_SATS))],
    )?;
    let sp_tx = alice.build_tx(std::slice::from_ref(&funding), sp_outputs, FEE)?;
    let sp_txid = broadcast(post_setup, &sp_tx).await?;
    let height = mine_and_scan(post_setup).await?;

    let receives = list_sp_receives(post_setup).await?;
    anyhow::ensure!(
        receives.iter().any(|r| r.outpoint.txid == sp_txid),
        "reorg-phase sp payment not detected",
    );
    anyhow::ensure!(
        wallet_balance_sats(post_setup).await? == balance_before + REORG_SP_SATS,
        "balance must include the reorg-phase payment",
    );

    // Invalidate the block, replace the payment with a higher-fee
    // double-spend (RBF), and mine past the original height.
    let block_hash = bitcoin_cli(
        post_setup,
        vec!["getblockhash".to_owned(), height.to_string()],
    )
    .await?;
    bitcoin_cli(
        post_setup,
        vec!["invalidateblock".to_owned(), block_hash.trim().to_owned()],
    )
    .await?;
    let conflict_tx = alice.build_tx(
        std::slice::from_ref(&funding),
        Vec::new(),
        Amount::from_sat(50_000),
    )?;
    let conflict_txid = broadcast(post_setup, &conflict_tx).await?;
    alice.utxos.push((
        OutPoint::new(conflict_txid, 0),
        conflict_tx.output[0].clone(),
    ));
    mine(post_setup, 2).await?;
    let tip = block_count(post_setup).await?;
    wait_for_scan(post_setup, tip).await?;

    let receives = list_sp_receives(post_setup).await?;
    anyhow::ensure!(
        !receives.iter().any(|r| r.outpoint.txid == sp_txid),
        "reorged-out sp receive must disappear from listings",
    );
    anyhow::ensure!(
        receives.len() == receives_before,
        "receive count must return to the pre-reorg state",
    );
    anyhow::ensure!(
        wallet_balance_sats(post_setup).await? == balance_before,
        "balance must drop back after the receive is reorged out",
    );
    Ok(())
}

pub async fn test_reusable_payments(pre_setup: PreSetup) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();

    // Fixed mnemonic so the test can mirror the wallet's derivations.
    let seed_path = pre_setup.directories.enforcer_dir.join("bob-seed.txt");
    std::fs::write(&seed_path, BOB_MNEMONIC)?;
    let setup_opts: SetupOpts = SetupOpts {
        wallet_seed_file: Some(seed_path),
        ..Default::default()
    };
    let mut post_setup = pre_setup.setup(Mode::Mempool, setup_opts, res_tx).await?;

    let bob = BobKeys::derive()?;
    let mut alice = Alice::derive()?;

    // Give Bob one spendable coinbase (mine 1 to Bob, then 100 to Core so it
    // matures), and fund Alice from Core's coinbase.
    let bob_address = post_setup
        .wallet_service_client
        .create_new_address(CreateNewAddressRequest::default())
        .await?
        .into_owned()
        .address;
    bitcoin_cli(
        &post_setup,
        vec!["generatetoaddress".to_owned(), "1".to_owned(), bob_address],
    )
    .await?;
    mine(&mut post_setup, 100).await?;
    fund_alice(&mut post_setup, &mut alice, 4, 2_000_000).await?;

    let pre_activation = phase_pre_activation_send(&mut post_setup, &mut alice, &bob).await?;
    let bob_base_address = phase_activation_and_address_cross_checks(&mut post_setup, &bob).await?;
    let v1_outpoints = phase_bip47_v1_inbound(&mut post_setup, &mut alice, &bob).await?;
    let v3_outpoint = phase_bip47_v3_inbound(&mut post_setup, &mut alice, &bob).await?;
    phase_silent_payments_inbound(&mut post_setup, &mut alice, &bob_base_address).await?;
    phase_outbound(&mut post_setup, &alice, &bob).await?;
    let bip47_outpoints: Vec<OutPoint> = v1_outpoints
        .into_iter()
        .chain(std::iter::once(v3_outpoint))
        .collect();
    phase_rescan_and_spend_back(&mut post_setup, &alice, &bip47_outpoints, pre_activation).await?;
    phase_reorg(&mut post_setup, &mut alice, &bob_base_address).await?;

    // Final consistency: the scanner ends exactly at the tip.
    let tip = block_count(&post_setup).await?;
    let status = post_setup
        .wallet_service_client
        .get_reusable_scan_status(GetReusableScanStatusRequest::default())
        .await?
        .into_owned();
    anyhow::ensure!(
        status.last_scanned_height == tip && !status.catching_up,
        "scanner must end exactly at the tip (scanned {}, tip {tip})",
        status.last_scanned_height,
    );

    tracing::info!("reusable payments roundtrip completed");
    Ok(())
}
