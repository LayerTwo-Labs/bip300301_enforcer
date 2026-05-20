use bitcoin::OutPoint;
use buffa::MessageField;
use buffa_types::google::protobuf::{StringValue, Timestamp, UInt32Value, UInt64Value};
use connectrpc::{ConnectError, error::ErrorCode};
use thiserror::Error;

use crate::{
    messages::{
        CoinbaseMessage, M1ProposeSidechain, M2AckSidechain, M3ProposeBundle, M4AckBundles,
    },
    proto::{
        common::{ConsensusHex, Hex, ReverseHex},
        mainchain::{
            BlockHeaderInfo, BlockInfo, Deposit, Network, SidechainDeclaration,
            WithdrawalBundleEvent, block_info, deposit, get_coinbase_psbt_request,
            get_sidechains_response, get_two_way_peg_data_response, send_transaction_request,
            sidechain_declaration, subscribe_events_response, wallet_transaction,
            withdrawal_bundle_event,
        },
    },
    types::SidechainNumber,
};

/// Wiring for `file_per_package=true` codegen.
///
/// `buf generate` emits one self-contained `<dotted.pkg>.rs` per proto
/// package under `lib/proto/generated/{buffa,connect}/`. This module
/// arranges them into a `cusf::<pkg>::v1::*` tree via `include!()` so
/// the rest of the crate can reach types by their proto package path.
///
/// The wiring lives here because anything inside the codegen output
/// directories is wiped by `buf generate --clean`. Keeping it in
/// `lib/proto.rs` makes the build survive a clean regen.
///
/// `#![allow(...)]` mirrors `buffa_codegen::ALLOW_LINTS`. Copy in any
/// new lint that fires under `cargo clippy -D warnings` after a regen.
#[allow(
    non_camel_case_types,
    dead_code,
    unused_imports,
    unused_qualifications,
    clippy::derivable_impls,
    clippy::match_single_binding,
    clippy::uninlined_format_args,
    clippy::doc_lazy_continuation,
    clippy::module_inception,
    // buffa 0.7 codegen emits inline `#[allow(...)]` attributes; the workspace
    // denies `clippy::allow_attributes`, so permit them inside generated code.
    clippy::allow_attributes
)]
pub mod generated {
    pub mod buffa {
        pub mod cusf {
            pub mod common {
                pub mod v1 {
                    include!("proto/generated/buffa/cusf.common.v1.rs");
                }
            }
            pub mod crypto {
                pub mod v1 {
                    include!("proto/generated/buffa/cusf.crypto.v1.rs");
                }
            }
            pub mod mainchain {
                pub mod v1 {
                    include!("proto/generated/buffa/cusf.mainchain.v1.rs");
                }
            }
            pub mod sidechain {
                pub mod v1 {
                    include!("proto/generated/buffa/cusf.sidechain.v1.rs");
                }
            }
        }
    }

    pub mod connect {
        pub mod cusf {
            // `cusf.common.v1` has no services, so no connect-side file.
            pub mod crypto {
                pub mod v1 {
                    include!("proto/generated/connect/cusf.crypto.v1.rs");
                }
            }
            pub mod mainchain {
                pub mod v1 {
                    include!("proto/generated/connect/cusf.mainchain.v1.rs");
                }
            }
            pub mod sidechain {
                pub mod v1 {
                    include!("proto/generated/connect/cusf.sidechain.v1.rs");
                }
            }
        }
    }
}

pub mod common {
    pub use crate::proto::generated::buffa::cusf::common::v1::*;
}

pub mod crypto {
    pub use crate::proto::generated::buffa::cusf::crypto::v1::*;
}

pub mod mainchain {
    pub use crate::proto::generated::buffa::cusf::mainchain::v1::*;

    #[derive(Copy, Clone, Debug)]
    pub struct HeaderSyncProgress {
        pub current_height: Option<u32>,
    }

