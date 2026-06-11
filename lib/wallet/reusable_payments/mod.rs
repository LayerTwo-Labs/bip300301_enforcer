pub use ::bip47;
pub mod r#impl;
pub mod scan;
pub mod silent_payments;
#[cfg(test)]
mod silent_payments_test_vectors;
mod util;

pub use self::{
    bip47::{PaymentCode, Version as Bip47Version},
    r#impl::is_bip352_eligible_spk,
    silent_payments::SilentPaymentAddress,
};

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
    pub sender_payment_code: PaymentCode,
    pub next_receive_index: u32,
    pub total_received_sats: u64,
    pub first_seen_unix: i64,
}
