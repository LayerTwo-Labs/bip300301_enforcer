use thiserror::Error;

/// Encoded file descriptor set, for gRPC reflection
pub static ENCODED_FILE_DESCRIPTOR_SET: &[u8] =
    tonic::include_file_descriptor_set!("file_descriptor_set");

pub trait ToStatus {
    fn builder(&self) -> StatusBuilder;
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
    fn builder(&self) -> StatusBuilder {
        StatusBuilder::new(self).code(tonic::Code::InvalidArgument)
    }
}

impl From<Error> for tonic::Status {
    fn from(err: Error) -> Self {
        err.builder().into()
    }
}

pub mod common {
    use std::error::Error as _;

    tonic::include_proto!("cusf.common.v1");

    impl ConsensusHex {
        pub fn decode<Message, T>(self, field_name: &str) -> Result<T, super::Error>
        where
            Message: prost::Name,
            T: bitcoin::consensus::Decodable,
        {
            let Self { hex } = self;
            let hex = hex.ok_or_else(|| super::Error::missing_field::<Self>("hex"))?;
            bitcoin::consensus::encode::deserialize_hex(&hex).map_err(|err| {
                super::Error::invalid_field_value::<Message, _>(field_name, &hex, err)
            })
        }

        /// Variant of [`Self::decode`] that returns a `tonic::Status` error
        #[allow(clippy::result_large_err)]
        pub fn decode_tonic<Message, T>(self, field_name: &str) -> Result<T, tonic::Status>
        where
            Message: prost::Name,
            T: bitcoin::consensus::Decodable,
        {
            self.decode::<Message, _>(field_name).map_err(|err| {
                let mut msg = err.to_string();
                if let Some(source) = err.source() {
                    msg = format!("{msg}: {source:#}")
                }
                tonic::Status::new(tonic::Code::InvalidArgument, msg)
            })
        }

        pub fn encode<T>(value: &T) -> Self
        where
            T: bitcoin::consensus::Encodable,
        {
            let hex = bitcoin::consensus::encode::serialize_hex(value);
            Self { hex: Some(hex) }
        }
    }

    impl Hex {
        pub fn decode<Message, T>(self, field_name: &str) -> Result<T, super::Error>
        where
            Message: prost::Name,
            T: hex::FromHex,
            <T as hex::FromHex>::Error: std::error::Error + Send + Sync + 'static,
        {
            let Self { hex } = self;
            let hex = hex.ok_or_else(|| super::Error::missing_field::<Self>("hex"))?;
            T::from_hex(&hex).map_err(|err| {
                super::Error::invalid_field_value::<Message, _>(field_name, &hex, err)
            })
        }

        /// Variant of [`Self::decode`] that returns a `tonic::Status` error
        #[allow(clippy::result_large_err)]
        pub fn decode_tonic<Message, T>(self, field_name: &str) -> Result<T, tonic::Status>
        where
            Message: prost::Name,
            T: hex::FromHex,
            <T as hex::FromHex>::Error: std::error::Error + Send + Sync + 'static,
        {
            self.decode::<Message, _>(field_name).map_err(|err| {
                let mut msg = err.to_string();
                if let Some(source) = err.source() {
                    msg = format!("{msg}: {source:#}")
                }
                tonic::Status::new(tonic::Code::InvalidArgument, msg)
            })
        }

        pub fn encode<T>(value: &T) -> Self
        where
            T: hex::ToHex,
        {
            let hex = value.encode_hex();
            Self { hex: Some(hex) }
        }
    }

    impl ReverseHex {
        pub fn decode<Message, T>(self, field_name: &str) -> Result<T, super::Error>
        where
            Message: prost::Name,
            T: bitcoin::consensus::Decodable,
        {
            let Self { hex } = self;
            let hex = hex.ok_or_else(|| super::Error::missing_field::<Self>("hex"))?;
            let mut bytes = hex::decode(&hex).map_err(|err| {
                super::Error::invalid_field_value::<Message, _>(field_name, &hex, err)
            })?;
            bytes.reverse();
            bitcoin::consensus::deserialize(&bytes).map_err(|err| {
                super::Error::invalid_field_value::<Message, _>(field_name, &hex, err)
            })
        }

