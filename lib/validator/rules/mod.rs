//! Soft-fork rule registry and composition (hub-side model).
//!
//! Target architecture: a **hub** owns Core ZMQ/RPC; **workers** register and
//! vote Accept/Reject. See `docs/MULTI_ENFORCER.md`.
//!
//! **Consent (normative):** a registered rule must consent. Explicit Accept =
//! yes. Explicit Reject, timeout, or failure = **no** (fail-closed). Missing
//! answers are not soft-Accept. Deregister to leave the voting set.
//!
//! * **P1:** in-process [`SoftForkRule`] + [`RuleEngine`]; feature-gated
//!   drivechain / bip360 adapters; mempool `validate_tx` composition via
//!   [`aggregate_and`].
//! * **P2:** [`ipc`] wire schema + [`RuleBackend::{Local,Remote}`](RuleBackend).

pub mod ipc;

#[cfg(feature = "bip360")]
pub mod bip360;
#[cfg(feature = "drivechain")]
pub mod drivechain;

use std::fmt;

/// Stable rule identity (e.g. `"drivechain"`, `"bip360"`).
///
/// **Accepted residual honesty (not unlocked this loop):** `RuleId = &'static str`
/// only — no owned/dynamic worker ids (`String`/`Cow`/`Arc<str>`). Hub CLI maps
/// names through [`ipc::static_rule_id`] allowlist (`drivechain` / `bip360`);
/// freeform names fail parse. Do not invent freeform registration without
/// product unlock.
pub type RuleId = &'static str;

/// Outcome of one rule's vote on a tx or block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleVote {
    /// Explicit yes.
    Accept,
    /// Explicit no, with a human-readable reason.
    Reject { reason: String },
}

impl RuleVote {
    pub fn reject(reason: impl Into<String>) -> Self {
        Self::Reject {
            reason: reason.into(),
        }
    }

    pub fn is_accept(&self) -> bool {
        matches!(self, Self::Accept)
    }
}

/// How the hub obtained a vote — used for logging and fail-closed policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoteSource {
    /// Worker/rule returned a definitive vote.
    Explicit(RuleVote),
    /// No answer within the deadline (counts as **no**).
    Timeout,
    /// Transport or process failure (counts as **no**).
    Failure { detail: String },
}

impl VoteSource {
    /// Consent policy: only explicit Accept is yes; everything else is no.
    pub fn consents(&self) -> bool {
        matches!(self, Self::Explicit(RuleVote::Accept))
    }

    pub fn is_error_path(&self) -> bool {
        matches!(self, Self::Timeout | Self::Failure { .. })
    }

    pub fn as_reject_reason(&self) -> String {
        match self {
            Self::Explicit(RuleVote::Accept) => String::new(),
            Self::Explicit(RuleVote::Reject { reason }) => reason.clone(),
            Self::Timeout => "rule did not answer in time (timeout counts as reject)".into(),
            Self::Failure { detail } => format!("rule failure counts as reject: {detail}"),
        }
    }
}

/// Per-rule result after asking for consent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleBallot {
    pub rule_id: RuleId,
    pub source: VoteSource,
}

impl RuleBallot {
    pub fn consents(&self) -> bool {
        self.source.consents()
    }

    pub fn explicit(rule_id: RuleId, vote: RuleVote) -> Self {
        Self {
            rule_id,
            source: VoteSource::Explicit(vote),
        }
    }

    pub fn from_ok(rule_id: RuleId, ok: bool, reject_reason: impl Into<String>) -> Self {
        if ok {
            Self::explicit(rule_id, RuleVote::Accept)
        } else {
            Self::explicit(rule_id, RuleVote::reject(reject_reason))
        }
    }

    pub fn timeout(rule_id: RuleId) -> Self {
        Self {
            rule_id,
            source: VoteSource::Timeout,
        }
    }

    pub fn failure(rule_id: RuleId, detail: impl Into<String>) -> Self {
        Self {
            rule_id,
            source: VoteSource::Failure {
                detail: detail.into(),
            },
        }
    }
}

/// Aggregate decision over all **registered** rules (AND).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateDecision {
    Accept,
    Reject {
        /// First (or all) rejecting ballots for logs / RPC errors.
        rejections: Vec<RuleBallot>,
    },
}

impl AggregateDecision {
    pub fn is_accept(&self) -> bool {
        matches!(self, Self::Accept)
    }
}

/// Compose ballots with fail-closed AND.
///
/// * Empty registry → Accept (no registered rules to satisfy).
/// * Any non-consenting ballot → Reject.
pub fn aggregate_and(ballots: &[RuleBallot]) -> AggregateDecision {
    let rejections: Vec<RuleBallot> = ballots.iter().filter(|b| !b.consents()).cloned().collect();
    if rejections.is_empty() {
        AggregateDecision::Accept
    } else {
        AggregateDecision::Reject { rejections }
    }
}

/// Max chars of reject/failure detail written to error logs (DoS / log flood).
pub const MAX_LOG_REASON_CHARS: usize = 256;

/// Truncate a reason string for logging (full detail remains on the ballot).
///
/// Strips CR/LF and other ASCII control characters to reduce log-injection risk,
/// then caps length at [`MAX_LOG_REASON_CHARS`].
pub fn truncate_for_log(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = cleaned.chars().count();
    if count <= MAX_LOG_REASON_CHARS {
        cleaned
    } else {
        let t: String = cleaned.chars().take(MAX_LOG_REASON_CHARS).collect();
        format!("{t}…(truncated)")
    }
}

/// Log Timeout / Failure ballots at error level (consent policy visibility).
/// Failure/timeout detail is truncated for logs; ballot retains full detail.
pub fn log_error_path_ballots(ballots: &[RuleBallot]) {
    for ballot in ballots {
        if ballot.source.is_error_path() {
            let reason = truncate_for_log(&ballot.source.as_reject_reason());
            tracing::error!(
                rule_id = ballot.rule_id,
                reason = %reason,
                "rule consent error-path (fail-closed: counts as reject)"
            );
        }
    }
}

/// In-process rule (P1). Remote workers produce [`VoteSource`] the same way via
/// [`RuleBackend`].
pub trait SoftForkRule: Send + Sync {
    fn id(&self) -> RuleId;

