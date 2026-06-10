use std::collections::{BTreeMap, HashMap};

use bitcoin::{
    Network,
    bech32::{
        Bech32m, Fe32, Hrp,
        primitives::{
            decode::CheckedHrpstring,
            iter::{ByteIterExt, Fe32IterExt},
        },
    },
    bip32::{ChildNumber, DerivationPath},
    hashes::{Hash, HashEngine, sha256},
    secp256k1::{PublicKey, Scalar, Secp256k1, SecretKey, Signing, Verification, ecdh},
};

use super::util;

const HRP_MAINNET: &str = "sp";
const HRP_TESTNET: &str = "tsp";

const SP_VERSION_V0: Fe32 = Fe32::Q;

const SP_PAYLOAD_LEN: usize = 66;

const BIP352_PURPOSE: u32 = 352;

const TAG_INPUTS: &[u8] = b"BIP0352/Inputs";
const TAG_SHARED_SECRET: &[u8] = b"BIP0352/SharedSecret";
const TAG_LABEL: &[u8] = b"BIP0352/Label";

pub const CHANGE_LABEL_M: u32 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HrpKind {
    Mainnet,
    Testnet,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("bech32 decode: {0}")]
    Bech32(String),
    #[error("expected HRP \"sp\" or \"tsp\", got {got:?}")]
    BadHrp { got: String },
    #[error("expected SP version 0 ('q'), got {got}")]
    BadVersion { got: u8 },
    #[error("payload length {got}, expected {SP_PAYLOAD_LEN}")]
    BadPayloadLength { got: usize },
    #[error("invalid secp256k1 pubkey: {0}")]
    BadPubkey(#[from] bitcoin::secp256k1::Error),
    #[error("address HRP {hrp_kind:?} does not match expected network {expected:?}")]
    NetworkMismatch {
        hrp_kind: HrpKind,
        expected: Network,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("ECDH yielded zero or out-of-range scalar")]
    InvalidScalar,
    #[error("computed point is the identity / at infinity")]
    PointAtInfinity,
    #[error("sum of input private keys is zero")]
    InputSumZero,
    #[error("secp256k1: {0}")]
    Secp256k1(#[from] bitcoin::secp256k1::Error),
    #[error("BIP32 derivation: {0}")]
    Bip32(#[from] bitcoin::bip32::Error),
    #[error("too many outputs for one scan key: BIP352 caps at 2323")]
    TooManyOutputs,
    #[error("no eligible inputs")]
    NoEligibleInputs,
    #[error("input {outpoint} script type is not eligible per BIP352")]
    IneligibleInput { outpoint: bitcoin::OutPoint },
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
pub struct SilentPaymentAddress {
    pub scan: PublicKey,
    pub spend: PublicKey,
    pub network: Network,
}

impl SilentPaymentAddress {
    pub fn base(scan: PublicKey, spend: PublicKey, network: Network) -> Self {
        Self {
            scan,
            spend,
            network,
        }
    }
}

impl std::fmt::Display for SilentPaymentAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hrp_str = match self.network {
            Network::Bitcoin => HRP_MAINNET,
            _ => HRP_TESTNET,
        };
        let hrp = Hrp::parse(hrp_str).expect("static HRP literal is valid bech32 HRP");

        let mut payload = Vec::with_capacity(SP_PAYLOAD_LEN);
        payload.extend_from_slice(&self.scan.serialize());
        payload.extend_from_slice(&self.spend.serialize());
        debug_assert_eq!(payload.len(), SP_PAYLOAD_LEN);

        for c in payload
            .into_iter()
            .bytes_to_fes()
            .with_checksum::<Bech32m>(&hrp)
            .with_witness_version(SP_VERSION_V0)
            .chars()
        {
            f.write_char(c)?;
        }
        Ok(())
    }
}

impl std::str::FromStr for SilentPaymentAddress {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut checked =
            CheckedHrpstring::new::<Bech32m>(s).map_err(|e| ParseError::Bech32(e.to_string()))?;
        let hrp_str = checked.hrp().to_string();
        let network = match hrp_str.as_str() {
            HRP_MAINNET => Network::Bitcoin,
            HRP_TESTNET => Network::Testnet,
            _ => return Err(ParseError::BadHrp { got: hrp_str }),
        };

        let version = checked
            .remove_witness_version()
            .ok_or(ParseError::BadVersion { got: 0xff })?;
        let version_u8 = version.to_u8();
        if version_u8 == 31 {
            return Err(ParseError::BadVersion { got: version_u8 });
        }

        let payload: Vec<u8> = checked.byte_iter().collect();
        if version == SP_VERSION_V0 {
            if payload.len() != SP_PAYLOAD_LEN {
                return Err(ParseError::BadPayloadLength { got: payload.len() });
            }
        } else if payload.len() < SP_PAYLOAD_LEN {
            return Err(ParseError::BadPayloadLength { got: payload.len() });
        }
        let scan = PublicKey::from_slice(&payload[0..33])?;
        let spend = PublicKey::from_slice(&payload[33..66])?;
        Ok(Self {
            scan,
            spend,
            network,
        })
    }
}

