use std::collections::HashMap;

use bitcoin::{
    Amount, BlockHash, Network, OutPoint, Txid, XOnlyPublicKey,
    hashes::Hash as _,
    secp256k1::{PublicKey, Secp256k1, SecretKey, Signing, Verification},
};

use super::{
    bip47, silent_payments,
    util::{self, hash160},
};

#[derive(Clone, Debug)]
pub struct Bip47ReceiveSource {
    pub sender_payment_code: String,
    pub i: u32,
    pub version: bip47::Version,
}

pub struct Bip47ScanData {
    pub v1_notif_priv: SecretKey,
    pub v3_notif_priv: SecretKey,
    /// Own v3 payment code identifier (the `F` key we watch for).
    pub v3_identifier: [u8; 33],
    pub receive_lookup: HashMap<[u8; 20], Bip47ReceiveSource>,
    /// How many receive indices to derive per payer
    pub lookahead: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanCursor {
    pub birthday_height: u32,
    pub last_scanned_height: u32,
    pub last_scanned_block_hash: Option<BlockHash>,
}

#[derive(Clone, Copy, Debug)]
pub struct ScanStatus {
    pub tip_height: u32,
    pub last_scanned_height: u32,
    pub birthday_height: u32,
    pub catching_up: bool,
}

pub struct ScanContext {
    pub v1_notif_priv: SecretKey,
    pub v3_notif_priv: SecretKey,
    pub v1_notif_pkh: [u8; 20],
    pub v3_identifier: [u8; 33],
    pub b_scan: SecretKey,
    pub b_spend_pub: PublicKey,
    pub labels: silent_payments::LabelSet,
    pub bip47_receive_lookup: HashMap<[u8; 20], Bip47ReceiveSource>,
    pub bip47_lookahead: u32,
}

impl ScanContext {
    pub fn new<C: Signing + Verification>(
        bip47_data: Bip47ScanData,
        b_scan: SecretKey,
        b_spend_pub: PublicKey,
        labels: silent_payments::LabelSet,
        secp: &Secp256k1<C>,
    ) -> Self {
        let v1_notif_pkh = hash160(&bip47_data.v1_notif_priv.public_key(secp).serialize());
        Self {
            v1_notif_priv: bip47_data.v1_notif_priv,
            v3_notif_priv: bip47_data.v3_notif_priv,
            v1_notif_pkh,
            v3_identifier: bip47_data.v3_identifier,
            b_scan,
            b_spend_pub,
            labels,
            bip47_receive_lookup: bip47_data.receive_lookup,
            bip47_lookahead: bip47_data.lookahead,
        }
    }
}

impl Drop for ScanContext {
    fn drop(&mut self) {
        self.v1_notif_priv.non_secure_erase();
        self.v3_notif_priv.non_secure_erase();
        self.b_scan.non_secure_erase();
    }
}

pub struct ScrubOnDrop(SecretKey);

impl ScrubOnDrop {
    pub fn new(key: SecretKey) -> Self {
        Self(key)
    }
}

impl std::ops::Deref for ScrubOnDrop {
    type Target = SecretKey;
    fn deref(&self) -> &SecretKey {
        &self.0
    }
}

impl Drop for ScrubOnDrop {
    fn drop(&mut self) {
        self.0.non_secure_erase();
    }
}

#[derive(Clone, Debug)]
pub struct ScannedInput {
    pub outpoint: OutPoint,
    pub pubkey: Option<PublicKey>,
}

#[derive(Clone, Debug)]
pub struct ScannedTx {
    pub txid: Txid,
    pub inputs: Vec<ScannedInput>,
    /// First input exposing a pubkey per BIP47's designated input rules.
    pub bip47_designated: Option<(OutPoint, PublicKey)>,
    /// BIP-352 says to skip txs spending any SegWit version > 1 output
    pub spends_segwit_gt_v1: bool,
    pub taproot_outputs: Vec<(u32, XOnlyPublicKey, Amount)>,
    pub op_return_data: Vec<Vec<u8>>,
    pub p2pkh_outputs: Vec<(u32, [u8; 20], Amount)>,
    pub p2wpkh_outputs: Vec<(u32, [u8; 20], Amount)>,
    pub p2pk_outputs: Vec<(u32, [u8; 33], Amount)>,
    pub bare_multisig_1of3: Vec<(u32, [[u8; 33]; 3])>,
}

#[derive(Clone, Debug)]
pub enum ScanEvent {
    Bip47Notification {
        sender: bip47::PaymentCode,
        notification_txid: Txid,
    },
    SilentPaymentReceive {
        txid: Txid,
        match_: silent_payments::Match,
    },
    Bip47PaymentReceive {
        txid: Txid,
        vout: u32,
        amount: Amount,
        script_pubkey: bitcoin::ScriptBuf,
        source: Bip47ReceiveSource,
    },
}

pub fn scan_tx<C: Signing + Verification>(
    tx: &ScannedTx,
    ctx: &ScanContext,
    secp: &Secp256k1<C>,
) -> Result<Vec<ScanEvent>, silent_payments::CryptoError> {
    let mut events = Vec::new();

    let matches_v1 = tx
        .p2pkh_outputs
        .iter()
        .any(|(_, pkh, _)| *pkh == ctx.v1_notif_pkh);
    if matches_v1 && let Some((designated_outpoint, sender_pub)) = tx.bip47_designated {
        for op_return in &tx.op_return_data {
            if op_return.len() == 80
                && let Ok(sender) = bip47::unblind(
                    &ctx.v1_notif_priv,
                    op_return,
                    &sender_pub,
                    designated_outpoint,
                    bip47::Version::V1,
                )
            {
                events.push(ScanEvent::Bip47Notification {
                    sender,
                    notification_txid: tx.txid,
                });
                break;
            }
        }
    }

    for (_vout, keys) in &tx.bare_multisig_1of3 {
        let Some(fpos) = keys.iter().position(|k| *k == ctx.v3_identifier) else {
            continue;
        };
        let others: Vec<&[u8; 33]> = keys
            .iter()
            .enumerate()
            .filter_map(|(i, k)| (i != fpos).then_some(k))
            .collect();
        for (a_bytes, g_bytes) in [(others[0], others[1]), (others[1], others[0])] {
            let Ok(a_pub) = PublicKey::from_slice(a_bytes) else {
                continue;
            };
            if let Ok(sender) = bip47::unblind(
                &ctx.v3_notif_priv,
                g_bytes,
                &a_pub,
                OutPoint::null(),
                bip47::Version::V3,
            ) {
                events.push(ScanEvent::Bip47Notification {
                    sender,
                    notification_txid: tx.txid,
                });
                break;
            }
        }
    }

    events.extend(detect_bip47_receives(tx, &ctx.bip47_receive_lookup));

    if tx.spends_segwit_gt_v1 || tx.taproot_outputs.is_empty() {
        return Ok(events);
    }

    let mut sum_a: Option<PublicKey> = None;
    for pk in tx.inputs.iter().filter_map(|i| i.pubkey) {
        sum_a = match sum_a {
            None => Some(pk),
            Some(acc) => acc.combine(&pk).ok(),
        };
    }
    let Some(sum_a) = sum_a else {
        return Ok(events);
    };

    let outpoint_l = match util::lex_min_outpoint(tx.inputs.iter().map(|i| i.outpoint)) {
        Some(op) => op,
        None => return Ok(events),
    };

    let matches = silent_payments::scan_tx(
        &sum_a,
        outpoint_l,
        &tx.taproot_outputs,
        &ctx.b_scan,
        &ctx.b_spend_pub,
        &ctx.labels,
        secp,
    )?;

    for m in matches {
        events.push(ScanEvent::SilentPaymentReceive {
            txid: tx.txid,
            match_: m,
        });
    }

    Ok(events)
}

pub fn detect_bip47_receives(
    tx: &ScannedTx,
    lookup: &HashMap<[u8; 20], Bip47ReceiveSource>,
) -> Vec<ScanEvent> {
    let mut events = Vec::new();
    if lookup.is_empty() {
        return events;
    }
    // v1 pays P2PKH only; v3 allows P2PK, P2WPKH, and P2PKH.
    for (vout, pkh, amount) in &tx.p2pkh_outputs {
        if let Some(source) = lookup.get(pkh) {
            events.push(ScanEvent::Bip47PaymentReceive {
                txid: tx.txid,
                vout: *vout,
                amount: *amount,
                script_pubkey: p2pkh_spk(pkh),
                source: source.clone(),
            });
        }
    }
    for (vout, pkh, amount) in &tx.p2wpkh_outputs {
        if let Some(source) = lookup.get(pkh)
            && source.version == bip47::Version::V3
        {
            events.push(ScanEvent::Bip47PaymentReceive {
                txid: tx.txid,
                vout: *vout,
                amount: *amount,
                script_pubkey: p2wpkh_spk(pkh),
                source: source.clone(),
            });
        }
    }
    for (vout, key, amount) in &tx.p2pk_outputs {
        if let Some(source) = lookup.get(&hash160(key))
            && source.version == bip47::Version::V3
        {
            events.push(ScanEvent::Bip47PaymentReceive {
                txid: tx.txid,
                vout: *vout,
                amount: *amount,
                script_pubkey: p2pk_spk(key),
                source: source.clone(),
            });
        }
    }
    events
}

fn p2pkh_spk(pkh: &[u8; 20]) -> bitcoin::ScriptBuf {
    bitcoin::ScriptBuf::new_p2pkh(&bitcoin::PubkeyHash::from_raw_hash(
        bitcoin::hashes::hash160::Hash::from_byte_array(*pkh),
    ))
}

fn p2wpkh_spk(pkh: &[u8; 20]) -> bitcoin::ScriptBuf {
    bitcoin::ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_raw_hash(
        bitcoin::hashes::hash160::Hash::from_byte_array(*pkh),
    ))
}