    /// Vote on a mempool transaction context.
    fn validate_tx(&self, ctx: &RuleTxContext<'_>) -> Result<RuleVote, RuleCallError>;

    /// Vote on connecting a block.
    fn connect_block(&self, ctx: &RuleBlockContext<'_>) -> Result<RuleVote, RuleCallError>;
}

/// Current-tip ctip (treasury UTXO) entry for rules.v1 SoftForkRule.
///
/// Wire size is bounded (one entry per active sidechain with a ctip). Historical
/// `ctip_outpoint_to_value_seq` rows are **not** included — residual for full
/// historical spend detection (Local dual-AND still catches those). Unbounded
/// over chain life; not Path A wire material.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DrivechainCtipSnapshot {
    pub sidechain_number: u8,
    /// Bitcoin display hex of the ctip outpoint txid.
    pub outpoint_txid_hex: String,
    pub outpoint_vout: u32,
    pub value_sats: u64,
}

/// Pending M6 withdrawal-bundle id on the wire (Path A extract).
///
/// Bounded by active pending proposals per sidechain (not full treasury history).
/// SoftForkRule matches `compute_m6id` against this list and enforces
/// `vote_count > withdrawal_bundle_inclusion_threshold`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DrivechainPendingM6idSnapshot {
    pub sidechain_number: u8,
    /// Bitcoin display hex of the blinded M6id (txid).
    pub m6id_hex: String,
    pub vote_count: u16,
    pub proposal_height: u32,
}

/// Drivechain tip + active sidechain + current-ctip + pending-M6 snapshot for
/// rules.v1 SoftForkRule.
///
/// Wire: `drivechain_state` on ValidateTx / ConnectBlock. Not full BlockHandler
/// LMDB. SoftForkRule uses snapshot fields for **independent** checks when
/// present (`None` → `missing_drivechain_state`):
/// * [`tip_hash_hex`](Self::tip_hash_hex) — M8 `prev_mainchain_block_hash` must match
/// * [`active_sidechain_numbers`](Self::active_sidechain_numbers) — multi OP_DRIVECHAIN
///   on the same *active* slot Rejects
/// * [`ctips`](Self::ctips) — structural M5/M6 current-ctip checks (spend/replace,
///   zero-diff, missing deposit address, M6 shape)
/// * [`pending_m6ids`](Self::pending_m6ids) +
///   [`withdrawal_bundle_inclusion_threshold`](Self::withdrawal_bundle_inclusion_threshold)
///   — M6 pending match + vote threshold (Path A)
/// * Connect path: coinbase M7 → accepted_bmm match for M8 (block-local; no LMDB);
///   sequential ctip + pending fold across block txs
///
/// Residual Path B (not on wire): historical `ctip_outpoint_to_value_seq`
/// (unbounded over chain life; full map not practical; cap/paginate dishonest)
/// and M1–M4 proposal/ack DB (partial extract dual-AND-unsafe). SoftForkRule
/// **Accepts** those markers when Path A checks pass; Local dual-AND remains
/// production authority. Slim-hub remote-only is **not** full M1–M8 authority.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DrivechainStateSnapshot {
    pub tip_height: u32,
    /// Bitcoin display hex of the mainchain tip hash (used for M8 tip check).
    pub tip_hash_hex: String,
    /// Active sidechain slot numbers (u8; multi-OP_DRIVECHAIN check).
    #[serde(default)]
    pub active_sidechain_numbers: Vec<u8>,
    /// Current ctip per sidechain (rules.v1; empty when none / omitted by old peers).
    #[serde(default)]
    pub ctips: Vec<DrivechainCtipSnapshot>,
    /// Pending M6ids per sidechain (Path A; empty when none / omitted by old peers).
    #[serde(default)]
    pub pending_m6ids: Vec<DrivechainPendingM6idSnapshot>,
    /// BIP 300: successful M6 requires `vote_count >` this (hub network threshold).
    /// Hub must send its live value so dual-AND does not false-Reject valid M6.
    #[serde(default)]
    pub withdrawal_bundle_inclusion_threshold: u16,
}

/// Mempool validation context shared by Local rules and IPC payloads.
///
/// Adapters that need a full Bitcoin tx set `tx`. Optional `parent_txs` supply
/// prevouts for BIP 360 remote/local spends. Optional `drivechain_state` is the
/// tip/sidechain snapshot for drivechain SoftForkRule (rules.v1). Label-only
/// contexts are used by unit tests and health-style Local stubs.
#[derive(Debug, Clone, Copy)]
pub struct RuleTxContext<'a> {
    pub height: u32,
    /// Opaque tag for tests / IPC correlation.
    pub label: &'a str,
    /// When set, feature-gated adapters may run real validation.
    pub tx: Option<&'a bitcoin::Transaction>,
    /// Parent transactions keyed by txid (for BIP 360 mempool prevouts).
    pub parent_txs: Option<&'a std::collections::HashMap<bitcoin::Txid, bitcoin::Transaction>>,
    /// Drivechain tip + active sidechains (rules.v1 `drivechain_state`).
    pub drivechain_state: Option<&'a DrivechainStateSnapshot>,
}

impl<'a> RuleTxContext<'a> {
    pub fn label_only(height: u32, label: &'a str) -> Self {
        Self {
            height,
            label,
            tx: None,
            parent_txs: None,
            drivechain_state: None,
        }
    }

    pub fn with_tx(height: u32, label: &'a str, tx: &'a bitcoin::Transaction) -> Self {
        Self {
            height,
            label,
            tx: Some(tx),
            parent_txs: None,
            drivechain_state: None,
        }
    }

    pub fn with_tx_and_parents(
        height: u32,
        label: &'a str,
        tx: &'a bitcoin::Transaction,
        parent_txs: &'a std::collections::HashMap<bitcoin::Txid, bitcoin::Transaction>,
    ) -> Self {
        Self {
            height,
            label,
            tx: Some(tx),
            parent_txs: Some(parent_txs),
            drivechain_state: None,
        }
    }

    /// Attach drivechain tip/sidechain snapshot for SoftForkRule / remote wire.
    pub fn with_drivechain_state(mut self, state: &'a DrivechainStateSnapshot) -> Self {
        self.drivechain_state = Some(state);
        self
    }
}