impl SilentPaymentAddress {
    pub fn parse_for_network(s: &str, expected: Network) -> Result<Self, ParseError> {
        let parsed: Self = s.parse()?;
        let hrp_kind = match parsed.network {
            Network::Bitcoin => HrpKind::Mainnet,
            _ => HrpKind::Testnet,
        };
        let expected_kind = match expected {
            Network::Bitcoin => HrpKind::Mainnet,
            Network::Testnet | Network::Signet | Network::Regtest => HrpKind::Testnet,
            _ => HrpKind::Testnet,
        };
        if hrp_kind != expected_kind {
            return Err(ParseError::NetworkMismatch { hrp_kind, expected });
        }
        Ok(Self {
            network: expected,
            ..parsed
        })
    }
}

fn require_nonzero_scalar(bytes: &[u8; 32]) -> Result<(), CryptoError> {
    if bytes == &[0u8; 32] {
        return Err(CryptoError::InvalidScalar);
    }
    Ok(())
}

fn tagged_hash(tag: &[u8], data: &[u8]) -> [u8; 32] {
    let tag_hash: sha256::Hash = sha256::Hash::hash(tag);
    let mut engine = sha256::Hash::engine();
    engine.input(tag_hash.as_byte_array());
    engine.input(tag_hash.as_byte_array());
    engine.input(data);
    let out: sha256::Hash = sha256::Hash::from_engine(engine);
    let mut b = [0u8; 32];
    b.copy_from_slice(out.as_byte_array());
    b
}

#[cfg(test)]
pub fn derive_keys_from_seed<C: Signing + Verification>(
    seed: &[u8],
    network: Network,
    account: u32,
    secp: &Secp256k1<C>,
) -> Result<(SecretKey, SecretKey, SilentPaymentAddress), CryptoError> {
    let net_kind = bitcoin::NetworkKind::from(network);
    let master = bitcoin::bip32::Xpriv::new_master(net_kind, seed)?;

    let coin = util::bip44_coin_type(network);
    let scan_path = sp_path(coin, account, 1)?;
    let spend_path = sp_path(coin, account, 0)?;

    let scan_xpriv = master.derive_priv(secp, &scan_path)?;
    let spend_xpriv = master.derive_priv(secp, &spend_path)?;

    let scan = scan_xpriv.private_key;
    let spend = spend_xpriv.private_key;
    let scan_pub = scan.public_key(secp);
    let spend_pub = spend.public_key(secp);

    let addr = SilentPaymentAddress::base(scan_pub, spend_pub, network);
    Ok((scan, spend, addr))
}

pub(in crate::wallet) fn sp_path(
    coin: u32,
    account: u32,
    branch: u32,
) -> Result<DerivationPath, CryptoError> {
    let components = [
        ChildNumber::from_hardened_idx(BIP352_PURPOSE)?,
        ChildNumber::from_hardened_idx(coin)?,
        ChildNumber::from_hardened_idx(account)?,
        ChildNumber::from_hardened_idx(branch)?,
        ChildNumber::from_normal_idx(0)?,
    ];
    Ok(DerivationPath::from(components.as_slice()))
}

