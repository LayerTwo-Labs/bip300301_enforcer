# Multi-leaf P2MR — algorithm-per-leaf model

## Purpose

BIP 360 wallets may place **different signature schemes in different merkle leaves** so spenders reveal only the leaf they use. The enforcer validates the revealed leaf script and its merkle path; unrevealed leaves are not executed.

This matches the algorithm-exclusion model in [`BIP360_OVERLOAD_ADDENDUM.md`](./BIP360_OVERLOAD_ADDENDUM.md).

## Three-leaf trial tree

`ThreeAlgorithmP2mrTree` builds a nested three-leaf P2MR tree (same shape as overload vector `overload_three_leaf_complex`):

| Leaf index | Algorithm | Leaf script |
|------------|-----------|-------------|
| 0 | Schnorr (secp256k1) | `PUSH32 <pk> OP_CHECKSIG` |
| 1 | ML-DSA-44 | `PUSHDATA2 <pk> OP_CHECKSIG` |
| 2 | SLH-DSA-SHA2-128s | `PUSH32 <pk> OP_CHECKSIG` |

Tree layout:

```text
           root
          /    \
    Schnorr    inner
              /    \
        ML-DSA    SLH
```

Construction uses `bitcoin_p2mr_pqc::p2mr::P2mrBuilder` (`add_leaf_with_ver`, `finalize`) and derives per-leaf control blocks from `TapTree::script_leaves()`.

Fixed test entropy (shared with single-leaf trials):

- Schnorr: `0x11` × 32 bytes
- ML-DSA: `0x22` × 128 bytes
- SLH: `0x88` × 128 bytes

Deterministic merkle root with that entropy: `741e59741422eab4fe6dcc3bb3c73ad4d5279a613b999381b3d47d4b05e02f9c` (TDD regression lock in unit tests).

## Code locations

| Component | Path |
|-----------|------|
| Tree builder + unit tests | `lib/validator/quantum/multi_leaf.rs` |
| Shared control-block padding | `lib/validator/quantum/merkle.rs` (`control_block_bytes_for_enforcer`) |
| Block spend helpers | `build_block_three_leaf_p2mr_spend`, `build_three_leaf_p2mr_spend_txs` |
| Integration trials | `integration_tests/test_bip360_multi_leaf.rs` |

## Integration trial matrix (Phase C)

| Trial | Expect | Description |
|-------|--------|-------------|
| `bip360_valid_multi_leaf_schnorr_spend` | Accepted | Reveal Schnorr leaf, same-block |
| `bip360_valid_multi_leaf_mldsa_spend` | Accepted | Reveal ML-DSA leaf, same-block |
| `bip360_valid_multi_leaf_slh_spend` | Accepted | Reveal SLH leaf, same-block |
| `bip360_valid_multi_leaf_cross_block_mldsa_spend` | Accepted | Fund multi-leaf in block N, spend ML-DSA leaf in N+1 |
| `bip360_valid_multi_leaf_cross_block_schnorr_spend` | Accepted | Fund multi-leaf in block N, spend Schnorr leaf in N+1 |
| `bip360_valid_multi_leaf_cross_block_slh_spend` | Accepted | Fund multi-leaf in block N, spend SLH leaf in N+1 |
| `bip360_invalid_multi_leaf_wrong_control_block` | Rejected | Correct script, control block from another leaf → `merkle root mismatch for leaf script` |
| `bip360_invalid_multi_leaf_cross_block_wrong_control_block` | Rejected | Cross-block variant; funding block accepted first |
| `bip360_invalid_multi_leaf_tampered_signature_mldsa` | Rejected | Tampered ML-DSA signature on revealed ML-DSA leaf (same-block) |

**9 trials** in Phase C multi-leaf module (**31 trials total** with Phases A+B).

## Unit tests

```bash
cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 quantum::multi_leaf
```

Covers deterministic merkle root, placement-depth topology, sign+validate per leaf index, invalid leaf index rejection, `build_block_three_leaf_p2mr_spend` prevout wiring, and wrong-control-block rejection via `validate_p2mr_input_spend`.

## Deferred follow-ups

- **Full tamper matrix** — Single-leaf trials cover Schnorr/ML-DSA/SLH tampered sig, bad pubkey, and merkle-path variants across same-block and cross-block modes. Multi-leaf currently includes one tampered-signature trial (ML-DSA same-block) plus wrong-control-block invalid cases. Extending to full parity is optional hardening.
- **Live regtest execution** — Trials are compile-verified and registered. Running against stock bitcoind requires `integrationtests.env` (human setup step); see `integration_tests/README.md`.

## Status

Compile-verified locally. Live regtest execution requires `integrationtests.env` and stock bitcoind (same as other BIP 360 trials).