/// Block connect context. Feature-gated BlockHandler connect produces rule
/// ballots and composes them with [`RuleEngine::decide`] (same AND as mempool).
///
/// `chain_p2mr_utxos` supplies confirmed P2MR outputs for remote/local bip360
/// SoftForkRule connect (rules.v1 `chain_p2mr_utxos_hex`). Hub may still apply
/// its own local UTXO diff after remote Accept.
///
/// `drivechain_state` supplies tip/active sidechains for drivechain SoftForkRule.
#[derive(Debug, Clone, Copy)]
pub struct RuleBlockContext<'a> {
    pub height: u32,
    pub label: &'a str,
    pub block: Option<&'a bitcoin::Block>,
    /// Confirmed-chain P2MR UTXO set (OutPoint → TxOut) for bip360 connect.
    pub chain_p2mr_utxos: Option<&'a std::collections::HashMap<bitcoin::OutPoint, bitcoin::TxOut>>,
    /// Drivechain tip + active sidechains (rules.v1 `drivechain_state`).
    pub drivechain_state: Option<&'a DrivechainStateSnapshot>,
}

impl<'a> RuleBlockContext<'a> {
    pub fn label_only(height: u32, label: &'a str) -> Self {
        Self {
            height,
            label,
            block: None,
            chain_p2mr_utxos: None,
            drivechain_state: None,
        }
    }

    pub fn with_block(height: u32, label: &'a str, block: &'a bitcoin::Block) -> Self {
        Self {
            height,
            label,
            block: Some(block),
            chain_p2mr_utxos: None,
            drivechain_state: None,
        }
    }

    pub fn with_block_and_chain_utxos(
        height: u32,
        label: &'a str,
        block: &'a bitcoin::Block,
        chain_p2mr_utxos: &'a std::collections::HashMap<bitcoin::OutPoint, bitcoin::TxOut>,
    ) -> Self {
        Self {
            height,
            label,
            block: Some(block),
            chain_p2mr_utxos: Some(chain_p2mr_utxos),
            drivechain_state: None,
        }
    }

    /// Attach drivechain tip/sidechain snapshot for SoftForkRule / remote wire.
    pub fn with_drivechain_state(mut self, state: &'a DrivechainStateSnapshot) -> Self {
        self.drivechain_state = Some(state);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleCallError {
    pub detail: String,
}

impl RuleCallError {
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for RuleCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.detail)
    }
}

impl std::error::Error for RuleCallError {}

/// Hub-side registry of registered rules (in-process for P1 Local backends).
///
/// **Concurrency:** not internally synchronized. Do not call `register` /
/// `deregister` concurrently with `validate_tx` / `connect_block`. If shared
/// across threads, wrap in an external `Mutex` (or rebuild the registry on the
/// voting thread only).
#[derive(Default)]
pub struct RuleEngine {
    rules: Vec<Box<dyn SoftForkRule>>,
}

impl RuleEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a rule. While registered, its consent is required on every
    /// aggregate decision.
    pub fn register(&mut self, rule: Box<dyn SoftForkRule>) {
        let id = rule.id();
        self.rules.retain(|r| r.id() != id);
        self.rules.push(rule);
    }

    /// Deregister by id — rule leaves the voting set.
    pub fn deregister(&mut self, id: RuleId) -> bool {
        let before = self.rules.len();
        self.rules.retain(|r| r.id() != id);
        self.rules.len() != before
    }

    pub fn registered_ids(&self) -> Vec<RuleId> {
        self.rules.iter().map(|r| r.id()).collect()
    }

    fn ballot_from_result(rule_id: RuleId, result: Result<RuleVote, RuleCallError>) -> RuleBallot {
        match result {
            Ok(vote) => RuleBallot::explicit(rule_id, vote),
            Err(err) => RuleBallot::failure(rule_id, err.detail),
        }
    }

    /// Collect ballots for mempool validation (does not aggregate).
    pub fn collect_tx_ballots(&self, ctx: &RuleTxContext<'_>) -> Vec<RuleBallot> {
        self.rules
            .iter()
            .map(|r| Self::ballot_from_result(r.id(), r.validate_tx(ctx)))
            .collect()
    }

    /// Collect ballots for block connect (does not aggregate).
    pub fn collect_block_ballots(&self, ctx: &RuleBlockContext<'_>) -> Vec<RuleBallot> {
        self.rules
            .iter()
            .map(|r| Self::ballot_from_result(r.id(), r.connect_block(ctx)))
            .collect()
    }

    /// Ask every registered rule for mempool consent; AND + fail-closed.
    /// Error-path ballots (Timeout/Failure) are logged at `tracing::error!`.
    pub fn validate_tx(&self, ctx: &RuleTxContext<'_>) -> AggregateDecision {
        let ballots = self.collect_tx_ballots(ctx);
        log_error_path_ballots(&ballots);
        aggregate_and(&ballots)
    }

    /// Same for block connect.
    pub fn connect_block(&self, ctx: &RuleBlockContext<'_>) -> AggregateDecision {
        let ballots = self.collect_block_ballots(ctx);
        log_error_path_ballots(&ballots);
        aggregate_and(&ballots)
    }

    /// Aggregate pre-built ballots (e.g. from BlockHandler facades) with the
    /// same consent logging as in-process engine evaluation.
    pub fn decide(ballots: &[RuleBallot]) -> AggregateDecision {
        log_error_path_ballots(ballots);
        aggregate_and(ballots)
    }

    /// Simulate a timeout for a rule (tests / IPC layer): counts as no.
    pub fn ballot_timeout(rule_id: RuleId) -> RuleBallot {
        RuleBallot::timeout(rule_id)
    }
}

/// Backend that can vote: Local trait object or Remote IPC client (P2).
pub enum RuleBackend {
    Local(Box<dyn SoftForkRule>),
    Remote(ipc::RemoteRuleClient),
}

impl RuleBackend {
    pub fn id(&self) -> RuleId {
        match self {
            Self::Local(rule) => rule.id(),
            Self::Remote(client) => client.rule_id(),
        }
    }

