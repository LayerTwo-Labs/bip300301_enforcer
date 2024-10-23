use thiserror::Error;

/// Encoded file descriptor set, for gRPC reflection
pub static ENCODED_FILE_DESCRIPTOR_SET: &[u8] =
    tonic::include_file_descriptor_set!("file_descriptor_set");

#[derive(Debug, Error)]
pub enum Error {
    #[error("Invalid enum variant in field `{field_name}` of message `{message_name}`: `{variant_name}`")]
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

    pub fn invalid_field_value<Message>(field_name: &str, value: &str) -> Self
    where
        Message: prost::Name,
    {
        Self::InvalidFieldValue {
            field_name: field_name.to_owned(),
            message_name: Message::full_name(),
            value: value.to_owned(),
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

pub mod mainchain {
    tonic::include_proto!("cusf.mainchain.v1");

    use subscribe_events_response::event::{ConnectBlock, DisconnectBlock};
    #[allow(unused_imports)]
    pub use validator_service_server::{
        self as server, ValidatorService as Service, ValidatorServiceServer as Server,
    };

    use crate::{
        messages::{CoinbaseMessage, M4AckBundles},
        types::SidechainNumber,
    };

    use super::Error;

    impl ConsensusHex {
        pub fn decode<Message, T>(self, field_name: &str) -> Result<T, super::Error>
        where
            Message: prost::Name,
            T: bitcoin::consensus::Decodable,
        {
            let Self { hex } = self;
            let hex = hex.ok_or_else(|| super::Error::missing_field::<Self>("hex"))?;
            bitcoin::consensus::encode::deserialize_hex(&hex)
                .map_err(|_err| super::Error::invalid_field_value::<Message>(field_name, &hex))
        }

        pub fn decode_hex<Message, T>(self, field_name: &str) -> Result<T, super::Error>
        where
            Message: prost::Name,
            T: hex::FromHex,
        {
            let Self { hex } = self;
            let hex = hex.ok_or_else(|| super::Error::missing_field::<Self>("hex"))?;
            T::from_hex(&hex)
                .map_err(|_err| super::Error::invalid_field_value::<Message>(field_name, &hex))
        }

        /// Variant of [`Self::decode`] that returns a `tonic::Status` error
        pub fn decode_tonic<Message, T>(self, field_name: &str) -> Result<T, tonic::Status>
        where
            Message: prost::Name,
            T: bitcoin::consensus::Decodable,
        {
            self.decode::<Message, _>(field_name)
                .map_err(|err| tonic::Status::from_error(Box::new(err)))
        }

        pub fn encode<T>(value: &T) -> Self
        where
            T: bitcoin::consensus::Encodable,
        {
            let hex = bitcoin::consensus::encode::serialize_hex(value);
            Self { hex: Some(hex) }
        }

        pub fn encode_hex<T>(value: &T) -> Self
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
            let mut bytes = hex::decode(&hex)
                .map_err(|_| super::Error::invalid_field_value::<Message>(field_name, &hex))?;
            bytes.reverse();
            bitcoin::consensus::deserialize(&bytes)
                .map_err(|_err| super::Error::invalid_field_value::<Message>(field_name, &hex))
        }

        /// Variant of [`Self::decode`] that returns a `tonic::Status` error
        pub fn decode_tonic<Message, T>(self, field_name: &str) -> Result<T, tonic::Status>
        where
            Message: prost::Name,
            T: bitcoin::consensus::Decodable,
        {
            self.decode::<Message, _>(field_name)
                .map_err(|err| tonic::Status::from_error(Box::new(err)))
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

    impl From<crate::types::SidechainDeclaration> for SidechainDeclarationV0 {
        fn from(declaration: crate::types::SidechainDeclaration) -> Self {
            Self {
                title: Some(declaration.title),
                description: Some(declaration.description),
                hash_id_1: Some(ConsensusHex::encode(&declaration.hash_id_1)),
                hash_id_2: Some(ConsensusHex::encode_hex(&declaration.hash_id_2)),
            }
        }
    }

    impl TryFrom<SidechainDeclarationV0> for crate::types::SidechainDeclaration {
        type Error = super::Error;

        fn try_from(declaration: SidechainDeclarationV0) -> Result<Self, Self::Error> {
            let SidechainDeclarationV0 {
                title,
                description,
                hash_id_1,
                hash_id_2,
            } = declaration;
            let title =
                title.ok_or_else(|| Error::missing_field::<SidechainDeclarationV0>("title"))?;
            let description = description
                .ok_or_else(|| Error::missing_field::<SidechainDeclarationV0>("description"))?;
            let hash_id_1 = hash_id_1
                .ok_or_else(|| Error::missing_field::<SidechainDeclarationV0>("hash_id_1"))?
                .decode::<SidechainDeclarationV0, _>("hash_id_1")?;
            let hash_id_2 = hash_id_2
                .ok_or_else(|| Error::missing_field::<SidechainDeclarationV0>("hash_id_2"))?
                .decode_hex::<SidechainDeclarationV0, _>("hash_id_2")?;
            Ok(Self {
                title,
                description,
                hash_id_1,
                hash_id_2,
            })
        }
    }

    impl From<SidechainDeclarationV0> for SidechainDeclaration {
        fn from(declaration: SidechainDeclarationV0) -> Self {
            Self {
                version: Some(sidechain_declaration::Version::V0(declaration)),
            }
        }
    }

    impl From<crate::types::SidechainDeclaration> for SidechainDeclaration {
        fn from(declaration: crate::types::SidechainDeclaration) -> Self {
            SidechainDeclarationV0::from(declaration).into()
        }
    }

    impl TryFrom<SidechainDeclaration> for crate::types::SidechainDeclaration {
        type Error = super::Error;

        fn try_from(declaration: SidechainDeclaration) -> Result<Self, Self::Error> {
            let SidechainDeclaration { version } = declaration;
            let version =
                version.ok_or_else(|| Error::missing_field::<SidechainDeclaration>("version"))?;
            match version {
                sidechain_declaration::Version::V0(v0) => v0.try_into(),
            }
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

    impl TryFrom<get_coinbase_psbt_request::ProposeSidechain> for CoinbaseMessage {
        type Error = super::Error;

        fn try_from(
            propose_sidechain: get_coinbase_psbt_request::ProposeSidechain,
        ) -> Result<Self, Self::Error> {
            use get_coinbase_psbt_request::ProposeSidechain;
            let ProposeSidechain {
                sidechain_number,
                data,
            } = propose_sidechain;
            let sidechain_number: u8 = {
                let sidechain_number: u32 = sidechain_number.ok_or_else(|| {
                    Self::Error::missing_field::<ProposeSidechain>("sidechain_number")
                })?;
                sidechain_number.try_into().map_err(|_| {
                    Self::Error::invalid_field_value::<ProposeSidechain>(
                        "sidechain_number",
                        &sidechain_number.to_string(),
                    )
                })?
            };
            let data: Vec<u8> = data
                .ok_or_else(|| Self::Error::missing_field::<ProposeSidechain>("data"))?
                .decode::<ProposeSidechain, _>("data")?;
            Ok(CoinbaseMessage::M1ProposeSidechain {
                sidechain_number,
                data,
            })
        }
    }

    impl TryFrom<get_coinbase_psbt_request::AckSidechain> for CoinbaseMessage {
        type Error = super::Error;

        fn try_from(
            ack_sidechain: get_coinbase_psbt_request::AckSidechain,
        ) -> Result<Self, Self::Error> {
            use get_coinbase_psbt_request::AckSidechain;
            let AckSidechain {
                sidechain_number,
                data_hash,
            } = ack_sidechain;
            let sidechain_number: u8 = {
                let sidechain_number: u32 = sidechain_number.ok_or_else(|| {
                    Self::Error::missing_field::<AckSidechain>("sidechain_number")
                })?;
                sidechain_number.try_into().map_err(|_| {
                    Self::Error::invalid_field_value::<AckSidechain>(
                        "sidechain_number",
                        &sidechain_number.to_string(),
                    )
                })?
            };
            let data_hash: [u8; 32] = data_hash
                .ok_or_else(|| Self::Error::missing_field::<AckSidechain>("data_hash"))?
                .decode::<AckSidechain, _>("data_hash")?;
            Ok(CoinbaseMessage::M2AckSidechain {
                sidechain_number,
                data_hash,
            })
        }
    }

    impl TryFrom<get_coinbase_psbt_request::ProposeBundle> for CoinbaseMessage {
        type Error = super::Error;

        fn try_from(
            propose_bundle: get_coinbase_psbt_request::ProposeBundle,
        ) -> Result<Self, Self::Error> {
            use get_coinbase_psbt_request::ProposeBundle;
            let ProposeBundle {
                sidechain_number,
                bundle_txid,
            } = propose_bundle;
            let sidechain_number: u8 = {
                let sidechain_number: u32 = sidechain_number.ok_or_else(|| {
                    Self::Error::missing_field::<ProposeBundle>("sidechain_number")
                })?;
                sidechain_number.try_into().map_err(|_| {
                    Self::Error::invalid_field_value::<ProposeBundle>(
                        "sidechain_number",
                        &sidechain_number.to_string(),
                    )
                })?
            };
            let bundle_txid: [u8; 32] = bundle_txid
                .ok_or_else(|| Self::Error::missing_field::<ProposeBundle>("bundle_txid"))?
                .decode::<ProposeBundle, _>("bundle_txid")?;
            Ok(CoinbaseMessage::M3ProposeBundle {
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
            }
        }
    }

    impl From<bitcoin::OutPoint> for OutPoint {
        fn from(outpoint: bitcoin::OutPoint) -> Self {
            Self {
                txid: Some(ReverseHex::encode(&outpoint.txid)),
                vout: outpoint.vout,
            }
        }
    }

    impl From<bitcoin::TxOut> for Output {
        fn from(output: bitcoin::TxOut) -> Self {
            Self {
                address: Some(ConsensusHex::encode(&output.script_pubkey)),
                value_sats: output.value.to_sat(),
            }
        }
    }

    impl From<crate::types::Deposit> for (SidechainNumber, Deposit) {
        fn from(deposit: crate::types::Deposit) -> Self {
            let crate::types::Deposit {
                sidechain_id,
                sequence_number,
                outpoint,
                output,
            } = deposit;
            let deposit = Deposit {
                sequence_number,
                outpoint: Some(outpoint.into()),
                output: Some(output.into()),
            };
            (sidechain_id, deposit)
        }
    }

    impl From<crate::types::WithdrawalBundleEventKind> for WithdrawalBundleEventType {
        fn from(kind: crate::types::WithdrawalBundleEventKind) -> Self {
            match kind {
                crate::types::WithdrawalBundleEventKind::Failed => {
                    WithdrawalBundleEventType::Failed
                }
                crate::types::WithdrawalBundleEventKind::Submitted => {
                    WithdrawalBundleEventType::Submitted
                }
                crate::types::WithdrawalBundleEventKind::Succeeded => {
                    WithdrawalBundleEventType::Succeded
                }
            }
        }
    }

    impl From<crate::types::WithdrawalBundleEvent> for (SidechainNumber, WithdrawalBundleEvent) {
        fn from(event: crate::types::WithdrawalBundleEvent) -> Self {
            let crate::types::WithdrawalBundleEvent {
                sidechain_id,
                m6id,
                kind,
            } = event;
            let withdrawal_bundle_event_type = WithdrawalBundleEventType::from(kind) as i32;
            let event = WithdrawalBundleEvent {
                m6id: Some(ConsensusHex::encode(&m6id)),
                withdrawal_bundle_event_type,
            };
            (sidechain_id, event)
        }
    }

    impl crate::types::BlockInfo {
        pub fn into_proto(self, sidechain_number: SidechainNumber) -> BlockInfo {
            let deposits = self
                .deposits
                .into_iter()
                .filter_map(|deposit| {
                    let (deposit_sidechain_number, deposit) = deposit.into();
                    if deposit_sidechain_number == sidechain_number {
                        Some(deposit)
                    } else {
                        None
                    }
                })
                .collect();
            let withdrawal_bundle_events = self
                .withdrawal_bundle_events
                .into_iter()
                .filter_map(|event| {
                    let (event_sidechain_number, event) = event.into();
                    if event_sidechain_number == sidechain_number {
                        Some(event)
                    } else {
                        None
                    }
                })
                .collect();
            let bmm_commitment = self.bmm_commitments.get(&sidechain_number);
            BlockInfo {
                deposits,
                withdrawal_bundle_events,
                bmm_commitment: bmm_commitment.map(ConsensusHex::encode),
            }
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
            let block_info = block_info.into_proto(sidechain_number);
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
                        block_info: Some(block_info.into_proto(sidechain_number)),
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
}

pub mod sidechain {
    tonic::include_proto!("cusf.sidechain.v1");
}