    impl From<HeaderSyncProgress> for SubscribeHeaderSyncProgressResponse {
        fn from(progress: HeaderSyncProgress) -> Self {
            Self {
                current_height: progress
                    .current_height
                    .map(super::wrap_u32)
                    .unwrap_or_default(),
            }
        }
    }
}

pub mod sidechain {
    pub use crate::proto::generated::buffa::cusf::sidechain::v1::*;
}

pub use crate::proto::generated::connect::cusf::{
    crypto::v1 as crypto_service, mainchain::v1 as mainchain_service,
};

pub trait ToStatus {
    fn builder(&self) -> StatusBuilder<'_>;
}

/// Construct a `ConnectError` from an Error.
pub struct StatusBuilder<'a> {
    pub code: ErrorCode,
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
            code: ErrorCode::Unknown,
            fmt_message: Box::new(move |f| std::fmt::Display::fmt(err, f)),
            source: err.source().map(either::Right),
        }
    }

    pub fn code(mut self, code: ErrorCode) -> Self {
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

    pub fn to_connect_error(&self) -> ConnectError {
        let msg = format!(
            "{:#}",
            crate::display::DisplayFn::new(|f| self.status_message(f))
        );
        ConnectError::new(self.code, msg)
    }
}

impl From<StatusBuilder<'_>> for ConnectError {
    fn from(builder: StatusBuilder<'_>) -> Self {
        builder.to_connect_error()
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
    pub fn invalid_enum_variant<Message: buffa::MessageName>(
        field_name: &str,
        variant_name: &str,
    ) -> Self {
        Self::InvalidEnumVariant {
            field_name: field_name.to_owned(),
            message_name: Message::FULL_NAME.to_owned(),
            variant_name: variant_name.to_owned(),
        }
    }

    pub fn invalid_field_value<Message: buffa::MessageName, Error>(
        field_name: &str,
        value: &str,
        source: Error,
    ) -> Self
    where
        Error: std::error::Error + Send + Sync + 'static,
    {
        Self::InvalidFieldValue {
            field_name: field_name.to_owned(),
            message_name: Message::FULL_NAME.to_owned(),
            value: value.to_owned(),
            source: Box::new(source),
        }
    }

    pub fn invalid_repeated_value<Message: buffa::MessageName>(
        field_name: &str,
        value: &str,
    ) -> Self {
        Self::InvalidRepeatedValue {
            field_name: field_name.to_owned(),
            message_name: Message::FULL_NAME.to_owned(),
            value: value.to_owned(),
        }
    }

    pub fn missing_field<Message: buffa::MessageName>(field_name: &str) -> Self {
        Self::MissingField {
            field_name: field_name.to_owned(),
            message_name: Message::FULL_NAME.to_owned(),
        }
    }
}

impl ToStatus for Error {
    fn builder(&self) -> StatusBuilder<'_> {
        StatusBuilder::new(self).code(ErrorCode::InvalidArgument)
    }
}

impl From<Error> for ConnectError {
    fn from(err: Error) -> Self {
        err.builder().into()
    }
}

pub fn wrap_string(value: impl Into<String>) -> MessageField<StringValue> {
    MessageField::some(StringValue {
        value: value.into(),
        ..Default::default()
    })
}

pub fn wrap_u32(value: u32) -> MessageField<UInt32Value> {
    MessageField::some(UInt32Value {
        value,
        ..Default::default()
    })
}

pub fn wrap_u64(value: u64) -> MessageField<UInt64Value> {
    MessageField::some(UInt64Value {
        value,
        ..Default::default()
    })
}

pub fn wrap_timestamp(seconds: i64) -> MessageField<Timestamp> {
    MessageField::some(Timestamp {
        seconds,
        nanos: 0,
        ..Default::default()
    })
}

pub fn unwrap_string(field: MessageField<StringValue>) -> Option<String> {
    field.into_option().map(|sv| sv.value)
}