#[derive(Clone, Debug)]
pub struct LabelSet {
    points: BTreeMap<u32, PublicKey>,
    by_point: HashMap<PublicKey, u32>,
}

impl LabelSet {
    pub fn with_change<C: Signing + Verification>(
        b_scan: &SecretKey,
        secp: &Secp256k1<C>,
    ) -> Result<Self, CryptoError> {
        let mut s = Self {
            points: BTreeMap::new(),
            by_point: HashMap::new(),
        };
        s.add(b_scan, CHANGE_LABEL_M, secp)?;
        Ok(s)
    }

    pub fn add<C: Signing + Verification>(
        &mut self,
        b_scan: &SecretKey,
        m: u32,
        secp: &Secp256k1<C>,
    ) -> Result<PublicKey, CryptoError> {
        let point = label_point(b_scan, m, secp)?;
        if let Some(old) = self.points.insert(m, point) {
            self.by_point.remove(&old);
        }
        self.by_point.insert(point, m);
        Ok(point)
    }

    pub fn label_for(&self, point: &PublicKey) -> Option<u32> {
        self.by_point.get(point).copied()
    }
}

fn label_tweak(b_scan: &SecretKey, m: u32) -> [u8; 32] {
    let mut buf = [0u8; 32 + 4];
    buf[0..32].copy_from_slice(&b_scan.secret_bytes());
    buf[32..36].copy_from_slice(&m.to_be_bytes());
    tagged_hash(TAG_LABEL, &buf)
}

fn label_point<C: Signing + Verification>(
    b_scan: &SecretKey,
    m: u32,
    secp: &Secp256k1<C>,
) -> Result<PublicKey, CryptoError> {
    let tweak = label_tweak(b_scan, m);
    let sk = SecretKey::from_slice(&tweak).map_err(|_| CryptoError::InvalidScalar)?;
    Ok(sk.public_key(secp))
}

pub fn labeled_address<C: Signing + Verification>(
    b_scan: &SecretKey,
    b_spend: &SecretKey,
    m: u32,
    network: Network,
    secp: &Secp256k1<C>,
) -> Result<SilentPaymentAddress, CryptoError> {
    let scan_pub = b_scan.public_key(secp);
    let b_spend_pub = b_spend.public_key(secp);
    let tweak = label_tweak(b_scan, m);
    let labeled_spend = util::add_tweak_pub(secp, &b_spend_pub, &tweak)?;
    Ok(SilentPaymentAddress::base(scan_pub, labeled_spend, network))
}

pub type Recipient = (SilentPaymentAddress, bitcoin::Amount);

pub struct EligibleInput {
    pub outpoint: bitcoin::OutPoint,
    pub secret: SecretKey,
}

impl Drop for EligibleInput {
    fn drop(&mut self) {
        self.secret.non_secure_erase();
    }
}