        /// Variant of [`Self::decode`] that returns a `tonic::Status` error
        #[allow(clippy::result_large_err)]
        pub fn decode_tonic<Message, T>(self, field_name: &str) -> Result<T, tonic::Status>
        where
            Message: prost::Name,
            T: bitcoin::consensus::Decodable,
        {
            self.decode::<Message, _>(field_name).map_err(|err| {
                let mut msg = err.to_string();
                if let Some(source) = err.source() {
                    msg = format!("{msg}: {source:#}")
                }
                tonic::Status::new(tonic::Code::InvalidArgument, msg)
            })
        }

        pub fn encode<T>(value: &T) -> Self
        where
            T: bitcoin::consensus::Encodable,
        {
            let mut bytes = bitcoin::consensus::encode::serialize(value);
            bytes.reverse();
            Self {
                hex: Some(hex::encode(bytes)),
            }
        }
    }
}

pub mod crypto {
    tonic::include_proto!("cusf.crypto.v1");
}

pub mod mainchain {
    use crate::{
        messages::{
            CoinbaseMessage, M1ProposeSidechain, M2AckSidechain, M3ProposeBundle, M4AckBundles,
        },
        proto::common::{ConsensusHex, Hex, ReverseHex},
        types::SidechainNumber,
    };

    tonic::include_proto!("cusf.mainchain.v1");

    #[allow(unused_imports)]
    pub use self::{
        subscribe_events_response::event::{ConnectBlock, DisconnectBlock},
        validator_service_server::{
            self as server, ValidatorService as Service, ValidatorServiceServer as Server,
        },
    };

    impl From<&bitcoin::OutPoint> for OutPoint {
        fn from(outpoint: &bitcoin::OutPoint) -> Self {
            Self {
                txid: Some(ReverseHex::encode(&outpoint.txid)),
                vout: Some(outpoint.vout),
            }
        }
    }

    impl From<crate::types::SidechainDeclaration> for sidechain_declaration::V0 {
        fn from(declaration: crate::types::SidechainDeclaration) -> Self {
            Self {
                title: Some(declaration.title),
                description: Some(declaration.description),
                hash_id_1: Some(ConsensusHex::encode(&declaration.hash_id_1)),
                hash_id_2: Some(Hex::encode(&declaration.hash_id_2)),
            }
        }
    }

    impl TryFrom<sidechain_declaration::V0> for crate::types::SidechainDeclaration {
        type Error = super::Error;

        fn try_from(declaration: sidechain_declaration::V0) -> Result<Self, Self::Error> {
            let sidechain_declaration::V0 {
                title,
                description,
                hash_id_1,
                hash_id_2,
            } = declaration;
            let title = title
                .ok_or_else(|| super::Error::missing_field::<sidechain_declaration::V0>("title"))?;
            let description = description.ok_or_else(|| {
                super::Error::missing_field::<sidechain_declaration::V0>("description")
            })?;
            let hash_id_1 = hash_id_1
                .ok_or_else(|| {
                    super::Error::missing_field::<sidechain_declaration::V0>("hash_id_1")
                })?
                .decode::<sidechain_declaration::V0, _>("hash_id_1")?;
            let hash_id_2 = hash_id_2
                .ok_or_else(|| {
                    super::Error::missing_field::<sidechain_declaration::V0>("hash_id_2")
                })?
                .decode::<sidechain_declaration::V0, _>("hash_id_2")?;
            Ok(Self {
                title,
                description,
                hash_id_1,
                hash_id_2,
            })
        }
    }

    impl From<sidechain_declaration::V0> for sidechain_declaration::SidechainDeclaration {
        fn from(declaration: sidechain_declaration::V0) -> Self {
            sidechain_declaration::SidechainDeclaration::V0(declaration)
        }
    }

    impl From<sidechain_declaration::V0> for SidechainDeclaration {
        fn from(declaration: sidechain_declaration::V0) -> Self {
            Self {
                sidechain_declaration: Some(declaration.into()),
            }
        }
    }

    impl From<crate::types::SidechainDeclaration> for SidechainDeclaration {
        fn from(declaration: crate::types::SidechainDeclaration) -> Self {
            sidechain_declaration::V0::from(declaration).into()
        }
    }

    impl TryFrom<sidechain_declaration::SidechainDeclaration> for crate::types::SidechainDeclaration {
        type Error = super::Error;

        fn try_from(
            declaration: sidechain_declaration::SidechainDeclaration,
        ) -> Result<Self, Self::Error> {
            match declaration {
                sidechain_declaration::SidechainDeclaration::V0(v0) => v0.try_into(),
            }
        }
    }

