# CUSF BIP 360 Enforcer

> **Project status:** [`../../STATUS.md`](../../STATUS.md) — canonical project
> summary (test counts, integration trials, remaining human steps). All work is
> local uncommitted until the upstream PR is opened.

This document describes the BIP 360 (P2MR + post-quantum cryptography)
enforcement layer in the `bip300301_enforcer` fork.

## Cargo features

| Feature              | Default  | Description                                                         |
| -------------------- | -------- | ------------------------------------------------------------------- |
| `drivechain`         | yes      | BIP 300/301 drivechain rules (existing behavior)                    |
| `bip360`             | no       | BIP 360 P2MR output + PQC signature verification (`validator/pqc/`) |
| `rustls` / `openssl` | `rustls` | TLS backend (unchanged)                                             |
| `shrincs`            | no       | Reserved placeholder — no implementation                            |

Build examples:

```bash
cargo build                                          # drivechain (default)
cargo build --no-default-features --features bip360  # BIP 360 only
cargo build --features "drivechain,bip360"           # both rule sets
```

Default `cargo build`, `just test-drivechain`, and `just drivechain-smoke` pass
**without** `bip360`. Optional deps `bitcoin-p2mr-pqc` and `bitcoinpqc` are
enabled only by the `bip360` feature. All PQC code is gated with
`#[cfg(feature = "bip360")]` (module, CLI flags, DB tables, integration trials).
Drivechain logic is gated with `#[cfg(feature = "drivechain")]`.

Upstream regression smoke (no bip360; CI wiring is **HITL** — Phase D delivered
local `bip360-verify-full`):

```bash
just drivechain-smoke   # cargo build -p bip300301_enforcer && just test-drivechain
```

When both features are enabled, **both** rule sets must pass for block/tx
acceptance.

## Activation height

BIP 360 rules apply at and after `--activation-height` (default `0` on regtest).

```bash
cargo run --features bip360 -- --activation-height 100 ...
cargo run --features bip360 -- --pqc-verify-budget-ms 500 ...
```

### Per-block PQC verify budget

During `connect_block`, ML-DSA and SLH-DSA verification wall time is accumulated
across the block. When the budget is exceeded, further signature checks in that
block are rejected (`BlockVerifyBudgetExhausted` for any scheme;
`PqcVerifyBudgetExceeded` when a PQC verify pushes elapsed time over the limit).

| Setting                     | Default | CLI flag                 |
| --------------------------- | ------- | ------------------------ |
| Per-block PQC verify budget | 500 ms  | `--pqc-verify-budget-ms` |

**Mempool vs block asymmetry:** `accept_tx` (mempool) does **not** apply the
per-block budget (`pqc_budget` is unset). A transaction can pass mempool
validation and still cause block rejection when batched with other PQC spends
that exhaust the block budget.

## Overloaded tapscript signature opcodes (no OP_SUBSTR)

P2MR leaves reuse existing BIP 342 tapscript signature-check opcodes — no new
opcode numbers and no `OP_SUBSTR` (0x7f) as a PQ algorithm tag:

| Opcode                   | Byte | Role                                                                      |
| ------------------------ | ---- | ------------------------------------------------------------------------- |
| `OP_CHECKSIG`            | 0xac | Primary overloaded verifier                                               |
| `OP_CHECKSIGVERIFY`      | 0xad | Same as `OP_CHECKSIG`, fails on invalid sig                               |
| `OP_CHECKSIGADD`         | 0xba | Overloaded verifier (one sig site per opcode; see BIP342 deviation below) |
| `OP_CHECKMULTISIG`       | 0xae | N-site overload (`OP_0 PUSH pk… OP_N`); **not** Bitcoin M-of-N            |
| `OP_CHECKMULTISIGVERIFY` | 0xaf | Same as `OP_CHECKMULTISIG`, fails on invalid sig                          |

### BIP342 deviations (CUSF overload model)