fn p2pk_spk(key: &[u8; 33]) -> bitcoin::ScriptBuf {
    bitcoin::script::Builder::new()
        .push_slice(key)
        .push_opcode(bitcoin::opcodes::all::OP_CHECKSIG)
        .into_script()
}

pub(in crate::wallet) fn derive_bip47_lookup_entries<C: Signing + Verification>(
    root: &bitcoin::bip32::Xpriv,
    sender_code: &bip47::PaymentCode,
    start: u32,
    lookahead: u32,
    network: Network,
    secp: &Secp256k1<C>,
) -> Result<HashMap<[u8; 20], Bip47ReceiveSource>, bip47::CryptoError> {
    let sender_code_str = sender_code.to_string();
    let version = sender_code.version();
    let sender_a0 = sender_code.notification_pubkey(secp)?;
    let mut entries = HashMap::new();
    for i in start..start.saturating_add(lookahead) {
        let child = bitcoin::bip32::ChildNumber::from_normal_idx(i)
            .map_err(|_| bip47::CryptoError::IndexTooLarge { i })?;
        let b_i = match root.derive_priv(secp, &[child]) {
            Ok(x) => ScrubOnDrop::new(x.private_key),
            Err(_) => continue,
        };
        let pk = match bip47::derive_receive_pubkey(&b_i, &sender_a0, version, network, secp) {
            Ok(pk) => pk,
            Err(_) => continue,
        };
        entries.insert(
            hash160(&pk.serialize()),
            Bip47ReceiveSource {
                sender_payment_code: sender_code_str.clone(),
                i,
                version,
            },
        );
    }
    Ok(entries)
}