    impl TryFrom<SidechainDeclaration> for crate::types::SidechainDeclaration {
        type Error = super::Error;

        fn try_from(declaration: SidechainDeclaration) -> Result<Self, Self::Error> {
            let SidechainDeclaration {
                sidechain_declaration,
            } = declaration;
            sidechain_declaration
                .ok_or_else(|| {
                    super::Error::missing_field::<SidechainDeclaration>("sidechain_declaration")
                })?
                .try_into()
        }
    }

    impl From<bitcoin::Network> for Network {
        fn from(network: bitcoin::Network) -> Self {
            match network {
                bitcoin::Network::Bitcoin => Network::Mainnet,
                bitcoin::Network::Regtest => Network::Regtest,
                bitcoin::Network::Signet => Network::Signet,
                bitcoin::Network::Testnet => Network::Testnet,
                _ => Network::Unknown,
            }
        }
    }

    impl TryFrom<get_coinbase_psbt_request::ProposeSidechain> for M1ProposeSidechain {
        type Error = super::Error;

        fn try_from(
            propose_sidechain: get_coinbase_psbt_request::ProposeSidechain,
        ) -> Result<Self, Self::Error> {
            use get_coinbase_psbt_request::ProposeSidechain;
            let ProposeSidechain {
                sidechain_number,
                data,
            } = propose_sidechain;
            let sidechain_number: SidechainNumber = {
                let sidechain_number: u32 = sidechain_number.ok_or_else(|| {
                    Self::Error::missing_field::<ProposeSidechain>("sidechain_number")
                })?;
                sidechain_number.try_into().map_err(|err| {
                    Self::Error::invalid_field_value::<ProposeSidechain, _>(
                        "sidechain_number",
                        &sidechain_number.to_string(),
                        err,
                    )
                })?
            };
            let description: Vec<u8> = data
                .ok_or_else(|| Self::Error::missing_field::<ProposeSidechain>("data"))?
                .decode::<ProposeSidechain, _>("data")?;
            Ok(M1ProposeSidechain {
                sidechain_number,
                description: description.into(),
            })
        }
    }

    impl TryFrom<get_coinbase_psbt_request::AckSidechain> for M2AckSidechain {
        type Error = super::Error;

        fn try_from(
            ack_sidechain: get_coinbase_psbt_request::AckSidechain,
        ) -> Result<Self, Self::Error> {
            use get_coinbase_psbt_request::AckSidechain;
            let AckSidechain {
                sidechain_number,
                data_hash,
            } = ack_sidechain;
            let sidechain_number: SidechainNumber = {
                let sidechain_number: u32 = sidechain_number.ok_or_else(|| {
                    Self::Error::missing_field::<AckSidechain>("sidechain_number")
                })?;
                sidechain_number.try_into().map_err(|err| {
                    Self::Error::invalid_field_value::<AckSidechain, _>(
                        "sidechain_number",
                        &sidechain_number.to_string(),
                        err,
                    )
                })?
            };
            let description_hash = data_hash
                .ok_or_else(|| Self::Error::missing_field::<AckSidechain>("data_hash"))?
                .decode::<AckSidechain, _>("data_hash")?;
            Ok(M2AckSidechain {
                sidechain_number,
                description_hash,
            })
        }
    }

    impl TryFrom<get_coinbase_psbt_request::ProposeBundle> for M3ProposeBundle {
        type Error = super::Error;

        fn try_from(
            propose_bundle: get_coinbase_psbt_request::ProposeBundle,
        ) -> Result<Self, Self::Error> {
            use get_coinbase_psbt_request::ProposeBundle;
            let ProposeBundle {
                sidechain_number,
                bundle_txid,
            } = propose_bundle;
            let sidechain_number: SidechainNumber = {
                let sidechain_number: u32 = sidechain_number.ok_or_else(|| {
                    Self::Error::missing_field::<ProposeBundle>("sidechain_number")
                })?;
                sidechain_number.try_into().map_err(|err| {
                    Self::Error::invalid_field_value::<ProposeBundle, _>(
                        "sidechain_number",
                        &sidechain_number.to_string(),
                        err,
                    )
                })?
            };
            let bundle_txid: [u8; 32] = bundle_txid
                .ok_or_else(|| Self::Error::missing_field::<ProposeBundle>("bundle_txid"))?
                .decode::<ProposeBundle, _>("bundle_txid")?;
            Ok(M3ProposeBundle {
                sidechain_number,
                bundle_txid,
            })
        }
    }

