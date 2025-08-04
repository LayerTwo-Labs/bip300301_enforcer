use bitcoin::{
    hashes::{Hash, HashEngine, hmac, ripemd160, sha256, sha512},
    key::Secp256k1,
};

use crate::{
    proto::{
        common::{ConsensusHex, Hex},
        crypto::{
            HmacSha512Request, HmacSha512Response, Ripemd160Request, Ripemd160Response,
            Secp256k1SecretKeyToPublicKeyRequest, Secp256k1SecretKeyToPublicKeyResponse,
            Secp256k1SignRequest, Secp256k1SignResponse, Secp256k1VerifyRequest,
            Secp256k1VerifyResponse, crypto_service_server::CryptoService,
        },
    },
    server::{invalid_field_value, missing_field},
};

#[derive(Debug, Default)]
pub struct CryptoServiceServer;

const SECP256K1_SECRET_KEY_LENGTH: usize = 32;
const SECP256K1_PUBLIC_KEY_LENGTH: usize = 33;
const SECP256K1_SIGNATURE_COMPACT_SERIALIZATION_LENGTH: usize = 64;

#[tonic::async_trait]
impl CryptoService for CryptoServiceServer {
    async fn ripemd160(
        &self,
        request: tonic::Request<Ripemd160Request>,
    ) -> std::result::Result<tonic::Response<Ripemd160Response>, tonic::Status> {
        let Ripemd160Request { msg } = request.into_inner();
        let msg: Vec<u8> = msg
            .ok_or_else(|| missing_field::<Ripemd160Request>("msg"))?
            .decode_tonic::<Ripemd160Request, _>("msg")?;
        let digest = ripemd160::Hash::hash(&msg);
        let response = Ripemd160Response {
            digest: Some(Hex::encode(&digest.as_byte_array())),
        };
        Ok(tonic::Response::new(response))
    }

    async fn hmac_sha512(
        &self,
        request: tonic::Request<HmacSha512Request>,
    ) -> std::result::Result<tonic::Response<HmacSha512Response>, tonic::Status> {
        let HmacSha512Request { key, msg } = request.into_inner();
        let key: Vec<u8> = key
            .ok_or_else(|| missing_field::<HmacSha512Request>("key"))?
            .decode_tonic::<HmacSha512Request, _>("key")?;
        let msg: Vec<u8> = msg
            .ok_or_else(|| missing_field::<HmacSha512Request>("msg"))?
            .decode_tonic::<HmacSha512Request, _>("msg")?;
        let mut engine = hmac::HmacEngine::<sha512::Hash>::new(&key);
        engine.input(&msg);
        let hmac = hmac::Hmac::<sha512::Hash>::from_engine(engine);
        let response = HmacSha512Response {
            hmac: Some(Hex::encode(&hmac.as_byte_array())),
        };
        Ok(tonic::Response::new(response))
    }

