#[path = "../generated/cusf.common.v1.rs"]
mod generated;

pub mod v1 {
    use std::error::Error as _;

    pub use super::generated::*;

    impl ConsensusHex {
        pub fn decode<Message, T>(self, field_name: &str) -> Result<T, crate::proto::Error>
        where
            Message: prost::Name,
            T: bitcoin::consensus::Decodable,
        {
            let Self { hex } = self;
            let hex = hex.ok_or_else(|| crate::proto::Error::missing_field::<Self>("hex"))?;
            bitcoin::consensus::encode::deserialize_hex(&hex).map_err(|err| {
                crate::proto::Error::invalid_field_value::<Message, _>(field_name, &hex, err)
            })
        }

        /// Variant of [`Self::decode`] that returns a `tonic::Status` error
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
        pub fn decode<Message, T>(self, field_name: &str) -> Result<T, crate::proto::Error>
        where
            Message: prost::Name,
            T: hex::FromHex,
            <T as hex::FromHex>::Error: std::error::Error + Send + Sync + 'static,
        {
            let Self { hex } = self;
            let hex = hex.ok_or_else(|| crate::proto::Error::missing_field::<Self>("hex"))?;
            T::from_hex(&hex).map_err(|err| {
                crate::proto::Error::invalid_field_value::<Message, _>(field_name, &hex, err)
            })
        }

        /// Variant of [`Self::decode`] that returns a `tonic::Status` error
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
        pub fn decode<Message, T>(self, field_name: &str) -> Result<T, crate::proto::Error>
        where
            Message: prost::Name,
            T: bitcoin::consensus::Decodable,
        {
            let Self { hex } = self;
            let hex = hex.ok_or_else(|| crate::proto::Error::missing_field::<Self>("hex"))?;
            let mut bytes = hex::decode(&hex).map_err(|err| {
                crate::proto::Error::invalid_field_value::<Message, _>(field_name, &hex, err)
            })?;
            bytes.reverse();
            bitcoin::consensus::deserialize(&bytes).map_err(|err| {
                crate::proto::Error::invalid_field_value::<Message, _>(field_name, &hex, err)
            })
        }

        /// Variant of [`Self::decode`] that returns a `tonic::Status` error
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