pub fn compute_outputs<C: Signing + Verification>(
    eligible_inputs: &[EligibleInput],
    all_outpoints: &[bitcoin::OutPoint],
    recipients: &[Recipient],
    secp: &Secp256k1<C>,
) -> Result<Vec<bitcoin::TxOut>, CryptoError> {
    if eligible_inputs.is_empty() {
        return Err(CryptoError::NoEligibleInputs);
    }
    let inputs = eligible_inputs;

    let a = sum_private_keys(inputs)?;
    let cap_a = a.public_key(secp);

    let outpoint_l = util::lex_min_outpoint(all_outpoints.iter().copied())
        .ok_or(CryptoError::NoEligibleInputs)?;
    let mut inputs_data = Vec::with_capacity(36 + 33);
    inputs_data.extend_from_slice(&util::serialize_outpoint(outpoint_l));
    inputs_data.extend_from_slice(&cap_a.serialize());
    let input_hash = tagged_hash(TAG_INPUTS, &inputs_data);
    require_nonzero_scalar(&input_hash)?;

    let mut groups: BTreeMap<[u8; 33], Vec<&Recipient>> = BTreeMap::new();
    for r in recipients {
        groups.entry(r.0.scan.serialize()).or_default().push(r);
    }

    let mut out = Vec::with_capacity(recipients.len());
    for (_scan_ser, group) in groups {
        if group.len() > 2323 {
            return Err(CryptoError::TooManyOutputs);
        }
        let b_scan = group[0].0.scan;

        let scaled = scalar_mul(&a, &input_hash)?;
        let ecdh_point = ecdh::shared_secret_point(&b_scan, &scaled);
        let ecdh_pub = compressed_point(&ecdh_point);

        for (k, r) in group.iter().enumerate() {
            let b_m = r.0.spend;
            let amount = r.1;
            let mut buf = [0u8; 33 + 4];
            buf[0..33].copy_from_slice(&ecdh_pub.serialize());
            buf[33..37].copy_from_slice(&(k as u32).to_be_bytes());
            let tk = tagged_hash(TAG_SHARED_SECRET, &buf);
            require_nonzero_scalar(&tk)?;
            let pk = util::add_tweak_pub(secp, &b_m, &tk)?;
            let (xonly, _parity) = pk.x_only_public_key();
            let script = bitcoin::ScriptBuf::new_p2tr_tweaked(
                bitcoin::key::TweakedPublicKey::dangerous_assume_tweaked(xonly),
            );
            out.push(bitcoin::TxOut {
                value: amount,
                script_pubkey: script,
            });
        }
    }
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct Match {
    pub vout: u32,
    pub output_xonly: bitcoin::XOnlyPublicKey,
    pub amount: bitcoin::Amount,
    pub tweak_k: u32,
    pub label: Option<u32>,
}

pub fn scan_tx<C: Verification>(
    input_pubkeys_sum: &PublicKey,
    outpoint_l: bitcoin::OutPoint,
    taproot_outputs: &[(u32, bitcoin::XOnlyPublicKey, bitcoin::Amount)],
    b_scan: &SecretKey,
    b_spend_pub: &PublicKey,
    labels: &LabelSet,
    secp: &Secp256k1<C>,
) -> Result<Vec<Match>, CryptoError> {
    let mut inputs_data = Vec::with_capacity(36 + 33);
    inputs_data.extend_from_slice(&util::serialize_outpoint(outpoint_l));
    inputs_data.extend_from_slice(&input_pubkeys_sum.serialize());
    let input_hash = tagged_hash(TAG_INPUTS, &inputs_data);
    require_nonzero_scalar(&input_hash)?;

    let scaled = scalar_mul(b_scan, &input_hash)?;
    let ecdh_point = ecdh::shared_secret_point(input_pubkeys_sum, &scaled);
    let ecdh_pub = compressed_point(&ecdh_point);

    let mut remaining: Vec<(u32, bitcoin::XOnlyPublicKey, bitcoin::Amount)> =
        taproot_outputs.to_vec();
    let mut matches = Vec::new();
    let mut k: u32 = 0;
    while k < 2323 && !remaining.is_empty() {
        let mut buf = [0u8; 33 + 4];
        buf[0..33].copy_from_slice(&ecdh_pub.serialize());
        buf[33..37].copy_from_slice(&k.to_be_bytes());
        let tk = tagged_hash(TAG_SHARED_SECRET, &buf);
        if require_nonzero_scalar(&tk).is_err() {
            tracing::warn!(
                k,
                "scan_tx: tagged_hash yielded invalid scalar, stopping scan"
            );
            return Ok(matches);
        }
        let pk_candidate = match util::add_tweak_pub(secp, b_spend_pub, &tk) {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(k, "scan_tx: tweak yielded point at infinity, stopping scan");
                return Ok(matches);
            }
        };
        let (candidate_x, _) = pk_candidate.x_only_public_key();

        let mut matched_idx: Option<(usize, Option<u32>)> = None;
        for (idx, (_vout, out_x, _amount)) in remaining.iter().enumerate() {
            if *out_x == candidate_x {
                matched_idx = Some((idx, None));
                break;
            }
            let mut label_hit: Option<u32> = None;
            for parity in [
                bitcoin::secp256k1::Parity::Even,
                bitcoin::secp256k1::Parity::Odd,
            ] {
                let out_full = out_x.public_key(parity);
                if let Ok(residual) = subtract_pub(&out_full, &pk_candidate, secp)
                    && let Some(m) = labels.label_for(&residual)
                {
                    label_hit = Some(m);
                    break;
                }
            }
            if let Some(m) = label_hit {
                matched_idx = Some((idx, Some(m)));
                break;
            }
        }

        match matched_idx {
            Some((idx, label)) => {
                let (vout, out_x, amount) = remaining.swap_remove(idx);
                matches.push(Match {
                    vout,
                    output_xonly: out_x,
                    amount,
                    tweak_k: k,
                    label,
                });
                k += 1;
            }
            None => break,
        }
    }
    Ok(matches)
}