const NUMS_H: [u8; 32] = [
    0x50, 0x92, 0x9b, 0x74, 0xc1, 0xa0, 0x49, 0x54, 0xb7, 0x8b, 0x4b, 0x60, 0x35, 0xe9, 0x7a, 0x5e,
    0x07, 0x8a, 0x5a, 0x0f, 0x28, 0xec, 0x96, 0xd5, 0x47, 0xbf, 0xee, 0x9a, 0xce, 0x80, 0x3a, 0xc0,
];

// Port of `get_pubkey_from_input` (bip-0352 spec)
pub fn extract_input_pubkey(
    witness: &bitcoin::Witness,
    script_sig: &bitcoin::Script,
    prevout_script: &bitcoin::Script,
) -> Option<PublicKey> {
    let spk = prevout_script.as_bytes();

    if prevout_script.is_p2pkh() {
        let spk_hash = &spk[3..23];
        let sig_bytes = script_sig.as_bytes();
        if sig_bytes.len() < 33 {
            return None;
        }
        let tail = &sig_bytes[sig_bytes.len() - 33..];
        if hash160(tail) == spk_hash
            && let Ok(pk) = PublicKey::from_slice(tail)
        {
            return Some(pk);
        }
        for i in (33..sig_bytes.len()).rev() {
            let candidate = &sig_bytes[i - 33..i];
            if hash160(candidate) == spk_hash
                && let Ok(pk) = PublicKey::from_slice(candidate)
            {
                return Some(pk);
            }
        }
        return None;
    }

    if prevout_script.is_p2sh() {
        let sig_bytes = script_sig.as_bytes();
        if sig_bytes.is_empty() {
            return None;
        }
        let redeem = &sig_bytes[1..];
        let redeem_script = bitcoin::Script::from_bytes(redeem);
        if !redeem_script.is_p2wpkh() {
            return None;
        }
        let pk_bytes = witness.last()?;
        if pk_bytes.len() != 33 {
            return None;
        }
        return PublicKey::from_slice(pk_bytes).ok();
    }

    if prevout_script.is_p2wpkh() {
        let pk_bytes = witness.last()?;
        if pk_bytes.len() != 33 {
            return None;
        }
        return PublicKey::from_slice(pk_bytes).ok();
    }

    if prevout_script.is_p2tr() {
        if spk.len() != 34 {
            return None;
        }

        let mut stack: Vec<&[u8]> = witness.iter().collect();
        if stack.is_empty() {
            return None;
        }

        if stack.len() >= 2
            && let Some(last) = stack.last()
            && !last.is_empty()
            && last[0] == 0x50
        {
            stack.pop();
        }

        if stack.len() > 1 {
            let control_block = stack.last()?;
            if control_block.len() < 33 {
                return None;
            }
            let internal_key = &control_block[1..33];
            if internal_key == NUMS_H {
                return None;
            }
        }

        let xonly = XOnlyPublicKey::from_slice(&spk[2..]).ok()?;
        return Some(xonly.public_key(bitcoin::secp256k1::Parity::Even));
    }

    None
}

pub fn extract_bip47_designated_pubkey(
    witness: &bitcoin::Witness,
    script_sig: &bitcoin::Script,
    prevout_script: &bitcoin::Script,
) -> Option<PublicKey> {
    let spk = prevout_script.as_bytes();

    if prevout_script.is_p2pk() {
        return pubkey_from_first_push(prevout_script);
    }

    if let Some(keys) = parse_bare_multisig_keys(prevout_script) {
        return keys
            .into_iter()
            .find_map(|k| PublicKey::from_slice(&k).ok());
    }

    if prevout_script.is_p2pkh() {
        let spk_hash = &spk[3..23];
        return pubkey_in_script_hashing_to(script_sig, spk_hash);
    }

    if prevout_script.is_p2sh() {
        // The redeem script is the final push of the scriptSig
        let redeem_bytes =
            script_sig
                .instructions()
                .flatten()
                .fold(None, |acc, ins| match ins {
                    bitcoin::script::Instruction::PushBytes(b) => Some(b.as_bytes().to_vec()),
                    _ => acc,
                })?;
        let redeem = bitcoin::Script::from_bytes(&redeem_bytes);
        if redeem.is_p2wpkh() {
            let pk_bytes = witness.last()?;
            return PublicKey::from_slice(pk_bytes).ok();
        }
        if redeem.is_p2pk() {
            return pubkey_from_first_push(redeem);
        }
        if let Some(keys) = parse_bare_multisig_keys(redeem) {
            return keys
                .into_iter()
                .find_map(|k| PublicKey::from_slice(&k).ok());
        }
        return None;
    }

    extract_input_pubkey(witness, script_sig, prevout_script)
}

