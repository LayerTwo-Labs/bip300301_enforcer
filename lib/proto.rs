use thiserror::Error;

pub static ENCODED_FILE_DESCRIPTOR_SETS: &[&[u8]] = &[
    cusf::common::v1::FILE_DESCRIPTOR_SET,
    cusf::crypto::v1::FILE_DESCRIPTOR_SET,
    cusf::mainchain::v1::FILE_DESCRIPTOR_SET,
    cusf::sidechain::v1::FILE_DESCRIPTOR_SET,
    google::protobuf::FILE_DESCRIPTOR_SET,
];

pub trait ToStatus {
    fn builder(&self) -> StatusBuilder<'_>;
}

/// Construct a `tonic::Status` from an Error
pub struct StatusBuilder<'a> {
    pub code: tonic::Code,
    /// Defines the message, without source
    pub fmt_message: Box<dyn Fn(&mut std::fmt::Formatter<'_>) -> std::fmt::Result + 'a>,
    pub source:
        Option<either::Either<Box<StatusBuilder<'a>>, &'a (dyn std::error::Error + 'static)>>,
}

impl<'a> StatusBuilder<'a> {
    /// Default builder, using error source chain.
    /// Use this for errors without a source error, or errors with source
    /// errors that do not implement `ToStatus`.
    /// Transparent error variants should use this on their source when
    /// implementing `ToStatus`, if their source error does not impl
    /// `ToStatus`. If the source error does impl `ToStatus`, delegate to
    /// the source error's `StatusBuilder`.
    /// Non-transparent error variants should use `StatusBuilder::with_code`
    /// if the source error impls `ToStatus`.
    pub fn new<E>(err: &'a E) -> Self
    where
        E: std::error::Error,
    {
        Self {
            code: tonic::Code::Unknown,
            fmt_message: Box::new(move |f| std::fmt::Display::fmt(err, f)),
            source: err.source().map(either::Right),
        }
    }

    pub fn code(mut self, code: tonic::Code) -> Self {
        self.code = code;
        self
    }

    /// Defines the message, without source
    pub fn message<F>(mut self, message: F) -> Self
    where
        F: Fn(&mut std::fmt::Formatter<'_>) -> std::fmt::Result + 'a,
    {
        self.fmt_message = Box::new(message);
        self
    }

    /// Set the source to another `StatusBuilder`.
    pub fn source(mut self, source: Self) -> Self {
        self.source = Some(either::Left(Box::new(source)));
        self
    }

    /// Inherit code from a source status builder.
    /// Use this for non-transparent error variants, where the source
    /// implements `ToStatus`.
    /// Transparent error variants should delegate to the source error's
    /// `StatusBuilder` instead of using this function.
    pub fn with_code<T>(err_msg: &'a T, source_builder: Self) -> Self
    where
        T: std::fmt::Display,
    {
        Self {
            code: source_builder.code,
            fmt_message: Box::new(move |f| std::fmt::Display::fmt(&err_msg, f)),
            source: Some(either::Left(Box::new(source_builder))),
        }
    }

    /// Full status message, including source errors in alternate mode
    fn status_message(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let () = (self.fmt_message)(f)?;
        if !f.alternate() {
            return Ok(());
        }
        match &self.source {
            Some(either::Left(source)) => {
                std::fmt::Display::fmt(": ", f)?;
                source.status_message(f)
            }
            Some(either::Right(source)) => {
                std::fmt::Display::fmt(": ", f)?;
                std::fmt::Display::fmt(source, f)?;
                let mut source = *source;
                while let Some(cause) = source.source() {
                    source = cause;
                    std::fmt::Display::fmt(": ", f)?;
                    std::fmt::Display::fmt(source, f)?;
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub fn to_status(&self) -> tonic::Status {
        let msg = format!(
            "{:#}",
            crate::display::DisplayFn::new(|f| self.status_message(f))
        );
        tonic::Status::new(self.code, msg)
    }
}

impl From<StatusBuilder<'_>> for tonic::Status {
    fn from(builder: StatusBuilder<'_>) -> Self {
        builder.to_status()
    }
}

#[derive(miette::Diagnostic, Debug, Error)]
pub enum Error {
    #[error(
        "Invalid enum variant in field `{field_name}` of message `{message_name}`: `{variant_name}`"
    )]
    InvalidEnumVariant {
        field_name: String,
        message_name: String,
        variant_name: String,
    },
    #[error("Invalid field value in field `{field_name}` of message `{message_name}`: `{value}`")]
    InvalidFieldValue {
        field_name: String,
        message_name: String,
        value: String,
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    #[error(
        "Invalid value in repeated field `{field_name}` of message `{message_name}`: `{value}`"
    )]
    InvalidRepeatedValue {
        field_name: String,
        message_name: String,
        value: String,
    },
    #[error("Missing field in message `{message_name}`: `{field_name}`")]
    MissingField {
        field_name: String,
        message_name: String,
    },
    #[error("Unknown enum tag in field `{field_name}` of message `{message_name}`: `{tag}`")]
    UnknownEnumTag {
        field_name: String,
        message_name: String,
        tag: i32,
    },
}

impl Error {
    pub fn invalid_enum_variant<Message>(field_name: &str, variant_name: &str) -> Self
    where
        Message: prost::Name,
    {
        Self::InvalidEnumVariant {
            field_name: field_name.to_owned(),
            message_name: Message::full_name(),
            variant_name: variant_name.to_owned(),
        }
    }

    pub fn invalid_field_value<Message, Error>(field_name: &str, value: &str, source: Error) -> Self
    where
        Message: prost::Name,
        Error: std::error::Error + Send + Sync + 'static,
    {
        Self::InvalidFieldValue {
            field_name: field_name.to_owned(),
            message_name: Message::full_name(),
            value: value.to_owned(),
            source: Box::new(source),
        }
    }

    pub fn invalid_repeated_value<Message>(field_name: &str, value: &str) -> Self
    where
        Message: prost::Name,
    {
        Self::InvalidRepeatedValue {
            field_name: field_name.to_owned(),
            message_name: Message::full_name(),
            value: value.to_owned(),
        }
    }

    pub fn missing_field<Message>(field_name: &str) -> Self
    where
        Message: prost::Name,
    {
        Self::MissingField {
            field_name: field_name.to_owned(),
            message_name: Message::full_name(),
        }
    }
}

impl ToStatus for Error {
    fn builder(&self) -> StatusBuilder<'_> {
        StatusBuilder::new(self).code(tonic::Code::InvalidArgument)
    }
}

impl From<Error> for tonic::Status {
    fn from(err: Error) -> Self {
        err.builder().into()
    }
}

pub mod cusf;
pub mod google;

pub use self::cusf::{
    common::v1 as common, crypto::v1 as crypto, mainchain::v1 as mainchain,
    sidechain::v1 as sidechain,
};
