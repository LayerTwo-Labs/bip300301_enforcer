use std::{fmt, str::FromStr};

use bitcoin::{
    Network, OutPoint,
    address::Address,
    base58,
    bip32::{ChainCode, ChildNumber, Xpriv, Xpub},
    hashes::{Hash, HashEngine, Hmac, HmacEngine, sha256, sha512},
    secp256k1::{PublicKey, Scalar, Secp256k1, SecretKey, Verification, ecdh},
};

use super::util;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Version {
    V1 = 1,
    V3 = 3,
}

const V1_BASE58_VERSION_BYTE: u8 = 0x47;
const V1_PAYLOAD_LEN: usize = 80;

const V3_MAGIC: u8 = 0x22;
const V3_VERSION_BYTE: u8 = 0x03;
const V3_PAYLOAD_LEN: usize = 35;
const V3_BLINDED_LEN: usize = 33;

const PUBKEY_LEN: usize = 33;
const CHAIN_CODE_LEN: usize = 32;

const V3_COIN_BYTES_MAINNET: [u8; 4] = [0, 0, 0, 0];
const V3_COIN_BYTES_TESTNET: [u8; 4] = [0, 0, 0, 1];

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("base58 decode: {0}")]
    Base58(String),
    #[error(
        "payment code length {got}, expected {} (v1) or {} (v3)",
        V1_PAYLOAD_LEN,
        V3_PAYLOAD_LEN
    )]
    BadLength { got: usize },
    #[error(
        "v1 base58 version byte 0x{got:02x}, want 0x{:02x}",
        V1_BASE58_VERSION_BYTE
    )]
    V1BadVersionByte { got: u8 },
    #[error(
        "v3 payload prefix [0x{got_magic:02x}, 0x{got_version:02x}], want [0x{:02x}, 0x{:02x}]",
        V3_MAGIC,
        V3_VERSION_BYTE
    )]
    V3BadMagic { got_magic: u8, got_version: u8 },
    #[error("v1 internal version byte 0x{got:02x}, only 0x01 supported")]
    V1BadInternalVersion { got: u8 },
    #[error("invalid sign byte 0x{got:02x}, expected 0x02 or 0x03")]
    BadSignByte { got: u8 },
    #[error("invalid secp256k1 pubkey: {0}")]
    BadPubkey(String),
}

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("ECDH yielded an invalid scalar (zero or out of range)")]
    InvalidScalar,
    #[error("tweak resulted in the point at infinity")]
    PointAtInfinity,
    #[error("secp256k1: {0}")]
    Secp256k1(#[from] bitcoin::secp256k1::Error),
    #[error("BIP32 derivation: {0}")]
    Bip32(#[from] bitcoin::bip32::Error),
    #[error("network not supported for BIP47 (only Bitcoin/Testnet/Signet/Regtest)")]
    UnsupportedNetwork,
    #[error("BIP32 non-hardened child index out of range: {i} >= 2^31")]
    IndexTooLarge { i: u32 },
}

impl From<util::TweakError> for CryptoError {
    fn from(e: util::TweakError) -> Self {
        match e {
            util::TweakError::InvalidScalar => Self::InvalidScalar,
            util::TweakError::PointAtInfinity => Self::PointAtInfinity,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentCode {
    version: Version,
    pubkey: PublicKey,
    chain_code: ChainCode,
    features: u8,
    reserved: [u8; 13],
}

impl PaymentCode {
    pub fn version(&self) -> Version {
        self.version
    }

    pub fn pubkey(&self) -> &PublicKey {
        &self.pubkey
    }

    pub fn chain_code(&self) -> &ChainCode {
        &self.chain_code
    }

    pub fn from_xpub(xpub: &Xpub, version: Version) -> Self {
        let pubkey = xpub.public_key;
        let chain_code = match version {
            Version::V1 => xpub.chain_code,
            Version::V3 => v3_chain_code(&pubkey),
        };
        Self {
            version,
            pubkey,
            chain_code,
            features: 0,
            reserved: [0; 13],
        }
    }

    pub fn notification_pubkey<C: Verification>(
        &self,
        secp: &Secp256k1<C>,
    ) -> Result<PublicKey, CryptoError> {
        self.derive_pubkey(secp, 0)
    }

    pub fn derive_pubkey<C: Verification>(
        &self,
        secp: &Secp256k1<C>,
        i: u32,
    ) -> Result<PublicKey, CryptoError> {
        let master = self.as_xpub();
        let child_number =
            ChildNumber::from_normal_idx(i).map_err(|_| CryptoError::IndexTooLarge { i })?;
        let child = master.ckd_pub(secp, child_number)?;
        Ok(child.public_key)
    }

    fn as_xpub(&self) -> Xpub {
        Xpub {
            network: bitcoin::NetworkKind::Main,
            depth: 3,
            parent_fingerprint: Default::default(),
            child_number: ChildNumber::from_hardened_idx(0).expect("valid hardened index"),
            public_key: self.pubkey,
            chain_code: self.chain_code,
        }
    }

    pub fn identifier(&self) -> [u8; 33] {
        let version_byte: u8 = match self.version {
            Version::V1 => 0x01,
            Version::V3 => 0x03,
        };
        let mut engine: HmacEngine<sha512::Hash> = HmacEngine::new(self.chain_code.as_bytes());
        engine.input(&[version_byte]);
        let mac: Hmac<sha512::Hash> = Hmac::from_engine(engine);
        let mut out = [0u8; 33];
        out[0] = 0x02;
        out[1..].copy_from_slice(&mac[..32]);
        out
    }

    pub fn serialize_payload(&self) -> Vec<u8> {
        match self.version {
            Version::V1 => {
                let mut out = vec![0u8; V1_PAYLOAD_LEN];
                let compressed = self.pubkey.serialize();
                out[0] = 0x01;
                out[1] = self.features;
                out[2] = compressed[0];
                out[3..3 + 32].copy_from_slice(&compressed[1..]);
                out[35..35 + CHAIN_CODE_LEN].copy_from_slice(self.chain_code.as_bytes());
                out[67..67 + 13].copy_from_slice(&self.reserved);
                out
            }
            Version::V3 => {
                let mut out = vec![0u8; V3_PAYLOAD_LEN];
                out[0] = V3_MAGIC;
                out[1] = V3_VERSION_BYTE;
                out[2..2 + PUBKEY_LEN].copy_from_slice(&self.pubkey.serialize());
                out
            }
        }
    }
}

impl fmt::Display for PaymentCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.version {
            Version::V1 => {
                let payload = self.serialize_payload();
                let mut buf = Vec::with_capacity(1 + payload.len());
                buf.push(V1_BASE58_VERSION_BYTE);
                buf.extend_from_slice(&payload);
                f.write_str(&base58::encode_check(&buf))
            }
            Version::V3 => {
                let payload = self.serialize_payload();
                f.write_str(&base58::encode_check(&payload))
            }
        }
    }
}

impl FromStr for PaymentCode {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let raw = base58::decode_check(s).map_err(|e| ParseError::Base58(e.to_string()))?;
        if raw.len() == V3_PAYLOAD_LEN {
            if raw[0] != V3_MAGIC || raw[1] != V3_VERSION_BYTE {
                return Err(ParseError::V3BadMagic {
                    got_magic: raw[0],
                    got_version: raw[1],
                });
            }
            let pubkey = PublicKey::from_slice(&raw[2..2 + PUBKEY_LEN])
                .map_err(|e| ParseError::BadPubkey(e.to_string()))?;
            let chain_code = v3_chain_code(&pubkey);
            return Ok(Self {
                version: Version::V3,
                pubkey,
                chain_code,
                features: 0,
                reserved: [0; 13],
            });
        }

        if raw.is_empty() {
            return Err(ParseError::BadLength { got: 0 });
        }
        let (version_byte, payload) = (raw[0], &raw[1..]);
        if version_byte != V1_BASE58_VERSION_BYTE {
            return Err(ParseError::V1BadVersionByte { got: version_byte });
        }
        if payload.len() != V1_PAYLOAD_LEN {
            return Err(ParseError::BadLength { got: payload.len() });
        }
        payment_code_from_v1_payload(payload)
    }
}

fn v3_chain_code(pubkey: &PublicKey) -> ChainCode {
    let pk = pubkey.serialize();
    let outer: sha256::Hash = sha256::Hash::hash(pk.as_ref());
    let inner: sha256::Hash = sha256::Hash::hash(outer.as_byte_array());
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(inner.as_byte_array());
    ChainCode::from(bytes)
}

pub fn blind(
    sender_code: &PaymentCode,
    sender_priv: &SecretKey,
    recipient_notif_pub: &PublicKey,
    sender_outpoint: OutPoint,
    secp: &Secp256k1<bitcoin::secp256k1::All>,
) -> Vec<u8> {
    let secret_x = ecdh_x(sender_priv, recipient_notif_pub);
    match sender_code.version {
        Version::V1 => {
            let payload = sender_code.serialize_payload();
            debug_assert_eq!(payload.len(), V1_PAYLOAD_LEN);
            let mask = v1_blinding_mask(secret_x, sender_outpoint);
            let mut out = payload;
            for i in 0..32 {
                out[3 + i] ^= mask[i];
            }
            for i in 0..32 {
                out[35 + i] ^= mask[32 + i];
            }
            out
        }
        Version::V3 => {
            let sender_pub = sender_priv.public_key(secp);
            let mask = v3_blinding_mask(secret_x, &sender_pub);
            let sender_compressed = sender_code.pubkey.serialize();
            let mut out = vec![0u8; V3_BLINDED_LEN];
            out[0] = sender_compressed[0];
            for i in 0..32 {
                out[1 + i] = sender_compressed[1 + i] ^ mask[i];
            }
            out
        }
    }
}

pub fn unblind(
    receiver_priv: &SecretKey,
    blob: &[u8],
    sender_pub: &PublicKey,
    sender_outpoint: OutPoint,
    version: Version,
) -> Result<PaymentCode, ParseError> {
    let secret_x = ecdh_x(receiver_priv, sender_pub);
    match version {
        Version::V1 => {
            if blob.len() != V1_PAYLOAD_LEN {
                return Err(ParseError::BadLength { got: blob.len() });
            }
            let mask = v1_blinding_mask(secret_x, sender_outpoint);
            let mut payload = blob.to_vec();
            for i in 0..32 {
                payload[3 + i] ^= mask[i];
            }
            for i in 0..32 {
                payload[35 + i] ^= mask[32 + i];
            }
            payment_code_from_v1_payload(&payload)
        }
        Version::V3 => {
            if blob.len() != V3_BLINDED_LEN {
                return Err(ParseError::BadLength { got: blob.len() });
            }
            let mask = v3_blinding_mask(secret_x, sender_pub);
            let mut compressed = [0u8; PUBKEY_LEN];
            compressed[0] = blob[0];
            for i in 0..32 {
                compressed[1 + i] = blob[1 + i] ^ mask[i];
            }
            let pubkey = PublicKey::from_slice(&compressed)
                .map_err(|e| ParseError::BadPubkey(e.to_string()))?;
            let chain_code = v3_chain_code(&pubkey);
            Ok(PaymentCode {
                version: Version::V3,
                pubkey,
                chain_code,
                features: 0,
                reserved: [0; 13],
            })
        }
    }
}

// v1: HMAC key = outpoint, data = ecdh_X.
fn v1_blinding_mask(ecdh_x: [u8; 32], outpoint: OutPoint) -> [u8; 64] {
    let op_bytes = util::serialize_outpoint(outpoint);
    let mut engine: HmacEngine<sha512::Hash> = HmacEngine::new(&op_bytes);
    engine.input(&ecdh_x);
    let mac: Hmac<sha512::Hash> = Hmac::from_engine(engine);
    let mut out = [0u8; 64];
    out.copy_from_slice(&mac[..]);
    out
}

fn v3_blinding_mask(ecdh_x: [u8; 32], sender_pub: &PublicKey) -> [u8; 64] {
    let sender_compressed = sender_pub.serialize();
    let mut engine: HmacEngine<sha512::Hash> = HmacEngine::new(&ecdh_x);
    engine.input(&sender_compressed);
    let mac: Hmac<sha512::Hash> = Hmac::from_engine(engine);
    let mut out = [0u8; 64];
    out.copy_from_slice(&mac[..]);
    out
}

/// v3 notification output: `OP_1 <A=ephemeral pub> <F=recipient identifier> <G=blinded code> OP_3 OP_CHECKMULTISIG`.
pub fn v3_notification_script(a: &[u8; 33], f: &[u8; 33], g: &[u8; 33]) -> bitcoin::ScriptBuf {
    use bitcoin::opcodes::all::{OP_CHECKMULTISIG, OP_PUSHNUM_1, OP_PUSHNUM_3};
    bitcoin::script::Builder::new()
        .push_opcode(OP_PUSHNUM_1)
        .push_slice(a)
        .push_slice(f)
        .push_slice(g)
        .push_opcode(OP_PUSHNUM_3)
        .push_opcode(OP_CHECKMULTISIG)
        .into_script()
}

/// Match a 1-of-3 bare multisig over three 33-byte keys, returned in script order
pub fn parse_1of3_multisig(script: &bitcoin::Script) -> Option<[[u8; 33]; 3]> {
    let b = script.as_bytes();
    // OP_1 (1) + 3 * (push33 (1) + key (33)) + OP_3 (1) + OP_CHECKMULTISIG (1)
    if b.len() != 105 || b[0] != 0x51 || b[103] != 0x53 || b[104] != 0xae {
        return None;
    }
    let mut keys = [[0u8; 33]; 3];
    for (i, key) in keys.iter_mut().enumerate() {
        let base = 1 + i * 34;
        if b[base] != 33 {
            return None;
        }
        key.copy_from_slice(&b[base + 1..base + 34]);
    }
    Some(keys)
}

fn payment_code_from_v1_payload(payload: &[u8]) -> Result<PaymentCode, ParseError> {
    if payload.len() != V1_PAYLOAD_LEN {
        return Err(ParseError::BadLength { got: payload.len() });
    }
    if payload[0] != 0x01 {
        return Err(ParseError::V1BadInternalVersion { got: payload[0] });
    }
    let sign_byte = payload[2];
    if sign_byte != 0x02 && sign_byte != 0x03 {
        return Err(ParseError::BadSignByte { got: sign_byte });
    }
    let mut compressed = [0u8; PUBKEY_LEN];
    compressed[0] = sign_byte;
    compressed[1..].copy_from_slice(&payload[3..3 + 32]);
    let pubkey =
        PublicKey::from_slice(&compressed).map_err(|e| ParseError::BadPubkey(e.to_string()))?;
    let mut cc = [0u8; CHAIN_CODE_LEN];
    cc.copy_from_slice(&payload[35..35 + CHAIN_CODE_LEN]);
    let chain_code = ChainCode::from(cc);
    let mut reserved = [0u8; 13];
    reserved.copy_from_slice(&payload[67..67 + 13]);
    Ok(PaymentCode {
        version: Version::V1,
        pubkey,
        chain_code,
        features: payload[1],
        reserved,
    })
}

pub fn derive_send_address(
    sender_priv: &SecretKey,
    recipient: &PaymentCode,
    i: u32,
    network: Network,
    secp: &Secp256k1<bitcoin::secp256k1::All>,
) -> Result<Address, CryptoError> {
    let pk = derive_send_pubkey(sender_priv, recipient, i, network, secp)?;
    Ok(match recipient.version {
        Version::V1 => p2pkh_address(&pk, network),
        Version::V3 => p2wpkh_address(&pk, network),
    })
}

pub fn derive_send_pubkey<C: Verification + bitcoin::secp256k1::Signing>(
    sender_priv: &SecretKey,
    recipient: &PaymentCode,
    i: u32,
    network: Network,
    secp: &Secp256k1<C>,
) -> Result<PublicKey, CryptoError> {
    let b_i = recipient.derive_pubkey(secp, i)?;
    let s = compute_send_scalar(sender_priv, &b_i, recipient.version, network)?;
    Ok(util::add_tweak_pub(secp, &b_i, &s)?)
}

// TODO: spend side
#[cfg(test)]
pub fn derive_receive_priv(
    receiver_priv_at_i: &SecretKey,
    sender_notification_pub: &PublicKey,
    version: Version,
    network: Network,
) -> Result<SecretKey, CryptoError> {
    let s = compute_send_scalar(
        receiver_priv_at_i,
        sender_notification_pub,
        version,
        network,
    )?;
    let scalar = Scalar::from_be_bytes(s).map_err(|_| CryptoError::InvalidScalar)?;
    receiver_priv_at_i
        .add_tweak(&scalar)
        .map_err(CryptoError::from)
}

pub fn derive_receive_pubkey<C: Verification + bitcoin::secp256k1::Signing>(
    receiver_priv_at_i: &SecretKey,
    sender_notification_pub: &PublicKey,
    version: Version,
    network: Network,
    secp: &Secp256k1<C>,
) -> Result<PublicKey, CryptoError> {
    let s = compute_send_scalar(
        receiver_priv_at_i,
        sender_notification_pub,
        version,
        network,
    )?;
    let receiver_pub = receiver_priv_at_i.public_key(secp);
    Ok(util::add_tweak_pub(secp, &receiver_pub, &s)?)
}

fn compute_send_scalar(
    priv_key: &SecretKey,
    pub_key: &PublicKey,
    version: Version,
    network: Network,
) -> Result<[u8; 32], CryptoError> {
    let secret_x = ecdh_x(priv_key, pub_key);
    let s_bytes = match version {
        Version::V1 => {
            let h: sha256::Hash = sha256::Hash::hash(&secret_x);
            let mut out = [0u8; 32];
            out.copy_from_slice(h.as_byte_array());
            out
        }
        Version::V3 => {
            let coin = match network {
                Network::Bitcoin => V3_COIN_BYTES_MAINNET,
                Network::Testnet | Network::Signet | Network::Regtest => V3_COIN_BYTES_TESTNET,
                _ => return Err(CryptoError::UnsupportedNetwork),
            };
            let mut engine: HmacEngine<sha512::Hash> = HmacEngine::new(&secret_x);
            engine.input(&coin);
            let mac: Hmac<sha512::Hash> = Hmac::from_engine(engine);
            let h: sha256::Hash = sha256::Hash::hash(&mac[..]);
            let mut out = [0u8; 32];
            out.copy_from_slice(h.as_byte_array());
            out
        }
    };
    let _ = Scalar::from_be_bytes(s_bytes).map_err(|_| CryptoError::InvalidScalar)?;
    Ok(s_bytes)
}

fn ecdh_x(priv_key: &SecretKey, pub_key: &PublicKey) -> [u8; 32] {
    let point = ecdh::shared_secret_point(pub_key, priv_key);
    let mut x = [0u8; 32];
    x.copy_from_slice(&point[..32]);
    x
}

fn p2pkh_address(pub_key: &PublicKey, network: Network) -> Address {
    use bitcoin::hashes::{Hash as _, hash160};
    let pkh_bytes = hash160::Hash::hash(&pub_key.serialize());
    let pkh = bitcoin::PubkeyHash::from_raw_hash(pkh_bytes);
    Address::p2pkh(pkh, network)
}

fn p2wpkh_address(pub_key: &PublicKey, network: Network) -> Address {
    Address::p2wpkh(&bitcoin::CompressedPublicKey(*pub_key), network)
}

/// P2PKH address of the payment code's notification pubkey
pub fn notification_address<C: Verification>(
    code: &PaymentCode,
    network: Network,
    secp: &Secp256k1<C>,
) -> Result<Address, CryptoError> {
    Ok(p2pkh_address(&code.notification_pubkey(secp)?, network))
}

pub fn v3_root_xpriv<C: Verification + bitcoin::secp256k1::Signing>(
    account: &Xpriv,
    network: Network,
    secp: &Secp256k1<C>,
) -> Result<Xpriv, CryptoError> {
    let account_xpub = Xpub::from_priv(secp, account);
    let v3_code = PaymentCode::from_xpub(&account_xpub, Version::V3);
    Ok(Xpriv {
        network: bitcoin::NetworkKind::from(network),
        depth: 3,
        parent_fingerprint: Default::default(),
        child_number: ChildNumber::from_hardened_idx(0)?,
        private_key: account.private_key,
        chain_code: *v3_code.chain_code(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod v1_vectors {
        use bitcoin::{
            bip32::{ChildNumber, DerivationPath, Xpriv, Xpub},
            hashes::{Hash, hash160},
        };

        use super::*;

        const ALICE_SEED_HEX: &str = "64dca76abc9c6f0cf3d212d248c380c4622c8f93b2c425ec6a5567fd5db57e10d3e6f94a2f6af4ac2edb8998072aad92098db73558c323777abf5bd1082d970a";
        const ALICE_PAYMENT_CODE: &str = "PM8TJTLJbPRGxSbc8EJi42Wrr6QbNSaSSVJ5Y3E4pbCYiTHUskHg13935Ubb7q8tx9GVbh2UuRnBc3WSyJHhUrw8KhprKnn9eDznYGieTzFcwQRya4GA";
        const ALICE_NOTIFICATION_ADDRESS: &str = "1JDdmqFLhpzcUwPeinhJbUPw4Co3aWLyzW";
        const ALICE_A0_PRIVHEX: &str =
            "8d6a8ecd8ee5e0042ad0cb56e3a971c760b5145c3917a8e7beaf0ed92d7a520c";
        const ALICE_A0_PUBHEX: &str =
            "0353883a146a23f988e0f381a9507cbdb3e3130cd81b3ce26daf2af088724ce683";

        const BOB_SEED_HEX: &str = "87eaaac5a539ab028df44d9110defbef3797ddb805ca309f61a69ff96dbaa7ab5b24038cf029edec5235d933110f0aea8aeecf939ed14fc20730bba71e4b1110";
        const BOB_PAYMENT_CODE: &str = "PM8TJS2JxQ5ztXUpBBRnpTbcUXbUHy2T1abfrb3KkAAtMEGNbey4oumH7Hc578WgQJhPjBxteQ5GHHToTYHE3A1w6p7tU6KSoFmWBVbFGjKPisZDbP97";

        const ALICE_TO_BOB: [&str; 10] = [
            "141fi7TY3h936vRUKh1qfUZr8rSBuYbVBK",
            "12u3Uued2fuko2nY4SoSFGCoGLCBUGPkk6",
            "1FsBVhT5dQutGwaPePTYMe5qvYqqjxyftc",
            "1CZAmrbKL6fJ7wUxb99aETwXhcGeG3CpeA",
            "1KQvRShk6NqPfpr4Ehd53XUhpemBXtJPTL",
            "1KsLV2F47JAe6f8RtwzfqhjVa8mZEnTM7t",
            "1DdK9TknVwvBrJe7urqFmaxEtGF2TMWxzD",
            "16DpovNuhQJH7JUSZQFLBQgQYS4QB9Wy8e",
            "17qK2RPGZMDcci2BLQ6Ry2PDGJErrNojT5",
            "1GxfdfP286uE24qLZ9YRP3EWk2urqXgC4s",
        ];

        const DESIGNATED_OUTPOINT_HEX: &str =
            "86f411ab1c8e70ae8a0795ab7a6757aea6e4d5ae1826fc7b8f00c597d500609c01000000";
        const DESIGNATED_INPUT_WIF: &str = "Kx983SRhAZpAhj7Aac1wUXMJ6XZeyJKqCxJJ49dxEbYCT4a1ozRD";
        const NOTIFICATION_SHARED_SECRET_X_HEX: &str =
            "736a25d9250238ad64ed5da03450c6a3f4f8f4dcdf0b58d1ed69029d76ead48d";
        const NOTIFICATION_BLINDING_MASK_HEX: &str = "be6e7a4256cac6f4d4ed4639b8c39c4cb8bece40010908e70d17ea9d77b4dc57f1da36f2d6641ccb37cf2b9f3146686462e0fa3161ae74f88c0afd4e307adbd5";
        const ALICE_BLINDED_PAYLOAD_HEX: &str = "010002063e4eb95e62791b06c50e1a3a942e1ecaaa9afbbeb324d16ae6821e091611fa96c0cf048f607fe51a0327f5e2528979311c78cb2de0d682c61e1180fc3d543b00000000000000000000000000";

        fn derive_at(seed_hex: &str, path: &[ChildNumber]) -> Xpriv {
            let secp = Secp256k1::new();
            let seed = hex::decode(seed_hex).unwrap();
            let master = Xpriv::new_master(bitcoin::NetworkKind::Main, &seed).unwrap();
            master
                .derive_priv(&secp, &DerivationPath::from(path))
                .unwrap()
        }

        fn bip47_account(seed_hex: &str) -> Xpriv {
            derive_at(
                seed_hex,
                &[
                    ChildNumber::from_hardened_idx(47).unwrap(),
                    ChildNumber::from_hardened_idx(0).unwrap(),
                    ChildNumber::from_hardened_idx(0).unwrap(),
                ],
            )
        }

        fn pubkey_to_p2pkh_mainnet(pk: &PublicKey) -> String {
            let h = hash160::Hash::hash(&pk.serialize());
            let pkh = bitcoin::PubkeyHash::from_raw_hash(h);
            bitcoin::Address::p2pkh(pkh, Network::Bitcoin).to_string()
        }

        #[test]
        fn v1_official_vectors() {
            let secp = Secp256k1::new();

            let alice_acct = bip47_account(ALICE_SEED_HEX);
            let alice = PaymentCode::from_xpub(&Xpub::from_priv(&secp, &alice_acct), Version::V1);
            assert_eq!(alice.to_string(), ALICE_PAYMENT_CODE);
            let a0 = alice_acct
                .derive_priv(&secp, &[ChildNumber::from_normal_idx(0).unwrap()])
                .unwrap()
                .private_key;
            assert_eq!(hex::encode(a0.secret_bytes()), ALICE_A0_PRIVHEX);
            let a0_pub = a0.public_key(&secp);
            assert_eq!(hex::encode(a0_pub.serialize()), ALICE_A0_PUBHEX);
            assert_eq!(pubkey_to_p2pkh_mainnet(&a0_pub), ALICE_NOTIFICATION_ADDRESS);

            let bob = PaymentCode::from_str(BOB_PAYMENT_CODE).unwrap();
            for (i, expected) in ALICE_TO_BOB.iter().enumerate() {
                let pk = derive_send_pubkey(&a0, &bob, i as u32, Network::Bitcoin, &secp).unwrap();
                assert_eq!(&pubkey_to_p2pkh_mainnet(&pk), expected, "address {i}");
            }

            let desig_priv = bitcoin::PrivateKey::from_wif(DESIGNATED_INPUT_WIF)
                .unwrap()
                .inner;
            let bob_b0 = bob.notification_pubkey(&secp).unwrap();
            let secret_x = ecdh_x(&desig_priv, &bob_b0);
            assert_eq!(hex::encode(secret_x), NOTIFICATION_SHARED_SECRET_X_HEX);
            let outpoint = parse_outpoint(DESIGNATED_OUTPOINT_HEX);
            let mask = v1_blinding_mask(secret_x, outpoint);
            assert_eq!(hex::encode(mask), NOTIFICATION_BLINDING_MASK_HEX);
            let blinded = blind(&alice, &desig_priv, &bob_b0, outpoint, &secp);
            assert_eq!(hex::encode(&blinded), ALICE_BLINDED_PAYLOAD_HEX);

            let bob_notif_priv = bip47_account(BOB_SEED_HEX)
                .derive_priv(&secp, &[ChildNumber::from_normal_idx(0).unwrap()])
                .unwrap()
                .private_key;
            let alice_input_pub = desig_priv.public_key(&secp);
            let recovered = unblind(
                &bob_notif_priv,
                &blinded,
                &alice_input_pub,
                outpoint,
                Version::V1,
            )
            .expect("unblind v1");
            assert_eq!(recovered.to_string(), ALICE_PAYMENT_CODE);
        }

        fn parse_outpoint(hex_s: &str) -> OutPoint {
            let raw = hex::decode(hex_s).unwrap();
            assert_eq!(raw.len(), 36);
            let mut txid_bytes = [0u8; 32];
            txid_bytes.copy_from_slice(&raw[0..32]);
            let vout = u32::from_le_bytes([raw[32], raw[33], raw[34], raw[35]]);
            let txid = bitcoin::Txid::from_raw_hash(
                bitcoin::hashes::sha256d::Hash::from_byte_array(txid_bytes),
            );
            OutPoint::new(txid, vout)
        }
    }

    mod v3_vectors {
        use bitcoin::bip32::{ChildNumber, DerivationPath, Xpriv, Xpub};

        use super::*;

        const ALICE_SEED_HEX: &str = "64dca76abc9c6f0cf3d212d248c380c4622c8f93b2c425ec6a5567fd5db57e10d3e6f94a2f6af4ac2edb8998072aad92098db73558c323777abf5bd1082d970a";
        const ALICE_PC_V3: &str = "PD1jTsa1rjnbMMLVbj5cg2c8KkFY32KWtPRqVVpSBkv1jf8zjHJVu";
        const ALICE_NOTIF_PRIV_HEX: &str =
            "7167db816df3e03b4f4df749dd1c1cf5b9a81ae0ce0b2f4dc5d8b75aea4e77e0";
        const ALICE_NOTIF_PUB_HEX: &str =
            "030a5280a538fe5a134b77d96f5cd9d050c11021d86e8b4cf327f064a7c76b0db4";

        const BOB_PC_V3: &str = "PD1jFsimY3DQUe7qGtx3z8BohTaT6r4kwJMCYXwp7uY8z6BSaFrpM";
        const BOB_NOTIF_PRIV_HEX: &str =
            "6850fcb45313e30f941f91d49bbad21260161c9ea7ed4a322930176db945f0bd";

        const ALICE_SEND_TO_BOB_TESTNET: [&str; 10] = [
            "03dc41458b939d966a0e141281c2a7c5faf184dc43bc26160f0ffc3c583600c9b6",
            "02513de274f78ce0c8cb827f25aae2ade941ac9d482002fc04ef60d580c5403afd",
            "033cf4391b3e7daad0220b572d796ac0711e93e2ef389119d3ec0bed2debf0472a",
            "02de639a0d80bc8b6976e71e5242b7f0ba5e9f8f6b317c0a180884424600bcaafc",
            "036b08a58e0d664505c95e2e0ceaa87e34c82cc6ed91a94980fc631967cc8d931f",
            "029d5dae4c27c59a9c207a1beafab9f1b8bef93e19b8bbd7614dae37e8f7c0210c",
            "03003668a8915ba65adb9ff8cfcce7f8d5aae2655a210e1e863eda6cb41dd5e1d2",
            "03a857d0bef97a0e5ffb1911e7cd13ced1bdce9c2a6a838dd5bdd8e805f44b8cc9",
            "02bcfcdc2e7fbdaebf1fa69a74ccd219c919981353433538ff98979c252609c564",
            "029dccbb87fec52713f90afbbef3e78dddef4dfa6858bbf2a5fd2fcd2582a5cf7f",
        ];

        const BOB_SEND_TO_ALICE_MAINNET: [&str; 10] = [
            "024edba30e70855e7846e850982f2eb3aefe33b292cc9a744604367de14cc018b8",
            "03a769eb57ce38dc3f7d80c4464bc61b02153a8e881c472d6d3e99b1d8fe53100c",
            "038f8e84682fb78ec6fdf3560020df035e144ce60bb9b09dd99b606d130140bd2c",
            "0210964b717a97430e9ca206bf84e1b0834385a03af3c749d60ad632d31e511954",
            "03b24c25099596f0984e4eedcc6147d1faff269a79f919e5d42414ea0691749174",
            "0285b4cda5356a7333510fac98fc27da4df8a3fcf6f50df594fbe6013e78d64114",
            "02a6946888b559db413f94a6de3aa974d4c22d881f132f753297baef510219327c",
            "02ab944a2509a27b9b9f569736e6cb45cb1c900627573a01bb9dffe38131103a12",
            "0276442e645c3f5e412b60ffac771a67b0ef1b652b18f18d101e9c6f70365cd183",
            "039386636f65cbc72a70bacc0f43ee17862ead8d37941e72b630f37e048ef2d405",
        ];

        const ALICE_IDENTIFIER_V3_HEX: &str =
            "0205be1671949473c1b252db7aff98a8704841ad7cd19596f9d64ed81bd3e58bc8";
        const BOB_IDENTIFIER_V3_HEX: &str =
            "02ce75616fcd80345bca54dabd279b155f960c57260378455b872269221de231b6";

        const BOB_EPHEMERAL_PRIV_HEX: &str =
            "0fb05a28df58b2add0d01eb491962b79092239e4d9396442eed83144b6541f4c";
        const BOB_BLINDED_G_HEX: &str =
            "027f88837a6d02949388c80c43efd352bea4483bb86764ba3dfa5b0c11e97b0ebc";

        const ALICE_EPHEMERAL_PUB_HEX: &str =
            "0383b5e54776628baacee0cbb66b4db31aa95176dba1f62cabf0415103d0fdbda6";
        const ALICE_BLINDED_G_HEX: &str =
            "0292d97c287932848852890ded442311623e32ebfeba12e2020b41c2fbe12f3812";

        fn hex_decode(s: &str) -> Vec<u8> {
            hex::decode(s).expect("valid hex")
        }

        #[test]
        fn v3_official_vectors() {
            let secp = Secp256k1::new();

            assert_eq!(
                crate::wallet::reusable_payments::util::bip44_coin_type(Network::Bitcoin),
                0
            );
            let seed = hex_decode(ALICE_SEED_HEX);
            let master = Xpriv::new_master(bitcoin::NetworkKind::Main, &seed).unwrap();
            let account = master
                .derive_priv(
                    &secp,
                    &DerivationPath::from(
                        [
                            ChildNumber::from_hardened_idx(47).unwrap(),
                            ChildNumber::from_hardened_idx(0).unwrap(),
                            ChildNumber::from_hardened_idx(0).unwrap(),
                        ]
                        .as_slice(),
                    ),
                )
                .unwrap();
            let alice = PaymentCode::from_xpub(&Xpub::from_priv(&secp, &account), Version::V3);
            assert_eq!(alice.to_string(), ALICE_PC_V3);

            let v3_root = v3_root_xpriv(&account, Network::Bitcoin, &secp).unwrap();
            let notif = v3_root
                .derive_priv(&secp, &[ChildNumber::from_normal_idx(0).unwrap()])
                .unwrap()
                .private_key;
            assert_eq!(hex::encode(notif.secret_bytes()), ALICE_NOTIF_PRIV_HEX);
            assert_eq!(
                hex::encode(notif.public_key(&secp).serialize()),
                ALICE_NOTIF_PUB_HEX
            );

            let bob = PaymentCode::from_str(BOB_PC_V3).unwrap();
            let bob_notif_priv = SecretKey::from_slice(&hex_decode(BOB_NOTIF_PRIV_HEX)).unwrap();
            for (i, expected) in ALICE_SEND_TO_BOB_TESTNET.iter().enumerate() {
                let pk =
                    derive_send_pubkey(&notif, &bob, i as u32, Network::Regtest, &secp).unwrap();
                assert_eq!(&hex::encode(pk.serialize()), expected, "testnet send {i}");
            }
            for (i, expected) in BOB_SEND_TO_ALICE_MAINNET.iter().enumerate() {
                let pk =
                    derive_send_pubkey(&bob_notif_priv, &alice, i as u32, Network::Bitcoin, &secp)
                        .unwrap();
                assert_eq!(&hex::encode(pk.serialize()), expected, "mainnet send {i}");
            }

            assert_eq!(hex::encode(alice.identifier()), ALICE_IDENTIFIER_V3_HEX);
            assert_eq!(hex::encode(bob.identifier()), BOB_IDENTIFIER_V3_HEX);

            let bob_eph = SecretKey::from_slice(&hex_decode(BOB_EPHEMERAL_PRIV_HEX)).unwrap();
            let alice_notif_pub = alice.notification_pubkey(&secp).unwrap();
            let g = blind(&bob, &bob_eph, &alice_notif_pub, OutPoint::null(), &secp);
            assert_eq!(hex::encode(&g), BOB_BLINDED_G_HEX);

            let alice_eph_pub =
                PublicKey::from_slice(&hex_decode(ALICE_EPHEMERAL_PUB_HEX)).unwrap();
            let g = hex_decode(ALICE_BLINDED_G_HEX);
            let recovered = unblind(
                &bob_notif_priv,
                &g,
                &alice_eph_pub,
                OutPoint::null(),
                Version::V3,
            )
            .expect("unblind v3");
            assert_eq!(recovered.to_string(), ALICE_PC_V3);
        }

        #[test]
        fn v3_notification_script_round_trips() {
            let a = [0x02u8; 33];
            let mut f = [0u8; 33];
            f[0] = 0x02;
            f[32] = 0x7f;
            let g = [0x03u8; 33];
            let script = v3_notification_script(&a, &f, &g);
            assert_eq!(script.as_bytes().len(), 105);
            let keys = parse_1of3_multisig(&script).expect("parse back");
            assert_eq!(keys, [a, f, g]);

            let not_multisig = bitcoin::ScriptBuf::from_bytes(vec![0x51, 0x21]);
            assert!(parse_1of3_multisig(&not_multisig).is_none());
        }
    }
}
