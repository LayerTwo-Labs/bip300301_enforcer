//! Consensus limits for BIP 360 P2MR validation.

use std::time::Duration;

/// BIP 340 / SLH-DSA-SHA2-128s x-only public key size in bytes.
pub const SLH_DSA_PUBLIC_KEY_SIZE: usize = 32;

/// ML-DSA-44 (FIPS 204) public key size in bytes (`bitcoinpqc::public_key_size`).
pub const ML_DSA_44_PUBLIC_KEY_SIZE: usize = 1312;

/// Schnorr (BIP 340) signature size in bytes (no sighash byte).
pub const SCHNORR_SIGNATURE_SIZE: usize = 64;

/// ML-DSA-44 (FIPS 204) signature size in bytes.
pub const ML_DSA_44_SIGNATURE_SIZE: usize = 2420;

/// SLH-DSA-SHA2-128s signature size in bytes.
pub const SLH_DSA_128S_SIGNATURE_SIZE: usize = 7856;

/// Maximum allowed witness stack elements for a P2MR script-path spend (Core default).
pub const MAX_P2MR_WITNESS_STACK: usize = 100;

/// Maximum leaf script size (bytes) for P2MR script-path leaves.
pub const MAX_P2MR_LEAF_SCRIPT_SIZE: usize = 10_000;

/// Maximum witness weight units for signature elements per input (1 WU per byte).
///
/// Must accommodate a kitchen-sink leaf (Schnorr + ML-DSA-44 + SLH-DSA in one witness).
pub const MAX_PQC_SIG_WU_PER_INPUT: usize = 12_288;

/// Default per-block wall-time budget for PQC signature verification.
pub const DEFAULT_PQC_VERIFY_BUDGET_MS: u64 = 500;

/// Tolerance for classifying variable-length PQC signatures by size.
pub const PQC_SIGNATURE_SIZE_TOLERANCE: usize = 4;

/// Estimate witness weight units for a signature element (prototype: 1 WU per byte).
#[must_use]
pub const fn estimate_sig_wu(sig_len: usize) -> usize {
    sig_len
}

/// Per-block accumulator for PQC verification wall time.
#[derive(Clone, Debug)]
pub struct PqcVerifyBudget {
    budget: Duration,
    elapsed: Duration,
}

impl PqcVerifyBudget {
    /// Create a budget with the default [`DEFAULT_PQC_VERIFY_BUDGET_MS`] limit.
    #[must_use]
    pub fn new_default() -> Self {
        Self::with_budget_ms(DEFAULT_PQC_VERIFY_BUDGET_MS)
    }

    /// Create a budget with the given millisecond wall-time limit.
    #[must_use]
    pub fn with_budget_ms(ms: u64) -> Self {
        Self {
            budget: Duration::from_millis(ms),
            elapsed: Duration::ZERO,
        }
    }

    /// Record elapsed PQC verification time and return whether the budget is exceeded.
    pub fn record(&mut self, duration: Duration) -> bool {
        self.elapsed += duration;
        self.elapsed > self.budget
    }

    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    #[must_use]
    pub fn budget(&self) -> Duration {
        self.budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slh_sig_fits_per_input_wu_budget() {
        assert!(estimate_sig_wu(SLH_DSA_128S_SIGNATURE_SIZE + 1) <= MAX_PQC_SIG_WU_PER_INPUT);
    }

    #[test]
    fn kitchen_sink_triple_sig_fits_per_input_wu_budget() {
        let total = estimate_sig_wu(SCHNORR_SIGNATURE_SIZE)
            + estimate_sig_wu(ML_DSA_44_SIGNATURE_SIZE)
            + estimate_sig_wu(SLH_DSA_128S_SIGNATURE_SIZE);
        assert_eq!(total, 10_340);
        assert!(total <= MAX_PQC_SIG_WU_PER_INPUT);
    }

    #[test]
    fn budget_exceeded_after_record() {
        let mut budget = PqcVerifyBudget::with_budget_ms(1);
        assert!(!budget.record(Duration::from_millis(1)));
        assert!(budget.record(Duration::from_millis(1)));
    }
}
