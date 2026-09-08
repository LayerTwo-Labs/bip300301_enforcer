use buffa::{Inline, MessageField};
use buffa_types::google::protobuf::UInt32Value;
use connectrpc::ConnectError;

use crate::types::SidechainNumber;

pub mod block_producer;
pub mod crypto;
pub mod mining;
pub mod validator;
pub mod wallet;

pub(crate) fn invalid_field_value<Message: buffa::MessageName, Error>(
    field_name: &str,
    value: &str,
    source: Error,
) -> ConnectError
where
    Error: std::error::Error + Send + Sync + 'static,
{
    crate::proto::Error::invalid_field_value::<Message, _>(field_name, value, source).into()
}

pub(crate) fn missing_field<Message: buffa::MessageName>(field_name: &str) -> ConnectError {
    crate::proto::Error::missing_field::<Message>(field_name).into()
}

pub(crate) fn internal_err<E: std::fmt::Display>(err: E) -> ConnectError {
    ConnectError::internal(err.to_string())
}

/// Validate a withdrawal bundle and store it for the next M3 proposal.
///
/// Both `BlockProducerService.ProposeWithdrawalBundle` and
/// `WalletService.BroadcastWithdrawalBundle` land here, so one place holds the
/// rule. The generic parameter names the calling message, so a field error
/// reports the RPC the caller actually used.
pub(crate) async fn store_withdrawal_bundle<Message: buffa::MessageName>(
    producer: &crate::block_producer::BlockProducer,
    sidechain_id: SidechainNumber,
    transaction_bytes: &[u8],
) -> Result<crate::types::M6id, ConnectError> {
    // An inactive slot has no treasury to withdraw from, so per BIP300 M3 a
    // bundle proposed for it is a no-op. A reorg can deactivate the slot after
    // this gate, so the block builder skips such a bundle as well.
    let active = producer
        .validator()
        .get_active_sidechains()
        .map_err(internal_err)?
        .iter()
        .any(|sidechain| sidechain.proposal.sidechain_number == sidechain_id);
    if !active {
        return Err(ConnectError::failed_precondition(format!(
            "cannot accept a withdrawal bundle for sidechain {sidechain_id}: not active"
        )));
    }
    // With no treasury UTXO an M6 has nothing to spend, so the bundle could
    // never pay out.
    match producer.validator().try_get_ctip(sidechain_id) {
        Ok(None) => {
            return Err(ConnectError::failed_precondition(format!(
                "cannot accept a withdrawal bundle for sidechain {sidechain_id}: no treasury UTXO"
            )));
        }
        Ok(Some(_)) => (),
        Err(err) => return Err(internal_err(err)),
    }
    // A blinded M6 is a zero-input tx that Core and sidechains serialize in
    // legacy form, which the standard decoder cannot read.
    let transaction = crate::types::BlindedM6::deserialize(transaction_bytes).map_err(|err| {
        invalid_field_value::<Message, _>("transaction", &hex::encode(transaction_bytes), err)
    })?;
    producer
        .db()
        .put_withdrawal_bundle(sidechain_id, &transaction)
        .await
        .map_err(internal_err)
}

/// Decode a `MessageField<UInt32Value>` sidechain id from a request, mapping any
/// failure to a `ConnectError` carrying the message's name.
pub(crate) fn parse_sidechain_id<Message: buffa::MessageName>(
    field: MessageField<UInt32Value, Inline<UInt32Value>>,
    field_name: &str,
) -> Result<SidechainNumber, ConnectError> {
    let raw =
        crate::proto::unwrap_u32(field).ok_or_else(|| missing_field::<Message>(field_name))?;
    SidechainNumber::try_from(raw)
        .map_err(|err| invalid_field_value::<Message, _>(field_name, &raw.to_string(), err))
}
