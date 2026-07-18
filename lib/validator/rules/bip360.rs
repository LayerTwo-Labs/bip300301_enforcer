//! BIP 360 (P2MR / PQC) soft-fork rule adapter.
//!
//! Thin facade over `validator::pqc` so Local backends and mempool composition
//! share one path. Parent prevouts for mempool spends are supplied via
//! [`Bip360TxParents`] when calling [`vote_mempool_transaction`], or via
//! [`RuleTxContext::parent_txs`] / rules.v1 `parent_txs_hex` on the wire.
//!
//! SoftForkRule is **fail-closed**: missing tx → `missing_tx`. With a tx, runs
//! `pqc::validate_mempool_transaction` with context parents (empty if absent —
//! P2MR spends fail closed without parents). Connect requires `chain_p2mr_utxos`
//! on context (rules.v1 `chain_p2mr_utxos_hex`); missing set → Reject. No
//! label-based Accept on production.

use std::{borrow::Borrow, collections::HashMap};

use bitcoin::{OutPoint, Transaction, TxOut, Txid};

use super::{
    RuleBallot, RuleBlockContext, RuleCallError, RuleId, RuleTxContext, RuleVote, SoftForkRule,
};
use crate::validator::pqc::{
    self, activation::Bip360Activation, limits::DEFAULT_PQC_VERIFY_BUDGET_MS,
};

/// Stable id used in ballots, IPC handshake, and logs.
pub const RULE_ID: RuleId = "bip360";

/// Parent-tx map for BIP 360 mempool prevout lookup (same shape as BlockHandler).
pub type Bip360TxParents<'a, TxRef> = &'a HashMap<Txid, TxRef>;

/// In-process BIP 360 rule (Local backend / worker).
#[derive(Debug, Clone, Copy)]
pub struct Bip360Rule {
    pub activation_height: u32,
    /// Wall-time budget for PQC verify during connect SoftForkRule path.
    pub pqc_verify_budget_ms: u64,
}

impl Bip360Rule {
    pub fn new(activation_height: u32) -> Self {
        Self {
            activation_height,
            pqc_verify_budget_ms: DEFAULT_PQC_VERIFY_BUDGET_MS,
        }
    }

    pub fn with_pqc_budget(mut self, budget_ms: u64) -> Self {
        self.pqc_verify_budget_ms = budget_ms;
        self
    }

    pub fn activation(&self) -> Bip360Activation {
        Bip360Activation(self.activation_height)
    }

    /// Call existing `pqc::validate_and_diff_block_transactions` and map to a vote.
    ///
    /// Diff is computed but **not applied** here — hub still owns persistent
    /// UTXO diff after remote Accept.
    pub fn vote_connect_block(
        &self,
        block: &bitcoin::Block,
        height: u32,
        chain_p2mr_utxos: &HashMap<OutPoint, TxOut>,
    ) -> RuleVote {
        match pqc::validate_and_diff_block_transactions(
            block,
            height,
            self.activation(),
            chain_p2mr_utxos,
            self.pqc_verify_budget_ms,
        ) {
            Ok(_) => RuleVote::Accept,
            Err(err) => {
                tracing::debug!(?err, "BIP 360 connect SoftForkRule validation failed");
                RuleVote::reject(err.to_string())
            }
        }
    }

    /// Call existing `pqc::validate_mempool_transaction` and map to a vote.
    pub fn vote_mempool_transaction<TxRef>(
        &self,
        tx: &Transaction,
        height: u32,
        parent_txs: Bip360TxParents<'_, TxRef>,
    ) -> RuleVote
    where
        TxRef: Borrow<Transaction>,
    {
        match pqc::validate_mempool_transaction(tx, height, self.activation(), parent_txs) {
            Ok(()) => RuleVote::Accept,
            Err(err) => {
                tracing::debug!(?err, "BIP 360 mempool validation failed");
                RuleVote::reject(err.to_string())
            }
        }
    }

    /// Ballot from a pqc mempool Result (BlockHandler dual-path).
    ///
    /// Validation `Err` → **explicit Reject** (not transport Failure).
    pub fn ballot_from_pqc_result(result: Result<(), pqc::PqcValidationError>) -> RuleBallot {
        match result {
            Ok(()) => RuleBallot::explicit(RULE_ID, RuleVote::Accept),
            Err(err) => {
                tracing::debug!(?err, "BIP 360 mempool validation failed");
                RuleBallot::explicit(RULE_ID, RuleVote::reject(err.to_string()))
            }
        }
    }

    /// Convert boolean ok (legacy path) into a ballot.
    pub fn ballot_from_validation_ok(ok: bool) -> RuleBallot {
        RuleBallot::from_ok(RULE_ID, ok, "bip360 mempool validation failed")
    }

