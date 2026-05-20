use bitcoin::{
    hashes::{Hash, HashEngine, hmac, ripemd160, sha256, sha512},
    key::Secp256k1,
};
use connectrpc::{RequestContext, ServiceRequest, ServiceResult};

use crate::{
    proto::{
        common::{ConsensusHex, Hex},
        crypto::{
            HmacSha512Request, HmacSha512Response, Ripemd160Request, Ripemd160Response,
            Secp256k1SecretKeyToPublicKeyRequest, Secp256k1SecretKeyToPublicKeyResponse,
            Secp256k1SignRequest, Secp256k1SignResponse, Secp256k1VerifyRequest,
            Secp256k1VerifyResponse,
        },
        crypto_service::CryptoService,
    },
    server::{invalid_field_value, missing_field},
};

#[derive(Debug, Default)]
pub struct CryptoServiceServer;

const SECP256K1_SECRET_KEY_LENGTH: usize = 32;
const SECP256K1_PUBLIC_KEY_LENGTH: usize = 33;
const SECP256K1_SIGNATURE_COMPACT_SERIALIZATION_LENGTH: usize = 64;

#[expect(refining_impl_trait_reachable)]
impl CryptoService for CryptoServiceServer {
    async fn ripemd160(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, Ripemd160Request>,
    ) -> ServiceResult<Ripemd160Response> {
        let request = request.to_owned_message();
        let msg: Vec<u8> = request
            .msg
            .into_option()
            .ok_or_else(|| missing_field::<Ripemd160Request>("msg"))?
            .decode_status::<Ripemd160Request, _>("msg")?;
        let digest = ripemd160::Hash::hash(&msg);
        Ok(connectrpc::Response::new(Ripemd160Response {
            digest: buffa::MessageField::some(Hex::encode(&digest.as_byte_array())),
        }))
    }

    async fn hmac_sha512(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, HmacSha512Request>,
    ) -> ServiceResult<HmacSha512Response> {
        let request = request.to_owned_message();
        let key: Vec<u8> = request
            .key
            .into_option()
            .ok_or_else(|| missing_field::<HmacSha512Request>("key"))?
            .decode_status::<HmacSha512Request, _>("key")?;
        let msg: Vec<u8> = request
            .msg
            .into_option()
            .ok_or_else(|| missing_field::<HmacSha512Request>("msg"))?
            .decode_status::<HmacSha512Request, _>("msg")?;
        let mut engine = hmac::HmacEngine::<sha512::Hash>::new(&key);
        engine.input(&msg);
        let hmac = hmac::Hmac::<sha512::Hash>::from_engine(engine);
        Ok(connectrpc::Response::new(HmacSha512Response {
            hmac: buffa::MessageField::some(Hex::encode(&hmac.as_byte_array())),
        }))
    }

    async fn secp256k1_secret_key_to_public_key(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, Secp256k1SecretKeyToPublicKeyRequest>,
    ) -> ServiceResult<Secp256k1SecretKeyToPublicKeyResponse> {
        let request = request.to_owned_message();
        let secret_key: [u8; SECP256K1_SECRET_KEY_LENGTH] = request
            .secret_key
            .into_option()
            .ok_or_else(|| missing_field::<Secp256k1SecretKeyToPublicKeyRequest>("secret_key"))?
            .decode_status::<Secp256k1SecretKeyToPublicKeyRequest, _>("secret_key")?;
        let secret_key = bitcoin::secp256k1::SecretKey::from_slice(&secret_key).map_err(|err| {
            invalid_field_value::<Secp256k1SecretKeyToPublicKeyRequest, _>("secret_key", "", err)
        })?;
        let secp = Secp256k1::new();
        let public_key = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &secret_key);
        let public_key: [u8; SECP256K1_PUBLIC_KEY_LENGTH] = public_key.serialize();
        Ok(connectrpc::Response::new(
            Secp256k1SecretKeyToPublicKeyResponse {
                public_key: buffa::MessageField::some(ConsensusHex::encode(&public_key)),
            },
        ))
    }

    async fn secp256k1_sign(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, Secp256k1SignRequest>,
    ) -> ServiceResult<Secp256k1SignResponse> {
        let request = request.to_owned_message();
        let message: Vec<u8> = request
            .message
            .into_option()
            .ok_or_else(|| missing_field::<Secp256k1SignRequest>("message"))?
            .decode_status::<Secp256k1SignRequest, _>("message")?;
        let digest = sha256::Hash::hash(&message).to_byte_array();
        let message = bitcoin::secp256k1::Message::from_digest(digest);
        let secret_key: [u8; SECP256K1_SECRET_KEY_LENGTH] = request
            .secret_key
            .into_option()
            .ok_or_else(|| missing_field::<Secp256k1SignRequest>("secret_key"))?
            .decode_status::<Secp256k1SignRequest, _>("secret_key")?;
        let secret_key = bitcoin::secp256k1::SecretKey::from_slice(&secret_key)
            .map_err(|err| invalid_field_value::<Secp256k1SignRequest, _>("secret_key", "", err))?;
        let secp = Secp256k1::new();
        let signature = secp.sign_ecdsa(&message, &secret_key);
        let signature = signature.serialize_compact();
        Ok(connectrpc::Response::new(Secp256k1SignResponse {
            signature: buffa::MessageField::some(Hex::encode(&signature)),
        }))
    }

    async fn secp256k1_verify(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, Secp256k1VerifyRequest>,
    ) -> ServiceResult<Secp256k1VerifyResponse> {
        let request = request.to_owned_message();
        let message: Vec<u8> = request
            .message
            .into_option()
            .ok_or_else(|| missing_field::<Secp256k1VerifyRequest>("message"))?
            .decode_status::<Secp256k1VerifyRequest, _>("message")?;
        let digest = sha256::Hash::hash(&message).to_byte_array();
        let message = bitcoin::secp256k1::Message::from_digest(digest);
        let signature: Vec<u8> = request
            .signature
            .into_option()
            .ok_or_else(|| missing_field::<Secp256k1VerifyRequest>("signature"))?
            .decode_status::<Secp256k1VerifyRequest, _>("signature")?;
        let signature_len = signature.len();
        let signature: [u8; SECP256K1_SIGNATURE_COMPACT_SERIALIZATION_LENGTH] =
            signature.try_into().map_err(|_| {
                invalid_field_value::<Secp256k1VerifyRequest, _>(
                    "signature",
                    &signature_len.to_string(),
                    std::io::Error::other(format!(
                        "invalid signature length, must be {SECP256K1_SIGNATURE_COMPACT_SERIALIZATION_LENGTH}"
                    )),
                )
            })?;
        let signature =
            bitcoin::secp256k1::ecdsa::Signature::from_compact(&signature).map_err(|err| {
                invalid_field_value::<Secp256k1VerifyRequest, _>("signature", "", err)
            })?;
        let public_key: [u8; SECP256K1_PUBLIC_KEY_LENGTH] = request
            .public_key
            .into_option()
            .ok_or_else(|| missing_field::<Secp256k1VerifyRequest>("public_key"))?
            .decode_status::<Secp256k1VerifyRequest, _>("public_key")?;
        let public_key = bitcoin::secp256k1::PublicKey::from_slice(&public_key).map_err(|err| {
            invalid_field_value::<Secp256k1VerifyRequest, _>("public_key", "", err)
        })?;
        let secp = Secp256k1::new();
        let valid = secp.verify_ecdsa(&message, &signature, &public_key).is_ok();
        Ok(connectrpc::Response::new(Secp256k1VerifyResponse { valid }))
    }
}