/// TODO: spend side
#[cfg(test)]
pub fn recover_spending_key<C: Signing + Verification>(
    b_spend: &SecretKey,
    b_scan: &SecretKey,
    input_pubkeys_sum: &PublicKey,
    outpoint_l: bitcoin::OutPoint,
    tweak_k: u32,
    label: Option<u32>,
    secp: &Secp256k1<C>,
) -> Result<SecretKey, CryptoError> {
    let mut inputs_data = Vec::with_capacity(36 + 33);
    inputs_data.extend_from_slice(&util::serialize_outpoint(outpoint_l));
    inputs_data.extend_from_slice(&input_pubkeys_sum.serialize());
    let input_hash = tagged_hash(TAG_INPUTS, &inputs_data);
    require_nonzero_scalar(&input_hash)?;
    let scaled = scalar_mul(b_scan, &input_hash)?;
    let ecdh_point = ecdh::shared_secret_point(input_pubkeys_sum, &scaled);
    let ecdh_pub = compressed_point(&ecdh_point);

    let mut buf = [0u8; 33 + 4];
    buf[0..33].copy_from_slice(&ecdh_pub.serialize());
    buf[33..37].copy_from_slice(&tweak_k.to_be_bytes());
    let tk = tagged_hash(TAG_SHARED_SECRET, &buf);
    require_nonzero_scalar(&tk)?;

    let tk_scalar = Scalar::from_be_bytes(tk).map_err(|_| CryptoError::InvalidScalar)?;
    let mut d = b_spend
        .add_tweak(&tk_scalar)
        .map_err(CryptoError::Secp256k1)?;
    if let Some(m) = label {
        let lt = label_tweak(b_scan, m);
        let lt_scalar = Scalar::from_be_bytes(lt).map_err(|_| CryptoError::InvalidScalar)?;
        d = d.add_tweak(&lt_scalar).map_err(CryptoError::Secp256k1)?;
    }
    if d.public_key(secp).x_only_public_key().1 == bitcoin::secp256k1::Parity::Odd {
        d = d.negate();
    }
    Ok(d)
}

fn sum_private_keys(inputs: &[EligibleInput]) -> Result<SecretKey, CryptoError> {
    if inputs.is_empty() {
        return Err(CryptoError::NoEligibleInputs);
    }
    let mut acc: Option<SecretKey> = None;
    for input in inputs {
        acc = match acc {
            None => Some(input.secret),
            Some(prev) => prev.add_tweak(&Scalar::from(input.secret)).ok(),
        };
    }
    acc.ok_or(CryptoError::InputSumZero)
}

fn scalar_mul(a: &SecretKey, tweak: &[u8; 32]) -> Result<SecretKey, CryptoError> {
    let tweak_scalar = Scalar::from_be_bytes(*tweak).map_err(|_| CryptoError::InvalidScalar)?;
    a.mul_tweak(&tweak_scalar).map_err(CryptoError::Secp256k1)
}

fn subtract_pub<C: Verification>(
    a: &PublicKey,
    b: &PublicKey,
    secp: &Secp256k1<C>,
) -> Result<PublicKey, CryptoError> {
    let neg_b = b.negate(secp);
    a.combine(&neg_b).map_err(|_| CryptoError::PointAtInfinity)
}

fn compressed_point(raw_xy: &[u8; 64]) -> PublicKey {
    let mut buf = [0u8; 65];
    buf[0] = 0x04;
    buf[1..].copy_from_slice(raw_xy);
    PublicKey::from_slice(&buf).expect("shared_secret_point always yields a valid curve point")
}

