//! Drivechain (BIP 300/301) soft-fork rule adapter.
//!
//! Full M5–M8 / treasury validation still lives in `BlockHandler`
//! (`validate_tx_drivechain_on_child`, `handle_transaction`). This module
//! provides:
//! * [`DrivechainRule`] — fail-closed [`SoftForkRule`] for Local/Remote shells
//! * ballot helpers so `BlockHandler::validate_tx` composes via
//!   [`super::RuleEngine::decide`] with the same AND semantics as today
//!
//! SoftForkRule **never soft-Accepts** production payloads without context:
//! * missing tx/block → `missing_tx` / `missing_block`
//! * missing [`DrivechainStateSnapshot`] → `missing_drivechain_state`
//! * with snapshot **present** → runs **independent snapshot checks** that Local
//!   also enforces (no dual-AND brick of valid Local-Accept paths):
//!   * M8 `prev_mainchain_block_hash` must equal snapshot `tip_hash_hex`
//!     (same as BlockHandler `BmmRequestExpired`)
//!   * multiple OP_DRIVECHAIN outputs for the **same active** sidechain number
//!     → Reject (same as `MultipleOpDrivechainOutputs`)
//!   * **current-ctip** structural M5/M6 (spend/replace, zero-diff, missing
//!     deposit address, M6 input/vout shape) when `ctips` is populated
//!   * **pending M6id** match + `vote_count > withdrawal_bundle_inclusion_threshold`
//!     (Path A extract of `pending_m6ids`)
//!   * Connect path: coinbase M7 map + M8 must match (accepted_bmm block-local);
//!     **sequential ctip + pending fold** across block txs (mirror Local apply
//!     order) so same-slot chained M5/M6 in one block does not false-Reject
//!     under dual-AND
//!   * plain txs and other DC markers that pass those checks → Accept
//! * Inactive-slot OP_DRIVECHAIN remains Accept (mainchain anyone-can-spend;
//!   Local ignores as CTIP).
//!
//! Residual Path B (Local dual-AND only — intentional SoftForkRule Accept):
//! * historical `ctip_outpoint_to_value_seq` (unbounded over chain life — not on
//!   wire; cap/paginate would omit Local-Reject spends → dishonest independent
//!   authority)
//! * full M1–M4 proposal/ack DB (partial extract dual-AND-unsafe)
//!
//! Worker capability: `validation=drivechain-ctip-m8-pending-m6-checks-on-wire`
//! (allowlisted real). No wire expansion this loop → no capability rename.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    str::FromStr,
};

use bitcoin::{Amount, OutPoint, Transaction, Txid};

use super::{
    DrivechainCtipSnapshot, DrivechainStateSnapshot, RuleBallot, RuleBlockContext, RuleCallError,
    RuleId, RuleTxContext, RuleVote, SoftForkRule,
};
use crate::messages::{
    CoinbaseMessage, M8BmmRequest, compute_m6id, parse_m8_tx, parse_op_drivechain,
    try_parse_op_return_address,
};

/// Stable id used in ballots, IPC handshake, and logs.
pub const RULE_ID: RuleId = "drivechain";

/// In-process drivechain rule (Local backend / worker shell).
///
/// When [`DrivechainStateSnapshot`] is present, SoftForkRule runs independent
/// Path A checks then Accepts remaining payloads:
/// * M8 tip match against `tip_hash_hex`
/// * multi OP_DRIVECHAIN on the same *active* sidechain
/// * current-ctip structural M5/M6 (`ctips`)
/// * pending M6id match + vote threshold (`pending_m6ids`)
/// * connect: coinbase M7 → accepted_bmm M8 + sequential ctip/pending fold
///
/// Path B residual (historical `ctip_outpoint_to_value_seq`, M1–M4 proposal DB)
/// SoftForkRule **Accepts** when Path A checks pass — Local dual-AND remains
/// authority. Hub Local ballots from [`ballot_from_validation_ok`] after full
/// DB paths still AND with remote.
#[derive(Debug, Default, Clone, Copy)]
pub struct DrivechainRule;

impl DrivechainRule {
    pub fn new() -> Self {
        Self
    }

    /// Convert a BlockHandler drivechain boolean into a consent ballot.
    pub fn ballot_from_validation_ok(ok: bool) -> RuleBallot {
        RuleBallot::from_ok(RULE_ID, ok, "drivechain mempool validation failed")
    }

    /// Connect-level helper (tests / future extraction).
    pub fn ballot_connect_ok(ok: bool, reason: impl Into<String>) -> RuleBallot {
        RuleBallot::from_ok(RULE_ID, ok, reason)
    }
}

/// Test/helper inventory of **drivechain-related** script markers (M8, OP_DRIVECHAIN,
/// coinbase M1–M4/M7). Used by unit tests to flag txs/blocks that touch BIP 300/301
/// surfaces — **not** a claim that full LMDB is required, and **not** equivalent to
/// Path B residual-only inventory.
///
/// Path A SoftForkRule already independent-Rejects M8 tip, multi OP_DRIVECHAIN,
/// current-ctip M5/M6, pending M6id/votes, and connect M7/M8 when snapshot fields
/// are present. True Path B residuals (historical `ctip_outpoint_to_value_seq`,
/// full M1–M4 proposal/ack DB) remain Local dual-AND even when this helper returns
/// `Some`. Production SoftForkRule does not soft-Accept or brick based on these
/// strings.
pub fn drivechain_db_required_reason_tx(tx: &Transaction) -> Option<&'static str> {
    if let Some(out0) = tx.output.first() {
        let script = out0.script_pubkey.as_bytes();
        if M8BmmRequest::parse(script).is_ok() {
            // Inventory only — Path A SoftForkRule may independent-Reject M8 tip.
            return Some("drivechain_residual_marker_m8");
        }
    }
    for o in &tx.output {
        let bytes = o.script_pubkey.as_bytes();
        if parse_op_drivechain(bytes).is_ok() {
            // Inventory only — Path A covers current-ctip structure; historical
            // spend detection remains Local dual-AND (Path B).
            return Some("drivechain_residual_marker_m5_or_m6");
        }
        if CoinbaseMessage::parse(&o.script_pubkey).is_ok() {
            // Inventory only — M7 is Path A (connect); M1–M4 proposal DB is Path B.
            return Some("drivechain_residual_marker_coinbase_message");
        }
    }
    None
}

/// Scan a block for drivechain-related inventory markers (see
/// [`drivechain_db_required_reason_tx`]).
pub fn drivechain_db_required_reason_block(block: &bitcoin::Block) -> Option<&'static str> {
    for tx in &block.txdata {
        if let Some(reason) = drivechain_db_required_reason_tx(tx) {
            return Some(reason);
        }
    }
    None
}

type CtipBySidechain = HashMap<u8, (OutPoint, u64)>;
type CtipByOutpoint = HashMap<OutPoint, (u8, u64)>;

/// Parse snapshot ctips into lookup maps. Malformed txid hex → Reject reason.
fn parse_ctip_maps(
    state: &DrivechainStateSnapshot,
) -> Result<(CtipBySidechain, CtipByOutpoint), String> {
    let mut by_sc: CtipBySidechain = HashMap::new();
    let mut by_op: CtipByOutpoint = HashMap::new();
    for c in &state.ctips {
        let txid = Txid::from_str(&c.outpoint_txid_hex)
            .map_err(|_| format!("malformed_ctip_txid_sidechain_{}", c.sidechain_number))?;
        let op = OutPoint {
            txid,
            vout: c.outpoint_vout,
        };
        by_sc.insert(c.sidechain_number, (op, c.value_sats));
        by_op.insert(op, (c.sidechain_number, c.value_sats));
    }
    Ok((by_sc, by_op))
}

