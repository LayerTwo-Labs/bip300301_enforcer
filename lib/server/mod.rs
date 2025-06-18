pub mod crypto;
pub mod validator;
pub mod wallet;

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