**`OP_CHECKSIGADD`:** BIP342 pops `(pubkey, accumulator, sig)` from the stack
and increments the accumulator on success (MuSig2-style scripts). The CUSF
overload model treats each `OP_CHECKSIGADD` like `OP_CHECKSIG`: one preceding
pubkey push and one witness signature per site. Key-aggregation scripts that
rely on stack semantics are not supported.

**`OP_CHECKMULTISIG` / `OP_CHECKMULTISIGVERIFY`:** Bitcoin classic multisig is
**M-of-N** (witness supplies M ≤ N signatures). The CUSF overload model requires
**exactly N witness signatures** for the N pubkey pushes before `OP_N` — each
(pubkey, sig) pair is verified independently via size-gated duck typing.
Example: `OP_0 PUSH pk₁ PUSH pk₂ OP_2 OP_CHECKMULTISIG` requires **two** witness
sigs (not one for 1-of-2). Only the last N pubkey pushes before `OP_N` are used.

At each signature-check site the enforcer
(`schemes::verify_overloaded_checksig`):

1. Extracts the pubkey from the immediately preceding push
   (`PUSH <pk> OP_CHECKSIG`).
2. **Signature size** (witness element, in script order) classifies the
   verifier: 64 → Schnorr, ~2420 → ML-DSA-44, ~7856 → SLH-DSA-SHA2-128s.
3. **Pubkey size** is checked for consistency with the classified scheme (32 B
   for Schnorr/SLH; 1312 B for ML-DSA-44). Mismatches are rejected.
4. Parses optional trailing sighash byte (defaults to `SIGHASH_DEFAULT` for bare
   64-byte Schnorr).
5. Recomputes tapscript sighash with the parsed type and verifies.

| Sig size (bytes) | Algorithm                   | Pubkey size |
| ---------------- | --------------------------- | ----------- |
| 64               | BIP 340 Schnorr (secp256k1) | 32          |
| ~2420            | ML-DSA-44 (FIPS 204)        | 1312        |
| ~7856            | SLH-DSA-SHA2-128s           | 32          |

**Hybrid EC+PQ in one leaf** uses multiple `OP_CHECKSIG` call sites (not one
opcode verifying both keys):

```text
PUSH32 <ec_pk> OP_CHECKSIG
PUSH32 <slh_pk> OP_CHECKSIG
OP_BOOLAND OP_VERIFY
```

Witness (bottom → top): `[ec_sig, slh_sig, leaf_script, control_block]` —
signatures are consumed in script execution order.

**Kitchen-sink triple-algo leaf** combines Schnorr, ML-DSA-44, and SLH-DSA in
one leaf (all three schemes on one sat pile end-to-end):

```text
PUSH32 <ec_pk> OP_CHECKSIG
PUSHDATA2(1312) <mldsa_pk> OP_CHECKSIG
PUSH32 <slh_pk> OP_CHECKSIG
OP_BOOLAND OP_BOOLAND OP_VERIFY
```

Witness (bottom → top):
`[ec_sig (64B), mldsa_sig (~2420B), slh_sig (~7856B), leaf_script, control_block]`.
Total signature WU = **10_340** (64 + 2420 + 7856); per-input cap raised to
**12_288 WU** (`MAX_PQC_SIG_WU_PER_INPUT` in `limits.rs`) so kitchen-sink spends
validate.

**Exclusion** (different algorithms in different leaves) is a wallet/miner
concern; the enforcer validates whichever leaf is revealed.

Leaf scripts containing `OP_SUBSTR` as an opcode are rejected.

## Stock Bitcoin Core deployment model

The enforcer runs alongside **one unmodified `bitcoind`** (no consensus
patches):

1. **ZMQ `sequence`** — the mempool enforcer (`cusf-enforcer-mempool`) watches
   the mainchain tip and receives new block/tx notifications.
2. **`getblock` / block connect** — the validator's
   `CusfEnforcer::connect_block` applies CUSF rules (drivechain and/or BIP 360,
   per Cargo features).