fn pubkey_from_first_push(script: &bitcoin::Script) -> Option<PublicKey> {
    for ins in script.instructions().flatten() {
        if let bitcoin::script::Instruction::PushBytes(b) = ins {
            return PublicKey::from_slice(b.as_bytes()).ok();
        }
    }
    None
}

/// Keys of a bare `OP_m <keys...> OP_n OP_CHECKMULTISIG` script
fn parse_bare_multisig_keys(script: &bitcoin::Script) -> Option<Vec<Vec<u8>>> {
    use bitcoin::{opcodes::all::OP_CHECKMULTISIG, script::Instruction};
    let instructions: Vec<Instruction> = script.instructions().collect::<Result<_, _>>().ok()?;
    let [
        Instruction::Op(m),
        mid @ ..,
        Instruction::Op(n),
        Instruction::Op(last),
    ] = instructions.as_slice()
    else {
        return None;
    };
    let opnum = |op: &bitcoin::Opcode| -> Option<u8> {
        let code = op.to_u8();
        (0x51..=0x60).contains(&code).then(|| code - 0x50)
    };
    if *last != OP_CHECKMULTISIG || opnum(m)? > opnum(n)? || mid.len() != opnum(n)? as usize {
        return None;
    }
    mid.iter()
        .map(|ins| match ins {
            Instruction::PushBytes(b) if b.as_bytes().len() == 33 || b.as_bytes().len() == 65 => {
                Some(b.as_bytes().to_vec())
            }
            _ => None,
        })
        .collect()
}