    impl From<get_coinbase_psbt_request::ack_bundles::RepeatPrevious> for M4AckBundles {
        fn from(repeat_previous: get_coinbase_psbt_request::ack_bundles::RepeatPrevious) -> Self {
            let get_coinbase_psbt_request::ack_bundles::RepeatPrevious {} = repeat_previous;
            Self::RepeatPrevious
        }
    }

    impl From<get_coinbase_psbt_request::ack_bundles::LeadingBy50> for M4AckBundles {
        fn from(leading_by_50: get_coinbase_psbt_request::ack_bundles::LeadingBy50) -> Self {
            let get_coinbase_psbt_request::ack_bundles::LeadingBy50 {} = leading_by_50;
            Self::LeadingBy50
        }
    }

    impl TryFrom<get_coinbase_psbt_request::ack_bundles::Upvotes> for M4AckBundles {
        type Error = super::Error;

        fn try_from(
            upvotes: get_coinbase_psbt_request::ack_bundles::Upvotes,
        ) -> Result<Self, Self::Error> {
            use get_coinbase_psbt_request::ack_bundles::Upvotes;
            let Upvotes { upvotes } = upvotes;
            let mut two_bytes = false;
            for upvote in &upvotes {
                if *upvote > u16::MAX as u32 {
                    let err = Self::Error::invalid_repeated_value::<Upvotes>(
                        "upvotes",
                        &upvote.to_string(),
                    );
                    return Err(err);
                } else if *upvote > u8::MAX as u32 {
                    two_bytes = true;
                }
            }
            if two_bytes {
                let upvotes = upvotes.into_iter().map(|upvote| upvote as u16).collect();
                Ok(Self::TwoBytes { upvotes })
            } else {
                let upvotes = upvotes.into_iter().map(|upvote| upvote as u8).collect();
                Ok(Self::OneByte { upvotes })
            }
        }
    }

    impl TryFrom<get_coinbase_psbt_request::ack_bundles::AckBundles> for M4AckBundles {
        type Error = super::Error;

        fn try_from(
            ack_bundles: get_coinbase_psbt_request::ack_bundles::AckBundles,
        ) -> Result<Self, Self::Error> {
            use get_coinbase_psbt_request::ack_bundles::AckBundles;
            match ack_bundles {
                AckBundles::LeadingBy50(leading_by_50) => Ok(leading_by_50.into()),
                AckBundles::RepeatPrevious(repeat_previous) => Ok(repeat_previous.into()),
                AckBundles::Upvotes(upvotes) => upvotes.try_into(),
            }
        }
    }

    impl TryFrom<get_coinbase_psbt_request::AckBundles> for M4AckBundles {
        type Error = super::Error;

        fn try_from(
            ack_bundles: get_coinbase_psbt_request::AckBundles,
        ) -> Result<Self, Self::Error> {
            use get_coinbase_psbt_request::AckBundles;
            let AckBundles { ack_bundles } = ack_bundles;
            ack_bundles
                .ok_or_else(|| Self::Error::missing_field::<AckBundles>("ack_bundles"))?
                .try_into()
        }
    }

    impl TryFrom<get_coinbase_psbt_request::AckBundles> for CoinbaseMessage {
        type Error = super::Error;

        fn try_from(
            ack_bundles: get_coinbase_psbt_request::AckBundles,
        ) -> Result<Self, Self::Error> {
            ack_bundles.try_into().map(Self::M4AckBundles)
        }
    }

    impl From<crate::types::HeaderInfo> for BlockHeaderInfo {
        fn from(header_info: crate::types::HeaderInfo) -> Self {
            Self {
                block_hash: Some(ReverseHex::encode(&header_info.block_hash)),
                prev_block_hash: Some(ReverseHex::encode(&header_info.prev_block_hash)),
                height: header_info.height,
                work: Some(ConsensusHex::encode(&header_info.work.to_le_bytes())),
                timestamp: header_info.timestamp as u64,
            }
        }
    }

    impl From<&crate::types::Deposit> for (SidechainNumber, Deposit) {
        fn from(deposit: &crate::types::Deposit) -> Self {
            let crate::types::Deposit {
                sidechain_id,
                sequence_number,
                outpoint,
                address,
                value,
            } = deposit;
            let output = deposit::Output {
                address: Some(Hex::encode(&address)),
                value_sats: Some(value.to_sat()),
            };
            let deposit = Deposit {
                sequence_number: Some(*sequence_number),
                outpoint: Some(outpoint.into()),
                output: Some(output),
            };
            (*sidechain_id, deposit)
        }
    }