    /// Obtain a ballot for mempool validation (fail-closed on remote errors).
    pub fn validate_tx(&self, ctx: &RuleTxContext<'_>) -> RuleBallot {
        match self {
            Self::Local(rule) => RuleEngine::ballot_from_result(rule.id(), rule.validate_tx(ctx)),
            Self::Remote(client) => client.validate_tx(ctx),
        }
    }

    pub fn connect_block(&self, ctx: &RuleBlockContext<'_>) -> RuleBallot {
        match self {
            Self::Local(rule) => RuleEngine::ballot_from_result(rule.id(), rule.connect_block(ctx)),
            Self::Remote(client) => client.connect_block(ctx),
        }
    }
}

/// Hub registry over Local and/or Remote backends (P2 composition surface).
///
/// **Concurrency:** same contract as [`RuleEngine`] — not internally
/// synchronized; serialize register/deregister against voting.
#[derive(Default)]
pub struct BackendEngine {
    backends: Vec<RuleBackend>,
}

impl BackendEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, backend: RuleBackend) {
        let id = backend.id();
        self.backends.retain(|b| b.id() != id);
        self.backends.push(backend);
    }

    /// Deregister by id — backend leaves the voting set (mirrors [`RuleEngine`]).
    pub fn deregister(&mut self, id: RuleId) -> bool {
        let before = self.backends.len();
        self.backends.retain(|b| b.id() != id);
        self.backends.len() != before
    }

    pub fn registered_ids(&self) -> Vec<RuleId> {
        self.backends.iter().map(|b| b.id()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    pub fn contains(&self, id: RuleId) -> bool {
        self.backends.iter().any(|b| b.id() == id)
    }

    /// Collect ballots without aggregating (for AND with BlockHandler local ballots).
    pub fn collect_tx_ballots(&self, ctx: &RuleTxContext<'_>) -> Vec<RuleBallot> {
        self.backends.iter().map(|b| b.validate_tx(ctx)).collect()
    }

    /// Collect connect ballots without aggregating.
    pub fn collect_block_ballots(&self, ctx: &RuleBlockContext<'_>) -> Vec<RuleBallot> {
        self.backends.iter().map(|b| b.connect_block(ctx)).collect()
    }

    pub fn validate_tx(&self, ctx: &RuleTxContext<'_>) -> AggregateDecision {
        RuleEngine::decide(&self.collect_tx_ballots(ctx))
    }

    pub fn connect_block(&self, ctx: &RuleBlockContext<'_>) -> AggregateDecision {
        RuleEngine::decide(&self.collect_block_ballots(ctx))
    }
}

/// Build the same mempool ballots as `BlockHandler::validate_tx` feature gates,
/// then AND via [`RuleEngine::decide`]. Used as the regression surface for
/// combined-feature composition without spinning a full validator DB.
///
/// * `drivechain_ok`: `None` = feature off (no ballot); `Some(ok)` = ballot
/// * `bip360_ok`: same
///
/// Ballot ids match production rule constants when those modules are compiled
/// (`"drivechain"` / `"bip360"`); this helper stays feature-agnostic for tests.
pub fn compose_mempool_feature_decision(
    drivechain_ok: Option<bool>,
    bip360_ok: Option<bool>,
) -> AggregateDecision {
    let mut ballots = Vec::new();
    if let Some(ok) = drivechain_ok {
        ballots.push(RuleBallot::from_ok(
            "drivechain",
            ok,
            "drivechain mempool validation failed",
        ));
    }
    if let Some(ok) = bip360_ok {
        ballots.push(RuleBallot::from_ok(
            "bip360",
            ok,
            "bip360 mempool validation failed",
        ));
    }
    RuleEngine::decide(&ballots)
}

/// Connect-level vote helper used by BlockHandler and tests.
///
/// Maps a boolean ok (from existing connect validation) into a rule ballot.
pub fn connect_level_ballot(
    rule_id: RuleId,
    ok: bool,
    reject_reason: impl Into<String>,
) -> RuleBallot {
    RuleBallot::from_ok(rule_id, ok, reject_reason)
}

/// Build the same connect-level ballots as `BlockHandler::connect_block` feature
/// gates, then AND via [`RuleEngine::decide`].
///
/// * `drivechain_ok`: `None` = feature off (no ballot); `Some(ok)` = ballot
/// * `bip360_ok`: same
///
/// Mirrors [`compose_mempool_feature_decision`] for the tip path.
pub fn compose_connect_feature_decision(
    drivechain_ok: Option<bool>,
    bip360_ok: Option<bool>,
) -> AggregateDecision {
    let mut ballots = Vec::new();
    if let Some(ok) = drivechain_ok {
        ballots.push(RuleBallot::from_ok(
            "drivechain",
            ok,
            "drivechain connect_block validation failed",
        ));
    }
    if let Some(ok) = bip360_ok {
        ballots.push(RuleBallot::from_ok(
            "bip360",
            ok,
            "bip360 connect_block validation failed",
        ));
    }
    RuleEngine::decide(&ballots)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysAccept {
        id: RuleId,
    }

    impl SoftForkRule for AlwaysAccept {
        fn id(&self) -> RuleId {
            self.id
        }
        fn validate_tx(&self, _: &RuleTxContext<'_>) -> Result<RuleVote, RuleCallError> {
            Ok(RuleVote::Accept)
        }
        fn connect_block(&self, _: &RuleBlockContext<'_>) -> Result<RuleVote, RuleCallError> {
            Ok(RuleVote::Accept)
        }
    }

    struct AlwaysReject {
        id: RuleId,
    }

    impl SoftForkRule for AlwaysReject {
        fn id(&self) -> RuleId {
            self.id
        }
        fn validate_tx(&self, _: &RuleTxContext<'_>) -> Result<RuleVote, RuleCallError> {
            Ok(RuleVote::reject("nope"))
        }
        fn connect_block(&self, _: &RuleBlockContext<'_>) -> Result<RuleVote, RuleCallError> {
            Ok(RuleVote::reject("nope"))
        }
    }

    struct AlwaysFail {
        id: RuleId,
    }

    impl SoftForkRule for AlwaysFail {
        fn id(&self) -> RuleId {
            self.id
        }
        fn validate_tx(&self, _: &RuleTxContext<'_>) -> Result<RuleVote, RuleCallError> {
            Err(RuleCallError::new("boom"))
        }
        fn connect_block(&self, _: &RuleBlockContext<'_>) -> Result<RuleVote, RuleCallError> {
            Err(RuleCallError::new("boom"))
        }
    }

    fn tx_ctx() -> RuleTxContext<'static> {
        RuleTxContext::label_only(1, "t")
    }

    #[test]
    fn empty_registry_accepts() {
        let engine = RuleEngine::new();
        assert!(engine.validate_tx(&tx_ctx()).is_accept());
    }

    #[test]
    fn all_accept_accepts() {
        let mut engine = RuleEngine::new();
        engine.register(Box::new(AlwaysAccept { id: "a" }));
        engine.register(Box::new(AlwaysAccept { id: "b" }));
        assert!(engine.validate_tx(&tx_ctx()).is_accept());
    }

    #[test]
    fn one_explicit_reject_rejects() {
        let mut engine = RuleEngine::new();
        engine.register(Box::new(AlwaysAccept { id: "a" }));
        engine.register(Box::new(AlwaysReject { id: "b" }));
        match engine.validate_tx(&tx_ctx()) {
            AggregateDecision::Reject { rejections } => {
                assert_eq!(rejections.len(), 1);
                assert_eq!(rejections[0].rule_id, "b");
                assert!(!rejections[0].source.is_error_path());
            }
            AggregateDecision::Accept => panic!("expected reject"),
        }
    }

    #[test]
    fn failure_counts_as_no_and_is_error_path() {
        let mut engine = RuleEngine::new();
        engine.register(Box::new(AlwaysAccept { id: "a" }));
        engine.register(Box::new(AlwaysFail { id: "b" }));
        match engine.validate_tx(&tx_ctx()) {
            AggregateDecision::Reject { rejections } => {
                assert_eq!(rejections.len(), 1);
                assert!(rejections[0].source.is_error_path());
                assert!(
                    rejections[0]
                        .source
                        .as_reject_reason()
                        .contains("failure counts as reject")
                );
            }
            AggregateDecision::Accept => panic!("failure must not soft-accept"),
        }
    }

    #[test]
    fn timeout_counts_as_no() {
        let ballots = vec![
            RuleBallot {
                rule_id: "a",
                source: VoteSource::Explicit(RuleVote::Accept),
            },
            RuleEngine::ballot_timeout("b"),
        ];
        match aggregate_and(&ballots) {
            AggregateDecision::Reject { rejections } => {
                assert_eq!(rejections[0].rule_id, "b");
                assert!(matches!(rejections[0].source, VoteSource::Timeout));
            }
            AggregateDecision::Accept => panic!("timeout must count as no"),
        }
    }

    #[test]
    fn deregister_removes_from_voting_set() {
        let mut engine = RuleEngine::new();
        engine.register(Box::new(AlwaysReject { id: "b" }));
        engine.register(Box::new(AlwaysAccept { id: "a" }));
        assert!(!engine.validate_tx(&tx_ctx()).is_accept());
        assert!(engine.deregister("b"));
        assert!(engine.validate_tx(&tx_ctx()).is_accept());
        assert_eq!(engine.registered_ids(), vec!["a"]);
    }

    #[test]
    fn reregister_replaces_same_id() {
        let mut engine = RuleEngine::new();
        engine.register(Box::new(AlwaysReject { id: "a" }));
        engine.register(Box::new(AlwaysAccept { id: "a" }));
        assert_eq!(engine.registered_ids(), vec!["a"]);
        assert!(engine.validate_tx(&tx_ctx()).is_accept());
    }

    #[test]
    fn decide_aggregates_prebuilt_ballots_like_and() {
        let ballots = vec![
            RuleBallot::from_ok("drivechain", true, ""),
            RuleBallot::from_ok("bip360", false, "pqc failed"),
        ];
        assert!(!RuleEngine::decide(&ballots).is_accept());
        let both_ok = vec![
            RuleBallot::from_ok("drivechain", true, ""),
            RuleBallot::from_ok("bip360", true, ""),
        ];
        assert!(RuleEngine::decide(&both_ok).is_accept());
    }

    #[test]
    fn connect_level_ballot_helper() {
        let ok = connect_level_ballot("drivechain", true, "unused");
        assert!(ok.consents());
        let bad = connect_level_ballot("bip360", false, "bad block");
        assert!(!bad.consents());
        assert!(bad.source.as_reject_reason().contains("bad block"));
    }

    #[test]
    fn backend_engine_local_x2_and() {
        let mut engine = BackendEngine::new();
        engine.register(RuleBackend::Local(Box::new(AlwaysAccept { id: "a" })));
        engine.register(RuleBackend::Local(Box::new(AlwaysAccept { id: "b" })));
        assert!(engine.validate_tx(&tx_ctx()).is_accept());
        engine.register(RuleBackend::Local(Box::new(AlwaysReject { id: "b" })));
        assert!(!engine.validate_tx(&tx_ctx()).is_accept());
    }

    #[test]
    fn backend_engine_deregister_removes_from_voting_set() {
        let mut engine = BackendEngine::new();
        engine.register(RuleBackend::Local(Box::new(AlwaysReject { id: "b" })));
        engine.register(RuleBackend::Local(Box::new(AlwaysAccept { id: "a" })));
        assert!(!engine.validate_tx(&tx_ctx()).is_accept());
        assert!(engine.deregister("b"));
        assert!(!engine.deregister("b"));
        assert!(engine.validate_tx(&tx_ctx()).is_accept());
        assert_eq!(engine.registered_ids(), vec!["a"]);
    }

    /// Regression: BlockHandler::validate_tx composes feature ballots with AND
    /// via RuleEngine::decide — same matrix as pre-engine `dc && bip360`.
    #[test]
    fn truncate_for_log_caps_length() {
        let long = "x".repeat(500);
        let t = truncate_for_log(&long);
        assert!(t.chars().count() < 500);
        assert!(t.ends_with("…(truncated)"));
        assert_eq!(truncate_for_log("short"), "short");
    }

    #[test]
    fn truncate_for_log_strips_control_chars() {
        let dirty = "bad\nline\rwith\tctrl\u{0001}chars";
        let t = truncate_for_log(dirty);
        assert!(!t.contains('\n'));
        assert!(!t.contains('\r'));
        assert!(!t.contains('\u{0001}'));
        assert!(t.contains("bad"));
        assert!(t.contains("line"));
    }

    #[test]
    fn validate_tx_feature_ballot_composition_matches_prior_and() {
        // combined both ok
        assert!(compose_mempool_feature_decision(Some(true), Some(true)).is_accept());
        // either false → reject
        assert!(!compose_mempool_feature_decision(Some(false), Some(true)).is_accept());
        assert!(!compose_mempool_feature_decision(Some(true), Some(false)).is_accept());
        assert!(!compose_mempool_feature_decision(Some(false), Some(false)).is_accept());
        // drivechain-only
        assert!(compose_mempool_feature_decision(Some(true), None).is_accept());
        assert!(!compose_mempool_feature_decision(Some(false), None).is_accept());
        // bip360-only
        assert!(compose_mempool_feature_decision(None, Some(true)).is_accept());
        assert!(!compose_mempool_feature_decision(None, Some(false)).is_accept());
        // neither feature (empty) → accept
        assert!(compose_mempool_feature_decision(None, None).is_accept());
    }

    #[test]
    fn connect_feature_ballot_composition_matches_prior_and() {
        assert!(compose_connect_feature_decision(Some(true), Some(true)).is_accept());
        assert!(!compose_connect_feature_decision(Some(false), Some(true)).is_accept());
        assert!(!compose_connect_feature_decision(Some(true), Some(false)).is_accept());
        assert!(!compose_connect_feature_decision(Some(false), Some(false)).is_accept());
        assert!(compose_connect_feature_decision(Some(true), None).is_accept());
        assert!(!compose_connect_feature_decision(Some(false), None).is_accept());
        assert!(compose_connect_feature_decision(None, Some(true)).is_accept());
        assert!(!compose_connect_feature_decision(None, Some(false)).is_accept());
        assert!(compose_connect_feature_decision(None, None).is_accept());
    }

    /// Tokens that must never appear on SCORE / PR-green surfaces (live tip residual).
    /// Shared by validate-script code, check-rules-engine job, and Justfile SCORE recipe.
    const FORBIDDEN_SCORE_LIVE_TIP: &[&str] = &[
        "live-tip-rules-workers-e2e",
        "sigterm-rules-workers-e2e",
        "CUSF_LIVE_TIP_E2E",
        "BITCOIND",
        "integrationtests.env",
    ];

    /// Exact HITL dual-AND gate for live tip (never automatic PR).
    const HITL_LIVE_TIP_IF: &str =
        "github.event_name == 'workflow_dispatch' && inputs.run_live_tip_e2e == true";

    /// PR CI live tip residual honesty (SCORE lock).
    ///
    /// Automatic PR green matrix (`check-rules-engine` / `validate-rules-engine`)
    /// must stay free of bitcoind / live tip / SIGTERM e2e. Live tip remains HITL
    /// (`workflow_dispatch` input `run_live_tip_e2e` only). Do **not** invent
    /// automatic PR CI bitcoind live tip without product unlock.
    ///
    /// Paths are relative to this source file → enforcer root
    /// (`lib/validator/rules/` → `../../../`); parent RESIDUAL is one more level up.
    #[test]
    fn pr_ci_live_tip_residual_honesty_score_lock() {
        // validate-rules-engine.sh = SCORE matrix body (must not require bitcoind).
        const VALIDATE: &str = include_str!("../../../scripts/validate-rules-engine.sh");
        assert!(
            VALIDATE.contains("validate-rules-engine"),
            "SCORE script identity marker missing"
        );
        assert!(
            VALIDATE.contains("SCORE / PR green matrix honesty"),
            "SCORE script must document PR green residual honesty banner"
        );
        // Executable lines only (comments may name residual recipes for operators).
        let validate_code = strip_shell_comment_lines(VALIDATE);
        assert!(
            !validate_code.is_empty(),
            "stripped SCORE script must be non-empty (strip must not vacuous-pass)"
        );
        assert!(
            validate_code.contains("set -euo pipefail"),
            "stripped SCORE script must keep set -euo pipefail"
        );
        assert!(
            validate_code.contains("cargo test -p bip300301_enforcer_lib rules::"),
            "stripped SCORE script must keep rules:: cargo test"
        );
        assert_no_forbidden_score_live_tip(&validate_code, "validate-rules-engine.sh code");

        // GH workflow: check-rules-engine vs HITL live tip job separation.
        const WORKFLOW: &str =
            include_str!("../../../.github/workflows/check_lint_build_release.yaml");
        assert!(
            WORKFLOW.contains("check-rules-engine:"),
            "missing check-rules-engine job"
        );
        assert!(
            WORKFLOW.contains("Do NOT require bitcoind / live tip here (PR green matrix)."),
            "check-rules-engine must document no-bitcoind PR green policy"
        );
        assert!(
            WORKFLOW.contains("live-tip-rules-workers-e2e-hitl:"),
            "missing HITL live tip job"
        );
        assert!(
            WORKFLOW.contains("run_live_tip_e2e"),
            "missing workflow_dispatch input run_live_tip_e2e"
        );

        // Fail closed if someone wires live tip into the PR green job body.
        // Extract the check-rules-engine job block (until next top-level job key).
        // Strip full-line `#` comments (inter-job residual notes may name BITCOIND).
        let check_job =
            strip_yaml_full_line_comments(extract_yaml_job_body(WORKFLOW, "check-rules-engine"));
        assert!(
            !check_job.trim().is_empty(),
            "check-rules-engine body must be non-empty after comment strip"
        );
        assert!(
            check_job.contains("./scripts/validate-rules-engine.sh"),
            "check-rules-engine body must run validate-rules-engine.sh"
        );
        assert_no_forbidden_score_live_tip(&check_job, "check-rules-engine job body");

        // HITL gate must be the job's real non-comment `if:` (not a residual note).
        let hitl_raw = extract_yaml_job_body(WORKFLOW, "live-tip-rules-workers-e2e-hitl");
        let hitl_job = strip_yaml_full_line_comments(hitl_raw);
        assert!(
            !hitl_job.trim().is_empty(),
            "HITL live tip job body must be non-empty after comment strip"
        );
        let hitl_if = extract_gha_job_if_value(&hitl_job)
            .expect("HITL live tip job must have a non-comment if: line");
        assert_eq!(
            hitl_if, HITL_LIVE_TIP_IF,
            "HITL live tip if: must be exact dual-AND gate (workflow_dispatch + run_live_tip_e2e); \
             never pull_request automatic / dispatch-only"
        );
        assert!(
            hitl_job.contains("CUSF_LIVE_TIP_E2E"),
            "HITL job must set CUSF_LIVE_TIP_E2E for fail-closed opt-in"
        );

        // Justfile: SCORE recipe body is exactly the single SCORE script (no append).
        const JUSTFILE: &str = include_str!("../../../Justfile");
        let score_recipe = extract_just_recipe_body(JUSTFILE, "validate-rules-engine");
        assert!(
            !score_recipe.trim().is_empty(),
            "validate-rules-engine Just recipe body must be non-empty"
        );
        let score_recipe_code = strip_shell_comment_lines(score_recipe);
        assert_eq!(
            score_recipe_code.trim(),
            "./scripts/validate-rules-engine.sh",
            "just validate-rules-engine must be exactly the SCORE script (no extra lines)"
        );
        assert_no_forbidden_score_live_tip(&score_recipe_code, "Justfile validate-rules-engine");

        // Opt-in recipes remain separate top-level targets (not folded into SCORE).
        let live_recipe = extract_just_recipe_body(JUSTFILE, "live-tip-rules-workers-e2e");
        assert!(
            strip_shell_comment_lines(live_recipe)
                .contains("./scripts/live-tip-rules-workers-e2e.sh"),
            "live tip recipe must invoke live-tip script"
        );
        let sigterm_recipe = extract_just_recipe_body(JUSTFILE, "sigterm-rules-workers-e2e");
        assert!(
            strip_shell_comment_lines(sigterm_recipe)
                .contains("./scripts/sigterm-rules-workers-e2e.sh"),
            "sigterm recipe must invoke sigterm script"
        );
    }

    fn assert_no_forbidden_score_live_tip(surface: &str, label: &str) {
        for forbidden in FORBIDDEN_SCORE_LIVE_TIP {
            assert!(
                !surface.contains(forbidden),
                "{label} must not reference `{forbidden}` \
                 (PR green matrix stays free of bitcoind/live tip/SIGTERM e2e)"
            );
        }
    }

    /// Drop full-line `#` comments and blank lines (keeps code + inline tails).
    fn strip_shell_comment_lines(script: &str) -> String {
        script
            .lines()
            .filter(|line| {
                let t = line.trim_start();
                !t.is_empty() && !t.starts_with('#')
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Drop YAML full-line comments (`# …` after optional indent).
    fn strip_yaml_full_line_comments(yaml: &str) -> String {
        yaml.lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Extract indented body of a top-level GH Actions job key `name:`.
    fn extract_yaml_job_body<'a>(workflow: &'a str, job_name: &str) -> &'a str {
        let header = format!("  {job_name}:");
        let start = workflow
            .find(&header)
            .unwrap_or_else(|| panic!("job `{job_name}` not found in workflow"));
        let after = &workflow[start + header.len()..];
        // Next top-level job key: exactly two spaces, non-comment token, ends with `:`.
        let mut end = after.len();
        let mut offset = 0usize;
        for line in after.split_inclusive('\n') {
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if is_gha_top_level_job_key(trimmed) {
                end = offset;
                break;
            }
            offset += line.len();
        }
        &after[..end]
    }

    fn is_gha_top_level_job_key(line: &str) -> bool {
        // `  job-name:` only — not comments (`  # …`) or deeper keys (`    runs-on:`).
        if !line.starts_with("  ") || line.starts_with("   ") {
            return false;
        }
        let rest = &line[2..];
        if rest.is_empty() || rest.starts_with('#') || rest.starts_with(' ') {
            return false;
        }
        line.ends_with(':') && !rest.contains(": ")
    }

    /// First non-comment `if: <value>` on a GH Actions job body (indent ≥ 4 typical).
    fn extract_gha_job_if_value(job_body: &str) -> Option<String> {
        for line in job_body.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let Some(rest) = t.strip_prefix("if:") else {
                continue;
            };
            return Some(rest.trim().to_string());
        }
        None
    }

    /// Body of a Just recipe `name:` until the next top-level recipe key.
    ///
    /// Matches a line that starts with `recipe_name` and is a recipe header
    /// (`name:` or `name args…:`). Body begins after that header line.
    fn extract_just_recipe_body<'a>(justfile: &'a str, recipe_name: &str) -> &'a str {
        let mut offset = 0usize;
        let mut body_start = None;
        for line in justfile.split_inclusive('\n') {
            let trimmed = line.trim_end_matches(['\n', '\r']);
            // Recipe headers are top-level (no indent) and start with the name.
            if !trimmed.starts_with([' ', '\t', '#'])
                && (trimmed == format!("{recipe_name}:")
                    || trimmed.starts_with(&format!("{recipe_name} "))
                    || trimmed.starts_with(&format!("{recipe_name}:")))
                && trimmed.ends_with(':')
            {
                body_start = Some(offset + line.len());
                break;
            }
            offset += line.len();
        }
        let start = body_start.unwrap_or_else(|| panic!("Just recipe `{recipe_name}:` not found"));
        let after = &justfile[start..];
        let mut end = after.len();
        let mut off = 0usize;
        for line in after.split_inclusive('\n') {
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if is_just_top_level_recipe_key(trimmed) {
                end = off;
                break;
            }
            off += line.len();
        }
        &after[..end]
    }

    fn is_just_top_level_recipe_key(line: &str) -> bool {
        // Top-level: no leading whitespace, not a comment, ends with `:` (may have params).
        if line.is_empty() || line.starts_with([' ', '\t', '#']) {
            return false;
        }
        // Recipe headers look like `name:` or `name arg='x':` — require trailing `:`.
        if !line.ends_with(':') {
            return false;
        }
        // Attribute lines like `[private]` are not recipes.
        if line.starts_with('[') {
            return false;
        }
        true
    }

    /// Unit tests for SCORE residual-honesty strip/extract helpers (Issue 3).
    #[test]
    fn residual_honesty_helpers_unit_matrix() {
        // Shell strip: drop full-line `#`, keep code with inline tails, drop blanks.
        assert_eq!(
            strip_shell_comment_lines("# only\n\nset -e\necho ok # tail\n# end\n"),
            "set -e\necho ok # tail"
        );
        // Synthetic: comment naming BITCOIND stripped; export remains for forbidden scan.
        let shell = "# residual note BITCOIND soft-skip\nexport BITCOIND=/x\ncargo test\n";
        let code = strip_shell_comment_lines(shell);
        assert!(!code.contains("soft-skip"));
        assert!(code.contains("export BITCOIND=/x"));
        assert!(FORBIDDEN_SCORE_LIVE_TIP.iter().any(|t| code.contains(t)));

        // YAML strip: full-line comments only (join does not re-add trailing newline).
        assert_eq!(
            strip_yaml_full_line_comments("  run: x\n  # BITCOIND note\n  name: y\n"),
            "  run: x\n  name: y"
        );

        // Job key detector.
        assert!(is_gha_top_level_job_key("  check-rules-engine:"));
        assert!(!is_gha_top_level_job_key("    runs-on: ubuntu-latest"));
        assert!(!is_gha_top_level_job_key("  # comment:"));
        assert!(!is_gha_top_level_job_key("check-rules-engine:")); // must be 2-space jobs:

        // Extract stops at next two-space job key; inter-job comments stay in prior body.
        // Use concat! (not `\` line-cont) so leading job indent is preserved.
        let wf = concat!(
            "  job-a:\n",
            "    name: A\n",
            "  # inter-job BITCOIND note\n",
            "  job-b:\n",
            "    name: B\n",
        );
        let body_a = extract_yaml_job_body(wf, "job-a");
        assert!(body_a.contains("name: A"));
        assert!(body_a.contains("# inter-job BITCOIND note"));
        assert!(!body_a.contains("job-b:"));
        let stripped_a = strip_yaml_full_line_comments(body_a);
        assert!(!stripped_a.contains("BITCOIND"));
        assert!(stripped_a.contains("name: A"));

        // if: extraction binds real gate, ignores full-line comments.
        let hitl = concat!(
            "    name: Live tip\n",
            "    # if: github.event_name == 'pull_request'\n",
            "    if: github.event_name == 'workflow_dispatch' && inputs.run_live_tip_e2e == true\n",
            "    runs-on: ubuntu-latest\n",
        );
        assert_eq!(
            extract_gha_job_if_value(&strip_yaml_full_line_comments(hitl)).as_deref(),
            Some(HITL_LIVE_TIP_IF)
        );
        // Weakened gate must not match constant.
        let weak = "    if: github.event_name == 'pull_request'\n";
        assert_ne!(
            extract_gha_job_if_value(weak).as_deref(),
            Some(HITL_LIVE_TIP_IF)
        );

        // Just recipe body: single line SCORE; multi-line with forbidden would be visible.
        let just = concat!(
            "validate-rules-engine:\n",
            "    ./scripts/validate-rules-engine.sh\n",
            "\n",
            "build-hub-workers:\n",
            "    cargo build\n",
        );
        let body = extract_just_recipe_body(just, "validate-rules-engine");
        assert_eq!(
            strip_shell_comment_lines(body).trim(),
            "./scripts/validate-rules-engine.sh"
        );
        let multi = concat!(
            "validate-rules-engine:\n",
            "    ./scripts/validate-rules-engine.sh\n",
            "    ./scripts/live-tip-rules-workers-e2e.sh\n",
            "\n",
            "other:\n",
            "    true\n",
        );
        let multi_body = extract_just_recipe_body(multi, "validate-rules-engine");
        assert!(multi_body.contains("live-tip-rules-workers-e2e"));
        assert!(is_just_top_level_recipe_key("build-hub-workers:"));
        assert!(is_just_top_level_recipe_key(
            "build-hub-workers profile='debug':"
        ));
        assert!(!is_just_top_level_recipe_key("    cargo build"));
        assert!(!is_just_top_level_recipe_key("# validate-rules-engine:"));
    }

    /// Mid-handler cancel + hub SIGTERM remain accepted residual (docs honesty).
    /// SCORE keeps only accept-loop shutdown unit coverage (`serve_uds_loop_honors_shutdown_flag`);
    /// process SIGTERM is opt-in outside SCORE; hub SIGTERM needs bitcoind (not invented).
    /// Locks both MULTI_ENFORCER (enforcer residual SSoT) and parent RESIDUAL.md.
    #[test]
    fn shutdown_residual_honesty_docs_inventory_lock() {
        const MULTI: &str = include_str!("../../../docs/MULTI_ENFORCER.md");
        // Stable AND anchors shipped this loop (no soft OR dead arms).
        for anchor in [
            "PR CI live tip residual honesty",
            "not PR green matrix",
            "mid-handler cancel",
            "Hub process SIGTERM",
            "Path B residual",
            "Dynamic `RuleId`",
            "static_rule_id",
            "pr_ci_live_tip_residual_honesty_score_lock",
            "shutdown_residual_honesty_docs_inventory_lock",
        ] {
            assert!(
                MULTI.contains(anchor),
                "MULTI_ENFORCER must inventory residual honesty anchor `{anchor}`"
            );
        }
        // Prefer honesty over SCORE invent: opt-in SIGTERM outside SCORE (dual-AND).
        assert!(
            MULTI.contains("sigterm-rules-workers-e2e") && MULTI.contains("outside SCORE"),
            "worker SIGTERM e2e must be documented opt-in outside SCORE"
        );

        // Parent open-residual table must stay aligned (primary-vs-lower drift lock).
        const RESIDUAL: &str = include_str!("../../../../RESIDUAL.md");
        for anchor in [
            "pr_ci_live_tip_residual_honesty_score_lock",
            "shutdown_residual_honesty_docs_inventory_lock",
            "Mid-handler cancel",
            "Hub process SIGTERM",
            "Path B residual",
            "Dynamic `RuleId`",
            "PR CI live tip",
            "check-rules-engine",
        ] {
            assert!(
                RESIDUAL.contains(anchor),
                "parent RESIDUAL.md must inventory residual honesty anchor `{anchor}`"
            );
        }
    }
}
