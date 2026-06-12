use bitcoin::{
    OutPoint,
    secp256k1::{PublicKey, Scalar, Secp256k1, Verification},
};

pub(crate) enum TweakError {
    InvalidScalar,
    PointAtInfinity,
}

pub(crate) fn add_tweak_pub<C: Verification>(
    secp: &Secp256k1<C>,
    b: &PublicKey,
    t: &[u8; 32],
) -> Result<PublicKey, TweakError> {
    let scalar = Scalar::from_be_bytes(*t).map_err(|_| TweakError::InvalidScalar)?;
    b.add_exp_tweak(secp, &scalar)
        .map_err(|_| TweakError::PointAtInfinity)
}

pub(crate) fn serialize_outpoint(op: OutPoint) -> [u8; 36] {
    let mut out = [0u8; 36];
    out[0..32].copy_from_slice(op.txid.as_ref());
    out[32..36].copy_from_slice(&op.vout.to_le_bytes());
    out
}
