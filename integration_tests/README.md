# Integration tests

## Prerequisites

BIP 360 integration trials **require** stock `bitcoind` paths in `integrationtests.env` (use `just setup-core`). Drivechain trials use full `just setup` (patched + stock bitcoind + electrs).
Without it you will see errors such as:

```text
Error resolving environment variable (BITCOIND_UNPATCHED): environment variable not found
```

**Before running trials**, do one of the following:

```sh
just setup-core
# or, if you already have binaries and an env file:
export BIP300301_ENFORCER_INTEGRATION_TEST_ENV=integrationtests.env
```

The harness does not skip this requirement — trials are live regtest end-to-end tests.

## Setup

The integration tests require at least one environment variable to be set.
Environment variables can also be set via an env file, where the path to the env
file is set via environment variable. An example env file is provided
[here](/integration_tests/example.env). The path to the env file can be provided
by setting the `BIP300301_ENFORCER_INTEGRATION_TEST_ENV` variable, eg.

```sh
BIP300301_ENFORCER_INTEGRATION_TEST_ENV='integration_tests/example.env'
```

Variables set in an env file have higher precedence than environment variables.
If multiple declarations for the same environment variable exist in an env file,
the last one has highest precedence.

## BIP 360 trials (`--features bip360`)

Shared block builders live in `integration_tests/bip360_block.rs`. All trials use
stock Core (`BitcoindKind::Unpatched`), `Mode::NoMempool`, `--activation-height=0`,
and `--pqc-verify-budget-ms=5000` via `bip360_setup_opts()`.

Build and run:

```sh
cargo build --example integration_tests --features bip360
export BIP300301_ENFORCER_INTEGRATION_TEST_ENV=integrationtests.env
cargo run --example integration_tests --features bip360 -- --exact <trial_name>
```

Or use just recipes:

```sh
just demo-a   # bip360_valid_schnorr_spend
just demo-b   # bip360_invalid_block
just it-all   # all 33 block-only BIP 360 trials
just bip360-p2p-e2e   # dual-node P2P mempool E2E (trial #34; needs `just setup` for electrs)
```