/// Structural M5/M6 checks against **current** ctip snapshot + Path A pending
/// M6ids / vote threshold. No historical `ctip_outpoint_to_value_seq`. Dual-AND
/// safe: only Rejects paths Local also Rejects.
fn independent_ctip_reject_tx(tx: &Transaction, state: &DrivechainStateSnapshot) -> Option<String> {
    let (ctip_by_sc, outpoint_to_sc) = match parse_ctip_maps(state) {
        Ok(m) => m,
        Err(reason) => return Some(reason),
    };

    let active: HashSet<u8> = state.active_sidechain_numbers.iter().copied().collect();

    // Current-ctip spends in this tx.
    let mut spent: HashMap<u8, u64> = HashMap::new();
    for input in &tx.input {
        if let Some(&(sc, val)) = outpoint_to_sc.get(&input.previous_output) {
            spent.insert(sc, val);
        }
    }

    // New OP_DRIVECHAIN outputs for *active* slots only (inactive = anyone-can-spend).
    let mut new_ctips: HashMap<u8, (u32, u64)> = HashMap::new();
    for (vout, output) in tx.output.iter().enumerate() {
        if let Ok((_rest, sc)) = parse_op_drivechain(output.script_pubkey.as_bytes()) {
            let n: u8 = sc.into();
            if !active.contains(&n) {
                continue;
            }
            if new_ctips
                .insert(n, (vout as u32, output.value.to_sat()))
                .is_some()
            {
                return Some(format!("multiple_op_drivechain_sidechain_{n}"));
            }
        }
    }

    // BIP 300: spending a treasury UTXO requires a replacement treasury output.
    for sc in spent.keys() {
        if !new_ctips.contains_key(sc) {
            return Some(format!("treasury_spent_without_new_ctip_{sc}"));
        }
    }

    let mut saw_m5 = false;
    let mut saw_m6 = false;
    // Captured for pending-M6id check after shape classification.
    let mut m6_old_val: Option<(u8, u64)> = None;

    for (sc, (vout, new_val)) in &new_ctips {
        let old_val = if let Some(&spent_val) = spent.get(sc) {
            spent_val
        } else if ctip_by_sc.contains_key(sc) {
            // Existing current ctip must be spent (Local OldCtipUnspent).
            return Some(format!("old_ctip_unspent_{sc}"));
        } else {
            0 // first deposit for this slot
        };

        match new_val.cmp(&old_val) {
            Ordering::Equal => return Some(format!("ctip_zero_diff_{sc}")),
            Ordering::Greater => {
                saw_m5 = true;
                // Deposit requires OP_RETURN address immediately after treasury.
                let addr_ok = tx
                    .output
                    .get((*vout as usize) + 1)
                    .and_then(|o| try_parse_op_return_address(&o.script_pubkey))
                    .is_some();
                if !addr_ok {
                    return Some(format!("missing_deposit_address_{sc}"));
                }
            }
            Ordering::Less => {
                saw_m6 = true;
                m6_old_val = Some((*sc, old_val));
                // M6 shape checks after multi/ambiguous classification (below).
            }
        }
    }

    if saw_m5 && saw_m6 {
        return Some("m5_m6_ambiguous".into());
    }
    // Local InvalidM6::TreasuryOutputCount — more than one treasury on M6 path.
    // Checked before single-M6 input/vout shape so multi-spend multi-M6 is not
    // shadowed by m6_invalid_input_count (Local also Rejects; dual-AND safe).
    if saw_m6 && new_ctips.len() > 1 {
        return Some("m6_multiple_treasury_outputs".into());
    }
    if saw_m6 {
        // Single M6: Local InvalidM6 input count + treasury vout 0.
        let (sc, (vout, _)) = new_ctips
            .iter()
            .next()
            .expect("saw_m6 implies non-empty new_ctips");
        if tx.input.len() != 1 {
            return Some(format!("m6_invalid_input_count_{sc}"));
        }
        if *vout != 0 {
            return Some(format!("m6_treasury_vout_nonzero_{sc}"));
        }

        // Path A: pending M6id + vote threshold (Local handle_m6).
        let (_, old_val) = m6_old_val.expect("saw_m6 records old treasury value");
        if let Some(reason) = independent_m6_pending_reject(tx, *sc, old_val, state) {
            return Some(reason);
        }
    }

    None
}

/// Path A: M6 must match a pending m6id with sufficient votes (Local
/// `MissingPendingWithdrawal` / `InsufficientVoteCount`). Dual-AND safe.
fn independent_m6_pending_reject(
    tx: &Transaction,
    sidechain: u8,
    old_treasury_sats: u64,
    state: &DrivechainStateSnapshot,
) -> Option<String> {
    let (m6id, sc_num) = match compute_m6id(tx.clone(), Amount::from_sat(old_treasury_sats)) {
        Ok(v) => v,
        Err(_) => return Some(format!("m6_compute_m6id_failed_{sidechain}")),
    };
    let sc_u8: u8 = sc_num.into();
    if sc_u8 != sidechain {
        // Same invariant as Local assert after parse_op_drivechain / compute_m6id.
        return Some(format!("m6_sidechain_mismatch_{sidechain}"));
    }
    // Display hex of blinded M6id (Txid) — same encoding as hub snapshot.
    let m6id_hex = m6id.0.to_string();
    let info = state
        .pending_m6ids
        .iter()
        .find(|p| p.sidechain_number == sidechain && p.m6id_hex.eq_ignore_ascii_case(&m6id_hex));
    match info {
        None => Some(format!("m6_missing_pending_withdrawal_{sidechain}")),
        Some(p) if p.vote_count <= state.withdrawal_bundle_inclusion_threshold => {
            Some(format!("m6_insufficient_vote_count_{sidechain}"))
        }
        Some(_) => None,
    }
}

/// Connect-path accepted_bmm: build M7 map from coinbase; M8 must match.
/// Block-local — no LMDB. Mempool SoftForkRule does not call this (Local also
/// skips NotAcceptedByMiners when `accepted_bmm_requests` is None).
fn independent_m7_m8_reject_block(block: &bitcoin::Block) -> Option<String> {
    let coinbase = block.txdata.first()?;
    let mut accepted: HashMap<u8, crate::types::BmmCommitment> = HashMap::new();
    for o in &coinbase.output {
        if let Ok((_, CoinbaseMessage::M7BmmAccept(m7))) = CoinbaseMessage::parse(&o.script_pubkey)
        {
            let sc: u8 = m7.sidechain_number.into();
            if accepted.insert(sc, m7.sidechain_block_hash).is_some() {
                return Some(format!("multiple_m7_bmm_sidechain_{sc}"));
            }
        }
    }
    for tx in block.txdata.iter().skip(1) {
        if let Some(m8) = parse_m8_tx(tx) {
            let sc: u8 = m8.sidechain_number.into();
            match accepted.get(&sc) {
                Some(c) if *c == m8.sidechain_block_hash => {}
                _ => return Some(format!("m8_not_accepted_by_miners_{sc}")),
            }
        }
    }
    None
}

/// Independent SoftForkRule checks enabled by the snapshot (no full LMDB).
///
/// Returns `Some(Reject reason)` when the remote can fail-closed without Local.
/// Returns `None` when checks pass (caller may Accept).
pub fn independent_snapshot_reject_tx(
    tx: &Transaction,
    state: &DrivechainStateSnapshot,
) -> Option<String> {
    // M8: prev mainchain hash must match tip (BlockHandler BmmRequestExpired).
    if let Some(out0) = tx.output.first()
        && let Ok((_rest, m8)) = M8BmmRequest::parse(out0.script_pubkey.as_bytes())
    {
        let prev_hex = m8.prev_mainchain_block_hash.to_string();
        if state.tip_hash_hex.is_empty() {
            return Some("m8_missing_tip_hash".into());
        }
        if prev_hex != state.tip_hash_hex {
            return Some("m8_prev_mainchain_mismatch".into());
        }
        // M8 tip matches — mempool path Accepts here (M7 match is connect-only).
    }

    // Multiple OP_DRIVECHAIN for the same *active* sidechain → Reject
    // (matches HandleM5M6::MultipleOpDrivechainOutputs). Inactive slots are
    // ordinary anyone-can-spend outputs on mainchain — do not Reject.
    let active: HashSet<u8> = state.active_sidechain_numbers.iter().copied().collect();
    if !active.is_empty() {
        let mut counts: HashMap<u8, usize> = HashMap::new();
        for o in &tx.output {
            if let Ok((_rest, sc)) = parse_op_drivechain(o.script_pubkey.as_bytes()) {
                let n: u8 = sc.into();
                if active.contains(&n) {
                    let c = counts.entry(n).or_insert(0);
                    *c += 1;
                    if *c > 1 {
                        return Some(format!("multiple_op_drivechain_sidechain_{n}"));
                    }
                }
            }
        }
    }

    // Current-ctip structural M5/M6 (bounded wire; not full treasury history).
    if let Some(reason) = independent_ctip_reject_tx(tx, state) {
        return Some(reason);
    }

    None
}

/// After a treasury-updating tx Accepts, fold its new OP_DRIVECHAIN outputs into
/// the running ctip map so later txs in the same block see mid-block treasuries
/// (mirrors Local sequential `put_ctip` between txs). Parent-tip snapshot is the
/// starting point for the first touch.
fn fold_ctips_after_accepted_tx(state: &mut DrivechainStateSnapshot, tx: &Transaction) {
    let active: HashSet<u8> = state.active_sidechain_numbers.iter().copied().collect();
    if active.is_empty() {
        return;
    }
    let txid = tx.compute_txid();
    let mut updated = false;
    for (vout, output) in tx.output.iter().enumerate() {
        if let Ok((_rest, sc)) = parse_op_drivechain(output.script_pubkey.as_bytes()) {
            let n: u8 = sc.into();
            if !active.contains(&n) {
                continue;
            }
            let entry = DrivechainCtipSnapshot {
                sidechain_number: n,
                outpoint_txid_hex: txid.to_string(),
                outpoint_vout: vout as u32,
                value_sats: output.value.to_sat(),
            };
            if let Some(slot) = state.ctips.iter_mut().find(|c| c.sidechain_number == n) {
                *slot = entry;
            } else {
                state.ctips.push(entry);
            }
            updated = true;
        }
    }
    if updated {
        state.ctips.sort_by_key(|c| c.sidechain_number);
    }
}

/// After an accepted M6, remove its pending m6id (mirrors Local
/// `delete_pending_withdrawal`). Must run against **pre-fold** ctips so old
/// treasury value is still available for `compute_m6id`.
fn fold_pending_m6ids_after_accepted_tx(state: &mut DrivechainStateSnapshot, tx: &Transaction) {
    let Ok((_, outpoint_to_sc)) = parse_ctip_maps(state) else {
        return;
    };
    let active: HashSet<u8> = state.active_sidechain_numbers.iter().copied().collect();
    if active.is_empty() {
        return;
    }

    let mut spent: HashMap<u8, u64> = HashMap::new();
    for input in &tx.input {
        if let Some(&(sc, val)) = outpoint_to_sc.get(&input.previous_output) {
            spent.insert(sc, val);
        }
    }
    let mut new_vals: HashMap<u8, u64> = HashMap::new();
    for output in &tx.output {
        if let Ok((_rest, sc)) = parse_op_drivechain(output.script_pubkey.as_bytes()) {
            let n: u8 = sc.into();
            if active.contains(&n) {
                new_vals.insert(n, output.value.to_sat());
            }
        }
    }
    // At most one M6 per tx after independent checks.
    for (sc, new_val) in &new_vals {
        let Some(&old_val) = spent.get(sc) else {
            continue;
        };
        if *new_val >= old_val {
            continue; // M5 or zero-diff (zero-diff already Rejected)
        }
        let Ok((m6id, _)) = compute_m6id(tx.clone(), Amount::from_sat(old_val)) else {
            continue;
        };
        let m6id_hex = m6id.0.to_string();
        state
            .pending_m6ids
            .retain(|p| !(p.sidechain_number == *sc && p.m6id_hex.eq_ignore_ascii_case(&m6id_hex)));
        break;
    }
}