pub fn unwrap_u32(field: MessageField<UInt32Value>) -> Option<u32> {
    field.into_option().map(|uv| uv.value)
}

pub fn unwrap_u64(field: MessageField<UInt64Value>) -> Option<u64> {
    field.into_option().map(|uv| uv.value)
}

impl common::ConsensusHex {
    pub fn decode<Message: buffa::MessageName, T>(self, field_name: &str) -> Result<T, Error>
    where
        T: bitcoin::consensus::Decodable,
    {
        let hex = unwrap_string(self.hex).ok_or_else(|| Error::missing_field::<Self>("hex"))?;
        bitcoin::consensus::encode::deserialize_hex(&hex)
            .map_err(|err| Error::invalid_field_value::<Message, _>(field_name, &hex, err))
    }

    pub fn decode_status<Message: buffa::MessageName, T>(
        self,
        field_name: &str,
    ) -> Result<T, ConnectError>
    where
        T: bitcoin::consensus::Decodable,
    {
        self.decode::<Message, _>(field_name)
            .map_err(ConnectError::from)
    }

    pub fn encode<T>(value: &T) -> Self
    where
        T: bitcoin::consensus::Encodable,
    {
        let hex = bitcoin::consensus::encode::serialize_hex(value);
        Self {
            hex: wrap_string(hex),
        }
    }
}

impl common::Hex {
    pub fn decode<Message: buffa::MessageName, T>(self, field_name: &str) -> Result<T, Error>
    where
        T: hex::FromHex,
        <T as hex::FromHex>::Error: std::error::Error + Send + Sync + 'static,
    {
        let hex = unwrap_string(self.hex).ok_or_else(|| Error::missing_field::<Self>("hex"))?;
        T::from_hex(&hex)
            .map_err(|err| Error::invalid_field_value::<Message, _>(field_name, &hex, err))
    }

    pub fn decode_status<Message: buffa::MessageName, T>(
        self,
        field_name: &str,
    ) -> Result<T, ConnectError>
    where
        T: hex::FromHex,
        <T as hex::FromHex>::Error: std::error::Error + Send + Sync + 'static,
    {
        self.decode::<Message, _>(field_name)
            .map_err(ConnectError::from)
    }

    pub fn encode<T>(value: &T) -> Self
    where
        T: hex::ToHex,
    {
        let hex: String = value.encode_hex();
        Self {
            hex: wrap_string(hex),
        }
    }
}

impl common::ReverseHex {
    pub fn decode<Message: buffa::MessageName, T>(self, field_name: &str) -> Result<T, Error>
    where
        T: bitcoin::consensus::Decodable,
    {
        let hex = unwrap_string(self.hex).ok_or_else(|| Error::missing_field::<Self>("hex"))?;
        let mut bytes = hex::decode(&hex)
            .map_err(|err| Error::invalid_field_value::<Message, _>(field_name, &hex, err))?;
        bytes.reverse();
        bitcoin::consensus::deserialize(&bytes)
            .map_err(|err| Error::invalid_field_value::<Message, _>(field_name, &hex, err))
    }

    pub fn decode_status<Message: buffa::MessageName, T>(
        self,
        field_name: &str,
    ) -> Result<T, ConnectError>
    where
        T: bitcoin::consensus::Decodable,
    {
        self.decode::<Message, _>(field_name)
            .map_err(ConnectError::from)
    }

    pub fn encode<T>(value: &T) -> Self
    where
        T: bitcoin::consensus::Encodable,
    {
        let mut bytes = bitcoin::consensus::encode::serialize(value);
        bytes.reverse();
        Self {
            hex: wrap_string(hex::encode(bytes)),
        }
    }
}

impl From<&OutPoint> for mainchain::OutPoint {
    fn from(outpoint: &OutPoint) -> Self {
        Self {
            txid: MessageField::some(ReverseHex::encode(&outpoint.txid)),
            vout: wrap_u32(outpoint.vout),
        }
    }
}