    /// Connect-level ballot from boolean ok (BlockHandler tip path).
    pub fn ballot_connect_ok(ok: bool, reason: impl Into<String>) -> RuleBallot {
        RuleBallot::from_ok(RULE_ID, ok, reason)
    }

    /// Connect-level ballot from `pqc::validate_and_diff_block_transactions` Result.
    ///
    /// Validation `Err` → **explicit Reject** (not transport Failure).
    pub fn ballot_from_pqc_connect_result(
        result: Result<(), pqc::PqcValidationError>,
    ) -> RuleBallot {
        match result {
            Ok(()) => RuleBallot::explicit(RULE_ID, RuleVote::Accept),
            Err(err) => {
                tracing::debug!(?err, "BIP 360 connect_block validation failed");
                RuleBallot::explicit(RULE_ID, RuleVote::reject(err.to_string()))
            }
        }
    }
}

impl SoftForkRule for Bip360Rule {
    fn id(&self) -> RuleId {
        RULE_ID
    }

    fn validate_tx(&self, ctx: &RuleTxContext<'_>) -> Result<RuleVote, RuleCallError> {
        // Production path only — no label soft-Accept (wire cannot bypass policy).
        let Some(tx) = ctx.tx else {
            return Ok(RuleVote::reject("missing_tx"));
        };
        // Parents from context when present (rules.v1 parent_txs_hex / hub).
        // Empty map when absent: non-P2MR / inactive height may Accept; active
        // P2MR spends fail closed without parents.
        let empty: HashMap<Txid, Transaction> = HashMap::new();
        let parents = ctx.parent_txs.unwrap_or(&empty);
        Ok(self.vote_mempool_transaction(tx, ctx.height, parents))
    }

    fn connect_block(&self, ctx: &RuleBlockContext<'_>) -> Result<RuleVote, RuleCallError> {
        let Some(block) = ctx.block else {
            return Ok(RuleVote::reject("missing_block"));
        };
        // Fail-closed: real connect path needs the chain P2MR set on the wire/context.
        let Some(chain) = ctx.chain_p2mr_utxos else {
            return Ok(RuleVote::reject("missing_chain_p2mr_utxos"));
        };
        Ok(self.vote_connect_block(block, ctx.height, chain))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator::rules::{RuleEngine, SoftForkRule, VoteSource};

    fn test_only_label_vote(label: &str) -> Option<RuleVote> {
        if label == "test-accept" || label.starts_with("test-accept:") {
            return Some(RuleVote::Accept);
        }
        if label.contains("bip360-reject") || label == "test-reject" {
            return Some(RuleVote::reject("bip360 test reject"));
        }
        None
    }

    #[test]
    fn rule_id_is_stable() {
        assert_eq!(Bip360Rule::new(0).id(), "bip360");
    }

    #[test]
    fn ballot_from_ok_false_rejects() {
        let b = Bip360Rule::ballot_from_validation_ok(false);
        assert!(!b.consents());
        assert_eq!(b.rule_id, RULE_ID);
    }

    #[test]
    fn production_ignores_test_accept_label() {
        let rule = Bip360Rule::new(0);
        match rule
            .validate_tx(&RuleTxContext::label_only(0, "test-accept"))
            .unwrap()
        {
            RuleVote::Reject { reason } => assert!(reason.contains("missing_tx")),
            RuleVote::Accept => panic!("production must not soft-accept test-accept label"),
        }
        assert!(matches!(
            test_only_label_vote("test-accept"),
            Some(RuleVote::Accept)
        ));
        match test_only_label_vote("bip360-reject-case") {
            Some(RuleVote::Reject { reason }) => assert!(reason.contains("bip360")),
            other => panic!("expected reject helper, got {other:?}"),
        }
    }

    #[test]
    fn missing_tx_rejects() {
        let rule = Bip360Rule::new(0);
        match rule
            .validate_tx(&RuleTxContext::label_only(1, "prod"))
            .unwrap()
        {
            RuleVote::Reject { reason } => assert!(reason.contains("missing_tx")),
            RuleVote::Accept => panic!("must not soft-accept"),
        }
    }

    #[test]
    fn ballot_from_pqc_result_err_is_explicit_reject() {
        let b = Bip360Rule::ballot_from_pqc_result(Err(pqc::PqcValidationError::Block(
            "test vector failure".into(),
        )));
        assert_eq!(b.rule_id, RULE_ID);
        assert!(!b.consents());
        assert!(!b.source.is_error_path()); // validation reject, not transport failure
        match &b.source {
            VoteSource::Explicit(RuleVote::Reject { reason }) => {
                assert!(reason.contains("test vector failure"));
            }
            other => panic!("expected explicit reject, got {other:?}"),
        }
    }

    #[test]
    fn engine_and_with_drivechain_style_ballots() {
        // Simulate combined-feature mempool AND via prebuilt ballots.
        let ballots = vec![
            RuleBallot::from_ok("drivechain", true, ""),
            Bip360Rule::ballot_from_validation_ok(true),
        ];
        assert!(RuleEngine::decide(&ballots).is_accept());
        let reject = vec![
            RuleBallot::from_ok("drivechain", true, ""),
            Bip360Rule::ballot_from_validation_ok(false),
        ];
        assert!(!RuleEngine::decide(&reject).is_accept());
    }

    #[test]
    fn vote_mempool_inactive_height_accepts_empty_parents() {
        // activation_height = u32::MAX → never active → Accept without parents.
        let rule = Bip360Rule::new(u32::MAX);
        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![],
        };
        let parents: HashMap<Txid, Transaction> = HashMap::new();
        assert!(rule.vote_mempool_transaction(&tx, 0, &parents).is_accept());
    }