| Trial | Expect | Notes |
|-------|--------|-------|
| `bip360_valid_schnorr_spend` | Accepted | Same-block Schnorr P2MR spend |
| `bip360_valid_mldsa_spend` | Accepted | Same-block ML-DSA-44 spend |
| `bip360_valid_slh_spend` | Accepted | Same-block SLH-DSA spend |
| `bip360_valid_cross_block_schnorr_spend` | Accepted | Cross-block Schnorr (funding in prior block) |
| `bip360_valid_cross_block_mldsa_spend` | Accepted | Cross-block ML-DSA-44 |
| `bip360_valid_cross_block_slh_spend` | Accepted | Cross-block SLH-DSA |
| `bip360_valid_hybrid_ec_slh_spend` | Accepted | Same-block hybrid EC+SLH (`OP_BOOLAND OP_VERIFY`) |
| `bip360_valid_hybrid_ec_slh_cross_block_spend` | Accepted | Cross-block hybrid EC+SLH |
| `bip360_invalid_block` | Rejected | Empty witness stack |
| `bip360_invalid_signature` | Rejected | Tampered Schnorr signature (same-block) |
| `bip360_invalid_pubkey_size` | Rejected | ML-DSA sig + 32-byte pubkey (same-block) |
| `bip360_invalid_merkle_path` | Rejected | Corrupt control block (Schnorr, same-block) |
| `bip360_invalid_cross_block_signature_schnorr` | Rejected | Tampered Schnorr sig (cross-block) |
| `bip360_invalid_cross_block_signature_mldsa` | Rejected | Tampered ML-DSA-44 sig (cross-block) |
| `bip360_invalid_cross_block_signature_slh` | Rejected | Tampered SLH-DSA sig (cross-block) |
| `bip360_invalid_cross_block_merkle_path_schnorr` | Rejected | Bad control block (Schnorr, cross-block) |
| `bip360_invalid_cross_block_merkle_path_mldsa` | Rejected | Bad control block (ML-DSA, cross-block) |
| `bip360_invalid_cross_block_merkle_path_slh` | Rejected | Bad control block (SLH, cross-block) |
| `bip360_invalid_cross_block_pubkey_size_mldsa` | Rejected | ML-DSA sig + 32-byte pubkey (cross-block) |
| `bip360_invalid_hybrid_ec_slh_tamper_ec_sig` | Rejected | Tampered EC (Schnorr) sig only (hybrid, same-block) |
| `bip360_invalid_hybrid_ec_slh_tamper_slh_sig` | Rejected | Tampered SLH sig only (hybrid, same-block) |
| `bip360_invalid_hybrid_ec_slh_swap_sigs` | Rejected | EC/SLH sig positions swapped (hybrid, same-block) |
| `bip360_valid_kitchen_sink_spend` | Accepted | Same-block kitchen-sink leaf (Schnorr + ML-DSA + SLH, `OP_BOOLAND OP_BOOLAND OP_VERIFY`) |
| `bip360_invalid_kitchen_sink_tamper_ec_sig` | Rejected | Tampered EC (Schnorr) sig only (kitchen-sink, same-block) |
| `bip360_valid_multi_leaf_schnorr_spend` | Accepted | Three-leaf tree; reveal Schnorr leaf (same-block) |
| `bip360_valid_multi_leaf_mldsa_spend` | Accepted | Three-leaf tree; reveal ML-DSA leaf (same-block) |
| `bip360_valid_multi_leaf_slh_spend` | Accepted | Three-leaf tree; reveal SLH leaf (same-block) |
| `bip360_valid_multi_leaf_cross_block_mldsa_spend` | Accepted | Fund multi-leaf in block N, spend ML-DSA leaf in N+1 |
| `bip360_invalid_multi_leaf_wrong_control_block` | Rejected | Correct script, control block from another leaf (same-block) |
| `bip360_invalid_multi_leaf_cross_block_wrong_control_block` | Rejected | Wrong control block variant (cross-block; funding accepted) |
| `bip360_valid_multi_leaf_cross_block_schnorr_spend` | Accepted | Fund multi-leaf in block N, spend Schnorr leaf in N+1 |
| `bip360_valid_multi_leaf_cross_block_slh_spend` | Accepted | Fund multi-leaf in block N, spend SLH leaf in N+1 |
| `bip360_invalid_multi_leaf_tampered_signature_mldsa` | Rejected | Tampered ML-DSA signature on revealed ML-DSA leaf (same-block) |

**33 block-matrix trials total** (Phase A: 17 + Phase B hybrid: 5 + Phase C multi-leaf: 9 + kitchen-sink: 2).

## Dual-node P2P mempool E2E (trial #34)

Harness: `integration_tests/bip360_dual_node.rs`, `bip360_tx_report.rs`, `test_bip360_p2p_mempool_e2e.rs`.

| Trial | Expect | Notes |
|-------|--------|-------|
| `bip360_p2p_mempool_e2e` | Accepted | Two peered regtest nodes; 5 rounds (wallet→P2MR, Schnorr, hybrid, ML-DSA, kitchen-sink) on one sat pile; P2P inject + dual mempool + enforcer GBT mining |

**Prerequisites (stricter than block-matrix trials):**

- Full bootstrap: `just setup` (patched + stock bitcoind + **electrs**) — `setup-core` alone is insufficient.
- Enforcer built with `drivechain,bip360` (recipe `just bip360-p2p-e2e` handles this).
- Wallet + electrs enabled in harness (`enable_enforcer_wallet: true`).

```sh
just setup
just bip360-p2p-e2e
# or full stack:
just bip360-verify-full yes
```

**34 trials total** (33 block-matrix + 1 P2P E2E).

> **`BIP360_SKIP_REBUILD`:** `just it-all` / `just bip360-block-matrix` run
> `build-bip360` once, then set this env var for the trial loop. Do not export it
> manually — a drivechain-only enforcer left by `drivechain-smoke` will break
> bip360 CLI flags.

> **Compile-verified disclaimer:** All trials are **registered and compile-verified**
> (`cargo check --example integration_tests --features bip360`). **Live regtest execution**
> requires stock `bitcoind` and `integrationtests.env` (or `example.env`) —
> see `just setup-core`. Trials are not wired into CI.

See `docs/REGTEST_DEMO.md`, `docs/MULTI_LEAF_P2MR.md`, and [`AGENTS.md`](../AGENTS.md) for prerequisites and git policy.