3. **`submitblock`** — integration tests and miners submit candidate blocks to
   Core; Core accepts them into its chain view initially.
4. **`invalidateblock`** — when the enforcer rejects a connected block, the
   mempool adapter calls `invalidateblock` so the strict enforcer view diverges
   from stock Core's permissive validation.

Stock Core validates standard rules only; the enforcer adds CUSF constraints on
top.

## Enforcement points

- **Block connect** (`connect_block`): validates all non-coinbase txs in the
  block, merging a persistent P2MR UTXO set (confirmed prior blocks) with
  intra-block outputs. Spends of confirmed non-P2MR UTXOs from earlier blocks
  are not re-validated (prevout not in map → skip).
- **Mempool** (`accept_tx`): validates with explicit parent transactions from
  the mempool adapter.
- Rejection causes `invalidateblock` (blocks) or tx reject (mempool), consistent
  with existing CUSF enforcer behavior.

## Dependencies

- `bitcoin-p2mr-pqc` — P2MR types and merkle/control-block helpers (git pin:
  [cryptoquick/rust-bitcoin](https://github.com/cryptoquick/rust-bitcoin) `p2mr`
  @ `9093253a` / `0.32.6-p2mr-pqc.1`)
- `bitcoinpqc` — Schnorr + ML-DSA-44 + SLH-DSA-SHA2-128s verification (git pin:
  [cryptoquick/libbitcoinpqc-bindings](https://github.com/cryptoquick/libbitcoinpqc-bindings)
  PR [#1](https://github.com/cryptoquick/libbitcoinpqc-bindings/pull/1)
  `wasm-tests` @ `5ef7067`; native `libbitcoinpqc` submodule @ `b309f44` from
  [cryptoquick/libbitcoinpqc](https://github.com/cryptoquick/libbitcoinpqc) PR
  [#29](https://github.com/cryptoquick/libbitcoinpqc/pull/29))

No Kellnr / `crates.denver.space` required.

## Module layout

```
lib/validator/pqc/
  activation.rs   # activation height gating
  limits.rs       # consensus size limits
  p2mr_output.rs  # P2MR scriptPubKey validation
  merkle.rs       # control block / merkle path checks
  leaf_script.rs  # tapscript walker + sig-site extraction
  schemes.rs      # overloaded checksig verification
  spend.rs            # witness stack + spend validation
  p2mr_utxo.rs        # P2MR UTXO diff for block connect / disconnect
  overload_vectors.rs # JSON construction vector tests (overload model)
  signer_dev.rs       # shared P2MR signing helpers for tests + integration
  multi_leaf.rs       # three-leaf P2MR tree (algorithm-per-leaf model)
  mod.rs              # public entry points

lib/validator/dbs/
  p2mr_utxos.rs   # redb table: OutPoint → TxOut for confirmed P2MR outputs
```

## Implementation status

| Component                                                                 | Status                                                                                                        |
| ------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| Feature gating (`drivechain` default, `bip360` optional)                  | Done                                                                                                          |
| P2MR output + merkle + control block validation                           | Done                                                                                                          |
| Leaf script walker + `OP_SUBSTR` rejection                                | Done                                                                                                          |
| `verify_overloaded_checksig` (Schnorr, ML-DSA-44, SLH-DSA-SHA2-128s)      | Done                                                                                                          |
| Hybrid EC+PQ (multi-site `OP_CHECKSIG` + `OP_BOOLAND OP_VERIFY`)          | Done                                                                                                          |
| `OP_CHECKSIGADD` / `OP_CHECKMULTISIG*`                                    | Done                                                                                                          |
| DoS limits (witness stack, sig WU, per-block PQC budget)                  | Done                                                                                                          |
| Sighash matrix tests (non-`ALL` types, all schemes)                       | Done                                                                                                          |
| `connect_block` intra-block UTXO map                                      | Done                                                                                                          |
| Cross-block P2MR prevout lookup                                           | Done — `dbs/p2mr_utxos.rs` + incremental block validation                                                     |
| Mempool `accept_tx` with explicit parents                                 | Done                                                                                                          |
| Unit tests (`cargo test … --features bip360 pqc::`)                       | 123 passing (+ 1 ignored golden dump)                                                                         |
| CI `check-bip360`                                                         | Done                                                                                                          |
| Integration harness (`integration_tests/bip360_block.rs`)                 | Done — shared `submitblock` helpers                                                                           |
| Integration trials (34 green block-only in `it-all` + dual-node separate) | Registered; compile-verified; classic matrix live; TB-mine **PASS** 2026-07-16 — see live column below        |
| Integration trial `bip360_invalid_block`                                  | Live **PASS** (empty witness → `invalidateblock`)                                                             |
| Integration trial `bip360_valid_schnorr_spend`                            | Live **PASS**                                                                                                 |
| Integration trial `bip360_valid_mldsa_spend`                              | Live **PASS**                                                                                                 |
| Integration trial `bip360_valid_slh_spend`                                | Live **PASS**                                                                                                 |
| Integration trial `bip360_valid_cross_block_schnorr_spend`                | Live **PASS**                                                                                                 |
| Integration trial `bip360_valid_cross_block_mldsa_spend`                  | Live **PASS**                                                                                                 |
| Integration trial `bip360_valid_cross_block_slh_spend`                    | Live **PASS**                                                                                                 |
| Integration trial `bip360_invalid_signature`                              | Live **PASS**                                                                                                 |
| Integration trial `bip360_invalid_pubkey_size`                            | Live **PASS** (harness fix: `build_mldsa_sig_wrong_pubkey_spend_txs` — wrong merkle root before prevout bind) |
| Integration trial `bip360_invalid_merkle_path`                            | Live **PASS** (harness fix: `tamper_witness_control_block` — bogus merkle sibling, keep `0xc1`)               |
| Integration trial `bip360_valid_hybrid_ec_slh_spend`                      | Live **PASS** (hybrid EC+SLH, same-block)                                                                     |
| Integration trial `bip360_valid_hybrid_ec_slh_cross_block_spend`          | Live **PASS** (hybrid EC+SLH, cross-block)                                                                    |
| Integration trial `bip360_invalid_hybrid_ec_slh_tamper_ec_sig`            | Live **PASS** (tampered EC sig → reject)                                                                      |
| Integration trial `bip360_invalid_hybrid_ec_slh_tamper_slh_sig`           | Live **PASS** (tampered SLH sig → reject)                                                                     |
| Integration trial `bip360_invalid_hybrid_ec_slh_swap_sigs`                | Live **PASS** (swapped sig positions → reject)                                                                |
| Integration trial `bip360_valid_kitchen_sink_spend`                       | Live **PASS** (triple-algo kitchen-sink, same-block)                                                          |
| Integration trial `bip360_invalid_kitchen_sink_tamper_ec_sig`             | Live **PASS** (tampered EC sig on kitchen-sink → reject)                                                      |
| Integration trial `bip360_invalid_cross_block_pubkey_size_mldsa`          | Live **PASS** (same harness as `bip360_invalid_pubkey_size`)                                                  |
| Integration trial `bip360_p2p_mempool_e2e`                                | Live **HITL** — needs `just setup` + electrs — see P2P E2E section below                                      |
| Cross-block invalid-spend matrix (sig / merkle / pubkey)                  | Done — Schnorr, ML-DSA, SLH; hybrid invalid cross-block deferred (same-block coverage complete)               |
| Overload-model vectors (non-`OP_SUBSTR` scripts)                          | Done — `test_vectors/p2mr_overload_construction.json`, `pqc/overload_vectors.rs`                              |

### Phase D — full local verification

Canonical pre-submit (from `bip300301_enforcer/`):

```bash
just bip360-verify-full yes
```

Equivalent steps (in order):

1. `just drivechain-smoke` — default build + drivechain unit tests (**94**
   passed)
2. `just verify` — `check-bip360`, `pqc::` (**123** passed, 1 ignored),
   `p2mr-signer-smoke`, `test-drivechain`, `clippy-bip360`, `fmt-check`,
   `check-integration-build`
3. `just build-bip360` — rebuild enforcer with `bip360` CLI flags (required
   after step 1)
4. `just bip360-block-matrix` — 34 green block-only live trials incl. TB-mine
   (`just setup-core` for stock bitcoind)
5. `just bip360-p2p-e2e` — dual-node P2P E2E (`just setup` for electrs; not in
   `it-all`)

Pass `yes` to auto-run `setup-core` / `setup` when `integrationtests.env` or
`ELECTRS` is missing. P2P E2E still requires a working electrs binary.

**Phase D / TB-mine results:** steps 1–2 **PASS** (2026-07-15); classic block
matrix **PASS** 2026-07-15 (harness fixes in
`build_mldsa_sig_wrong_pubkey_spend_txs` and `tamper_witness_control_block`);
TB-mine **PASS** 2026-07-16 via `just bip360-tier-b-cusf`; step 5 dual-node is a
separate recipe (needs electrs via `just setup`).

Granular checks (subset of `just verify`):

```bash
just drivechain-smoke                                                         # default build + drivechain tests
cargo test -p bip300301_enforcer_lib                                          # 94 (drivechain default)
cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 pqc::  # 123
cargo clippy -p bip300301_enforcer_lib --no-default-features --features bip360 -- -D warnings
just verify                                                                   # full BIP 360 pre-submit (overlaps test-drivechain; run drivechain-smoke separately for default-binary build)
```

## Remaining work

See [`STATUS.md`](../../STATUS.md) for the canonical remaining-work list and
[`DESIGN.md`](../../DESIGN.md) for full project context. Ordered by priority.

### P0 — correctness and coverage

1. ~~**Cross-block prevout lookup**~~ — Done (`dbs/p2mr_utxos.rs`,
   `pqc/p2mr_utxo.rs`).
2. ~~**Integration harness**~~ — Done. Shared helpers in
   `integration_tests/bip360_block.rs`; trials registered in
   `integration_tests/integration_test.rs` (`Mode::NoMempool`,
   `BitcoindKind::Unpatched`, `--activation-height=0`):
   `bip360_valid_schnorr_spend`, `bip360_valid_mldsa_spend`,
   `bip360_valid_slh_spend`, cross-block valid trials
   (`bip360_valid_cross_block_*`), `bip360_invalid_block`, same-block invalid
   trials (`bip360_invalid_signature`, `bip360_invalid_pubkey_size`,
   `bip360_invalid_merkle_path`), and cross-block invalid trials
   (`bip360_invalid_cross_block_*`), and Phase B hybrid trials
   (`bip360_valid_hybrid_ec_slh_*`, `bip360_invalid_hybrid_ec_slh_*`), and Phase
   C multi-leaf trials (`bip360_valid_multi_leaf_*`,
   `bip360_invalid_multi_leaf_*`), kitchen-sink trials
   (`bip360_valid_kitchen_sink_spend`,
   `bip360_invalid_kitchen_sink_tamper_ec_sig`), P2P E2E
   (`bip360_p2p_mempool_e2e`), and TB-mine (`bip360_tier_b_cusf_miner`). Green
   block-only: **34** in `it-all`; dual-node / Tier A / TB-sendraw separate.
   Output-structure checks remain in `lib/validator/pqc/` unit tests.
3. ~~**Overload-model vectors**~~ — Done. See
   [`OVERLOAD_VECTORS.md`](./OVERLOAD_VECTORS.md)
   (`test_vectors/p2mr_overload_construction.json`, `pqc::overload_vectors`: 8
   passed; optional golden dump via `OVERLOAD_GOLDEN_DUMP=1`, not ignored).

### P1 — completeness

4. ~~**Deferred opcodes**~~ — Done (`OP_CHECKSIGADD`, `OP_CHECKMULTISIG`,
   `OP_CHECKMULTISIGVERIFY` in `leaf_script.rs` with size-gated verification).
5. ~~**DoS limits**~~ — Done (witness stack depth 100, 12_288 WU/sig budget per
   input — raised for kitchen-sink triple-algo witnesses at 10_340 WU;
   configurable 500 ms per-block PQC verify budget in `connect_block`).
6. ~~**Sighash coverage**~~ — Done (non-`ALL` sighash matrix tests for Schnorr,
   ML-DSA, SLH-DSA in `schemes.rs`).

### Dual-node P2P mempool E2E

Harness (`integration_tests/bip360_dual_node.rs`) runs two peered stock regtest
nodes, each with a mempool-enabled enforcer (`--enable-mempool`, wallet +
electrs). Five rounds on one sat pile: wallet→P2MR, Schnorr, hybrid EC+SLH,
ML-DSA, kitchen-sink. Non-standard txs are injected via
`broadcast_nonstandard_tx`; acceptance is checked on both Core mempools and
enforcer GBT templates (GBT inclusion proxies the enforcer `accept_tx` path).

```bash
just setup                    # electrs required — not setup-core alone
just bip360-p2p-e2e           # rebuilds enforcer with drivechain,bip360
just bip360-verify-full yes   # canonical pre-submit: full stack with auto-setup
```

### P2 — human / upstream (prepared)

7. **Upstream PR** — **prepared.** Complete package in
   [`docs/UPSTREAM_PR.md`](./UPSTREAM_PR.md): branch name, PR body, file-level
   change list, reviewer checklist, and exact `git checkout` / commit / push
   commands. **Human still required:** commit, push, open PR to
   [LayerTwo-Labs/bip300301_enforcer](https://github.com/LayerTwo-Labs/bip300301_enforcer).
8. **Regtest demo** — **prepared.** Walkthrough in
   [`docs/REGTEST_DEMO.md`](./REGTEST_DEMO.md); automation helper
   [`Justfile`](../Justfile) (`just demo-b` runs `test_bip360_invalid_block`).
   **Human still required:** live regtest demo walkthrough (wallet funding, RPC
   narration).

### P3 — deferred (prepared)

| Item                      | Status                      | Doc                                                                                                               |
| ------------------------- | --------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| External signer CLI       | **Done** (roundtrip tested) | [`P2MR_SIGNER.md`](./P2MR_SIGNER.md) — `lib/examples/p2mr_signer.rs`; `pqc::spend` `p2mr_signer_roundtrip_*`      |
| BIP overload addendum     | **Done** (draft)            | [`BIP360_OVERLOAD_ADDENDUM.md`](./BIP360_OVERLOAD_ADDENDUM.md)                                                    |
| Kellnr `bitcoinpqc` 0.4.0 | **Prepared**                | [`KELLNR_PUBLISH.md`](./KELLNR_PUBLISH.md). **Human:** publish + pin update                                       |
| Signet workshop           | **Prepared**                | [`SIGNET_WORKSHOP.md`](./SIGNET_WORKSHOP.md). **Human:** signet infra / workshop                                  |
| Mempool relay policy      | **Done**                    | [`MEMPOOL_RELAY_POLICY.md`](./MEMPOOL_RELAY_POLICY.md)                                                            |
| Core 31 ZMQ / BIP 360     | **Done** (findings)         | [`ZMQ_CUSF_BIP360_FINDINGS.md`](./ZMQ_CUSF_BIP360_FINDINGS.md) — ZMQ surface; stock Core cannot mempool v2 spends |
| SHRINCs                   | **Deferred**                | [`SHRINCS_DEFERRED.md`](./SHRINCS_DEFERRED.md)                                                                    |