    async fn secp256k1_secret_key_to_public_key(
        &self,
        request: tonic::Request<Secp256k1SecretKeyToPublicKeyRequest>,
    ) -> std::result::Result<tonic::Response<Secp256k1SecretKeyToPublicKeyResponse>, tonic::Status>
    {
        let Secp256k1SecretKeyToPublicKeyRequest { secret_key } = request.into_inner();
        let secret_key: [u8; SECP256K1_SECRET_KEY_LENGTH] = secret_key
            .ok_or_else(|| missing_field::<Secp256k1SecretKeyToPublicKeyRequest>("secret_key"))?
            .decode_tonic::<Secp256k1SecretKeyToPublicKeyRequest, _>("secret_key")?;
        let secret_key = bitcoin::secp256k1::SecretKey::from_slice(&secret_key).map_err(|err| {
            invalid_field_value::<Secp256k1SecretKeyToPublicKeyRequest, _>("secret_key", "", err)
        })?;
        let secp = Secp256k1::new();
        let public_key = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &secret_key);
        let public_key: [u8; SECP256K1_PUBLIC_KEY_LENGTH] = public_key.serialize();
        let response = Secp256k1SecretKeyToPublicKeyResponse {
            public_key: Some(ConsensusHex::encode(&public_key)),
        };
        Ok(tonic::Response::new(response))
    }

    async fn secp256k1_sign(
        &self,
        request: tonic::Request<Secp256k1SignRequest>,
    ) -> std::result::Result<tonic::Response<Secp256k1SignResponse>, tonic::Status> {
        let Secp256k1SignRequest {
            message,
            secret_key,
        } = request.into_inner();
        let message: Vec<u8> = message
            .ok_or_else(|| missing_field::<Secp256k1SignRequest>("message"))?
            .decode_tonic::<Secp256k1SignRequest, _>("message")?;
        let digest = sha256::Hash::hash(&message).to_byte_array();
        let message = bitcoin::secp256k1::Message::from_digest(digest);
        let secret_key: [u8; SECP256K1_SECRET_KEY_LENGTH] = secret_key
            .ok_or_else(|| missing_field::<Secp256k1SignRequest>("secret_key"))?
            .decode_tonic::<Secp256k1SignRequest, _>("secret_key")?;
        let secret_key = bitcoin::secp256k1::SecretKey::from_slice(&secret_key)
            .map_err(|err| invalid_field_value::<Secp256k1SignRequest, _>("secret_key", "", err))?;
        let secp = Secp256k1::new();
        let signature = secp.sign_ecdsa(&message, &secret_key);
        let signature = signature.serialize_compact();
        let response = Secp256k1SignResponse {
            signature: Some(Hex::encode(&signature)),
        };
        Ok(tonic::Response::new(response))
    }

    async fn secp256k1_verify(
        &self,
        request: tonic::Request<Secp256k1VerifyRequest>,
    ) -> std::result::Result<tonic::Response<Secp256k1VerifyResponse>, tonic::Status> {
        let Secp256k1VerifyRequest {
            message,
            signature,
            public_key,
        } = request.into_inner();
        let message: Vec<u8> = message
            .ok_or_else(|| missing_field::<Secp256k1VerifyRequest>("message"))?
            .decode_tonic::<Secp256k1VerifyRequest, _>("message")?;
        let digest = sha256::Hash::hash(&message).to_byte_array();
        let message = bitcoin::secp256k1::Message::from_digest(digest);
        let signature: Vec<u8> = signature
            .ok_or_else(|| missing_field::<Secp256k1VerifyRequest>("signature"))?
            .decode_tonic::<Secp256k1VerifyRequest, _>("signature")?;
        let signature_len = signature.len();
        let signature: [u8; SECP256K1_SIGNATURE_COMPACT_SERIALIZATION_LENGTH] =
            signature.try_into().map_err(|_err| {
                tonic::Status::new(
                    tonic::Code::InvalidArgument,
                    format!("invalid signature length {signature_len}, must be {SECP256K1_SIGNATURE_COMPACT_SERIALIZATION_LENGTH}"),
                )
            })?;
        let signature =
            bitcoin::secp256k1::ecdsa::Signature::from_compact(&signature).map_err(|err| {
                invalid_field_value::<Secp256k1VerifyRequest, _>(
                    "signature",
                    &hex::encode(signature),
                    err,
                )
            })?;
        let public_key: [u8; SECP256K1_PUBLIC_KEY_LENGTH] = public_key
            .ok_or_else(|| missing_field::<Secp256k1VerifyRequest>("public_key"))?
            .decode_tonic::<Secp256k1VerifyRequest, _>("public_key")?;
        let public_key = bitcoin::secp256k1::PublicKey::from_slice(&public_key).map_err(|err| {
            invalid_field_value::<Secp256k1VerifyRequest, _>(
                "public_key",
                &hex::encode(public_key),
                err,
            )
        })?;
        let secp = Secp256k1::new();
        let valid = secp.verify_ecdsa(&message, &signature, &public_key).is_ok();
        let response = Secp256k1VerifyResponse { valid };
        Ok(tonic::Response::new(response))
    }
}