use std::fmt::Write as _;

#[cfg(test)]
mod tests {
    use bitcoin::secp256k1::rand::{SeedableRng, rngs::StdRng};

    use super::*;

    fn det_seed(seed: u64) -> Vec<u8> {
        let mut rng = StdRng::seed_from_u64(seed);
        use bitcoin::secp256k1::rand::Rng;
        (0..64).map(|_| rng.r#gen()).collect()
    }

    #[test]
    fn bip352_official_sending_vectors() {
        const VECTORS: &str = super::super::silent_payments_test_vectors::VECTORS;
        let cases: serde_json::Value = serde_json::from_str(VECTORS).expect("parse vectors JSON");
        let cases = cases.as_array().expect("vectors is an array");
        let secp = Secp256k1::new();

        let mut passed = 0usize;
        let mut skipped = Vec::new();

        for (case_idx, case) in cases.iter().enumerate() {
            let comment = case["comment"].as_str().unwrap_or("");
            let Some(sending_arr) = case["sending"].as_array() else {
                skipped.push((case_idx, comment.to_string(), "no sending block"));
                continue;
            };
            for (sub_idx, send) in sending_arr.iter().enumerate() {
                let given = &send["given"];
                let expected = &send["expected"];
                let vin = given["vin"].as_array().expect("vin");
                let recipients_v = given["recipients"].as_array().expect("recipients");

                let mut inputs: Vec<EligibleInput> = Vec::new();
                let mut extracted_pubs: Vec<String> = Vec::new();
                for v in vin {
                    let sk_hex = v["private_key"].as_str().unwrap_or("");
                    let spk_hex = v["prevout"]["scriptPubKey"]["hex"].as_str().unwrap_or("");
                    let txid_hex = v["txid"].as_str().unwrap_or("");
                    let vout = v["vout"].as_u64().unwrap_or(0) as u32;

                    let script_sig_bytes =
                        hex::decode(v["scriptSig"].as_str().unwrap_or("")).unwrap();
                    let spk_bytes = hex::decode(spk_hex).unwrap();
                    let witness = decode_witness_hex(v["txinwitness"].as_str().unwrap_or(""));
                    if let Some(pk) = crate::wallet::reusable_payments::scan::extract_input_pubkey(
                        &witness,
                        bitcoin::Script::from_bytes(&script_sig_bytes),
                        bitcoin::Script::from_bytes(&spk_bytes),
                    ) {
                        extracted_pubs.push(hex::encode(pk.serialize()));
                    }

                    let Some((sk, _pk)) = eligible_sk_pk(sk_hex, spk_hex, &secp) else {
                        continue;
                    };
                    let mut txid_bytes = hex::decode(txid_hex).expect("txid hex");
                    txid_bytes.reverse();
                    let mut txid_arr = [0u8; 32];
                    txid_arr.copy_from_slice(&txid_bytes);
                    let txid = bitcoin::Txid::from_raw_hash(
                        bitcoin::hashes::sha256d::Hash::from_byte_array(txid_arr),
                    );
                    inputs.push(EligibleInput {
                        outpoint: bitcoin::OutPoint::new(txid, vout),
                        secret: sk,
                    });
                }

                let expected_priv_sum = expected["input_private_key_sum"]
                    .as_str()
                    .map(|s| s.to_string());
                let expected_pubs: Vec<String> = expected["input_pub_keys"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                assert_eq!(
                    extracted_pubs, expected_pubs,
                    "case {case_idx} ({comment}, sub {sub_idx}): input_pub_keys mismatch"
                );

                let mut recipients: Vec<Recipient> = Vec::new();
                for r in recipients_v {
                    let scan_hex = r["scan_pub_key"].as_str().expect("scan_pub_key");
                    let spend_hex = r["spend_pub_key"].as_str().expect("spend_pub_key");
                    let scan = PublicKey::from_slice(&hex::decode(scan_hex).unwrap()).unwrap();
                    let spend = PublicKey::from_slice(&hex::decode(spend_hex).unwrap()).unwrap();
                    let count = r["count"].as_u64().unwrap_or(1).max(1);
                    for _ in 0..count {
                        recipients.push((
                            SilentPaymentAddress::base(scan, spend, Network::Bitcoin),
                            bitcoin::Amount::from_sat(0),
                        ));
                    }
                }

                let expected_outputs_groups: Vec<Vec<String>> = expected["outputs"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|g| {
                                g.as_array()
                                    .map(|inner| {
                                        inner
                                            .iter()
                                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                            .collect()
                                    })
                                    .unwrap_or_default()
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                if recipients.len() > 2323 {
                    skipped.push((
                        case_idx,
                        format!("{comment} (sub {sub_idx})"),
                        "K_max boundary — see `k_max_refusal` test",
                    ));
                    continue;
                }

                let all_outpoints: Vec<bitcoin::OutPoint> = vin
                    .iter()
                    .map(|v| {
                        let mut txid_bytes = hex::decode(v["txid"].as_str().unwrap_or("")).unwrap();
                        txid_bytes.reverse();
                        let mut txid_arr = [0u8; 32];
                        txid_arr.copy_from_slice(&txid_bytes);
                        let txid = bitcoin::Txid::from_raw_hash(
                            bitcoin::hashes::sha256d::Hash::from_byte_array(txid_arr),
                        );
                        let vout_u = v["vout"].as_u64().unwrap_or(0) as u32;
                        bitcoin::OutPoint::new(txid, vout_u)
                    })
                    .collect();

                let result = compute_outputs(&inputs, &all_outpoints, &recipients, &secp);

                let expects_failure = expected_outputs_groups.iter().all(|g| g.is_empty());
                if inputs.is_empty() {
                    assert!(
                        expects_failure,
                        "case {case_idx} ({comment}, sub {sub_idx}): \
                         no inputs parsed but non-empty outputs expected"
                    );
                    passed += 1;
                    continue;
                }

                let outs = match result {
                    Ok(o) => o,
                    Err(e) => {
                        if expected_outputs_groups.iter().any(|g| !g.is_empty()) {
                            panic!("case {case_idx} ({comment}): compute_outputs failed: {e}");
                        }
                        passed += 1;
                        continue;
                    }
                };

                let mut got_sorted: Vec<String> = outs
                    .iter()
                    .map(|o| {
                        let bytes = o.script_pubkey.as_bytes();
                        hex::encode(&bytes[bytes.len() - 32..])
                    })
                    .collect();
                got_sorted.sort();

                let any_match = expected_outputs_groups.iter().any(|group| {
                    let mut sorted = group.clone();
                    sorted.sort();
                    sorted == got_sorted
                });
                assert!(
                    any_match,
                    "case {case_idx} ({comment}, sub {sub_idx}): no expected \
                     output group matches our result.\n  got:      {got_sorted:?}\n  expected groups: {expected_outputs_groups:?}"
                );

                if let Some(expected_sum) = expected_priv_sum {
                    let actual_sum = sum_private_keys(&inputs).expect("sum");
                    assert_eq!(
                        hex::encode(actual_sum.secret_bytes()),
                        expected_sum,
                        "case {case_idx}: input_private_key_sum mismatch"
                    );
                }

                passed += 1;
            }
        }

        tracing::info!(
            passed,
            skipped = skipped.len(),
            "bip352_official_sending_vectors completed"
        );
        for (idx, comment, reason) in &skipped {
            tracing::info!(case = idx, %comment, reason, "skipped");
        }
        assert!(
            passed >= 25,
            "expected at least 25 sending cases to pass; got {passed}"
        );
    }

    fn decode_witness_hex(s: &str) -> bitcoin::Witness {
        if s.is_empty() {
            return bitcoin::Witness::new();
        }
        let bytes = hex::decode(s).expect("witness hex");
        use bitcoin::consensus::Decodable;
        let mut cursor = bitcoin::io::Cursor::new(&bytes);
        bitcoin::Witness::consensus_decode(&mut cursor).expect("witness decode")
    }

    fn eligible_sk_pk<C: Signing + Verification>(
        sk_hex: &str,
        spk_hex: &str,
        secp: &Secp256k1<C>,
    ) -> Option<(SecretKey, PublicKey)> {
        if sk_hex.is_empty() {
            return None;
        }
        let sk_bytes = hex::decode(sk_hex).ok()?;
        let sk = SecretKey::from_slice(&sk_bytes).ok()?;
        let spk_bytes = hex::decode(spk_hex).ok()?;

        if spk_bytes.len() == 34 && spk_bytes[0] == 0x51 && spk_bytes[1] == 0x20 {
            let pk = sk.public_key(secp);
            let (xonly, parity) = pk.x_only_public_key();
            if xonly.serialize() != spk_bytes[2..] {
                return None;
            }
            let (out_sk, out_pk) = if parity == bitcoin::secp256k1::Parity::Odd {
                let neg = sk.negate();
                (neg, neg.public_key(secp))
            } else {
                (sk, pk)
            };
            return Some((out_sk, out_pk));
        }

        if spk_bytes.len() == 25
            && spk_bytes[0] == 0x76
            && spk_bytes[1] == 0xa9
            && spk_bytes[2] == 0x14
            && spk_bytes[23] == 0x88
            && spk_bytes[24] == 0xac
        {
            let pk = sk.public_key(secp);
            use bitcoin::hashes::{Hash, hash160};
            let h = hash160::Hash::hash(&pk.serialize());
            if h.as_byte_array() != &spk_bytes[3..23] {
                return None;
            }
            return Some((sk, pk));
        }

        if spk_bytes.len() == 22 && spk_bytes[0] == 0x00 && spk_bytes[1] == 0x14 {
            let pk = sk.public_key(secp);
            use bitcoin::hashes::{Hash, hash160};
            let h = hash160::Hash::hash(&pk.serialize());
            if h.as_byte_array() != &spk_bytes[2..22] {
                return None;
            }
            return Some((sk, pk));
        }

        if spk_bytes.len() == 23
            && spk_bytes[0] == 0xa9
            && spk_bytes[1] == 0x14
            && spk_bytes[22] == 0x87
        {
            let pk = sk.public_key(secp);
            use bitcoin::hashes::{Hash, hash160};
            let mut redeem = [0u8; 22];
            redeem[0] = 0x00;
            redeem[1] = 0x14;
            redeem[2..].copy_from_slice(hash160::Hash::hash(&pk.serialize()).as_byte_array());
            let h = hash160::Hash::hash(&redeem);
            if h.as_byte_array() != &spk_bytes[2..22] {
                return None;
            }
            return Some((sk, pk));
        }

        None
    }

    #[test]
    fn k_max_refusal() {
        let secp = Secp256k1::new();
        let seed = det_seed(50);
        let (_b_scan, _b_spend, addr) =
            derive_keys_from_seed(&seed, Network::Regtest, 0, &secp).unwrap();

        let mut rng_in = bitcoin::secp256k1::rand::rngs::StdRng::seed_from_u64(51);
        let in_sk = SecretKey::new(&mut rng_in);
        let in_op = bitcoin::OutPoint::new(
            bitcoin::Txid::from_raw_hash(bitcoin::hashes::sha256d::Hash::hash(b"k_max")),
            0,
        );
        let inputs = vec![EligibleInput {
            outpoint: in_op,
            secret: in_sk,
        }];

        let recipients: Vec<Recipient> =
            std::iter::repeat_with(|| (addr.clone(), bitcoin::Amount::from_sat(0)))
                .take(2324)
                .collect();

        let result = compute_outputs(&inputs, &[in_op], &recipients, &secp);
        assert!(
            matches!(result, Err(CryptoError::TooManyOutputs)),
            "expected TooManyOutputs, got {result:?}"
        );

        let recipients_ok: Vec<Recipient> =
            std::iter::repeat_with(|| (addr.clone(), bitcoin::Amount::from_sat(0)))
                .take(2323)
                .collect();
        let result_ok = compute_outputs(&inputs, &[in_op], &recipients_ok, &secp);
        assert!(result_ok.is_ok(), "expected 2323-recipient call to succeed");
        assert_eq!(result_ok.unwrap().len(), 2323);
    }
}