    impl From<withdrawal_bundle_event::event::Failed> for withdrawal_bundle_event::event::Event {
        fn from(failed: withdrawal_bundle_event::event::Failed) -> Self {
            Self::Failed(failed)
        }
    }

    impl From<withdrawal_bundle_event::event::Submitted> for withdrawal_bundle_event::event::Event {
        fn from(submitted: withdrawal_bundle_event::event::Submitted) -> Self {
            Self::Submitted(submitted)
        }
    }

    impl From<withdrawal_bundle_event::event::Succeeded> for withdrawal_bundle_event::event::Event {
        fn from(succeeded: withdrawal_bundle_event::event::Succeeded) -> Self {
            Self::Succeeded(succeeded)
        }
    }

    impl From<&crate::types::WithdrawalBundleEventKind> for withdrawal_bundle_event::event::Event {
        fn from(event_kind: &crate::types::WithdrawalBundleEventKind) -> Self {
            use withdrawal_bundle_event::event::{Failed, Submitted, Succeeded};

            use crate::types::WithdrawalBundleEventKind;
            match event_kind {
                WithdrawalBundleEventKind::Failed => Self::from(Failed {}),
                WithdrawalBundleEventKind::Submitted => Self::from(Submitted {}),
                WithdrawalBundleEventKind::Succeeded {
                    sequence_number,
                    transaction,
                } => Self::from(Succeeded {
                    sequence_number: Some(*sequence_number),
                    transaction: Some(ConsensusHex::encode(transaction)),
                }),
            }
        }
    }

    impl From<&crate::types::WithdrawalBundleEventKind> for withdrawal_bundle_event::Event {
        fn from(event_kind: &crate::types::WithdrawalBundleEventKind) -> Self {
            Self {
                event: Some(event_kind.into()),
            }
        }
    }

    impl From<&crate::types::WithdrawalBundleEvent> for (SidechainNumber, WithdrawalBundleEvent) {
        fn from(event: &crate::types::WithdrawalBundleEvent) -> Self {
            let sidechain_number = event.sidechain_id;
            let event = WithdrawalBundleEvent {
                m6id: Some(ConsensusHex::encode(&event.m6id.0)),
                event: Some((&event.kind).into()),
            };
            (sidechain_number, event)
        }
    }

    impl From<&crate::types::BlockEvent> for Option<(SidechainNumber, block_info::event::Event)> {
        fn from(
            event: &crate::types::BlockEvent,
        ) -> Option<(SidechainNumber, block_info::event::Event)> {
            use crate::types::BlockEvent;
            match event {
                BlockEvent::Deposit(deposit) => {
                    let (sidechain_number, deposit) = deposit.into();
                    Some((sidechain_number, block_info::event::Event::Deposit(deposit)))
                }
                BlockEvent::SidechainProposal { .. } => None,
                BlockEvent::WithdrawalBundle(bundle_event) => {
                    let (sidechain_number, bundle_event) = bundle_event.into();
                    Some((
                        sidechain_number,
                        block_info::event::Event::WithdrawalBundle(bundle_event),
                    ))
                }
            }
        }
    }

    impl From<&crate::types::BlockEvent> for Option<(SidechainNumber, block_info::Event)> {
        fn from(event: &crate::types::BlockEvent) -> Self {
            let (sidechain_number, event) = Option::<_>::from(event)?;
            Some((sidechain_number, block_info::Event { event: Some(event) }))
        }
    }

