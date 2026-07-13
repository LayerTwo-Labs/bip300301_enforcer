// Facade over the external `reusable-payments` crate: wallet code reaches it
// through this module, since the submodule name shadows the crate name here.
pub use ::reusable_payments::{bip44_coin_type, bip47, scan, silent_payments, spend};
pub mod r#impl;

#[derive(Clone, Debug)]
pub struct Bip47SendResult {
    pub notification_txid: Option<bitcoin::Txid>,
    pub payment_txid: bitcoin::Txid,
    pub sender_index: u32,
    pub version: bip47::Version,
}

#[derive(Clone, Debug)]
pub struct SilentPaymentReceive {
    pub txid: bitcoin::Txid,
    pub vout: u32,
    pub output_pubkey: bitcoin::XOnlyPublicKey,
    pub amount: bitcoin::Amount,
    pub tweak_k: u32,
    pub label_m: Option<u32>,
    pub label_name: Option<String>,
    pub height: u32,
    pub spent_in_txid: Option<bitcoin::Txid>,
}

#[derive(Clone, Debug)]
pub struct Bip47InboundPayer {
    pub sender_payment_code: bip47::PaymentCode,
    pub next_receive_index: u32,
    pub total_received_sats: u64,
    pub first_seen_unix: i64,
}

/// An unspent output the wallet owns via reusable payments (BIP47 or silent
/// payment), living outside the BDK descriptor. Carries what's needed to
/// recover its spend key.
pub struct ReusableOwnedOutput {
    pub outpoint: bitcoin::OutPoint,
    pub txout: bitcoin::TxOut,
    pub height: u32,
    pub key_source: ReusableKeySource,
}

/// How to recover the spend key for a [`ReusableOwnedOutput`].
pub enum ReusableKeySource {
    Bip47 {
        sender_code: bip47::PaymentCode,
        index: u32,
    },
    SilentPayment {
        tweak_k: u32,
        label: Option<u32>,
    },
}