impl From<crate::types::SidechainDeclaration> for sidechain_declaration::V0 {
    fn from(decl: crate::types::SidechainDeclaration) -> Self {
        Self {
            title: wrap_string(decl.title),
            description: wrap_string(decl.description),
            hash_id_1: MessageField::some(ConsensusHex::encode(&decl.hash_id_1)),
            hash_id_2: MessageField::some(Hex::encode(&decl.hash_id_2)),
        }
    }
}

impl TryFrom<sidechain_declaration::V0> for crate::types::SidechainDeclaration {
    type Error = Error;

    fn try_from(decl: sidechain_declaration::V0) -> Result<Self, Self::Error> {
        let sidechain_declaration::V0 {
            title,
            description,
            hash_id_1,
            hash_id_2,
            ..
        } = decl;
        let title = unwrap_string(title)
            .ok_or_else(|| Error::missing_field::<sidechain_declaration::V0>("title"))?;
        let description = unwrap_string(description)
            .ok_or_else(|| Error::missing_field::<sidechain_declaration::V0>("description"))?;
        let hash_id_1 = hash_id_1
            .into_option()
            .ok_or_else(|| Error::missing_field::<sidechain_declaration::V0>("hash_id_1"))?
            .decode::<sidechain_declaration::V0, _>("hash_id_1")?;
        let hash_id_2 = hash_id_2
            .into_option()
            .ok_or_else(|| Error::missing_field::<sidechain_declaration::V0>("hash_id_2"))?
            .decode::<sidechain_declaration::V0, _>("hash_id_2")?;
        Ok(Self {
            title,
            description,
            hash_id_1,
            hash_id_2,
        })
    }
}

impl From<sidechain_declaration::V0> for SidechainDeclaration {
    fn from(v0: sidechain_declaration::V0) -> Self {
        Self {
            sidechain_declaration: Some(v0.into()),
        }
    }
}

impl From<crate::types::SidechainDeclaration> for SidechainDeclaration {
    fn from(decl: crate::types::SidechainDeclaration) -> Self {
        sidechain_declaration::V0::from(decl).into()
    }
}

impl TryFrom<sidechain_declaration::SidechainDeclaration> for crate::types::SidechainDeclaration {
    type Error = Error;

    fn try_from(decl: sidechain_declaration::SidechainDeclaration) -> Result<Self, Self::Error> {
        match decl {
            sidechain_declaration::SidechainDeclaration::V0(v0) => (*v0).try_into(),
        }
    }
}

impl TryFrom<SidechainDeclaration> for crate::types::SidechainDeclaration {
    type Error = Error;

    fn try_from(decl: SidechainDeclaration) -> Result<Self, Self::Error> {
        let SidechainDeclaration {
            sidechain_declaration,
            ..
        } = decl;
        sidechain_declaration
            .ok_or_else(|| Error::missing_field::<SidechainDeclaration>("sidechain_declaration"))?
            .try_into()
    }
}

impl From<bitcoin::Network> for Network {
    fn from(network: bitcoin::Network) -> Self {
        match network {
            bitcoin::Network::Bitcoin => Network::NETWORK_MAINNET,
            bitcoin::Network::Regtest => Network::NETWORK_REGTEST,
            bitcoin::Network::Signet => Network::NETWORK_SIGNET,
            bitcoin::Network::Testnet => Network::NETWORK_TESTNET,
            bitcoin::Network::Testnet4 => Network::NETWORK_TESTNET,
        }
    }
}

impl TryFrom<get_coinbase_psbt_request::ProposeSidechain> for M1ProposeSidechain {
    type Error = Error;

