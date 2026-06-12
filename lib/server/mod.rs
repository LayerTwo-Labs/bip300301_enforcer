use buffa::MessageField;
use buffa_types::google::protobuf::UInt32Value;
use connectrpc::ConnectError;

use crate::types::SidechainNumber;

pub mod crypto;
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

/// Decode a `MessageField<UInt32Value>` sidechain id from a request, mapping any
/// failure to a `ConnectError` carrying the message's name.
pub(crate) fn parse_sidechain_id<Message: buffa::MessageName>(
    field: MessageField<UInt32Value>,
    field_name: &str,
) -> Result<SidechainNumber, ConnectError> {
    let raw =
        crate::proto::unwrap_u32(field).ok_or_else(|| missing_field::<Message>(field_name))?;
    SidechainNumber::try_from(raw)
        .map_err(|err| invalid_field_value::<Message, _>(field_name, &raw.to_string(), err))
}