    impl<E> From<&crate::types::SidechainBlockInfo<E>> for BlockInfo
    where
        E: std::borrow::Borrow<crate::types::BlockEvent>,
    {
        fn from(block_info: &crate::types::SidechainBlockInfo<E>) -> Self {
            let bmm_commitment = block_info.bmm_commitment.as_ref().map(ConsensusHex::encode);
            let events = block_info
                .events
                .iter()
                .filter_map(|event| {
                    let (_event_sidechain_number, event) = Option::<_>::from(event.borrow())?;
                    Some(event)
                })
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
                    block_header_info: Some(header_info.into()),
                    block_info: Some(block_info),
                })
            }
        }
    }

    impl crate::types::Event {
        pub fn into_proto(
            self,
            sidechain_number: SidechainNumber,
        ) -> subscribe_events_response::event::Event {
            match self {
                Self::ConnectBlock {
                    header_info,
                    block_info,
                } => {
                    let event = ConnectBlock {
                        header_info: Some(header_info.into()),
                        block_info: Some(block_info.as_proto(sidechain_number)),
                    };
                    subscribe_events_response::event::Event::ConnectBlock(event)
                }
                Self::DisconnectBlock { block_hash } => {
                    let event = DisconnectBlock {
                        block_hash: Some(ReverseHex::encode(&block_hash)),
                    };
                    subscribe_events_response::event::Event::DisconnectBlock(event)
                }
            }
        }
    }

    impl From<subscribe_events_response::event::Event> for subscribe_events_response::Event {
        fn from(event: subscribe_events_response::event::Event) -> Self {
            Self { event: Some(event) }
        }
    }

    impl From<crate::types::Sidechain> for get_sidechains_response::SidechainInfo {
        fn from(sidechain: crate::types::Sidechain) -> Self {
            use crate::types::SidechainDeclaration;
            let decl = SidechainDeclaration::try_from(&sidechain.proposal.description).ok();

            Self {
                sidechain_number: Some(sidechain.proposal.sidechain_number.0 as u32),
                description: Some(ConsensusHex::encode(&sidechain.proposal.description.0)),
                vote_count: Some(sidechain.status.vote_count as u32),
                proposal_height: Some(sidechain.status.proposal_height),
                activation_height: sidechain.status.activation_height,
                declaration: decl.map(|decl| decl.into()),
            }
        }
    }

    impl From<&bdk_wallet::chain::ChainPosition<bdk_wallet::chain::ConfirmationBlockTime>>
        for wallet_transaction::Confirmation
    {
        fn from(
            chain_position: &bdk_wallet::chain::ChainPosition<
                bdk_wallet::chain::ConfirmationBlockTime,
            >,
        ) -> Self {
            match chain_position {
                bdk_wallet::chain::ChainPosition::Confirmed {
                    anchor: conf_block_time,
                    transitively: _,
                } => Self {
                    height: conf_block_time.block_id.height,
                    block_hash: Some(ReverseHex::encode(&conf_block_time.block_id.hash)),
                    timestamp: Some(prost_types::Timestamp {
                        seconds: conf_block_time.confirmation_time as i64,
                        nanos: 0,
                    }),
                },
                bdk_wallet::chain::ChainPosition::Unconfirmed { last_seen } => {
                    let timestamp = last_seen.map(|last_seen| prost_types::Timestamp {
                        seconds: last_seen as i64,
                        nanos: 0,
                    });
                    Self {
                        height: 0,
                        block_hash: None,
                        timestamp,
                    }
                }
            }
        }
    }

    impl From<&crate::types::BDKWalletTransaction> for WalletTransaction {
        fn from(tx: &crate::types::BDKWalletTransaction) -> Self {
            Self {
                txid: Some(ReverseHex::encode(&tx.txid)),
                raw_transaction: Some(ConsensusHex::encode(&tx.tx)),
                fee_sats: tx.fee.to_sat(),
                received_sats: tx.received.to_sat(),
                sent_sats: tx.sent.to_sat(),
                confirmation_info: Some((&tx.chain_position).into()),
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
        type Error = super::Error;

        fn try_from(fee_rate: send_transaction_request::FeeRate) -> Result<Self, Self::Error> {
            use send_transaction_request::FeeRate;
            let FeeRate { fee } = fee_rate;
            fee.ok_or_else(|| Self::Error::missing_field::<FeeRate>("fee"))?
                .try_into()
                .map_err(|err: InvalidSatsPerVbyte| {
                    Self::Error::invalid_field_value::<FeeRate, _>(
                        "fee",
                        &err.sats_per_vbyte.to_string(),
                        err,
                    )
                })
        }
    }

    #[derive(Copy, Clone, Debug)]
    pub struct HeaderSyncProgress {
        pub current_height: Option<u32>,
    }

    impl From<HeaderSyncProgress> for SubscribeHeaderSyncProgressResponse {
        fn from(progress: HeaderSyncProgress) -> Self {
            Self {
                current_height: progress.current_height,
            }
        }
    }
}

pub mod sidechain {
    tonic::include_proto!("cusf.sidechain.v1");
}
