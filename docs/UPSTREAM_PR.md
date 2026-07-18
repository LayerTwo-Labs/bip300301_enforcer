# Upstream PR package — BIP 360 CUSF enforcer

Prepared for human submission to the upstream enforcer repository
([github.com/LayerTwo-Labs/bip300301_enforcer](https://github.com/LayerTwo-Labs/bip300301_enforcer)).

**Status:** All code and docs are ready locally; commit, push, and PR opening
are **human-only** steps.

---

## Suggested branch and title

| Field    | Value                                                                 |
| -------- | --------------------------------------------------------------------- |
| Branch   | `feature/bip360-cusf`                                                 |
| PR title | `feat: BIP 360 P2MR + PQC CUSF enforcement (optional bip360 feature)` |

---

## PR description (copy into GitHub)

### Summary

Adds an optional Cargo feature `bip360` that enforces BIP 360 P2MR output
structure and post-quantum signature verification on stock Bitcoin Core via the
existing CUSF enforcer skeleton. Drivechain rules remain behind the default
`drivechain` feature; builds can target BIP 360 only, drivechain only, or both.

### Motivation

CUSF enforcers let a single unmodified `bitcoind` run permissive consensus while
a companion process applies stricter rules and calls `invalidateblock` on
violations. This PR prototypes BIP 360 (P2MR script-path spends + overloaded
`OP_CHECKSIG` for Schnorr, ML-DSA-44, and SLH-DSA-SHA2-128s) on regtest/signet
without patching Core.

### Architecture

- **Hook points:** `BlockHandler::connect_block` and `CusfEnforcer::accept_tx`
  call into `lib/validator/pqc/` when `height >= --activation-height`.
- **Rejection path:** fatal validation → `ConnectBlockAction::Reject` → mempool
  adapter `invalidateblock` (unchanged CUSF flow).
- **Overload model:** existing tapscript sig opcodes are size-gated; `OP_SUBSTR`
  as a PQ tag is rejected (differs from ref-impl vectors — see open questions in
  `docs/CUSF-BIP360.md`).
- **Dependencies:** `bitcoin-p2mr-pqc` (git: `cryptoquick/rust-bitcoin` p2mr
  rev), `bitcoinpqc` (git pin:
  [cryptoquick/libbitcoinpqc-bindings](https://github.com/cryptoquick/libbitcoinpqc-bindings)
  PR [#1](https://github.com/cryptoquick/libbitcoinpqc-bindings/pull/1)
  `wasm-tests` @ `5ef7067`, libbitcoinpqc @ `b309f44`). Registry config in
  `.cargo/config.toml`.

### Cargo features

| Feature              | Default  | Role                               |
| -------------------- | -------- | ---------------------------------- |
| `drivechain`         | yes      | Existing BIP 300/301 rules         |
| `bip360`             | no       | P2MR output + PQC spend validation |
| `rustls` / `openssl` | `rustls` | TLS (unchanged)                    |

```bash
cargo build                                          # drivechain (default)
cargo build --no-default-features --features bip360  # BIP 360 only
cargo build --features "drivechain,bip360"           # both
```

New CLI flags (behind `bip360`):

- `--activation-height <n>` (default `0`)
- `--pqc-verify-budget-ms <ms>` (default `500`)

### Testing instructions

**Local CI stack (recommended before push):**

```bash
just verify          # bip360 check + pqc tests + drivechain tests + clippy + fmt + it build
just validate-rules-engine  # hub RuleEngine + worker crates (when touching rules/IPC)
just clippy          # broader feature sets (do not use cargo --all-features)
```

**Unit tests (BIP 360):**

```bash
cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 pqc::
cargo clippy -p bip300301_enforcer_lib --no-default-features --features bip360 -- -D warnings
```

**Drivechain default (unchanged):**

```bash
cargo test -p bip300301_enforcer_lib
```

**Opt-in integration (live bitcoind; recorded PASS 2026-07-17):**

```bash
just drivechain-blk-dat-e2e   # dual stock: Miner block == Alice blk*.dat
just bip360-blk-dat-e2e       # Bob P2MR mine → Alice disk match
just cusf-claims              # testmempoolaccept no-insert; stock rejects P2MR spend
just bip360-tier-b-cusf       # CUSF tip green (in matrix)
just bip360-tier-b-mempool    # Bob shapes 1–3 (needs BITCOIND_P2MR)
```

Workspace-root research notes (may be outside this git repo): `FINAL_REPORT.md`,
`RESIDUAL.md`, `HUMAN_REVIEW.md` under the `cusf/` workspace.

**Integration trials (regtest, stock Core):**

Prerequisites: run `just setup-core` (or have `integrationtests.env` with
`BITCOIND_UNPATCHED`), build the enforcer with `bip360` only, and use stock
Core. All trials use `bip360_setup_opts()` → `BitcoindKind::Unpatched`,
`--activation-height=0`, `--pqc-verify-budget-ms=5000`. Canonical one-liners:

```bash
just demo-a   # valid Schnorr spend retained
just demo-b   # empty witness → invalidateblock
```

Manual equivalent:

```bash
just setup-core     # optional: download stock bitcoind
just build-bip360
export BIP300301_ENFORCER_INTEGRATION_TEST_ENV=integrationtests.env  # or example.env
cargo run --example integration_tests --features bip360 -- --exact bip360_valid_schnorr_spend
cargo run --example integration_tests --features bip360 -- --exact bip360_invalid_block
```

| Trial                                                       | Expect                                                     |
| ----------------------------------------------------------- | ---------------------------------------------------------- |
| `bip360_valid_schnorr_spend`                                | Accepted                                                   |
| `bip360_valid_mldsa_spend`                                  | Accepted                                                   |
| `bip360_valid_slh_spend`                                    | Accepted                                                   |
| `bip360_valid_cross_block_schnorr_spend`                    | Accepted                                                   |
| `bip360_valid_cross_block_mldsa_spend`                      | Accepted                                                   |
| `bip360_valid_cross_block_slh_spend`                        | Accepted                                                   |
| `bip360_valid_hybrid_ec_slh_spend`                          | Accepted (hybrid EC+SLH, same-block)                       |
| `bip360_valid_hybrid_ec_slh_cross_block_spend`              | Accepted (hybrid EC+SLH, cross-block)                      |
| `bip360_invalid_block`                                      | Rejected (empty witness)                                   |
| `bip360_invalid_signature`                                  | Rejected (tampered sig, same-block)                        |
| `bip360_invalid_pubkey_size`                                | Rejected (ML-DSA sig + 32-byte pubkey, same-block)         |
| `bip360_invalid_merkle_path`                                | Rejected (bad control block, same-block)                   |
| `bip360_invalid_cross_block_signature_schnorr`              | Rejected (tampered Schnorr sig)                            |
| `bip360_invalid_cross_block_signature_mldsa`                | Rejected (tampered ML-DSA-44 sig)                          |
| `bip360_invalid_cross_block_signature_slh`                  | Rejected (tampered SLH-DSA sig)                            |
| `bip360_invalid_cross_block_merkle_path_schnorr`            | Rejected (bad control block, Schnorr)                      |
| `bip360_invalid_cross_block_merkle_path_mldsa`              | Rejected (bad control block, ML-DSA)                       |
| `bip360_invalid_cross_block_merkle_path_slh`                | Rejected (bad control block, SLH)                          |
| `bip360_invalid_cross_block_pubkey_size_mldsa`              | Rejected (ML-DSA sig + 32-byte pubkey)                     |
| `bip360_invalid_hybrid_ec_slh_tamper_ec_sig`                | Rejected (tampered EC sig, hybrid)                         |
| `bip360_invalid_hybrid_ec_slh_tamper_slh_sig`               | Rejected (tampered SLH sig, hybrid)                        |
| `bip360_invalid_hybrid_ec_slh_swap_sigs`                    | Rejected (swapped EC/SLH sig positions)                    |
| `bip360_valid_multi_leaf_schnorr_spend`                     | Accepted (three-leaf tree, Schnorr leaf)                   |
| `bip360_valid_multi_leaf_mldsa_spend`                       | Accepted (three-leaf tree, ML-DSA leaf)                    |
| `bip360_valid_multi_leaf_slh_spend`                         | Accepted (three-leaf tree, SLH leaf)                       |
| `bip360_valid_multi_leaf_cross_block_mldsa_spend`           | Accepted (multi-leaf fund N, ML-DSA spend N+1)             |
| `bip360_invalid_multi_leaf_wrong_control_block`             | Rejected (wrong control block, same-block)                 |
| `bip360_invalid_multi_leaf_cross_block_wrong_control_block` | Rejected (wrong control block, cross-block)                |
| `bip360_valid_multi_leaf_cross_block_schnorr_spend`         | Accepted (multi-leaf fund N, Schnorr spend N+1)            |
| `bip360_valid_multi_leaf_cross_block_slh_spend`             | Accepted (multi-leaf fund N, SLH spend N+1)                |
| `bip360_invalid_multi_leaf_tampered_signature_mldsa`        | Rejected (tampered ML-DSA sig, multi-leaf)                 |
| `bip360_valid_kitchen_sink_spend`                           | Accepted (kitchen-sink triple-algo leaf, same-block)       |
| `bip360_invalid_kitchen_sink_tamper_ec_sig`                 | Rejected (tampered EC sig, kitchen-sink)                   |
| `bip360_p2p_mempool_e2e`                                    | Accepted (dual-node P2P E2E; needs `just setup` + electrs) |

**Inventory:** **34** green block-only in `just it-all` (classic matrix +
TB-mine); dual-node P2P E2E, Tier A, TB-sendraw separate. See
`docs/REGTEST_DEMO.md` and `docs/MULTI_LEAF_P2MR.md`.

### CI changes

New job **`check-bip360`** in `.github/workflows/check_lint_build_release.yaml`:

1. `cargo check --no-default-features --features bip360 --all-targets`
2. `cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 pqc::`
3. P2MR signer smoke test (build `p2mr_signer` example + `p2mr_signer_roundtrip`
   tests + grep `signed_spend_tx_hex`)
4. `cargo clippy -p bip300301_enforcer_lib --no-default-features --features bip360 -- -D warnings`
5. `cargo clippy -p bip300301_enforcer_integration_tests --features bip360 -- -D warnings`
6. `cargo check --example integration_tests --features bip360`

Existing drivechain integration matrix is unchanged. Live BIP360 integration
trials are not wired into CI (require `integrationtests.env`, stock bitcoind via
`just setup-core`). Requires `.cargo/config.toml` in repo for Kellnr
(`check-bip360` job).

### Known limitations (call out in review)

**P0 / P1 prototype scope is complete.** Remaining gaps are intentional
deferrals or human-only steps (see [`cusf/STATUS.md`](../../STATUS.md)).

- **Cross-block P2MR prevout lookup** — Done (`dbs/p2mr_utxos.rs`). Integration
  trials cover valid + invalid cross-block spends for Schnorr, ML-DSA, and SLH,
  plus hybrid EC+SLH trials (34 green block-only in `it-all` incl.
  kitchen-sink + TB-mine; dual-node P2P separate).
- **Overload-model JSON vectors** — Done. Converted in
  `test_vectors/p2mr_overload_construction.json` (6 vectors: 5 construction + 1
  error); `pqc::overload_vectors` (8 passing + 1 ignored golden dump). See
  `docs/OVERLOAD_VECTORS.md`.
- **BIP author review** — Overload wording draft in
  `docs/BIP360_OVERLOAD_ADDENDUM.md`; not yet confirmed with BIP 360 authors.
- **Live integration trials** — Implemented and compile-verified (`cargo clippy`
  / `cargo check --example integration_tests --features bip360`). Green
  block-only suite: **34** in `it-all` (classic matrix + TB-mine); TB-mine
  **PASS** 2026-07-16 via `just bip360-tier-b-cusf`. Dual-node P2P E2E / Tier A
  / TB-sendraw are separate recipes (not folded into green matrix). Not wired
  into CI.
- **Upstream submission** — All code/docs ready locally; commit, push, and PR
  opening are human-only ([exact commands below](#exact-human-commands)).

### Documentation

- `cusf/CRITERIA_AUDIT.md` — criteria traceability audit (workspace sibling;
  optional PR attachment)
- `docs/CUSF-BIP360.md` — enforcer-specific BIP 360 reference
- `docs/MULTI_LEAF_P2MR.md` — algorithm-per-leaf P2MR design + Phase C trial
  matrix
- `docs/REGTEST_DEMO.md` — regtest demo walkthrough
- `docs/UPSTREAM_PR.md` — this file
- `AGENTS.md` — contributor/agent guide (fork context, TDD, workspace boundary,
  git signing policy, HITL guardrails)
- `README.md` — feature matrix pointer

---

## File-level change list

From `git status` / `git diff --stat` on the working tree (not yet committed).

### Modified (19 files, +1016 / −224 lines)

| File                                              | Summary                                                                         |
| ------------------------------------------------- | ------------------------------------------------------------------------------- |
| `.github/workflows/check_lint_build_release.yaml` | Add `check-bip360` CI job (6 steps incl. P2MR signer smoke test)                |
| `Cargo.toml`                                      | Workspace deps: `bitcoin-p2mr-pqc`, `bitcoinpqc`                                |
| `Cargo.lock`                                      | Lockfile for new deps                                                           |
| `README.md`                                       | Feature matrix + link to `docs/CUSF-BIP360.md`                                  |
| `app/Cargo.toml`                                  | `bip360` feature forwarding                                                     |
| `app/main.rs`                                     | Wire activation height + PQC budget into validator                              |
| `integration_tests/Cargo.toml`                    | `bip360` feature                                                                |
| `integration_tests/README.md`                     | BIP 360 trials table + run commands                                             |
| `integration_tests/integration_test.rs`           | Register BIP 360 trials (34 green block-only + dual-node / Tier A / TB-sendraw) |
| `integration_tests/lib.rs`                        | Module exports for BIP 360 tests                                                |
| `lib/Cargo.toml`                                  | `bip360` feature + optional deps                                                |
| `lib/cli.rs`                                      | `--activation-height`, `--pqc-verify-budget-ms`                                 |
| `lib/lib.rs`                                      | Export `validator::pqc` behind feature                                          |
| `lib/validator/cusf_enforcer.rs`                  | `accept_tx` BIP 360 hooks                                                       |
| `lib/validator/dbs/diff.rs`                       | P2MR UTXO diff hooks for block connect/disconnect                               |
| `lib/validator/dbs/mod.rs`                        | Export `p2mr_utxos` module                                                      |
| `lib/validator/mod.rs`                            | Feature-gate drivechain vs PQC modules                                          |
| `lib/validator/task/error.rs`                     | BIP 360 error variants                                                          |
| `lib/validator/task/mod.rs`                       | Feature-split drivechain handlers; `connect_block` pqc path                     |

### New (untracked)

| File                                             | Summary                                                                                       |
| ------------------------------------------------ | --------------------------------------------------------------------------------------------- |
| ~~`.cargo/config.toml`~~                         | Removed — no Kellnr; `bitcoin-p2mr-pqc` is a git pin                                          |
| `docs/BIP360_OVERLOAD_ADDENDUM.md`               | Draft BIP wording for overload model                                                          |
| `docs/CUSF-BIP360.md`                            | BIP 360 enforcer reference                                                                    |
| `docs/KELLNR_PUBLISH.md`                         | Kellnr publish steps for `bitcoinpqc` 0.4.0                                                   |
| `docs/MEMPOOL_RELAY_POLICY.md`                   | Mempool relay / budget asymmetry notes                                                        |
| `docs/OVERLOAD_VECTORS.md`                       | Ref-impl → overload mapping and conversion notes                                              |
| `docs/P2MR_SIGNER.md`                            | `p2mr_signer` example usage                                                                   |
| `docs/REGTEST_DEMO.md`                           | Regtest demo walkthrough                                                                      |
| `docs/SHRINCS_DEFERRED.md`                       | SHRINCs scope deferral                                                                        |
| `docs/SIGNET_WORKSHOP.md`                        | Signet workshop pointers                                                                      |
| `docs/MULTI_LEAF_P2MR.md`                        | Algorithm-per-leaf P2MR design + Phase C trial matrix                                         |
| `docs/UPSTREAM_PR.md`                            | This PR package                                                                               |
| `AGENTS.md`                                      | Contributor/agent guide (fork context, TDD, workspace boundary, git signing, HITL guardrails) |
| `docs/WOTS_DEFERRED.md`                          | WOTS+ scope deferral                                                                          |
| `docs/SECURITY_DESIGN_NOTES.md`                  | Scheme rationale, migration thoughts                                                          |
| `integration_tests/bip360_block.rs`              | Shared GBT / coinbase / submitblock helpers                                                   |
| `integration_tests/test_bip360_invalid_block.rs` | Empty-witness `submitblock` → `invalidateblock` trial                                         |
| `integration_tests/test_bip360_invalid_spend.rs` | Invalid sig / pubkey / merkle-path trials                                                     |
| `integration_tests/test_bip360_valid_spend.rs`   | Valid Schnorr / ML-DSA / SLH + cross-block + hybrid EC+SLH trials                             |
| `integration_tests/test_bip360_multi_leaf.rs`    | Multi-leaf P2MR trials (9: 6 valid + 3 invalid)                                               |
| `lib/examples/p2mr_signer.rs`                    | Minimal CLI signer for Schnorr / ML-DSA / SLH P2MR spends                                     |
| `lib/validator/dbs/p2mr_utxos.rs`                | redb table `OutPoint → TxOut` for confirmed P2MR outputs                                      |
| `lib/validator/pqc/activation.rs`                | Activation height gating                                                                      |
| `lib/validator/pqc/leaf_script.rs`               | Tapscript walker + sig sites                                                                  |
| `lib/validator/pqc/limits.rs`                    | Witness/sig DoS limits                                                                        |
| `lib/validator/pqc/merkle.rs`                    | Control block / merkle path                                                                   |
| `lib/validator/pqc/multi_leaf.rs`                | Three-leaf P2MR tree builder + unit tests                                                     |
| `lib/validator/pqc/mod.rs`                       | Public entry points                                                                           |
| `lib/validator/pqc/overload_vectors.rs`          | Overload-model construction vector tests                                                      |
| `lib/validator/pqc/p2mr_output.rs`               | P2MR `scriptPubKey` validation                                                                |
| `lib/validator/pqc/p2mr_utxo.rs`                 | Intra-block + cross-block P2MR UTXO diff application                                          |
| `lib/validator/pqc/schemes.rs`                   | Overloaded `OP_CHECKSIG` verification                                                         |
| `lib/validator/pqc/signer_dev.rs`                | Shared P2MR signing (`build_block_p2mr_spend`, `sign_p2mr_script_path_spend`)                 |
| `lib/validator/pqc/spend.rs`                     | Witness stack + spend validation                                                              |
| `Justfile`                                       | Task runner (`just verify`, `just setup-core`, `just demo-a` / `just demo-b`, `just it-all`)  |
| `test_vectors/p2mr_overload_construction.json`   | Converted P2MR construction JSON (no `OP_SUBSTR`)                                             |

---

## Reviewer checklist

- [ ] `drivechain` default build and tests unchanged
      (`cargo test -p bip300301_enforcer_lib`)
- [ ] `bip360`-only build compiles with no drivechain code linked
- [ ] `check-bip360` CI job steps match local verify commands
- [ ] Kellnr + git deps acceptable for upstream (or need crates.io publish
      first)
- [ ] Overloaded `OP_CHECKSIG` model documented vs ref-impl `OP_SUBSTR` —
      confirm with BIP authors
- [ ] `invalidateblock` still fires on `ConnectBlockAction::Reject` (wallet
      skips sync on reject)
- [ ] Activation height `0` default is appropriate for regtest prototype only
- [ ] Cross-block prevout lookup + green block-only suite (**34** in `it-all`:
      classic matrix + TB-mine, hybrid EC+SLH + kitchen-sink + multi-leaf)
      acceptable for prototype? Dual-node P2P E2E / Tier A / TB-sendraw are
      separate recipes.
- [ ] Integration trials use **stock** `bitcoind` (`BitcoindKind::Unpatched`)
- [ ] `check-bip360` includes integration crate clippy + example check

---

## Exact human commands

Run from the enforcer repo root (`bip300301_enforcer/`).

```bash
# 1. Review the full diff
git status
git diff
git diff --stat

# 2. Create feature branch
git checkout -b feature/bip360-cusf

# 3. Stage all BIP 360 changes
git add \
  .cargo/config.toml \
  .github/workflows/check_lint_build_release.yaml \
  AGENTS.md \
  Cargo.toml Cargo.lock \
  README.md \
  app/Cargo.toml app/main.rs \
  docs/ \
  integration_tests/ \
  lib/ \
  Justfile

# 4. Commit — GPG signing required (never use -c commit.gpgsign=false)
#    If signing fails, stop and fix your GPG setup before committing.
git commit -m "$(cat <<'EOF'
feat: add optional bip360 CUSF enforcement for P2MR + PQC

Gate BIP 360 P2MR output and post-quantum signature rules behind a
bip360 Cargo feature. Drivechain remains the default. Adds PQC
validator module, green block-only regtest suite (34 in it-all incl.
TB-mine + kitchen-sink; dual-node P2P separate; cross-block, hybrid EC+SLH,
multi-leaf P2MR), 123 PQC unit tests, CLI activation height, CI
check-bip360 job,
and docs including AGENTS.md.
EOF
)"

# 5. Push to your fork (replace ORIGIN with your fork remote if needed)
git remote -v   # confirm origin points at your fork or upstream
git push -u origin feature/bip360-cusf

# 6. Open PR against upstream master
#    https://github.com/LayerTwo-Labs/bip300301_enforcer/compare/master...YOUR_FORK:feature/bip360-cusf
```

If you do not have push access to upstream, fork first:

```bash
# On GitHub: fork the upstream bip300301_enforcer repository
git remote add fork git@github.com:YOUR_USER/bip300301_enforcer.git
git push -u fork feature/bip360-cusf
```

Then open the PR from your fork's `feature/bip360-cusf` → upstream `master`.

---

## Links

- Upstream repo: https://github.com/LayerTwo-Labs/bip300301_enforcer
- Parent design: `cusf/DESIGN.md` (workspace sibling — not in enforcer git repo)
- Canonical status: `cusf/STATUS.md` (workspace sibling — share separately or
  paste into PR description)
- **Criteria audit:** `cusf/CRITERIA_AUDIT.md` (workspace sibling — requirement
  traceability for reviewers; available for reviewers)
- BIP 360 ref-impl: `bitcoin/bips` branch `p2mr-v2`
- Regtest demo: [REGTEST_DEMO.md](./REGTEST_DEMO.md)
