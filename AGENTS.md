# Agent guide — BIP 360 enforcer fork

## Upstream relationship

This repository is a **local fork** of the upstream
[bip300301_enforcer](https://github.com/LayerTwo-Labs/bip300301_enforcer)
project. Work extends upstream with an optional `bip360` Cargo feature. Default
`drivechain` behavior is unchanged.

Human maintainers open upstream PRs from this working tree. Agents should
**not** assume changes are merged upstream.

## Code hygiene

- Use **self-documenting names** (e.g. `ThreeAlgorithmP2mrTree`, not opaque
  abbreviations).
- Write documentation in concise natural language; explain behavior plainly.
- Avoid lazy shorthand and thoughtless jargon in docs and identifiers.
- Do **not** put ephemeral identifiers (session IDs, temp paths, agent run
  tokens) in committed docs.
- Do **not** commit client names, compensation figures, private engagement
  details, or machine-local paths in docs.
- Track remaining work in [`cusf/STATUS.md`](../STATUS.md) and enforcer docs —
  do not leave open items only in chat.

## Workspace boundary

Agents must treat the **opened workspace root** as the default operating
boundary — the top-level directory of the IDE/editor workspace for this session.
The folder name on disk varies by clone; **do not assume a fixed path** (e.g. a
particular parent directory name). In this project layout, the workspace root is
the **parent of `bip300301_enforcer/`** and typically also contains sibling
repos (e.g. `cusf-enforcer-mempool/`) and workspace-level docs (`STATUS.md`,
`DESIGN.md`, etc.) outside the enforcer git repo.

- **Default:** Read, write, edit, and run commands only within that workspace
  root and its subdirectories.
- **Outside the workspace:** Requires **explicit human permission** before any
  access (e.g. `~/Downloads/`, other git repos, system config, home-directory
  paths).
- **Read-only when external:** If permitted to access paths outside the
  workspace, use **read-only** operations only — no writes, edits, deletes,
  installs, or commits outside the workspace.
- **Do not** store machine-local absolute paths from outside the workspace in
  committed docs (see § Code hygiene).

## Test-driven development

1. Write a **failing** unit or integration test that states the expected
   behavior.
2. Implement the smallest change that makes it pass.
3. Re-run the full verification commands below before finishing.

Red/green applies to validator logic (`lib/validator/pqc/`) and integration
trials alike.

## Cargo features

| Feature              | Default  | Purpose                                                            |
| -------------------- | -------- | ------------------------------------------------------------------ |
| `drivechain`         | yes      | BIP 300/301 sidechain rules (upstream default)                     |
| `bip360`             | no       | P2MR output + post-quantum signature validation (`validator/pqc/`) |
| `rustls` / `openssl` | `rustls` | TLS backend                                                        |
| `shrincs`            | no       | Reserved placeholder — no implementation                           |

Default `cargo build` and `just test-drivechain` use **drivechain only** (no
`bip360`, no `bitcoin-p2mr-pqc` / `bitcoinpqc` deps). BIP 360 code is behind
`#[cfg(feature = "bip360")]` throughout `lib/validator/pqc/`, CLI flags,
integration trials, and optional deps.

| Build | Command / recipe |
| ----- | ---------------- |
| Drivechain (default) | `cargo build -p bip300301_enforcer` → `bip300301_enforcer` |
| BIP 360 only | `just build-enforcer-bip360` → `bip360_enforcer` |
| Both rule sets (**AND**, one process) | `just build-enforcer-combined` / `just build` → `cusf_enforcer` |
| Hub + rule workers | `just build-hub-workers` → hub + `cusf_rules_drivechain` + `cusf_rules_bip360` |
| Rules engine tests | `just validate-rules-engine` / `./scripts/validate-rules-engine.sh` |

**Target:** hub + feature-compiled rule workers over IPC — one Core I/O owner;
workers register; **consent required**; no answer / failure = **no** (error log).
Combined features = **AND** of drivechain and bip360 ballots (same as hub
aggregation). See [`docs/MULTI_ENFORCER.md`](./docs/MULTI_ENFORCER.md). Do not
attach two full enforcer binaries to one bitcoind.

P2P E2E integration code uses `#[cfg(feature = "bip360")]` (same gating as
existing `validator/pqc/` and integration trials).

Build BIP 360 only:

```bash
cargo build --no-default-features --features bip360
```

Upstream regression smoke (default build, no bip360):

```bash
just drivechain-smoke
```

## Task runner (`just`)

Workflow commands live in [`Justfile`](./Justfile). Run from
`bip300301_enforcer/` or from the workspace root (root `justfile` imports this
file).

**Technical debt (documented, not blocking):** the Justfile is oversized (~1k
lines) — quality gate is small; most bulk is setup / regtest e2e / multiproc
bash. Tracked in [`cusf/RESIDUAL.md`](../RESIDUAL.md) under optional engineering.
Long-term direction: hermetic **Nix flakes** (idiomatic checks/packages/shells),
not bash-in-Nix. Do not expand Just further for new long workflows when a
`scripts/` or future flake check would do.

| Recipe                            | Purpose                                                                                                                                                                                                                                    |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `just test`                       | **Primary local gate** — one sequential recipe (banners `[1/3]`…`[3/3]`): (1) nightly `fmt --check`, (2) workspace clippy **check** (`-D warnings`, **never** `--fix`), (3) `cargo nextest` workspace + bip360-only lib with `-E 'not kind(example)'` (excludes libtest-mimic bitcoind harness). Requires `cargo-nextest` + nightly rustfmt. Bitcoind trials: `just it` / `it-all`. Not `validate-rules-engine`. Pass-through: `just test -- <nextest args>`. |
| `just fmt-check`                  | Nightly rustfmt `--check` only (prints a banner)                                                                                                                                                                                                            |
| `just clippy`                     | `fmt-check` then `_clippy-check` (no `--fix`). Write path only: `just clippy-fix`.                                                                                                                                                                          |
| `just verify`                     | Pre-submit subset: **fmt first**, then CI-style bip360/drivechain unit checks + `clippy-bip360`. Does **not** run full nextest or rules-engine — use `just test` / `just validate-rules-engine` as needed. |
| `just build-hub-workers`          | Hub (combined features) + `cusf_rules_drivechain` + `cusf_rules_bip360`                                                                                                                                                                    |
| `just validate-rules-engine`      | Rules engine unit tests + check hub/workers + UDS multiproc smoke (no `--all-features`). CI job `check-rules-engine` (**no** bitcoind/live tip). Capability policy + chain_p2mr_utxos + drivechain_state + Dynamic RuleId lock + **PR CI live tip residual honesty** locks (`pr_ci_live_tip_residual_honesty_score_lock`, `shutdown_residual_honesty_docs_inventory_lock`). Optional `--rules-worker RULE_ID=PATH` (`drivechain`/`bip360` only). |
| `just smoke-rules-workers-tip`    | Multiproc UDS tip smoke (no bitcoind required); optional `BITCOIND=…` / `CUSF_LIVE_TIP_E2E=1`.                                                                 |
| `just sigterm-rules-workers-e2e`  | Opt-in multiproc **worker** SIGTERM e2e (Health → `kill -TERM` → exit 0 + socket unlink). **Not** part of `validate-rules-engine` / SCORE. Hub process SIGTERM + mid-handler cancel remain residual. |
| `just live-tip-rules-workers-e2e` | Opt-in live tip (**HITL** only — not PR CI green matrix). Env unset → skip 0. `CUSF_LIVE_TIP_E2E=1` + missing `BITCOIND`/hub fail → non-zero. Manual GH: `workflow_dispatch` input `run_live_tip_e2e`. SCORE lock tests pin this separation. |
| `just build-enforcers`            | **Transitional** multi-feature artifacts; prefer `build-hub-workers` ([`docs/MULTI_ENFORCER.md`](./docs/MULTI_ENFORCER.md))                                                                                                                |
| `just drivechain-smoke`           | Default build + drivechain unit tests (upstream regression, no bip360; **local-only** — CI wiring is HITL)                                                                                                                                 |
| `just test-pqc`                   | PQC unit tests (`bip360` only; `pqc::` module)                                                                                                                                                                                             |
| `just test-quantum`               | Alias for `just test-pqc` (backwards compatibility)                                                                                                                                                                                        |
| `just test-drivechain`            | Drivechain unit tests on default features (no bip360)                                                                                                                                                                                      |
| `just test-it`                    | Drivechain integration trials (upstream default)                                                                                                                                                                                           |
| `just setup-core`                 | Download stock bitcoind; write `integrationtests.env` (BIP 360 live trials)                                                                                                                                                                |
| `just setup`                      | Full upstream bootstrap (patched + stock bitcoind + electrs) — drivechain trials only                                                                                                                                                      |
| `just build-bip360`               | Build enforcer + integration harness with `bip360` only (`--no-default-features`) — sufficient for block-matrix trials                                                                                                                     |
| `just demo-a` / `just demo-b`     | Regtest smoke demos (valid / invalidateblock)                                                                                                                                                                                              |
| `just it <trial>`                 | Run one BIP 360 integration trial                                                                                                                                                                                                          |
| `just it-all`                     | Run all 34 green block-only BIP 360 trials (classic matrix + TB-mine)                                                                                                                                                                      |
| `just bip360-block-matrix`        | Alias for `just it-all` (34 block-only trials)                                                                                                                                                                                             |
| `just bip360-tier-b-cusf`         | TB-mine CUSF mining path alone (PASS; also in `it-all`)                                                                                                                                                                                    |
| `just bip360-tier-b-cusf-factory` | TB-factory dual-process: Miner stock `submitblock` → Alice tip (PASS; not in `it-all`; `setup-core` enough)                                                                                                                                |
| `just bip360-tier-b-mempool`      | TB-sendraw opt-in **PASS** shapes 1+2+**3** (Schnorr + Core hybrid + overload kitchen-sink; not in `it-all`)                                                                                                                               |
| `just bip360-p2p-e2e`             | Dual-node P2P mempool E2E (`bip360_p2p_mempool_e2e`; not in `it-all`; rebuilds enforcer with `drivechain,bip360`; needs `just setup` for electrs)                                                                                          |
| `just bip360-verify-full`         | **Canonical pre-submit:** `drivechain-smoke` + `verify` + `build-bip360` + `bip360-block-matrix` + `bip360-p2p-e2e`; accepts `auto` param (e.g. `yes`) for setup when env missing. Phase D = local full verify; **CI wiring remains HITL** |

Append `yes` to auto-run setup when `integrationtests.env` is missing (e.g.
`just demo-b yes`).

## Integration test prerequisites

Live BIP 360 regtest trials need stock `bitcoind` and env configuration:

```bash
just setup-core
# or
export BIP300301_ENFORCER_INTEGRATION_TEST_ENV=integrationtests.env
```

Without this, trials fail with missing `BITCOIND_UNPATCHED` (or similar).
Compile verification does not require live bitcoind.

## Verification (before declaring done)

**Canonical pre-submit** (Phase D full stack):

```bash
cd bip300301_enforcer
just bip360-verify-full yes   # auto-setup-core/setup when env missing
```

Minimum gate (always run):

```bash
just drivechain-smoke   # upstream default regression
just verify             # BIP 360 pre-submit (matches CI check-bip360 + drivechain tests)
```

`bip360-verify-full` order: `drivechain-smoke` → `verify` → `build-bip360` →
`bip360-block-matrix` → `bip360-p2p-e2e`. The `build-bip360` step is required
because `drivechain-smoke` overwrites the enforcer binary with drivechain-only
features (no `--activation-height` CLI). Live block-matrix needs stock bitcoind
(`just setup-core`); P2P E2E needs electrs (`just setup`).

`it-all` / `bip360-block-matrix` set `BIP360_SKIP_REBUILD=1` after a single
`build-bip360` prelude so trials do not rebuild per iteration. **Do not export
`BIP360_SKIP_REBUILD` manually** — if set without a preceding `build-bip360`, a
stale drivechain-only enforcer binary will break trials (`unexpected argument`
on bip360 CLI flags).

Phase D delivers **local** recipe wiring and smoke execution. Adding
`drivechain-smoke` / `bip360-verify-full` to CI workflows is **human/HITL** per
guardrails below.

For full CI fmt parity, run `just fmt` (`cargo +nightly fmt`) — `just verify`
uses `fmt-check` (stable rustfmt), while CI `check-rustfmt` uses
`cargo +nightly fmt -- --check`.

## Git policy

- **Default: do not commit.** Leave changes in the working tree for human
  upstream PR preparation.
- If committing: **GPG signing must succeed.** Never bypass signing
  (`git -c commit.gpgsign=false` is forbidden). If signing fails, stop and
  report in the summary — do not commit unsigned code.

## Human-in-the-loop guardrails

Agents automate implementation and documentation; **humans own irreversible or
externally visible actions.** Criteria traceability:
[`cusf/CRITERIA_AUDIT.md`](../CRITERIA_AUDIT.md).

### Actions agents must NEVER do

| Action                                                                            | Why                                                                                                          |
| --------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| **Unsigned commits** or bypass GPG signing                                        | `git -c commit.gpgsign=false` and `--no-gpg-sign` are forbidden                                              |
| **Push to remotes** without explicit human instruction                            | Default is uncommitted local working tree                                                                    |
| **Open upstream PRs**                                                             | Prepared in `docs/UPSTREAM_PR.md`; human submits                                                             |
| **Run live integration trials** without env setup                                 | Requires `BITCOIND_UNPATCHED`, `integrationtests.env` — see below                                            |
| **Publish to Kellnr / crates.io**                                                 | Human-only per `docs/KELLNR_PUBLISH.md`                                                                      |
| **Deploy signet workshop infra**                                                  | Human-only per `docs/SIGNET_WORKSHOP.md`                                                                     |
| **Contact BIP 360 authors** on overload wording                                   | Draft in `docs/BIP360_OVERLOAD_ADDENDUM.md`; human coordinates review                                        |
| **Write or modify files outside the workspace** without explicit human permission | Default boundary is the opened workspace root; external access requires permission and is **read-only** only |

### Explicitly deferred (do not implement without new vectors/spec)

| Item                                  | Doc                                                      |
| ------------------------------------- | -------------------------------------------------------- |
| SHRINCs / XMSS hybrid backup paths    | [`docs/SHRINCS_DEFERRED.md`](./docs/SHRINCS_DEFERRED.md) |
| WOTS+ internals exposure              | [`docs/WOTS_DEFERRED.md`](./docs/WOTS_DEFERRED.md)       |
| Live BIP 360 integration trials in CI | Not wired into CI; unit tests run in `check-bip360` only |
| `shrincs` Cargo feature               | Placeholder only in `lib/Cargo.toml` — no functionality  |

### Human-only steps (prepared locally)

| Step                                                           | Reference                                                                       |
| -------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Review diff, **GPG-signed** commit, push `feature/bip360-cusf` | [`docs/UPSTREAM_PR.md`](./docs/UPSTREAM_PR.md) § Exact human commands           |
| Open upstream PR                                               | PR title/body in `docs/UPSTREAM_PR.md`                                          |
| Live regtest demo walkthrough                                  | [`docs/REGTEST_DEMO.md`](./docs/REGTEST_DEMO.md); `just demo-a` / `just demo-b` |
| Confirm overload model with BIP 360 authors                    | [`docs/BIP360_OVERLOAD_ADDENDUM.md`](./docs/BIP360_OVERLOAD_ADDENDUM.md)        |
| Kellnr `bitcoinpqc` 0.4.0 publish                              | [`docs/KELLNR_PUBLISH.md`](./docs/KELLNR_PUBLISH.md)                            |
| Signet deployment / workshop                                   | [`docs/SIGNET_WORKSHOP.md`](./docs/SIGNET_WORKSHOP.md)                          |
| Run live green block-only suite (34 in `it-all`)               | `just setup-core`; `just it-all` or `just it <trial>`                           |
| Run live dual-node P2P E2E (`bip360_p2p_mempool_e2e`)          | `just setup` (electrs required); `just bip360-p2p-e2e` (not in `it-all`)        |

## Key paths

| Area                | Path                                             |
| ------------------- | ------------------------------------------------ |
| PQC validator       | `lib/validator/pqc/`                             |
| Multi-leaf P2MR     | `lib/validator/pqc/multi_leaf.rs`                |
| Integration harness | `integration_tests/bip360_block.rs`              |
| P2P E2E harness     | `integration_tests/bip360_dual_node.rs`          |
| P2P tx metrics      | `integration_tests/bip360_tx_report.rs`          |
| Trial registry      | `integration_tests/integration_test.rs`          |
| Design notes        | `docs/MULTI_LEAF_P2MR.md`, `docs/CUSF-BIP360.md` |