    #[test]
    fn empty_parents_at_active_height_no_soft_accept_missing_tx() {
        // Active height + SoftForkRule without tx → missing_tx (never soft-Accept).
        let rule = Bip360Rule::new(0);
        match rule
            .validate_tx(&RuleTxContext::label_only(100, "prod"))
            .unwrap()
        {
            RuleVote::Reject { reason } => assert!(reason.contains("missing_tx")),
            RuleVote::Accept => panic!("must not soft-accept without tx at active height"),
        }
        // Active height + empty parents map + no inputs: no P2MR spend to fail —
        // Accept is correct (not a soft-Accept bypass of spends).
        let empty_tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![],
        };
        let parents: HashMap<Txid, Transaction> = HashMap::new();
        assert!(
            rule.vote_mempool_transaction(&empty_tx, 100, &parents)
                .is_accept()
        );
        // Context without parent_txs uses empty map (same path as remote without wire parents).
        let ctx = RuleTxContext::with_tx(100, "prod", &empty_tx);
        assert!(rule.validate_tx(&ctx).unwrap().is_accept());
        assert!(ctx.parent_txs.is_none());
    }

    #[test]
    fn connect_block_missing_block_rejects() {
        let rule = Bip360Rule::new(0);
        match rule
            .connect_block(&RuleBlockContext::label_only(1, "prod"))
            .unwrap()
        {
            RuleVote::Reject { reason } => assert!(reason.contains("missing_block")),
            RuleVote::Accept => panic!("must not soft-accept without block"),
        }
    }

    #[test]
    fn connect_block_missing_chain_set_rejects() {
        use bitcoin::{Network, blockdata::constants::genesis_block};
        let rule = Bip360Rule::new(0);
        let block = genesis_block(Network::Regtest);
        match rule
            .connect_block(&RuleBlockContext::with_block(0, "prod", &block))
            .unwrap()
        {
            RuleVote::Reject { reason } => {
                assert!(reason.contains("missing_chain_p2mr_utxos"), "got {reason}");
            }
            RuleVote::Accept => panic!("must not soft-accept without chain P2MR set"),
        }
    }

    #[test]
    fn connect_block_empty_chain_set_accepts_genesis_style() {
        // Empty chain set + inactive height (activation = MAX) → Accept via pqc path.
        use bitcoin::{Network, blockdata::constants::genesis_block};
        let rule = Bip360Rule::new(u32::MAX);
        let block = genesis_block(Network::Regtest);
        let chain: HashMap<OutPoint, TxOut> = HashMap::new();
        let ctx = RuleBlockContext::with_block_and_chain_utxos(0, "prod", &block, &chain);
        assert!(rule.connect_block(&ctx).unwrap().is_accept());
    }

    #[test]
    fn validate_tx_uses_parent_txs_from_context() {
        // With parents present on context, SoftForkRule forwards them (inactive
        // height still Accepts with any parent map).
        let rule = Bip360Rule::new(u32::MAX);
        let parent = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![],
        };
        let txid = parent.compute_txid();
        let mut parents = HashMap::new();
        parents.insert(txid, parent);
        let child = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![],
        };
        let ctx = RuleTxContext::with_tx_and_parents(0, "prod", &child, &parents);
        assert!(rule.validate_tx(&ctx).unwrap().is_accept());
    }

    #[test]
    fn ballot_from_pqc_connect_result_err_is_explicit_reject() {
        let b = Bip360Rule::ballot_from_pqc_connect_result(Err(pqc::PqcValidationError::Block(
            "connect fail".into(),
        )));
        assert!(!b.consents());
        assert!(!b.source.is_error_path());
    }
}