    fn try_from(propose: get_coinbase_psbt_request::ProposeSidechain) -> Result<Self, Self::Error> {
        use get_coinbase_psbt_request::ProposeSidechain;
        let ProposeSidechain {
            sidechain_number,
            data,
            ..
        } = propose;
        let sidechain_number: SidechainNumber = {
            let n = unwrap_u32(sidechain_number)
                .ok_or_else(|| Error::missing_field::<ProposeSidechain>("sidechain_number"))?;
            n.try_into().map_err(|err| {
                Error::invalid_field_value::<ProposeSidechain, _>(
                    "sidechain_number",
                    &n.to_string(),
                    err,
                )
            })?
        };
        let description: Vec<u8> = data
            .into_option()
            .ok_or_else(|| Error::missing_field::<ProposeSidechain>("data"))?
            .decode::<ProposeSidechain, _>("data")?;
        Ok(M1ProposeSidechain {
            sidechain_number,
            description: description.into(),
        })
    }
}

impl TryFrom<get_coinbase_psbt_request::AckSidechain> for M2AckSidechain {
    type Error = Error;

    fn try_from(ack: get_coinbase_psbt_request::AckSidechain) -> Result<Self, Self::Error> {
        use get_coinbase_psbt_request::AckSidechain;
        let AckSidechain {
            sidechain_number,
            data_hash,
            ..
        } = ack;
        let sidechain_number: SidechainNumber = {
            let n = unwrap_u32(sidechain_number)
                .ok_or_else(|| Error::missing_field::<AckSidechain>("sidechain_number"))?;
            n.try_into().map_err(|err| {
                Error::invalid_field_value::<AckSidechain, _>(
                    "sidechain_number",
                    &n.to_string(),
                    err,
                )
            })?
        };
        let description_hash = data_hash
            .into_option()
            .ok_or_else(|| Error::missing_field::<AckSidechain>("data_hash"))?
            .decode::<AckSidechain, _>("data_hash")?;
        Ok(M2AckSidechain {
            sidechain_number,
            description_hash,
        })
    }
}

impl TryFrom<get_coinbase_psbt_request::ProposeBundle> for M3ProposeBundle {
    type Error = Error;

    fn try_from(propose: get_coinbase_psbt_request::ProposeBundle) -> Result<Self, Self::Error> {
        use get_coinbase_psbt_request::ProposeBundle;
        let ProposeBundle {
            sidechain_number,
            bundle_txid,
            ..
        } = propose;
        let sidechain_number: SidechainNumber = {
            let n = unwrap_u32(sidechain_number)
                .ok_or_else(|| Error::missing_field::<ProposeBundle>("sidechain_number"))?;
            n.try_into().map_err(|err| {
                Error::invalid_field_value::<ProposeBundle, _>(
                    "sidechain_number",
                    &n.to_string(),
                    err,
                )
            })?
        };
        let bundle_txid: [u8; 32] = bundle_txid
            .into_option()
            .ok_or_else(|| Error::missing_field::<ProposeBundle>("bundle_txid"))?
            .decode::<ProposeBundle, _>("bundle_txid")?;
        Ok(M3ProposeBundle {
            sidechain_number,
            bundle_txid,
        })
    }
}

impl From<get_coinbase_psbt_request::ack_bundles::RepeatPrevious> for M4AckBundles {
    fn from(_: get_coinbase_psbt_request::ack_bundles::RepeatPrevious) -> Self {
        Self::RepeatPrevious
    }
}

impl From<get_coinbase_psbt_request::ack_bundles::LeadingBy50> for M4AckBundles {
    fn from(_: get_coinbase_psbt_request::ack_bundles::LeadingBy50) -> Self {
        Self::LeadingBy50
    }
}

impl TryFrom<get_coinbase_psbt_request::ack_bundles::Upvotes> for M4AckBundles {
    type Error = Error;

