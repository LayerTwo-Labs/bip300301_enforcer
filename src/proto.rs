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

pub mod sidechain {
    tonic::include_proto!("cusf.sidechain.v1");
    pub use sidechain_service_client::SidechainServiceClient as Client;
}

pub mod validator {
    tonic::include_proto!("cusf.validator.v1");

    use bip300301_messages::bitcoin::hashes::Hash as _;
    use subscribe_events_response::event::{ConnectBlock, DisconnectBlock};
    #[allow(unused_imports)]
    pub use validator_service_server::{
        self as server, ValidatorService as Service, ValidatorServiceServer as Server,
    };

    use crate::types::SidechainNumber;

    impl From<bip300301_messages::bitcoin::Network> for Network {
        fn from(network: bip300301_messages::bitcoin::Network) -> Self {
            match network {
                bip300301_messages::bitcoin::Network::Bitcoin => Network::Mainnet,
                bip300301_messages::bitcoin::Network::Regtest => Network::Regtest,
                bip300301_messages::bitcoin::Network::Signet => Network::Signet,
                bip300301_messages::bitcoin::Network::Testnet => Network::Testnet,
                _ => Network::Unknown,
            }
        }
    }

    impl TryFrom<get_coinbase_psbt_request::ProposeSidechain> for bip300301_messages::CoinbaseMessage {
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
            let data: Vec<u8> =
                data.ok_or_else(|| Self::Error::missing_field::<ProposeSidechain>("data"))?;
            Ok(bip300301_messages::CoinbaseMessage::M1ProposeSidechain {
                sidechain_number,
                data,
            })
        }
    }

    impl TryFrom<get_coinbase_psbt_request::AckSidechain> for bip300301_messages::CoinbaseMessage {
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
            let data_hash: [u8; 32] = {
                let data_hash = data_hash
                    .ok_or_else(|| Self::Error::missing_field::<AckSidechain>("data_hash"))?;
                data_hash.try_into().map_err(|data_hash| {
                    Self::Error::invalid_field_value::<AckSidechain>(
                        "data_hash",
                        &hex::encode(data_hash),
                    )
                })?
            };
            Ok(bip300301_messages::CoinbaseMessage::M2AckSidechain {
                sidechain_number,
                data_hash,
            })
        }
    }

    impl TryFrom<get_coinbase_psbt_request::ProposeBundle> for bip300301_messages::CoinbaseMessage {
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
            let bundle_txid: [u8; 32] = {
                let data_hash = bundle_txid
                    .ok_or_else(|| Self::Error::missing_field::<ProposeBundle>("bundle_txid"))?;
                data_hash.try_into().map_err(|data_hash| {
                    Self::Error::invalid_field_value::<ProposeBundle>(
                        "bundle_txid",
                        &hex::encode(data_hash),
                    )
                })?
            };
            Ok(bip300301_messages::CoinbaseMessage::M3ProposeBundle {
                sidechain_number,
                bundle_txid,
            })
        }
    }

    impl From<get_coinbase_psbt_request::ack_bundles::RepeatPrevious>
        for bip300301_messages::M4AckBundles
    {
        fn from(repeat_previous: get_coinbase_psbt_request::ack_bundles::RepeatPrevious) -> Self {
            let get_coinbase_psbt_request::ack_bundles::RepeatPrevious {} = repeat_previous;
            Self::RepeatPrevious
        }
    }

    impl From<get_coinbase_psbt_request::ack_bundles::LeadingBy50>
        for bip300301_messages::M4AckBundles
    {
        fn from(leading_by_50: get_coinbase_psbt_request::ack_bundles::LeadingBy50) -> Self {
            let get_coinbase_psbt_request::ack_bundles::LeadingBy50 {} = leading_by_50;
            Self::LeadingBy50
        }
    }

    impl TryFrom<get_coinbase_psbt_request::ack_bundles::Upvotes> for bip300301_messages::M4AckBundles {
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

    impl TryFrom<get_coinbase_psbt_request::ack_bundles::AckBundles>
        for bip300301_messages::M4AckBundles
    {
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

    impl TryFrom<get_coinbase_psbt_request::AckBundles> for bip300301_messages::M4AckBundles {
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

    impl TryFrom<get_coinbase_psbt_request::AckBundles> for bip300301_messages::CoinbaseMessage {
        type Error = super::Error;

        fn try_from(
            ack_bundles: get_coinbase_psbt_request::AckBundles,
        ) -> Result<Self, Self::Error> {
            ack_bundles.try_into().map(Self::M4AckBundles)
        }
    }

    impl From<crate::types::HeaderInfo> for BlockHeaderInfo {
        fn from(header_info: crate::types::HeaderInfo) -> Self {
            let block_hash = header_info.block_hash.to_byte_array().to_vec();
            let prev_block_hash = header_info.prev_block_hash.to_byte_array().to_vec();
            Self {
                block_hash,
                prev_block_hash,
                height: header_info.height,
                work: header_info.work.into(),
            }
        }
    }

    impl From<bip300301_messages::bitcoin::OutPoint> for OutPoint {
        fn from(outpoint: bip300301_messages::bitcoin::OutPoint) -> Self {
            Self {
                txid: outpoint.txid.to_raw_hash().as_byte_array().to_vec(),
                vout: outpoint.vout,
            }
        }
    }

    impl From<bip300301_messages::bitcoin::TxOut> for Output {
        fn from(output: bip300301_messages::bitcoin::TxOut) -> Self {
            Self {
                address: output.script_pubkey.into_bytes(),
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
                m6id: m6id.to_vec(),
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
            let bmm_commitment = self
                .bmm_commitments
                .get(&sidechain_number)
                .map(|commitment| commitment.to_vec());
            BlockInfo {
                deposits,
                withdrawal_bundle_events,
                bmm_commitment,
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
                        block_hash: block_hash.to_byte_array().to_vec(),
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