/// Independent SoftForkRule checks for a full block (any tx fails → Reject).
///
/// Walks txs in order with a **running** ctip + pending map: starts from
/// parent-tip snapshot, and after each Accept folds active OP_DRIVECHAIN outputs
/// and removes spent pending M6ids so same-slot chained M5/M6 matches Local
/// sequential apply. Also enforces connect-path M7 → accepted_bmm for M8
/// (block-local).
pub fn independent_snapshot_reject_block(
    block: &bitcoin::Block,
    state: &DrivechainStateSnapshot,
) -> Option<String> {
    let mut running = state.clone();
    for tx in &block.txdata {
        if let Some(reason) = independent_snapshot_reject_tx(tx, &running) {
            return Some(reason);
        }
        // Pending fold uses pre-ctip-fold treasuries; then advance ctips.
        fold_pending_m6ids_after_accepted_tx(&mut running, tx);
        fold_ctips_after_accepted_tx(&mut running, tx);
    }
    if let Some(reason) = independent_m7_m8_reject_block(block) {
        return Some(reason);
    }
    None
}

/// SoftForkRule vote: missing state → Reject; present → independent checks then Accept.
fn vote_with_state_tx(tx: &Transaction, state: Option<&DrivechainStateSnapshot>) -> RuleVote {
    let Some(state) = state else {
        return RuleVote::reject("missing_drivechain_state");
    };
    if let Some(reason) = independent_snapshot_reject_tx(tx, state) {
        return RuleVote::reject(reason);
    }
    RuleVote::Accept
}

fn vote_with_state_block(
    block: &bitcoin::Block,
    state: Option<&DrivechainStateSnapshot>,
) -> RuleVote {
    let Some(state) = state else {
        return RuleVote::reject("missing_drivechain_state");
    };
    if let Some(reason) = independent_snapshot_reject_block(block, state) {
        return RuleVote::reject(reason);
    }
    RuleVote::Accept
}

impl SoftForkRule for DrivechainRule {
    fn id(&self) -> RuleId {
        RULE_ID
    }

    fn validate_tx(&self, ctx: &RuleTxContext<'_>) -> Result<RuleVote, RuleCallError> {
        let Some(tx) = ctx.tx else {
            return Ok(RuleVote::reject("missing_tx"));
        };
        Ok(vote_with_state_tx(tx, ctx.drivechain_state))
    }

    fn connect_block(&self, ctx: &RuleBlockContext<'_>) -> Result<RuleVote, RuleCallError> {
        let Some(block) = ctx.block else {
            return Ok(RuleVote::reject("missing_block"));
        };
        Ok(vote_with_state_block(block, ctx.drivechain_state))
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxOut, Witness, absolute::LockTime,
        hashes::Hash, transaction::Version,
    };

    use super::*;
    use crate::validator::rules::{
        DrivechainCtipSnapshot, DrivechainPendingM6idSnapshot, RuleEngine, SoftForkRule,
    };

    fn sample_state() -> DrivechainStateSnapshot {
        DrivechainStateSnapshot {
            tip_height: 1,
            tip_hash_hex: "00".repeat(32),
            active_sidechain_numbers: vec![],
            ctips: vec![],
            pending_m6ids: vec![],
            withdrawal_bundle_inclusion_threshold: 0,
        }
    }

    fn state_with_tip(tip_hash_hex: String, active: Vec<u8>) -> DrivechainStateSnapshot {
        DrivechainStateSnapshot {
            tip_height: 1,
            tip_hash_hex,
            active_sidechain_numbers: active,
            ctips: vec![],
            pending_m6ids: vec![],
            withdrawal_bundle_inclusion_threshold: 0,
        }
    }

    fn state_with_ctip(
        tip_hash_hex: String,
        active: Vec<u8>,
        ctips: Vec<DrivechainCtipSnapshot>,
    ) -> DrivechainStateSnapshot {
        DrivechainStateSnapshot {
            tip_height: 1,
            tip_hash_hex,
            active_sidechain_numbers: active,
            ctips,
            pending_m6ids: vec![],
            withdrawal_bundle_inclusion_threshold: 0,
        }
    }

    fn state_with_ctip_and_pending(
        tip_hash_hex: String,
        active: Vec<u8>,
        ctips: Vec<DrivechainCtipSnapshot>,
        pending_m6ids: Vec<DrivechainPendingM6idSnapshot>,
        threshold: u16,
    ) -> DrivechainStateSnapshot {
        DrivechainStateSnapshot {
            tip_height: 1,
            tip_hash_hex,
            active_sidechain_numbers: active,
            ctips,
            pending_m6ids,
            withdrawal_bundle_inclusion_threshold: threshold,
        }
    }