    fn try_from(
        upvotes: get_coinbase_psbt_request::ack_bundles::Upvotes,
    ) -> Result<Self, Self::Error> {
        use get_coinbase_psbt_request::ack_bundles::Upvotes;
        let Upvotes { upvotes: vals, .. } = upvotes;
        let mut two_bytes = false;
        for upvote in &vals {
            if *upvote > u16::MAX as u32 {
                return Err(Error::invalid_repeated_value::<Upvotes>(
                    "upvotes",
                    &upvote.to_string(),
                ));
            } else if *upvote > u8::MAX as u32 {
                two_bytes = true;
            }
        }
        if two_bytes {
            Ok(Self::TwoBytes {
                upvotes: vals.into_iter().map(|u| u as u16).collect(),
            })
        } else {
            Ok(Self::OneByte {
                upvotes: vals.into_iter().map(|u| u as u8).collect(),
            })
        }
    }
}

impl TryFrom<get_coinbase_psbt_request::ack_bundles::AckBundles> for M4AckBundles {
    type Error = Error;

    fn try_from(
        ack: get_coinbase_psbt_request::ack_bundles::AckBundles,
    ) -> Result<Self, Self::Error> {
        use get_coinbase_psbt_request::ack_bundles::AckBundles;
        match ack {
            AckBundles::LeadingBy50(v) => Ok((*v).into()),
            AckBundles::RepeatPrevious(v) => Ok((*v).into()),
            AckBundles::Upvotes(v) => (*v).try_into(),
        }
    }
}

impl TryFrom<get_coinbase_psbt_request::AckBundles> for M4AckBundles {
    type Error = Error;

    fn try_from(ack: get_coinbase_psbt_request::AckBundles) -> Result<Self, Self::Error> {
        use get_coinbase_psbt_request::AckBundles;
        let AckBundles { ack_bundles, .. } = ack;
        ack_bundles
            .ok_or_else(|| Error::missing_field::<AckBundles>("ack_bundles"))?
            .try_into()
    }
}

impl TryFrom<get_coinbase_psbt_request::AckBundles> for CoinbaseMessage {
    type Error = Error;

    fn try_from(ack: get_coinbase_psbt_request::AckBundles) -> Result<Self, Self::Error> {
        ack.try_into().map(Self::M4AckBundles)
    }
}

impl From<crate::types::HeaderInfo> for BlockHeaderInfo {
    fn from(info: crate::types::HeaderInfo) -> Self {
        Self {
            block_hash: MessageField::some(ReverseHex::encode(&info.block_hash)),
            prev_block_hash: MessageField::some(ReverseHex::encode(&info.prev_block_hash)),
            height: info.height,
            work: MessageField::some(ConsensusHex::encode(&info.work.to_le_bytes())),
            timestamp: info.timestamp as u64,
        }
    }
}

impl From<&crate::types::Deposit> for Deposit {
    fn from(d: &crate::types::Deposit) -> Self {
        let crate::types::Deposit {
            sequence_number,
            outpoint,
            address,
            value,
        } = d;
        let output = deposit::Output {
            address: MessageField::some(Hex::encode(&address)),
            value_sats: wrap_u64(value.to_sat()),
        };
        Deposit {
            sequence_number: wrap_u64(*sequence_number),
            outpoint: MessageField::some(outpoint.into()),
            output: MessageField::some(output),
        }
    }
}

impl From<&crate::types::WithdrawalBundleEventKind> for withdrawal_bundle_event::event::Event {
    fn from(kind: &crate::types::WithdrawalBundleEventKind) -> Self {
        use withdrawal_bundle_event::event::Succeeded;

        use crate::types::WithdrawalBundleEventKind as K;
        match kind {
            K::Failed => Self::Failed(Box::default()),
            K::Submitted => Self::Submitted(Box::default()),
            K::Succeeded {
                sequence_number,
                transaction,
            } => Self::Succeeded(Box::new(Succeeded {
                sequence_number: wrap_u64(*sequence_number),
                transaction: MessageField::some(ConsensusHex::encode(transaction)),
            })),
        }
    }
}

impl From<&crate::types::WithdrawalBundleEventKind> for withdrawal_bundle_event::Event {
    fn from(kind: &crate::types::WithdrawalBundleEventKind) -> Self {
        Self {
            event: Some(kind.into()),
        }
    }
}

