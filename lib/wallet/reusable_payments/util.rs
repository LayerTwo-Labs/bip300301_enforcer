use bitcoin::{
    Network, OutPoint,
    hashes::Hash as _,
    secp256k1::{PublicKey, Scalar, Secp256k1, Verification},
};

pub(super) enum TweakError {
    InvalidScalar,
    PointAtInfinity,
}

pub(super) fn add_tweak_pub<C: Verification>(
    secp: &Secp256k1<C>,
    b: &PublicKey,
    t: &[u8; 32],
) -> Result<PublicKey, TweakError> {
    let scalar = Scalar::from_be_bytes(*t).map_err(|_| TweakError::InvalidScalar)?;
    b.add_exp_tweak(secp, &scalar)
        .map_err(|_| TweakError::PointAtInfinity)
}

pub(super) fn bip44_coin_type(network: Network) -> u32 {
    match network {
        Network::Bitcoin => 0,
        _ => 1,
    }
}

pub(super) fn hash160(data: &[u8]) -> [u8; 20] {
    bitcoin::hashes::hash160::Hash::hash(data).to_byte_array()
}

pub(super) fn serialize_outpoint(op: OutPoint) -> [u8; 36] {
    let mut out = [0u8; 36];
    out[0..32].copy_from_slice(op.txid.as_ref());
    out[32..36].copy_from_slice(&op.vout.to_le_bytes());
    out
}

pub(super) fn lex_min_outpoint(iter: impl IntoIterator<Item = OutPoint>) -> Option<OutPoint> {
    iter.into_iter()
        .map(|op| (serialize_outpoint(op), op))
        .min_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, op)| op)
}