fn pubkey_in_script_hashing_to(script: &bitcoin::Script, target_pkh: &[u8]) -> Option<PublicKey> {
    let bytes = script.as_bytes();
    for len in [33usize, 65] {
        if bytes.len() < len {
            continue;
        }
        for i in (len..=bytes.len()).rev() {
            let candidate = &bytes[i - len..i];
            if hash160(candidate) == target_pkh
                && let Ok(pk) = PublicKey::from_slice(candidate)
            {
                return Some(pk);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use bitcoin::secp256k1::rand::{SeedableRng, rngs::StdRng};

    use super::*;

    fn det_secp() -> Secp256k1<bitcoin::secp256k1::All> {
        Secp256k1::new()
    }

    fn det_seed(seed: u64) -> Vec<u8> {
        let mut rng = StdRng::seed_from_u64(seed);
        use bitcoin::secp256k1::rand::Rng;
        (0..64).map(|_| rng.r#gen()).collect()
    }

    fn det_txid(seed: u64) -> Txid {
        Txid::from_raw_hash(bitcoin::hashes::sha256d::Hash::hash(
            seed.to_le_bytes().as_ref(),
        ))
    }

    fn empty_scanned_tx(seed: u64) -> ScannedTx {
        ScannedTx {
            txid: det_txid(seed),
            inputs: vec![],
            bip47_designated: None,
            spends_segwit_gt_v1: false,
            taproot_outputs: vec![],
            op_return_data: vec![],
            p2pkh_outputs: vec![],
            p2wpkh_outputs: vec![],
            p2pk_outputs: vec![],
            bare_multisig_1of3: vec![],
        }
    }
    fn bip47_party(
        seed: u64,
        version: bip47::Version,
    ) -> (bip47::PaymentCode, bitcoin::bip32::Xpriv) {
        use bitcoin::bip32::{ChildNumber, DerivationPath, Xpriv, Xpub};
        let secp = det_secp();
        let master = Xpriv::new_master(bitcoin::NetworkKind::Test, &det_seed(seed)).unwrap();
        let account = master
            .derive_priv(
                &secp,
                &DerivationPath::from(
                    [
                        ChildNumber::from_hardened_idx(47).unwrap(),
                        ChildNumber::from_hardened_idx(1).unwrap(),
                        ChildNumber::from_hardened_idx(0).unwrap(),
                    ]
                    .as_slice(),
                ),
            )
            .unwrap();
        let code = bip47::PaymentCode::from_xpub(&Xpub::from_priv(&secp, &account), version);
        let root = match version {
            bip47::Version::V1 => account,
            bip47::Version::V3 => bip47::v3_root_xpriv(&account, Network::Regtest, &secp).unwrap(),
        };
        (code, root)
    }

    fn child_priv(root: &bitcoin::bip32::Xpriv, i: u32) -> SecretKey {
        root.derive_priv(
            &det_secp(),
            &[bitcoin::bip32::ChildNumber::from_normal_idx(i).unwrap()],
        )
        .unwrap()
        .private_key
    }

    #[test]
    fn bip352_send_scan_e2e_with_mixed_inputs_change_label_and_recover() {
        let secp = det_secp();
        let seed = det_seed(77);
        let (b_scan, b_spend, base_addr) =
            silent_payments::derive_keys_from_seed(&seed, Network::Regtest, 0, &secp)
                .expect("derive keys");
        let b_spend_pub = b_spend.public_key(&secp);

        let change_addr =
            silent_payments::labeled_address(&b_scan, &b_spend, 0, Network::Regtest, &secp)
                .expect("labeled change address");

        let mut rng = StdRng::seed_from_u64(78);
        let elig_sk = SecretKey::new(&mut rng);
        let elig_pk = elig_sk.public_key(&secp);

        let inelig_outpoint = OutPoint::new(
            bitcoin::Txid::from_raw_hash(bitcoin::hashes::sha256d::Hash::from_byte_array(
                [0x01u8; 32],
            )),
            0,
        );
        let elig_outpoint = OutPoint::new(
            bitcoin::Txid::from_raw_hash(bitcoin::hashes::sha256d::Hash::from_byte_array(
                [0xFFu8; 32],
            )),
            0,
        );
        let inelig_bytes: &[u8] = inelig_outpoint.txid.as_ref();
        let elig_bytes: &[u8] = elig_outpoint.txid.as_ref();
        assert!(
            inelig_bytes < elig_bytes,
            "ineligible outpoint must be lex-smaller to exercise outpoint_L bug"
        );

        let eligible_inputs = vec![silent_payments::EligibleInput {
            outpoint: elig_outpoint,
            secret: elig_sk,
        }];
        let all_outpoints = vec![inelig_outpoint, elig_outpoint];
        let base_amount = Amount::from_sat(42_000);
        let change_amount = Amount::from_sat(8_000);
        let outs = silent_payments::compute_outputs(
            &eligible_inputs,
            &all_outpoints,
            &[
                (base_addr, base_amount),
                (change_addr.clone(), change_amount),
            ],
            &secp,
        )
        .expect("sender compute_outputs");
        assert_eq!(outs.len(), 2);

        let xonly_for = |out: &bitcoin::TxOut| {
            let bytes = out.script_pubkey.as_bytes();
            XOnlyPublicKey::from_slice(&bytes[2..]).unwrap()
        };
        let taproot_outputs: Vec<_> = outs
            .iter()
            .enumerate()
            .map(|(i, o)| (i as u32, xonly_for(o), o.value))
            .collect();

        let mut scanned_tx = empty_scanned_tx(800);
        scanned_tx.inputs = vec![
            ScannedInput {
                outpoint: inelig_outpoint,
                pubkey: None,
            },
            ScannedInput {
                outpoint: elig_outpoint,
                pubkey: Some(elig_pk),
            },
        ];
        scanned_tx.taproot_outputs = taproot_outputs;

        let mut rng = StdRng::seed_from_u64(79);
        let v1 = SecretKey::new(&mut rng);
        let v3 = SecretKey::new(&mut rng);
        let labels = silent_payments::LabelSet::with_change(&b_scan, &secp).unwrap();
        let ctx = ScanContext::new(
            Bip47ScanData {
                v1_notif_priv: v1,
                v3_notif_priv: v3,
                v3_identifier: [0x02; 33],
                receive_lookup: HashMap::new(),
                lookahead: 20,
            },
            b_scan,
            b_spend_pub,
            labels,
            &secp,
        );

        let events = scan_tx(&scanned_tx, &ctx, &secp).expect("scan_tx");
        assert_eq!(events.len(), 2, "both outputs must be found");

        let mut found_base = false;
        let mut found_change = false;
        for ev in &events {
            let ScanEvent::SilentPaymentReceive { match_, .. } = ev else {
                panic!("expected SilentPaymentReceive, got {ev:?}");
            };
            match match_.label {
                None => {
                    assert_eq!(match_.amount, base_amount);
                    found_base = true;
                }
                Some(0) => {
                    assert_eq!(match_.amount, change_amount);
                    found_change = true;
                }
                Some(other) => panic!("unexpected label m={other}"),
            }

            let cap_a = elig_pk;
            let d = silent_payments::recover_spending_key(
                &b_spend,
                &b_scan,
                &cap_a,
                inelig_outpoint,
                match_.tweak_k,
                match_.label,
                &secp,
            )
            .expect("recover_spending_key");
            let (d_xonly, _) = d.public_key(&secp).x_only_public_key();
            assert_eq!(
                d_xonly, match_.output_xonly,
                "recovered key must sign output"
            );
        }
        assert!(found_base, "base recipient match");
        assert!(found_change, "change-label recipient match");
    }

    fn bip47_receive_round_trip(version: bip47::Version) {
        let secp = det_secp();
        let (send_code, send_root) = bip47_party(700, version);
        let (recv_code, recv_root) = bip47_party(701, version);
        let alice_a0 = child_priv(&send_root, 0);

        let target_i: u32 = 3;
        let recv_b_i = child_priv(&recv_root, target_i);
        let sender_notif_pub = send_code.notification_pubkey(&secp).unwrap();
        let pk = bip47::derive_receive_pubkey(
            &recv_b_i,
            &sender_notif_pub,
            version,
            Network::Regtest,
            &secp,
        )
        .unwrap();
        let pk_via_send =
            bip47::derive_send_pubkey(&alice_a0, &recv_code, target_i, Network::Regtest, &secp)
                .unwrap();
        assert_eq!(pk, pk_via_send);
        let recv_priv =
            bip47::derive_receive_priv(&recv_b_i, &sender_notif_pub, version, Network::Regtest)
                .unwrap();
        assert_eq!(recv_priv.public_key(&secp), pk);
        let pkh = hash160(&pk.serialize());

        let mut lookup = HashMap::new();
        lookup.insert(
            pkh,
            Bip47ReceiveSource {
                sender_payment_code: send_code.to_string(),
                i: target_i,
                version,
            },
        );

        let amount = Amount::from_sat(12_345);
        let mut tx = empty_scanned_tx(710);
        match version {
            bip47::Version::V1 => tx.p2pkh_outputs = vec![(7u32, pkh, amount)],
            bip47::Version::V3 => tx.p2wpkh_outputs = vec![(7u32, pkh, amount)],
        }

        let mut rng = StdRng::seed_from_u64(720);
        let scan_priv = SecretKey::new(&mut rng);
        let spend_priv = SecretKey::new(&mut rng);
        let v1 = SecretKey::new(&mut rng);
        let v3 = SecretKey::new(&mut rng);
        let labels = silent_payments::LabelSet::with_change(&scan_priv, &secp).unwrap();
        let ctx = ScanContext::new(
            Bip47ScanData {
                v1_notif_priv: v1,
                v3_notif_priv: v3,
                v3_identifier: [0x02; 33],
                receive_lookup: lookup,
                lookahead: 20,
            },
            scan_priv,
            spend_priv.public_key(&secp),
            labels,
            &secp,
        );

        let events = scan_tx(&tx, &ctx, &secp).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ScanEvent::Bip47PaymentReceive {
                vout,
                amount: a,
                source,
                ..
            } => {
                assert_eq!(*vout, 7);
                assert_eq!(*a, amount);
                assert_eq!(source.i, target_i);
                assert_eq!(source.version, version);
                assert_eq!(source.sender_payment_code, send_code.to_string());
            }
            other => panic!("expected Bip47PaymentReceive, got {other:?}"),
        }
    }

    #[test]
    fn scan_tx_detects_bip47_receives_v1_and_v3() {
        bip47_receive_round_trip(bip47::Version::V1);
        bip47_receive_round_trip(bip47::Version::V3);
    }

    #[test]
    fn same_block_notification_then_payment_detected_via_second_pass() {
        let secp = det_secp();

        let (recv_code, recv_v3_root) = bip47_party(900, bip47::Version::V3);
        let (send_code, send_v3_root) = bip47_party(901, bip47::Version::V3);
        let recv_notif_priv = child_priv(&recv_v3_root, 0);
        let recv_notif_pub = recv_notif_priv.public_key(&secp);
        let alice_a0 = child_priv(&send_v3_root, 0);

        let mut rng = StdRng::seed_from_u64(902);
        let ephemeral_priv = SecretKey::new(&mut rng);
        let ephemeral_pub = ephemeral_priv.public_key(&secp);
        let blob = bip47::blind(
            &send_code,
            &ephemeral_priv,
            &recv_notif_pub,
            OutPoint::null(),
            &secp,
        );
        let g: [u8; 33] = blob.try_into().unwrap();
        let script =
            bip47::v3_notification_script(&ephemeral_pub.serialize(), &recv_code.identifier(), &g);
        let mut notif_tx = empty_scanned_tx(904);
        notif_tx.inputs = vec![ScannedInput {
            outpoint: OutPoint::new(det_txid(903), 0),
            pubkey: None,
        }];
        notif_tx.bare_multisig_1of3 = vec![(0, bip47::parse_1of3_multisig(&script).unwrap())];

        let pay_pk =
            bip47::derive_send_pubkey(&alice_a0, &recv_code, 0, Network::Regtest, &secp).unwrap();
        let pay_pkh = hash160(&pay_pk.serialize());
        let amount = Amount::from_sat(50_000);
        let mut payment_tx = empty_scanned_tx(905);
        payment_tx.p2wpkh_outputs = vec![(1, pay_pkh, amount)];

        let b_scan = SecretKey::new(&mut rng);
        let b_spend = SecretKey::new(&mut rng);
        let v1_notif = SecretKey::new(&mut rng);
        let labels = silent_payments::LabelSet::with_change(&b_scan, &secp).unwrap();
        let ctx = ScanContext::new(
            Bip47ScanData {
                v1_notif_priv: v1_notif,
                v3_notif_priv: recv_notif_priv,
                v3_identifier: recv_code.identifier(),
                receive_lookup: HashMap::new(),
                lookahead: 20,
            },
            b_scan,
            b_spend.public_key(&secp),
            labels,
            &secp,
        );

        let mut pass1 = Vec::new();
        for tx in [&notif_tx, &payment_tx] {
            pass1.extend(scan_tx(tx, &ctx, &secp).unwrap());
        }
        let notif_events: Vec<_> = pass1
            .iter()
            .filter(|e| matches!(e, ScanEvent::Bip47Notification { .. }))
            .collect();
        assert_eq!(notif_events.len(), 1, "pass 1 must find the notification");
        assert!(
            !pass1
                .iter()
                .any(|e| matches!(e, ScanEvent::Bip47PaymentReceive { .. })),
            "pass 1 must miss the same-block payment (lookup is empty)"
        );

        let ScanEvent::Bip47Notification { sender, .. } = notif_events[0] else {
            unreachable!()
        };
        assert_eq!(sender.version(), bip47::Version::V3);
        assert_eq!(sender.pubkey(), send_code.pubkey());
        let entries =
            derive_bip47_lookup_entries(&recv_v3_root, sender, 0, 20, Network::Regtest, &secp)
                .expect("derive entries for newly notified payer");
        assert_eq!(entries.len(), 20);

        for txs in [[&notif_tx, &payment_tx], [&payment_tx, &notif_tx]] {
            let mut recovered = Vec::new();
            for tx in txs {
                recovered.extend(detect_bip47_receives(tx, &entries));
            }
            assert_eq!(recovered.len(), 1, "pass 2 must find the payment");
            match &recovered[0] {
                ScanEvent::Bip47PaymentReceive {
                    vout,
                    amount: a,
                    source,
                    ..
                } => {
                    assert_eq!(*vout, 1);
                    assert_eq!(*a, amount);
                    assert_eq!(source.i, 0);
                    assert_eq!(source.version, bip47::Version::V3);
                    assert_eq!(source.sender_payment_code, send_code.to_string());
                }
                other => panic!("expected Bip47PaymentReceive, got {other:?}"),
            }
        }
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

    fn outpoint_from_vector(txid_hex: &str, vout: u32) -> OutPoint {
        let mut txid_bytes = hex::decode(txid_hex).expect("txid hex");
        txid_bytes.reverse();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&txid_bytes);
        OutPoint::new(
            Txid::from_raw_hash(bitcoin::hashes::sha256d::Hash::from_byte_array(arr)),
            vout,
        )
    }

    #[test]
    fn bip352_official_receiving_vectors() {
        const VECTORS: &str = include_str!("silent_payments_test_vectors.json");
        let cases: serde_json::Value = serde_json::from_str(VECTORS).expect("parse vectors JSON");
        let cases = cases.as_array().expect("vectors is an array");
        let secp = det_secp();

        let mut subcases = 0usize;
        for (case_idx, case) in cases.iter().enumerate() {
            let comment = case["comment"].as_str().unwrap_or("");
            let Some(receiving) = case["receiving"].as_array() else {
                continue;
            };
            for (sub_idx, recv) in receiving.iter().enumerate() {
                subcases += 1;
                let given = &recv["given"];
                let expected = &recv["expected"];
                let ctx_label = format!("case {case_idx} ({comment}, sub {sub_idx})");

                let b_scan = SecretKey::from_slice(
                    &hex::decode(given["key_material"]["scan_priv_key"].as_str().unwrap()).unwrap(),
                )
                .unwrap();
                let b_spend = SecretKey::from_slice(
                    &hex::decode(given["key_material"]["spend_priv_key"].as_str().unwrap())
                        .unwrap(),
                )
                .unwrap();
                let b_spend_pub = b_spend.public_key(&secp);

                let mut labels = silent_payments::LabelSet::with_change(&b_scan, &secp).unwrap();
                let label_ms: Vec<u32> = given["labels"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_u64())
                            .map(|m| m as u32)
                            .collect()
                    })
                    .unwrap_or_default();
                for m in &label_ms {
                    labels.add(&b_scan, *m, &secp).unwrap();
                }

                let base = silent_payments::SilentPaymentAddress::base(
                    b_scan.public_key(&secp),
                    b_spend_pub,
                    Network::Bitcoin,
                );
                let mut our_addresses = vec![base.to_string()];
                for m in &label_ms {
                    our_addresses.push(
                        silent_payments::labeled_address(
                            &b_scan,
                            &b_spend,
                            *m,
                            Network::Bitcoin,
                            &secp,
                        )
                        .unwrap()
                        .to_string(),
                    );
                }
                our_addresses.sort();
                let mut expected_addresses: Vec<String> = expected["addresses"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                expected_addresses.sort();
                assert_eq!(our_addresses, expected_addresses, "{ctx_label}: addresses");

                let mut inputs = Vec::new();
                let mut spends_segwit_gt_v1 = false;
                for v in given["vin"].as_array().unwrap() {
                    let outpoint = outpoint_from_vector(
                        v["txid"].as_str().unwrap(),
                        v["vout"].as_u64().unwrap() as u32,
                    );
                    let script_sig_bytes =
                        hex::decode(v["scriptSig"].as_str().unwrap_or("")).unwrap();
                    let spk_bytes =
                        hex::decode(v["prevout"]["scriptPubKey"]["hex"].as_str().unwrap()).unwrap();
                    let spk = bitcoin::Script::from_bytes(&spk_bytes);
                    let witness = decode_witness_hex(v["txinwitness"].as_str().unwrap_or(""));
                    if let Some(version) = spk.witness_version()
                        && version.to_num() > 1
                    {
                        spends_segwit_gt_v1 = true;
                    }
                    let pubkey = extract_input_pubkey(
                        &witness,
                        bitcoin::Script::from_bytes(&script_sig_bytes),
                        spk,
                    );
                    inputs.push(ScannedInput { outpoint, pubkey });
                }

                // Invalid x-only pubkeys are skipped, as in block_to_scanned_txs.
                let taproot_outputs: Vec<(u32, XOnlyPublicKey, Amount)> = given["outputs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .filter_map(|(i, o)| {
                        let xonly =
                            XOnlyPublicKey::from_slice(&hex::decode(o.as_str().unwrap()).unwrap())
                                .ok()?;
                        Some((i as u32, xonly, Amount::from_sat(0)))
                    })
                    .collect();

                let outpoint_l = util::lex_min_outpoint(inputs.iter().map(|i| i.outpoint));
                let mut sum_a: Option<PublicKey> = None;
                for pk in inputs.iter().filter_map(|i| i.pubkey) {
                    sum_a = match sum_a {
                        None => Some(pk),
                        Some(acc) => acc.combine(&pk).ok(),
                    };
                }

                let mut tx = empty_scanned_tx(9000 + case_idx as u64);
                tx.inputs = inputs;
                tx.spends_segwit_gt_v1 = spends_segwit_gt_v1;
                tx.taproot_outputs = taproot_outputs;

                let mut rng = StdRng::seed_from_u64(31337);
                let ctx = ScanContext::new(
                    Bip47ScanData {
                        v1_notif_priv: SecretKey::new(&mut rng),
                        v3_notif_priv: SecretKey::new(&mut rng),
                        v3_identifier: [0x02; 33],
                        receive_lookup: HashMap::new(),
                        lookahead: 20,
                    },
                    b_scan,
                    b_spend_pub,
                    labels,
                    &secp,
                );

                let events = scan_tx(&tx, &ctx, &secp).expect(&ctx_label);
                let mut found: Vec<String> = Vec::new();
                for ev in &events {
                    let ScanEvent::SilentPaymentReceive { match_, .. } = ev else {
                        panic!("{ctx_label}: unexpected event {ev:?}");
                    };
                    found.push(hex::encode(match_.output_xonly.serialize()));

                    let d = silent_payments::recover_spending_key(
                        &b_spend,
                        &b_scan,
                        &sum_a.expect("matches imply a non-zero input pubkey sum"),
                        outpoint_l.expect("matches imply at least one input"),
                        match_.tweak_k,
                        match_.label,
                        &secp,
                    )
                    .expect(&ctx_label);
                    assert_eq!(
                        d.public_key(&secp).x_only_public_key().0,
                        match_.output_xonly,
                        "{ctx_label}: recovered key mismatch"
                    );
                }
                found.sort();
                match expected["outputs"].as_array() {
                    Some(outputs) => {
                        let mut expected_outputs: Vec<String> = outputs
                            .iter()
                            .filter_map(|o| o["pub_key"].as_str().map(|s| s.to_string()))
                            .collect();
                        expected_outputs.sort();
                        assert_eq!(found, expected_outputs, "{ctx_label}: found outputs");
                    }
                    None => {
                        let n = expected["n_outputs"].as_u64().unwrap() as usize;
                        assert_eq!(found.len(), n, "{ctx_label}: n_outputs");
                    }
                }
            }
        }
        assert!(
            subcases >= 29,
            "expected >= 29 receiving subcases, ran {subcases}"
        );
    }

    #[test]
    fn bip47_designated_pubkey_covers_p2pk_and_bare_multisig() {
        let secp = det_secp();
        let mut rng = StdRng::seed_from_u64(4747);
        let sk = SecretKey::new(&mut rng);
        let pk = sk.public_key(&secp);
        let other = SecretKey::new(&mut rng).public_key(&secp);
        let empty_sig = bitcoin::ScriptBuf::new();
        let empty_wit = bitcoin::Witness::new();

        let p2pk = p2pk_spk(&pk.serialize());
        assert_eq!(
            extract_bip47_designated_pubkey(&empty_wit, &empty_sig, &p2pk),
            Some(pk)
        );
        assert_eq!(extract_input_pubkey(&empty_wit, &empty_sig, &p2pk), None);

        let multisig = bitcoin::script::Builder::new()
            .push_opcode(bitcoin::opcodes::all::OP_PUSHNUM_1)
            .push_slice(pk.serialize())
            .push_slice(other.serialize())
            .push_opcode(bitcoin::opcodes::all::OP_PUSHNUM_2)
            .push_opcode(bitcoin::opcodes::all::OP_CHECKMULTISIG)
            .into_script();
        assert_eq!(
            extract_bip47_designated_pubkey(&empty_wit, &empty_sig, &multisig),
            Some(pk)
        );
        assert_eq!(
            extract_input_pubkey(&empty_wit, &empty_sig, &multisig),
            None
        );

        let uncompressed = pk.serialize_uncompressed();
        let pkh = hash160(&uncompressed);
        let p2pkh = p2pkh_spk(&pkh);
        let script_sig = bitcoin::script::Builder::new()
            .push_slice([0u8; 71]) // placeholder signature
            .push_slice(uncompressed)
            .into_script();
        assert_eq!(
            extract_bip47_designated_pubkey(&empty_wit, &script_sig, &p2pkh),
            Some(pk)
        );

        let mut witness = bitcoin::Witness::new();
        witness.push([0u8; 71]);
        witness.push(pk.serialize());
        let p2wpkh = p2wpkh_spk(&hash160(&pk.serialize()));
        assert_eq!(
            extract_bip47_designated_pubkey(&witness, &empty_sig, &p2wpkh),
            Some(pk)
        );
    }
}