impl From<&crate::types::SidechainWithdrawalBundleEvent> for WithdrawalBundleEvent {
    fn from(event: &crate::types::SidechainWithdrawalBundleEvent) -> Self {
        WithdrawalBundleEvent {
            m6id: MessageField::some(ConsensusHex::encode(&event.m6id.0)),
            event: MessageField::some((&event.kind).into()),
        }
    }
}

impl From<&crate::types::SidechainBlockEvent> for Option<block_info::event::Event> {
    fn from(event: &crate::types::SidechainBlockEvent) -> Self {
        use crate::types::SidechainBlockEvent;
        match event {
            SidechainBlockEvent::Deposit(deposit) => {
                Some(block_info::event::Event::Deposit(Box::new(deposit.into())))
            }
            SidechainBlockEvent::SidechainProposal { .. } => None,
            SidechainBlockEvent::WithdrawalBundle(bundle) => Some(
                block_info::event::Event::WithdrawalBundle(Box::new(bundle.into())),
            ),
        }
    }
}

impl From<&crate::types::SidechainBlockEvent> for Option<block_info::Event> {
    fn from(event: &crate::types::SidechainBlockEvent) -> Self {
        let event = Option::<_>::from(event)?;
        Some(block_info::Event { event: Some(event) })
    }
}

impl<E> From<&crate::types::SidechainBlockInfo<E>> for BlockInfo
where
    E: std::borrow::Borrow<crate::types::SidechainBlockEvent>,
{
    fn from(info: &crate::types::SidechainBlockInfo<E>) -> Self {
        let bmm_commitment = info
            .bmm_commitment
            .as_ref()
            .map(ConsensusHex::encode)
            .map(MessageField::some)
            .unwrap_or_default();
        let events = info
            .events
            .iter()
            .filter_map(|ev| Option::<_>::from(ev.borrow()))
            .collect();
        Self {
            bmm_commitment,
            events,
        }
    }
}

impl crate::types::BlockInfo {
    pub fn as_proto(&self, sidechain_number: SidechainNumber) -> BlockInfo {
        (&self.only_sidechain(sidechain_number)).into()
    }
}

impl crate::types::TwoWayPegData {
    pub fn into_proto(
        self,
        sidechain_number: SidechainNumber,
    ) -> Option<get_two_way_peg_data_response::ResponseItem> {
        let Self {
            header_info,
            block_info,
        } = self;
        let block_info = block_info.as_proto(sidechain_number);
        if block_info == BlockInfo::default() {
            None
        } else {
            Some(get_two_way_peg_data_response::ResponseItem {
                block_header_info: MessageField::some(header_info.into()),
                block_info: MessageField::some(block_info),
            })
        }
    }
}

impl crate::types::Event {
    pub fn into_proto(
        self,
        sidechain_number: SidechainNumber,
    ) -> subscribe_events_response::event::Event {
        use subscribe_events_response::event::{ConnectBlock, DisconnectBlock};
        match self {
            Self::ConnectBlock {
                header_info,
                block_info,
            } => {
                let cb = ConnectBlock {
                    header_info: MessageField::some(header_info.into()),
                    block_info: MessageField::some(block_info.as_proto(sidechain_number)),
                };
                subscribe_events_response::event::Event::ConnectBlock(Box::new(cb))
            }
            Self::DisconnectBlock { block_hash } => {
                let db = DisconnectBlock {
                    block_hash: MessageField::some(ReverseHex::encode(&block_hash)),
                };
                subscribe_events_response::event::Event::DisconnectBlock(Box::new(db))
            }
        }
    }
}

impl From<subscribe_events_response::event::Event> for subscribe_events_response::Event {
    fn from(ev: subscribe_events_response::event::Event) -> Self {
        Self { event: Some(ev) }
    }
}

