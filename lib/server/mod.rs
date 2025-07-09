pub mod crypto;
pub mod validator;
pub mod wallet;

fn custom_json_rpc_err<Error>(error: Error) -> jsonrpsee::types::ErrorObject<'static>
where
    Error: std::error::Error,
{
    let err_msg = format!("{:#}", crate::errors::ErrorChain::new(&error));
    jsonrpsee::types::ErrorObject::owned(-1, err_msg, Option::<()>::None)
}

pub(crate) fn invalid_field_value<Message, Error>(
    field_name: &str,
    value: &str,
    source: Error,
) -> tonic::Status
where
    Message: prost::Name,
    Error: std::error::Error + Send + Sync + 'static,
{
    crate::proto::Error::invalid_field_value::<Message, _>(field_name, value, source).into()
}

pub(crate) fn missing_field<Message>(field_name: &str) -> tonic::Status
where
    Message: prost::Name,
{
    crate::proto::Error::missing_field::<Message>(field_name).into()
}