    fn plain_tx() -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(50_000),
                script_pubkey: ScriptBuf::new(),
            }],
        }
    }

    fn op_drivechain_spk(sidechain: u8) -> ScriptBuf {
        use bitcoin::opcodes::{OP_TRUE, all::OP_PUSHBYTES_1};

        use crate::types::OP_DRIVECHAIN;
        ScriptBuf::from_bytes(vec![
            OP_DRIVECHAIN.to_u8(),
            OP_PUSHBYTES_1.to_u8(),
            sidechain,
            OP_TRUE.to_u8(),
        ])
    }

    fn op_drivechain_tx(sidechain: u8) -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(1000),
                script_pubkey: op_drivechain_spk(sidechain),
            }],
        }
    }

    fn multi_op_drivechain_tx(sidechain: u8) -> Transaction {
        let spk = op_drivechain_spk(sidechain);
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![
                TxOut {
                    value: Amount::from_sat(1000),
                    script_pubkey: spk.clone(),
                },
                TxOut {
                    value: Amount::from_sat(500),
                    script_pubkey: spk,
                },
            ],
        }
    }

    fn m8_tx(prev: bitcoin::BlockHash) -> Transaction {
        use crate::types::{BmmCommitment, SidechainNumber};
        let spk =
            M8BmmRequest::script_pubkey(SidechainNumber::from(1u8), BmmCommitment([2u8; 32]), prev)
                .unwrap();
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: spk,
            }],
        }
    }

    fn m8_tx_with_commitment(
        prev: bitcoin::BlockHash,
        sidechain: u8,
        commitment: [u8; 32],
    ) -> Transaction {
        use crate::types::{BmmCommitment, SidechainNumber};
        let spk = M8BmmRequest::script_pubkey(
            SidechainNumber::from(sidechain),
            BmmCommitment(commitment),
            prev,
        )
        .unwrap();
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: spk,
            }],
        }
    }

    fn fixed_txid(byte: u8) -> Txid {
        Txid::from_byte_array([byte; 32])
    }

    /// Non-palindromic txid so LE-vs-Display hex mixups fail tests.
    fn non_palindromic_txid() -> Txid {
        let mut b = [0u8; 32];
        b[0] = 0x01;
        b[1] = 0x23;
        b[2] = 0x45;
        b[30] = 0xAB;
        b[31] = 0xCD;
        Txid::from_byte_array(b)
    }

    fn ctip_entry(sc: u8, txid_byte: u8, vout: u32, value_sats: u64) -> DrivechainCtipSnapshot {
        DrivechainCtipSnapshot {
            sidechain_number: sc,
            outpoint_txid_hex: fixed_txid(txid_byte).to_string(),
            outpoint_vout: vout,
            value_sats,
        }
    }

    fn ctip_entry_txid(sc: u8, txid: Txid, vout: u32, value_sats: u64) -> DrivechainCtipSnapshot {
        DrivechainCtipSnapshot {
            sidechain_number: sc,
            outpoint_txid_hex: txid.to_string(),
            outpoint_vout: vout,
            value_sats,
        }
    }

    fn spend_ctip_tx(
        prev: OutPoint,
        sidechain: u8,
        new_value: u64,
        with_address: bool,
    ) -> Transaction {
        let mut outputs = vec![TxOut {
            value: Amount::from_sat(new_value),
            script_pubkey: op_drivechain_spk(sidechain),
        }];
        if with_address {
            outputs.push(TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::new_op_return(b"sidechain-addr"),
            });
        }
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: prev,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: outputs,
        }
    }

    /// Test-only label shortcuts — compiled only under `cfg(test)`, never on
    /// production SoftForkRule / worker paths.
    fn test_only_label_vote(label: &str) -> Option<RuleVote> {
        if label == "test-accept" || label.starts_with("test-accept:") {
            return Some(RuleVote::Accept);
        }
        if label.contains("drivechain-reject") || label == "test-reject" {
            return Some(RuleVote::reject("drivechain test reject"));
        }
        None
    }

    #[test]
    fn rule_id_is_stable() {
        assert_eq!(DrivechainRule::new().id(), "drivechain");
    }

    #[test]
    fn ballot_from_ok_false_rejects() {
        let b = DrivechainRule::ballot_from_validation_ok(false);
        assert!(!b.consents());
        assert_eq!(b.rule_id, RULE_ID);
    }

    #[test]
    fn ballot_from_ok_true_accepts() {
        assert!(DrivechainRule::ballot_from_validation_ok(true).consents());
    }

    #[test]
    fn test_only_label_helpers() {
        assert!(matches!(
            test_only_label_vote("test-accept"),
            Some(RuleVote::Accept)
        ));
        match test_only_label_vote("drivechain-reject-case") {
            Some(RuleVote::Reject { reason }) => assert!(reason.contains("drivechain")),
            other => panic!("expected reject, got {other:?}"),
        }
        // Production SoftForkRule ignores these labels.
        let rule = DrivechainRule::new();
        match rule
            .validate_tx(&RuleTxContext::label_only(0, "test-accept"))
            .unwrap()
        {
            RuleVote::Reject { reason } => assert!(reason.contains("missing_tx")),
            RuleVote::Accept => panic!("production must not soft-accept test-accept label"),
        }
    }

    #[test]
    fn missing_tx_rejects() {
        let rule = DrivechainRule::new();
        let vote = rule
            .validate_tx(&RuleTxContext::label_only(1, "prod"))
            .unwrap();
        match vote {
            RuleVote::Reject { reason } => assert!(reason.contains("missing_tx")),
            RuleVote::Accept => panic!("must not soft-accept without tx"),
        }
    }

    #[test]
    fn tx_without_state_missing_drivechain_state() {
        let tx = plain_tx();
        let rule = DrivechainRule::new();
        let vote = rule
            .validate_tx(&RuleTxContext::with_tx(1, "prod", &tx))
            .unwrap();
        match vote {
            RuleVote::Reject { reason } => assert!(reason.contains("missing_drivechain_state")),
            RuleVote::Accept => panic!("must not Accept without drivechain_state"),
        }
    }

    #[test]
    fn plain_tx_with_state_accepts() {
        let tx = plain_tx();
        let state = sample_state();
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("plain tx + state must Accept, got {other:?}"),
        }
    }

    #[test]
    fn op_drivechain_inactive_slot_with_state_accepts() {
        // Inactive-slot OP_DRIVECHAIN is ordinary anyone-can-spend on mainchain;
        // Local Accepts — remote must not brick under dual-AND.
        let tx = op_drivechain_tx(1);
        assert!(drivechain_db_required_reason_tx(&tx).is_some());
        let state = sample_state(); // empty active list
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => {
                panic!("inactive OP_DRIVECHAIN must Accept (Local AND), got {other:?}")
            }
        }
    }

    #[test]
    fn op_drivechain_active_first_deposit_with_address_accepts() {
        // First deposit (no current ctip) with address OP_RETURN → Accept.
        let mut tx = op_drivechain_tx(1);
        tx.output.push(TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::new_op_return(b"addr"),
        });
        let state = state_with_tip("00".repeat(32), vec![1]);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("first deposit with address must Accept, got {other:?}"),
        }
    }

    #[test]
    fn op_drivechain_active_first_deposit_missing_address_rejects() {
        let tx = op_drivechain_tx(1);
        let state = state_with_tip("00".repeat(32), vec![1]);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("missing_deposit_address"),
                    "reason={reason}"
                );
            }
            RuleVote::Accept => panic!("first deposit without address must Reject"),
        }
    }

    #[test]
    fn multiple_op_drivechain_active_rejects() {
        let tx = multi_op_drivechain_tx(7);
        let state = state_with_tip("00".repeat(32), vec![7]);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(reason.contains("multiple_op_drivechain"), "reason={reason}");
            }
            RuleVote::Accept => panic!("multi active OP_DRIVECHAIN must Reject independently"),
        }
    }

    #[test]
    fn multiple_op_drivechain_inactive_accepts() {
        // Local ignores inactive OP_DRIVECHAIN as CTIP — dual-AND must Accept.
        let tx = multi_op_drivechain_tx(7);
        let state = sample_state(); // empty active
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("inactive multi OP_DRIVECHAIN must Accept, got {other:?}"),
        }
    }

    /// Multi OP_DRIVECHAIN for slot 7 while only slot 3 is active → Accept
    /// (inactive multi is ordinary mainchain anyone-can-spend).
    #[test]
    fn multiple_op_drivechain_other_sidechain_active_accepts() {
        let tx = multi_op_drivechain_tx(7);
        let state = state_with_tip("00".repeat(32), vec![3]); // 7 not active
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("multi OP_DRIVECHAIN on inactive slot must Accept, got {other:?}"),
        }
    }

    /// Remote independent multi-active Reject + Local Accept → dual-AND Reject.
    #[test]
    fn dual_and_remote_multi_op_drivechain_reject_overrides_local_accept() {
        let tx = multi_op_drivechain_tx(7);
        let state = state_with_tip("00".repeat(32), vec![7]);
        let remote = DrivechainRule::new()
            .validate_tx(&RuleTxContext::with_tx(1, "r", &tx).with_drivechain_state(&state))
            .unwrap();
        assert!(
            matches!(&remote, RuleVote::Reject { reason } if reason.contains("multiple_op_drivechain")),
            "remote must independent-Reject multi-active, got {remote:?}"
        );
        let ballots = [
            DrivechainRule::ballot_from_validation_ok(true),
            RuleBallot::explicit("drivechain-remote", remote),
        ];
        assert!(
            !RuleEngine::decide(&ballots).is_accept(),
            "remote multi-active Reject must fail dual-AND"
        );
    }

    /// Non-palindromic tip bytes catch Display-hex endianness mixups.
    fn non_palindromic_tip() -> bitcoin::BlockHash {
        use bitcoin::{BlockHash, hashes::Hash};
        let mut bytes = [0u8; 32];
        bytes[0] = 0x01;
        bytes[1] = 0x02;
        bytes[30] = 0xfe;
        bytes[31] = 0xff;
        BlockHash::from_byte_array(bytes)
    }

    #[test]
    fn m8_tx_matching_tip_accepts() {
        let prev = non_palindromic_tip();
        let tx = m8_tx(prev);
        assert!(drivechain_db_required_reason_tx(&tx).is_some());
        let tip_hex = prev.to_string();
        // Non-palindrome: Display hex is reversed relative to internal byte array.
        assert_ne!(
            &tip_hex[..4],
            "0102",
            "fixture must exercise Display endianness"
        );
        assert_eq!(
            &tip_hex[..4],
            "fffe",
            "bitcoin Display reverses block hash bytes"
        );
        let state = state_with_tip(tip_hex, vec![]);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("M8 matching tip must Accept, got {other:?}"),
        }
    }

    #[test]
    fn m8_tx_mismatched_tip_rejects() {
        use bitcoin::{BlockHash, hashes::Hash};
        let prev = non_palindromic_tip();
        let mut wrong_bytes = [0xAAu8; 32];
        wrong_bytes[0] = 0xBB;
        wrong_bytes[31] = 0xCC;
        let wrong_tip = BlockHash::from_byte_array(wrong_bytes);
        let tx = m8_tx(prev);
        assert_ne!(prev.to_string(), wrong_tip.to_string());
        let state = state_with_tip(wrong_tip.to_string(), vec![]);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("m8_prev_mainchain_mismatch"),
                    "reason={reason}"
                );
            }
            RuleVote::Accept => panic!("M8 mismatched tip must Reject independently"),
        }
    }

    #[test]
    fn m8_tx_empty_tip_hash_rejects() {
        use bitcoin::{BlockHash, hashes::Hash};
        let prev = BlockHash::from_byte_array([3u8; 32]);
        let tx = m8_tx(prev);
        let state = state_with_tip(String::new(), vec![]);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(reason.contains("m8_missing_tip_hash"), "reason={reason}");
            }
            RuleVote::Accept => panic!("M8 with empty tip_hash_hex must fail-closed"),
        }
    }

    #[test]
    fn engine_registers_drivechain_rule_fail_closed() {
        let mut engine = RuleEngine::new();
        engine.register(Box::new(DrivechainRule::new()));
        assert_eq!(engine.registered_ids(), vec!["drivechain"]);
        assert!(
            !engine
                .validate_tx(&RuleTxContext::label_only(1, "ok"))
                .is_accept()
        );
        // Accept path for composition tests uses ballots, not label bypass.
        assert!(DrivechainRule::ballot_from_validation_ok(true).consents());
    }

    #[test]
    fn connect_block_missing_block_rejects() {
        let rule = DrivechainRule::new();
        match rule
            .connect_block(&RuleBlockContext::label_only(1, "prod"))
            .unwrap()
        {
            RuleVote::Reject { reason } => assert!(reason.contains("missing_block")),
            RuleVote::Accept => panic!("must not soft-accept without block"),
        }
    }

    #[test]
    fn connect_block_plain_with_state_accepts() {
        use bitcoin::{Network, blockdata::constants::genesis_block};
        let block = genesis_block(Network::Regtest);
        let state = sample_state();
        let rule = DrivechainRule::new();
        let ctx = RuleBlockContext::with_block(0, "prod", &block).with_drivechain_state(&state);
        match rule.connect_block(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("plain genesis + state must Accept, got {other:?}"),
        }
    }

    #[test]
    fn connect_block_missing_state_rejects() {
        use bitcoin::{Network, blockdata::constants::genesis_block};
        let block = genesis_block(Network::Regtest);
        let rule = DrivechainRule::new();
        match rule
            .connect_block(&RuleBlockContext::with_block(0, "prod", &block))
            .unwrap()
        {
            RuleVote::Reject { reason } => assert!(reason.contains("missing_drivechain_state")),
            RuleVote::Accept => panic!("must not Accept without state"),
        }
    }

    #[test]
    fn connect_block_m8_mismatch_rejects() {
        use bitcoin::{
            Block, BlockHash, CompactTarget, Network, block::Header,
            blockdata::constants::genesis_block, hashes::Hash,
        };
        let genesis = genesis_block(Network::Regtest);
        let prev = BlockHash::from_byte_array([3u8; 32]);
        let wrong_tip = BlockHash::from_byte_array([0xAAu8; 32]);
        let m8 = m8_tx(prev);
        // Minimal non-coinbase block with one M8 tx (connect scans all txs).
        let block = Block {
            header: Header {
                version: bitcoin::blockdata::block::Version::ONE,
                prev_blockhash: genesis.block_hash(),
                merkle_root: bitcoin::TxMerkleNode::from_byte_array([0; 32]),
                time: 0,
                bits: CompactTarget::from_consensus(0),
                nonce: 0,
            },
            txdata: vec![
                Transaction {
                    version: Version::TWO,
                    lock_time: LockTime::ZERO,
                    input: vec![TxIn::default()],
                    output: vec![TxOut {
                        value: Amount::from_sat(50),
                        script_pubkey: ScriptBuf::new(),
                    }],
                },
                m8,
            ],
        };
        let state = state_with_tip(wrong_tip.to_string(), vec![]);
        let rule = DrivechainRule::new();
        let ctx = RuleBlockContext::with_block(1, "prod", &block).with_drivechain_state(&state);
        match rule.connect_block(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("m8_prev_mainchain_mismatch"),
                    "reason={reason}"
                );
            }
            RuleVote::Accept => panic!("connect M8 mismatch must Reject"),
        }
    }

    /// Local BlockHandler Reject + remote SoftForkRule Accept → aggregate Reject
    /// (dual-AND; Local remains full M5–M6 / M7 authority).
    #[test]
    fn dual_and_local_reject_overrides_remote_style_accept() {
        // Distinct rule ids so both ballots participate in AND (hub Local + remote).
        let ballots = [
            DrivechainRule::ballot_from_validation_ok(false),
            RuleBallot::explicit("drivechain-remote", RuleVote::Accept),
        ];
        assert!(
            !RuleEngine::decide(&ballots).is_accept(),
            "Local fail + remote Accept must aggregate Reject under dual-AND"
        );
        // Control: both Accept → Accept
        assert!(
            RuleEngine::decide(&[
                DrivechainRule::ballot_from_validation_ok(true),
                RuleBallot::explicit("drivechain-remote", RuleVote::Accept),
            ])
            .is_accept()
        );
    }

    /// Remote independent Reject of invalid M8 + Local Accept → aggregate Reject.
    #[test]
    fn dual_and_remote_m8_reject_overrides_local_accept() {
        use bitcoin::{BlockHash, hashes::Hash};
        let prev = BlockHash::from_byte_array([3u8; 32]);
        let wrong = BlockHash::from_byte_array([0xBB; 32]);
        let tx = m8_tx(prev);
        let state = state_with_tip(wrong.to_string(), vec![]);
        let remote = DrivechainRule::new()
            .validate_tx(&RuleTxContext::with_tx(1, "r", &tx).with_drivechain_state(&state))
            .unwrap();
        let ballots = [
            DrivechainRule::ballot_from_validation_ok(true),
            RuleBallot::explicit("drivechain-remote", remote),
        ];
        assert!(
            !RuleEngine::decide(&ballots).is_accept(),
            "remote independent M8 Reject must fail dual-AND"
        );
    }

    // ── current-ctip structural M5/M6 ──

    #[test]
    fn old_ctip_unspent_rejects() {
        let ctip = ctip_entry(1, 0x11, 0, 1000);
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        // Active OP_DRIVECHAIN without spending old ctip.
        let mut tx = op_drivechain_tx(1);
        tx.output[0].value = Amount::from_sat(2000);
        tx.output.push(TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::new_op_return(b"addr"),
        });
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(reason.contains("old_ctip_unspent"), "reason={reason}");
            }
            RuleVote::Accept => panic!("old ctip unspent must Reject"),
        }
    }

    #[test]
    fn treasury_spent_without_new_ctip_rejects() {
        let ctip = ctip_entry(1, 0x22, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0x22),
            vout: 0,
        };
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        // Spend ctip but create no OP_DRIVECHAIN replacement.
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: prev,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(900),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("treasury_spent_without_new_ctip"),
                    "reason={reason}"
                );
            }
            RuleVote::Accept => panic!("treasury drain must Reject"),
        }
    }

    #[test]
    fn ctip_zero_diff_rejects() {
        let ctip = ctip_entry(1, 0x33, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0x33),
            vout: 0,
        };
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        let tx = spend_ctip_tx(prev, 1, 1000, true);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(reason.contains("ctip_zero_diff"), "reason={reason}");
            }
            RuleVote::Accept => panic!("zero-diff ctip must Reject"),
        }
    }

    #[test]
    fn valid_m5_spend_replace_with_address_accepts() {
        // Non-palindromic ctip txid (Issue 7): match path must use display hex.
        let old_txid = non_palindromic_txid();
        let ctip = ctip_entry_txid(1, old_txid, 0, 1000);
        let prev = OutPoint {
            txid: old_txid,
            vout: 0,
        };
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        let tx = spend_ctip_tx(prev, 1, 1500, true);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("valid M5 must Accept, got {other:?}"),
        }
    }

    #[test]
    fn old_ctip_unspent_non_palindromic_txid_rejects() {
        let old_txid = non_palindromic_txid();
        let ctip = ctip_entry_txid(1, old_txid, 0, 1000);
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        // Create OP_DRIVECHAIN without spending the non-palindromic ctip.
        let mut tx = op_drivechain_tx(1);
        tx.output[0].value = Amount::from_sat(2000);
        tx.output.push(TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::new_op_return(b"addr"),
        });
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(reason.contains("old_ctip_unspent"), "reason={reason}");
            }
            RuleVote::Accept => panic!("old ctip unspent (non-palindromic) must Reject"),
        }
    }

    #[test]
    fn m5_spend_replace_missing_address_rejects() {
        let ctip = ctip_entry(1, 0x55, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0x55),
            vout: 0,
        };
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        let tx = spend_ctip_tx(prev, 1, 1500, false);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("missing_deposit_address"),
                    "reason={reason}"
                );
            }
            RuleVote::Accept => panic!("M5 without address must Reject"),
        }
    }

    #[test]
    fn m6_shape_ok_missing_pending_rejects() {
        // Value decrease + 1 input + vout 0 but no pending m6id → independent Reject
        // (Local MissingPendingWithdrawal; dual-AND safe).
        let ctip = ctip_entry(1, 0x66, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0x66),
            vout: 0,
        };
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        let tx = spend_ctip_tx(prev, 1, 400, false);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("m6_missing_pending_withdrawal"),
                    "reason={reason}"
                );
            }
            RuleVote::Accept => panic!("M6 without pending must Reject independently"),
        }
    }

    #[test]
    fn m6_with_pending_and_sufficient_votes_accepts() {
        let ctip = ctip_entry(1, 0x66, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0x66),
            vout: 0,
        };
        let tx = spend_ctip_tx(prev, 1, 400, false);
        let (m6id, _) = compute_m6id(tx.clone(), Amount::from_sat(1000)).expect("m6id");
        // Non-palindromic m6id encoding check: use display hex from Txid.
        let pending = DrivechainPendingM6idSnapshot {
            sidechain_number: 1,
            m6id_hex: m6id.0.to_string(),
            vote_count: 6,
            proposal_height: 1,
        };
        let state = state_with_ctip_and_pending(
            "00".repeat(32),
            vec![1],
            vec![ctip],
            vec![pending],
            5, // SHORT threshold: vote_count > 5
        );
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("M6 with pending + votes must Accept, got {other:?}"),
        }
    }

    #[test]
    fn m6_insufficient_vote_count_rejects() {
        let ctip = ctip_entry(1, 0x67, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0x67),
            vout: 0,
        };
        let tx = spend_ctip_tx(prev, 1, 400, false);
        let (m6id, _) = compute_m6id(tx.clone(), Amount::from_sat(1000)).expect("m6id");
        let pending = DrivechainPendingM6idSnapshot {
            sidechain_number: 1,
            m6id_hex: m6id.0.to_string(),
            vote_count: 5, // not > threshold 5
            proposal_height: 1,
        };
        let state =
            state_with_ctip_and_pending("00".repeat(32), vec![1], vec![ctip], vec![pending], 5);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("m6_insufficient_vote_count"),
                    "reason={reason}"
                );
            }
            RuleVote::Accept => panic!("M6 with insufficient votes must Reject"),
        }
    }

    #[test]
    fn m6_wrong_pending_m6id_rejects() {
        let ctip = ctip_entry(1, 0x68, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0x68),
            vout: 0,
        };
        let tx = spend_ctip_tx(prev, 1, 400, false);
        // Pending entry for a different m6id (non-palindromic).
        let pending = DrivechainPendingM6idSnapshot {
            sidechain_number: 1,
            m6id_hex: non_palindromic_txid().to_string(),
            vote_count: 100,
            proposal_height: 1,
        };
        let state =
            state_with_ctip_and_pending("00".repeat(32), vec![1], vec![ctip], vec![pending], 0);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("m6_missing_pending_withdrawal"),
                    "reason={reason}"
                );
            }
            RuleVote::Accept => panic!("M6 with wrong pending m6id must Reject"),
        }
    }

    /// Dual-AND: remote independent pending Reject overrides Local Accept.
    #[test]
    fn dual_and_remote_m6_missing_pending_reject() {
        let ctip = ctip_entry(1, 0x69, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0x69),
            vout: 0,
        };
        let tx = spend_ctip_tx(prev, 1, 400, false);
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        let remote = DrivechainRule::new()
            .validate_tx(&RuleTxContext::with_tx(1, "r", &tx).with_drivechain_state(&state))
            .unwrap();
        assert!(
            matches!(&remote, RuleVote::Reject { reason } if reason.contains("m6_missing_pending")),
            "remote={remote:?}"
        );
        // Even if Local accepted (e.g. bug), remote pending Reject fails dual-AND.
        let ballots = [
            DrivechainRule::ballot_from_validation_ok(true),
            RuleBallot::explicit("drivechain-remote", remote),
        ];
        assert!(
            !RuleEngine::decide(&ballots).is_accept(),
            "remote missing-pending Reject must fail dual-AND"
        );
    }

    /// Dual-AND: remote insufficient-vote Reject overrides Local Accept.
    #[test]
    fn dual_and_remote_m6_insufficient_votes_reject() {
        let ctip = ctip_entry(1, 0x6A, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0x6A),
            vout: 0,
        };
        let tx = spend_ctip_tx(prev, 1, 400, false);
        let (m6id, _) = compute_m6id(tx.clone(), Amount::from_sat(1000)).expect("m6id");
        let pending = DrivechainPendingM6idSnapshot {
            sidechain_number: 1,
            m6id_hex: m6id.0.to_string(),
            vote_count: 5,
            proposal_height: 1,
        };
        let state =
            state_with_ctip_and_pending("00".repeat(32), vec![1], vec![ctip], vec![pending], 5);
        let remote = DrivechainRule::new()
            .validate_tx(&RuleTxContext::with_tx(1, "r", &tx).with_drivechain_state(&state))
            .unwrap();
        assert!(
            matches!(&remote, RuleVote::Reject { reason } if reason.contains("m6_insufficient_vote_count")),
            "remote={remote:?}"
        );
        let ballots = [
            DrivechainRule::ballot_from_validation_ok(true),
            RuleBallot::explicit("drivechain-remote", remote),
        ];
        assert!(
            !RuleEngine::decide(&ballots).is_accept(),
            "remote insufficient-vote Reject must fail dual-AND"
        );
    }

    /// Path A: M6 shape ok but payouts overspend treasury → `compute_m6id` fails
    /// (Local M6idError); dual-AND safe independent Reject.
    #[test]
    fn m6_compute_m6id_failed_overspend_rejects() {
        let ctip = ctip_entry(1, 0x6B, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0x6B),
            vout: 0,
        };
        // Shape: value decrease + 1 input + vout 0, but payouts > delta → m6id fail.
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: prev,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![
                TxOut {
                    value: Amount::from_sat(400),
                    script_pubkey: op_drivechain_spk(1),
                },
                TxOut {
                    value: Amount::from_sat(700),
                    script_pubkey: ScriptBuf::new(),
                },
            ],
        };
        assert!(
            compute_m6id(tx.clone(), Amount::from_sat(1000)).is_err(),
            "fixture must make compute_m6id fail"
        );
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(reason.contains("m6_compute_m6id_failed"), "reason={reason}");
            }
            RuleVote::Accept => panic!("overspend M6 must Reject via compute_m6id_failed"),
        }
    }

    /// Case-insensitive m6id_hex on wire still matches (hub may upper-case).
    #[test]
    fn m6_pending_m6id_hex_case_insensitive_accepts() {
        let ctip = ctip_entry(1, 0x6C, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0x6C),
            vout: 0,
        };
        let tx = spend_ctip_tx(prev, 1, 400, false);
        let (m6id, _) = compute_m6id(tx.clone(), Amount::from_sat(1000)).expect("m6id");
        let pending = DrivechainPendingM6idSnapshot {
            sidechain_number: 1,
            m6id_hex: m6id.0.to_string().to_uppercase(),
            vote_count: 6,
            proposal_height: 1,
        };
        let state =
            state_with_ctip_and_pending("00".repeat(32), vec![1], vec![ctip], vec![pending], 5);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("uppercase m6id_hex must Accept, got {other:?}"),
        }
    }

    /// Path B residual: SoftForkRule only sees **current** `ctips` on wire.
    /// Spending an outpoint **not** in `snapshot.ctips` SoftForkRule Accepts
    /// (historical map not on wire). Design-level residual: when Local's
    /// `ctip_outpoint_to_value_seq` holds that outpoint as a past treasury entry,
    /// Local dual-AND Rejects; SoftForkRule cannot see that map. This fixture
    /// does not construct Local LMDB — it locks SoftForkRule Accept only.
    #[test]
    fn historical_non_current_ctip_spend_accepts_path_b_residual() {
        let current = non_palindromic_txid();
        let historical = {
            let mut b = [0u8; 32];
            b[0] = 0xDE;
            b[1] = 0xAD;
            b[30] = 0xBE;
            b[31] = 0xEF;
            Txid::from_byte_array(b)
        };
        assert_ne!(current, historical);
        let ctip = ctip_entry_txid(1, current, 0, 1000);
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        let rule = DrivechainRule::new();

        // Path A control: spend *current* ctip without OP_DRIVECHAIN replace → Reject.
        let current_spend = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: current,
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(900),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        match rule
            .validate_tx(
                &RuleTxContext::with_tx(1, "prod", &current_spend).with_drivechain_state(&state),
            )
            .unwrap()
        {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("treasury_spent_without_new_ctip"),
                    "Path A control reason={reason}"
                );
            }
            RuleVote::Accept => panic!("current ctip drain must Path A Reject"),
        }

        // Path B residual: historical-only plain spend → SoftForkRule Accept.
        let hist_tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: historical,
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(900),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        match rule
            .validate_tx(&RuleTxContext::with_tx(1, "prod", &hist_tx).with_drivechain_state(&state))
            .unwrap()
        {
            RuleVote::Accept => {}
            other => panic!(
                "historical non-current spend must SoftForkRule Accept (Path B residual), got {other:?}"
            ),
        }

        // connect_block residual Accept for same body (Path A sequential fold N/A).
        let block = block_with_coinbase_and_txs(
            vec![TxOut {
                value: Amount::from_sat(50),
                script_pubkey: ScriptBuf::new(),
            }],
            vec![hist_tx.clone()],
        );
        match rule
            .connect_block(
                &RuleBlockContext::with_block(1, "prod", &block).with_drivechain_state(&state),
            )
            .unwrap()
        {
            RuleVote::Accept => {}
            other => panic!("connect historical residual must Accept, got {other:?}"),
        }

        // Inactive OP_DRIVECHAIN + historical-only input still Accepts (slot 9 not active).
        let mut inactive_hist = hist_tx;
        inactive_hist.output = vec![TxOut {
            value: Amount::from_sat(900),
            script_pubkey: op_drivechain_spk(9),
        }];
        match rule
            .validate_tx(
                &RuleTxContext::with_tx(1, "prod", &inactive_hist).with_drivechain_state(&state),
            )
            .unwrap()
        {
            RuleVote::Accept => {}
            other => panic!("inactive OP_DRIVECHAIN + historical input must Accept, got {other:?}"),
        }
    }

    /// Path B residual: M1–M4 coinbase markers SoftForkRule Accept with Path A
    /// state present (no proposal/ack DB on wire). Local dual-AND still enforces
    /// full M1–M4. Do not invent partial SoftForkRule Rejects for these markers.
    /// Co-presence: M1 + M8 without M7 still Path A Rejects `m8_not_accepted_by_miners`.
    #[test]
    fn m1_m4_coinbase_markers_accept_path_b_residual() {
        use bitcoin::{
            BlockHash,
            hashes::{Hash, sha256d},
        };

        use crate::{
            messages::{M1ProposeSidechain, M2AckSidechain, M3ProposeBundle, M4AckBundles},
            types::SidechainNumber,
        };

        let m1_spk: ScriptBuf = M1ProposeSidechain {
            sidechain_number: SidechainNumber::from(1u8),
            description: vec![0u8, b'x'].into(),
        }
        .try_into()
        .expect("m1 script");
        let m2_spk: ScriptBuf = M2AckSidechain {
            sidechain_number: SidechainNumber::from(1u8),
            description_hash: sha256d::Hash::from_byte_array([0x11; 32]),
        }
        .try_into()
        .expect("m2 script");
        let m3_spk: ScriptBuf = M3ProposeBundle {
            sidechain_number: SidechainNumber::from(1u8),
            bundle_txid: [0x22; 32],
        }
        .try_into()
        .expect("m3 script");
        let m4_spk: ScriptBuf = M4AckBundles::RepeatPrevious.try_into().expect("m4 script");

        // Production-shaped Path A state (active + current ctip) — residual Accept
        // must not require empty sample_state().
        let populated = state_with_ctip(
            "00".repeat(32),
            vec![1],
            vec![ctip_entry_txid(1, non_palindromic_txid(), 0, 1000)],
        );
        let rule = DrivechainRule::new();

        for (label, spk) in [
            ("m1", m1_spk.clone()),
            ("m2", m2_spk),
            ("m3", m3_spk),
            ("m4", m4_spk),
        ] {
            assert!(
                CoinbaseMessage::parse(&spk).is_ok(),
                "{label} must parse as CoinbaseMessage"
            );
            let block = block_with_coinbase_and_txs(
                vec![TxOut {
                    value: Amount::from_sat(50),
                    script_pubkey: spk,
                }],
                vec![],
            );
            assert!(
                drivechain_db_required_reason_block(&block).is_some(),
                "{label} residual marker inventory"
            );
            let ctx =
                RuleBlockContext::with_block(1, "prod", &block).with_drivechain_state(&populated);
            match rule.connect_block(&ctx).unwrap() {
                RuleVote::Accept => {}
                other => panic!(
                    "{label} coinbase must SoftForkRule Accept (Path B residual M1–M4), got {other:?}"
                ),
            }
        }

        // Path A co-presence: residual M1 coinbase + M8 without M7 → connect Reject.
        let prev = BlockHash::from_byte_array([3u8; 32]);
        let m8 = m8_tx(prev);
        let block = block_with_coinbase_and_txs(
            vec![TxOut {
                value: Amount::from_sat(50),
                script_pubkey: m1_spk,
            }],
            vec![m8],
        );
        let state = state_with_tip(prev.to_string(), vec![1]);
        match rule
            .connect_block(
                &RuleBlockContext::with_block(1, "prod", &block).with_drivechain_state(&state),
            )
            .unwrap()
        {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("m8_not_accepted_by_miners"),
                    "M1 residual must not mask Path A M7 check, reason={reason}"
                );
            }
            RuleVote::Accept => panic!("M1 + M8 without M7 must Path A Reject"),
        }
    }

    /// Dual-AND: actual Path B residual SoftForkRule Accept (historical / M1)
    /// + Local `ballot_from_validation_ok(false)` → aggregate Reject; Local-true
    /// + residual Accept → Accept.
    #[test]
    fn dual_and_residual_accept_with_local_false_rejects() {
        use crate::{messages::M1ProposeSidechain, types::SidechainNumber};

        let current = non_palindromic_txid();
        let historical = {
            let mut b = [0u8; 32];
            b[0] = 0xDE;
            b[1] = 0xAD;
            b[30] = 0xBE;
            b[31] = 0xEF;
            Txid::from_byte_array(b)
        };
        let state = state_with_ctip(
            "00".repeat(32),
            vec![1],
            vec![ctip_entry_txid(1, current, 0, 1000)],
        );
        let hist_tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: historical,
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(900),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let remote = DrivechainRule::new()
            .validate_tx(&RuleTxContext::with_tx(1, "r", &hist_tx).with_drivechain_state(&state))
            .unwrap();
        assert!(
            matches!(remote, RuleVote::Accept),
            "historical residual SoftForkRule must Accept, got {remote:?}"
        );
        assert!(
            !RuleEngine::decide(&[
                DrivechainRule::ballot_from_validation_ok(false),
                RuleBallot::explicit("drivechain-remote", remote.clone()),
            ])
            .is_accept(),
            "Local false + residual Accept must aggregate Reject"
        );
        assert!(
            RuleEngine::decide(&[
                DrivechainRule::ballot_from_validation_ok(true),
                RuleBallot::explicit("drivechain-remote", remote),
            ])
            .is_accept(),
            "Local true + residual Accept must Accept"
        );

        // M1 residual Accept composition (connect).
        let m1_spk: ScriptBuf = M1ProposeSidechain {
            sidechain_number: SidechainNumber::from(1u8),
            description: vec![0u8, b'x'].into(),
        }
        .try_into()
        .expect("m1");
        let block = block_with_coinbase_and_txs(
            vec![TxOut {
                value: Amount::from_sat(50),
                script_pubkey: m1_spk,
            }],
            vec![],
        );
        let remote_m1 = DrivechainRule::new()
            .connect_block(
                &RuleBlockContext::with_block(1, "r", &block).with_drivechain_state(&state),
            )
            .unwrap();
        assert!(
            matches!(remote_m1, RuleVote::Accept),
            "M1 residual SoftForkRule must Accept, got {remote_m1:?}"
        );
        assert!(
            !RuleEngine::decide(&[
                DrivechainRule::ballot_connect_ok(false, "local m1 reject"),
                RuleBallot::explicit("drivechain-remote", remote_m1),
            ])
            .is_accept(),
            "Local connect false + M1 residual Accept must aggregate Reject"
        );
    }

    /// Connect sequential fold: M6 then M5 same slot. Without pending+ctip fold,
    /// second M5 against static parent would `old_ctip_unspent`.
    #[test]
    fn connect_sequential_m6_then_m5_pending_and_ctip_fold_accepts() {
        let old_txid = non_palindromic_txid();
        let parent_ctip = ctip_entry_txid(1, old_txid, 0, 1000);
        let prev_a = OutPoint {
            txid: old_txid,
            vout: 0,
        };
        let m6 = spend_ctip_tx(prev_a, 1, 400, false);
        let (m6id, _) = compute_m6id(m6.clone(), Amount::from_sat(1000)).expect("m6id");
        let out_b = OutPoint {
            txid: m6.compute_txid(),
            vout: 0,
        };
        let m5 = spend_ctip_tx(out_b, 1, 1500, true);
        let pending = DrivechainPendingM6idSnapshot {
            sidechain_number: 1,
            m6id_hex: m6id.0.to_string(),
            vote_count: 6,
            proposal_height: 1,
        };
        let state = state_with_ctip_and_pending(
            "00".repeat(32),
            vec![1],
            vec![parent_ctip],
            vec![pending],
            5,
        );
        let block = block_with_coinbase_and_txs(
            vec![TxOut {
                value: Amount::from_sat(50),
                script_pubkey: ScriptBuf::new(),
            }],
            vec![m6.clone(), m5.clone()],
        );
        let rule = DrivechainRule::new();
        // Control: M5 alone vs static parent ctip A → old_ctip_unspent.
        match rule
            .validate_tx(&RuleTxContext::with_tx(1, "prod", &m5).with_drivechain_state(&state))
            .unwrap()
        {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("old_ctip_unspent"),
                    "M5 vs parent without fold must old_ctip_unspent, reason={reason}"
                );
            }
            RuleVote::Accept => panic!("M5 alone vs parent A must Reject without fold"),
        }
        // Connect: M6 folds ctip A→B + removes pending; M5 spends B → Accept.
        let ctx = RuleBlockContext::with_block(1, "prod", &block).with_drivechain_state(&state);
        match rule.connect_block(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("M6 then M5 sequential fold must Accept, got {other:?}"),
        }
    }

    #[test]
    fn m6_multiple_inputs_rejects() {
        let ctip = ctip_entry(1, 0x77, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0x77),
            vout: 0,
        };
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        let mut tx = spend_ctip_tx(prev, 1, 400, false);
        tx.input.push(TxIn {
            previous_output: OutPoint {
                txid: fixed_txid(0x88),
                vout: 1,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        });
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(reason.contains("m6_invalid_input_count"), "reason={reason}");
            }
            RuleVote::Accept => panic!("M6 multi-input must Reject"),
        }
    }

    #[test]
    fn malformed_ctip_txid_rejects_fail_closed() {
        let bad = DrivechainCtipSnapshot {
            sidechain_number: 1,
            outpoint_txid_hex: "not-a-txid".into(),
            outpoint_vout: 0,
            value_sats: 1,
        };
        let state = state_with_ctip("00".repeat(32), vec![1], vec![bad]);
        // Any path that consults ctips (OP_DRIVECHAIN active) must fail-closed.
        let mut tx = op_drivechain_tx(1);
        tx.output.push(TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::new_op_return(b"addr"),
        });
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(reason.contains("malformed_ctip_txid"), "reason={reason}");
            }
            RuleVote::Accept => panic!("malformed ctip must fail-closed"),
        }
    }

    // ── connect M7 / accepted_bmm ──

    fn block_with_coinbase_and_txs(
        coinbase_outputs: Vec<TxOut>,
        extra_txs: Vec<Transaction>,
    ) -> bitcoin::Block {
        use bitcoin::{
            Block, CompactTarget, Network, block::Header, blockdata::constants::genesis_block,
        };
        let genesis = genesis_block(Network::Regtest);
        let mut txdata = vec![Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn::default()],
            output: coinbase_outputs,
        }];
        txdata.extend(extra_txs);
        Block {
            header: Header {
                version: bitcoin::blockdata::block::Version::ONE,
                prev_blockhash: genesis.block_hash(),
                merkle_root: bitcoin::TxMerkleNode::from_byte_array([0; 32]),
                time: 0,
                bits: CompactTarget::from_consensus(0),
                nonce: 0,
            },
            txdata,
        }
    }

    #[test]
    fn connect_m8_without_m7_rejects() {
        use bitcoin::{BlockHash, hashes::Hash};
        let prev = BlockHash::from_byte_array([3u8; 32]);
        let m8 = m8_tx(prev);
        let block = block_with_coinbase_and_txs(
            vec![TxOut {
                value: Amount::from_sat(50),
                script_pubkey: ScriptBuf::new(),
            }],
            vec![m8],
        );
        let state = state_with_tip(prev.to_string(), vec![]);
        let rule = DrivechainRule::new();
        let ctx = RuleBlockContext::with_block(1, "prod", &block).with_drivechain_state(&state);
        match rule.connect_block(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("m8_not_accepted_by_miners"),
                    "reason={reason}"
                );
            }
            RuleVote::Accept => panic!("connect M8 without M7 must Reject"),
        }
    }

    #[test]
    fn connect_m8_with_matching_m7_accepts() {
        use bitcoin::{BlockHash, hashes::Hash};

        use crate::{
            messages::M7BmmAccept,
            types::{BmmCommitment, SidechainNumber},
        };
        let prev = BlockHash::from_byte_array([3u8; 32]);
        let commitment = [2u8; 32];
        let m7_spk: ScriptBuf = M7BmmAccept {
            sidechain_number: SidechainNumber::from(1u8),
            sidechain_block_hash: BmmCommitment(commitment),
        }
        .try_into()
        .unwrap();
        let m8 = m8_tx_with_commitment(prev, 1, commitment);
        let block = block_with_coinbase_and_txs(
            vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: m7_spk,
            }],
            vec![m8],
        );
        let state = state_with_tip(prev.to_string(), vec![]);
        let rule = DrivechainRule::new();
        let ctx = RuleBlockContext::with_block(1, "prod", &block).with_drivechain_state(&state);
        match rule.connect_block(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("M8 + matching M7 must Accept, got {other:?}"),
        }
    }

    #[test]
    fn connect_m8_with_wrong_m7_commitment_rejects() {
        use bitcoin::{BlockHash, hashes::Hash};

        use crate::{
            messages::M7BmmAccept,
            types::{BmmCommitment, SidechainNumber},
        };
        let prev = BlockHash::from_byte_array([3u8; 32]);
        let m7_spk: ScriptBuf = M7BmmAccept {
            sidechain_number: SidechainNumber::from(1u8),
            sidechain_block_hash: BmmCommitment([9u8; 32]),
        }
        .try_into()
        .unwrap();
        // M8 commitment differs from present M7 → same as missing for miners.
        let m8 = m8_tx_with_commitment(prev, 1, [2u8; 32]);
        let block = block_with_coinbase_and_txs(
            vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: m7_spk,
            }],
            vec![m8],
        );
        let state = state_with_tip(prev.to_string(), vec![]);
        let rule = DrivechainRule::new();
        let ctx = RuleBlockContext::with_block(1, "prod", &block).with_drivechain_state(&state);
        match rule.connect_block(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("m8_not_accepted_by_miners"),
                    "reason={reason}"
                );
            }
            RuleVote::Accept => panic!("wrong M7 commitment must Reject"),
        }
    }

    /// Same-slot chained M5s in one block: parent ctip A, tx1 A→B, tx2 B→C.
    /// Without sequential fold, tx2 would `old_ctip_unspent` against static A.
    #[test]
    fn connect_sequential_same_slot_m5_fold_accepts() {
        let old_txid = non_palindromic_txid();
        let parent_ctip = ctip_entry_txid(1, old_txid, 0, 1000);
        let state = state_with_ctip("00".repeat(32), vec![1], vec![parent_ctip]);
        let prev_a = OutPoint {
            txid: old_txid,
            vout: 0,
        };
        let m5_ab = spend_ctip_tx(prev_a, 1, 1500, true);
        let out_b = OutPoint {
            txid: m5_ab.compute_txid(),
            vout: 0,
        };
        let m5_bc = spend_ctip_tx(out_b, 1, 2000, true);
        let block = block_with_coinbase_and_txs(
            vec![TxOut {
                value: Amount::from_sat(50),
                script_pubkey: ScriptBuf::new(),
            }],
            vec![m5_ab.clone(), m5_bc.clone()],
        );
        // Control: second tx alone against parent-tip map → false old_ctip_unspent.
        let rule = DrivechainRule::new();
        match rule
            .validate_tx(&RuleTxContext::with_tx(1, "prod", &m5_bc).with_drivechain_state(&state))
            .unwrap()
        {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("old_ctip_unspent"),
                    "static parent map must false-Reject mid-block chain, reason={reason}"
                );
            }
            RuleVote::Accept => panic!("second M5 vs parent-tip only must Reject without fold"),
        }
        // Connect folds after first Accept → second sees B, dual-AND safe Accept.
        let ctx = RuleBlockContext::with_block(1, "prod", &block).with_drivechain_state(&state);
        match rule.connect_block(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("sequential same-slot M5s must Accept with fold, got {other:?}"),
        }
    }

    /// First deposit then replace in same block (no parent ctip → fold inserts).
    #[test]
    fn connect_sequential_first_deposit_then_m5_fold_accepts() {
        let state = state_with_ctip("00".repeat(32), vec![1], vec![]);
        let mut first = op_drivechain_tx(1);
        first.output[0].value = Amount::from_sat(1000);
        first.output.push(TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::new_op_return(b"addr1"),
        });
        // Need an input for second spend of first's treasury outpoint.
        let out0 = OutPoint {
            txid: first.compute_txid(),
            vout: 0,
        };
        let second = spend_ctip_tx(out0, 1, 1500, true);
        let block = block_with_coinbase_and_txs(
            vec![TxOut {
                value: Amount::from_sat(50),
                script_pubkey: ScriptBuf::new(),
            }],
            vec![first, second],
        );
        let rule = DrivechainRule::new();
        let ctx = RuleBlockContext::with_block(1, "prod", &block).with_drivechain_state(&state);
        match rule.connect_block(&ctx).unwrap() {
            RuleVote::Accept => {}
            other => panic!("first deposit + M5 fold must Accept, got {other:?}"),
        }
    }

    #[test]
    fn m6_treasury_vout_nonzero_rejects() {
        let ctip = ctip_entry(1, 0xAA, 0, 1000);
        let prev = OutPoint {
            txid: fixed_txid(0xAA),
            vout: 0,
        };
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        // M6 value decrease but OP_DRIVECHAIN at vout 1 (junk first).
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: prev,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![
                TxOut {
                    value: Amount::from_sat(1),
                    script_pubkey: ScriptBuf::new(),
                },
                TxOut {
                    value: Amount::from_sat(400),
                    script_pubkey: op_drivechain_spk(1),
                },
            ],
        };
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("m6_treasury_vout_nonzero"),
                    "reason={reason}"
                );
            }
            RuleVote::Accept => panic!("M6 treasury vout nonzero must Reject"),
        }
    }

    #[test]
    fn m5_m6_ambiguous_rejects() {
        // Two active slots: one M5 (value up) and one M6 (value down) in same tx.
        let ctip1 = ctip_entry(1, 0xB1, 0, 1000);
        let ctip2 = ctip_entry(2, 0xB2, 0, 1000);
        let state = state_with_ctip("00".repeat(32), vec![1, 2], vec![ctip1, ctip2]);
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![
                TxIn {
                    previous_output: OutPoint {
                        txid: fixed_txid(0xB1),
                        vout: 0,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                },
                TxIn {
                    previous_output: OutPoint {
                        txid: fixed_txid(0xB2),
                        vout: 0,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                },
            ],
            output: vec![
                TxOut {
                    value: Amount::from_sat(1500),
                    script_pubkey: op_drivechain_spk(1),
                },
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: ScriptBuf::new_op_return(b"addr"),
                },
                TxOut {
                    value: Amount::from_sat(400),
                    script_pubkey: op_drivechain_spk(2),
                },
            ],
        };
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(reason.contains("m5_m6_ambiguous"), "reason={reason}");
            }
            RuleVote::Accept => panic!("M5+M6 same tx must Reject"),
        }
    }

    #[test]
    fn m6_multiple_treasury_outputs_rejects() {
        // Two M6 decreases (both spent ctips) → saw_m6 && new_ctips.len() > 1.
        let state = state_with_ctip(
            "00".repeat(32),
            vec![1, 2],
            vec![ctip_entry(1, 0xC1, 0, 1000), ctip_entry(2, 0xC2, 0, 500)],
        );
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![
                TxIn {
                    previous_output: OutPoint {
                        txid: fixed_txid(0xC1),
                        vout: 0,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                },
                TxIn {
                    previous_output: OutPoint {
                        txid: fixed_txid(0xC2),
                        vout: 0,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                },
            ],
            output: vec![
                TxOut {
                    value: Amount::from_sat(400),
                    script_pubkey: op_drivechain_spk(1),
                },
                TxOut {
                    value: Amount::from_sat(200),
                    script_pubkey: op_drivechain_spk(2),
                },
            ],
        };
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("m6_multiple_treasury_outputs"),
                    "reason={reason}"
                );
            }
            RuleVote::Accept => panic!("M6 multi treasury must Reject"),
        }
    }

    /// Post-mutation snapshot (new ctip already current) + valid M5 spending the
    /// *old* outpoint → independent `old_ctip_unspent` (hub bug if wire is post-apply).
    #[test]
    fn valid_m5_against_post_mutation_ctip_snapshot_false_rejects() {
        let old = OutPoint {
            txid: fixed_txid(0x44),
            vout: 0,
        };
        // Snapshot as if Local already applied: current ctip is the *new* treasury.
        let new_ctip = DrivechainCtipSnapshot {
            sidechain_number: 1,
            outpoint_txid_hex: fixed_txid(0xEE).to_string(),
            outpoint_vout: 0,
            value_sats: 1500,
        };
        let state = state_with_ctip("00".repeat(32), vec![1], vec![new_ctip]);
        let tx = spend_ctip_tx(old, 1, 1500, true);
        let rule = DrivechainRule::new();
        let ctx = RuleTxContext::with_tx(1, "prod", &tx).with_drivechain_state(&state);
        match rule.validate_tx(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(
                    reason.contains("old_ctip_unspent"),
                    "post-mutation snapshot must false-Reject valid M5, reason={reason}"
                );
            }
            RuleVote::Accept => panic!("post-mutation ctip must not Accept this M5"),
        }
    }

    #[test]
    fn connect_multiple_m7_same_sidechain_rejects() {
        use crate::{
            messages::M7BmmAccept,
            types::{BmmCommitment, SidechainNumber},
        };
        let m7_spk: ScriptBuf = M7BmmAccept {
            sidechain_number: SidechainNumber::from(1u8),
            sidechain_block_hash: BmmCommitment([9u8; 32]),
        }
        .try_into()
        .unwrap();
        let block = block_with_coinbase_and_txs(
            vec![
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: m7_spk.clone(),
                },
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: m7_spk,
                },
            ],
            vec![],
        );
        let state = sample_state();
        let rule = DrivechainRule::new();
        let ctx = RuleBlockContext::with_block(1, "prod", &block).with_drivechain_state(&state);
        match rule.connect_block(&ctx).unwrap() {
            RuleVote::Reject { reason } => {
                assert!(reason.contains("multiple_m7_bmm"), "reason={reason}");
            }
            RuleVote::Accept => panic!("duplicate M7 must Reject"),
        }
    }

    #[test]
    fn dual_and_remote_ctip_reject_overrides_local_accept() {
        let ctip = ctip_entry(1, 0x99, 0, 1000);
        let state = state_with_ctip("00".repeat(32), vec![1], vec![ctip]);
        let mut tx = op_drivechain_tx(1);
        tx.output[0].value = Amount::from_sat(2000);
        tx.output.push(TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::new_op_return(b"addr"),
        });
        let remote = DrivechainRule::new()
            .validate_tx(&RuleTxContext::with_tx(1, "r", &tx).with_drivechain_state(&state))
            .unwrap();
        assert!(
            matches!(&remote, RuleVote::Reject { reason } if reason.contains("old_ctip_unspent")),
            "remote must independent-Reject old_ctip_unspent, got {remote:?}"
        );
        let ballots = [
            DrivechainRule::ballot_from_validation_ok(true),
            RuleBallot::explicit("drivechain-remote", remote),
        ];
        assert!(
            !RuleEngine::decide(&ballots).is_accept(),
            "remote ctip Reject must fail dual-AND"
        );
    }
}