impl From<crate::types::Sidechain> for get_sidechains_response::SidechainInfo {
    fn from(sc: crate::types::Sidechain) -> Self {
        let decl = crate::types::SidechainDeclaration::try_from(&sc.proposal.description).ok();

        Self {
            sidechain_number: wrap_u32(sc.proposal.sidechain_number.0 as u32),
            description: MessageField::some(ConsensusHex::encode(&sc.proposal.description.0)),
            vote_count: wrap_u32(sc.status.vote_count as u32),
            proposal_height: wrap_u32(sc.status.proposal_height),
            activation_height: sc
                .status
                .activation_height
                .map(wrap_u32)
                .unwrap_or_default(),
            declaration: decl
                .map(|d| MessageField::some(SidechainDeclaration::from(d)))
                .unwrap_or_default(),
        }
    }
}

impl From<&bdk_wallet::chain::ChainPosition<bdk_wallet::chain::ConfirmationBlockTime>>
    for wallet_transaction::Confirmation
{
    fn from(
        chain_position: &bdk_wallet::chain::ChainPosition<bdk_wallet::chain::ConfirmationBlockTime>,
    ) -> Self {
        match chain_position {
            bdk_wallet::chain::ChainPosition::Confirmed {
                anchor: conf,
                transitively: _,
            } => Self {
                height: conf.block_id.height,
                block_hash: MessageField::some(ReverseHex::encode(&conf.block_id.hash)),
                timestamp: wrap_timestamp(conf.confirmation_time as i64),
            },
            bdk_wallet::chain::ChainPosition::Unconfirmed {
                last_seen,
                first_seen: _,
            } => Self {
                height: 0,
                block_hash: MessageField::none(),
                timestamp: last_seen
                    .map(|s| wrap_timestamp(s as i64))
                    .unwrap_or_default(),
            },
        }
    }
}

impl From<&crate::types::BDKWalletTransaction> for mainchain::WalletTransaction {
    fn from(tx: &crate::types::BDKWalletTransaction) -> Self {
        Self {
            txid: MessageField::some(ReverseHex::encode(&tx.txid)),
            raw_transaction: MessageField::some(ConsensusHex::encode(&tx.tx)),
            fee_sats: tx.fee.to_sat(),
            received_sats: tx.received.to_sat(),
            sent_sats: tx.sent.to_sat(),
            confirmation_info: MessageField::some((&tx.chain_position).into()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Invalid sats per vbyte")]
pub struct InvalidSatsPerVbyte {
    pub sats_per_vbyte: u64,
}

impl TryFrom<send_transaction_request::fee_rate::Fee> for crate::types::FeePolicy {
    type Error = InvalidSatsPerVbyte;

    fn try_from(fee: send_transaction_request::fee_rate::Fee) -> Result<Self, Self::Error> {
        use send_transaction_request::fee_rate::Fee;
        match fee {
            Fee::SatPerVbyte(sats_per_vbyte) => {
                let rate = bitcoin::FeeRate::from_sat_per_vb(sats_per_vbyte)
                    .ok_or(InvalidSatsPerVbyte { sats_per_vbyte })?;
                Ok(rate.into())
            }
            Fee::Sats(sats) => {
                let amount = bitcoin::Amount::from_sat(sats);
                Ok(amount.into())
            }
        }
    }
}

impl TryFrom<send_transaction_request::FeeRate> for crate::types::FeePolicy {
    type Error = Error;

    fn try_from(fee_rate: send_transaction_request::FeeRate) -> Result<Self, Self::Error> {
        use send_transaction_request::FeeRate;
        let FeeRate { fee, .. } = fee_rate;
        fee.ok_or_else(|| Error::missing_field::<FeeRate>("fee"))?
            .try_into()
            .map_err(|err: InvalidSatsPerVbyte| {
                Error::invalid_field_value::<FeeRate, _>(
                    "fee",
                    &err.sats_per_vbyte.to_string(),
                    err,
                )
            })
    }
}
