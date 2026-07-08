# Agent guide — BIP 360 enforcer fork

## Upstream relationship

This repository is a **local fork** of the upstream [bip300301_enforcer](https://github.com/LayerTwo-Labs/bip300301_enforcer) project. Work extends upstream with an optional `bip360` Cargo feature. Default `drivechain` behavior is unchanged.

Human maintainers open upstream PRs from this working tree. Agents should **not** assume changes are merged upstream.

## Code hygiene

- Use **self-documenting names** (e.g. `ThreeAlgorithmP2mrTree`, not opaque abbreviations).
- Write documentation in concise natural language; explain behavior plainly.
- Avoid lazy shorthand and thoughtless jargon in docs and identifiers.
- Do **not** put ephemeral identifiers (session IDs, temp paths, agent run tokens) in committed docs.
- Do **not** commit client names, compensation figures, private engagement details, or machine-local paths in docs.
- Track remaining work in [`cusf/STATUS.md`](../STATUS.md) and enforcer docs — do not leave open items only in chat.

## Workspace boundary

Agents must treat the **opened workspace root** as the default operating boundary — the top-level directory of the IDE/editor workspace for this session. The folder name on disk varies by clone; **do not assume a fixed path** (e.g. a particular parent directory name). In this project layout, the workspace root is the **parent of `bip300301_enforcer/`** and typically also contains sibling repos (e.g. `cusf-enforcer-mempool/`) and workspace-level docs (`STATUS.md`, `DESIGN.md`, etc.) outside the enforcer git repo.

- **Default:** Read, write, edit, and run commands only within that workspace root and its subdirectories.
- **Outside the workspace:** Requires **explicit human permission** before any access (e.g. `~/Downloads/`, other git repos, system config, home-directory paths).
- **Read-only when external:** If permitted to access paths outside the workspace, use **read-only** operations only — no writes, edits, deletes, installs, or commits outside the workspace.
- **Do not** store machine-local absolute paths from outside the workspace in committed docs (see § Code hygiene).

## Test-driven development

1. Write a **failing** unit or integration test that states the expected behavior.
2. Implement the smallest change that makes it pass.
3. Re-run the full verification commands below before finishing.

Red/green applies to validator logic (`lib/validator/quantum/`) and integration trials alike.

## Cargo features

| Feature | Default | Purpose |
|---------|---------|---------|
| `drivechain` | yes | BIP 300/301 sidechain rules (upstream default) |
| `bip360` | no | P2MR output + post-quantum signature validation |
| `rustls` / `openssl` | `rustls` | TLS backend |

Build BIP 360 only:

```bash
cargo build --no-default-features --features bip360
```

## Task runner (`just`)

Workflow commands live in [`Justfile`](./Justfile). Run from `bip300301_enforcer/`
or from the workspace root (root `justfile` imports this file).

| Recipe | Purpose |
|--------|---------|
| `just verify` | Full pre-submit check (matches CI `check-bip360` + drivechain tests + fmt) |
| `just test-it` | Drivechain integration trials (upstream default) |
| `just setup-core` | Download stock bitcoind; write `integrationtests.env` (BIP 360 live trials) |
| `just setup` | Full upstream bootstrap (patched + stock bitcoind + electrs) — drivechain trials only |
| `just build-bip360` | Build enforcer + integration harness with `bip360` |
| `just demo-a` / `just demo-b` | Regtest smoke demos (valid / invalidateblock) |
| `just it <trial>` | Run one BIP 360 integration trial |
| `just it-all` | Run all 31 registered BIP 360 trials |

Append `yes` to auto-run setup when `integrationtests.env` is missing
(e.g. `just demo-b yes`).

## Integration test prerequisites

Live BIP 360 regtest trials need stock `bitcoind` and env configuration:

```bash
just setup-core
# or
export BIP300301_ENFORCER_INTEGRATION_TEST_ENV=integrationtests.env
```

Without this, trials fail with missing `BITCOIND_UNPATCHED` (or similar). Compile verification does not require live bitcoind.

## Verification (before declaring done)

```bash
cd bip300301_enforcer
just verify
```

## Git policy

- **Default: do not commit.** Leave changes in the working tree for human upstream PR preparation.
- If committing: **GPG signing must succeed.** Never bypass signing (`git -c commit.gpgsign=false` is forbidden). If signing fails, stop and report in the summary — do not commit unsigned code.

## Human-in-the-loop guardrails

Agents automate implementation and documentation; **humans own irreversible or externally visible actions.** Criteria traceability: [`cusf/CRITERIA_AUDIT.md`](../CRITERIA_AUDIT.md).

### Actions agents must NEVER do

| Action | Why |
|--------|-----|
| **Unsigned commits** or bypass GPG signing | `git -c commit.gpgsign=false` and `--no-gpg-sign` are forbidden |
| **Push to remotes** without explicit human instruction | Default is uncommitted local working tree |
| **Open upstream PRs** | Prepared in `docs/UPSTREAM_PR.md`; human submits |
| **Run live integration trials** without env setup | Requires `BITCOIND_UNPATCHED`, `integrationtests.env` — see below |
| **Publish to Kellnr / crates.io** | Human-only per `docs/KELLNR_PUBLISH.md` |
| **Deploy signet workshop infra** | Human-only per `docs/SIGNET_WORKSHOP.md` |
| **Contact BIP 360 authors** on overload wording | Draft in `docs/BIP360_OVERLOAD_ADDENDUM.md`; human coordinates review |
| **Write or modify files outside the workspace** without explicit human permission | Default boundary is the opened workspace root; external access requires permission and is **read-only** only |

### Explicitly deferred (do not implement without new vectors/spec)

| Item | Doc |
|------|-----|
| SHRINCs / XMSS hybrid backup paths | [`docs/SHRINCS_DEFERRED.md`](./docs/SHRINCS_DEFERRED.md) |
| WOTS+ internals exposure | [`docs/WOTS_DEFERRED.md`](./docs/WOTS_DEFERRED.md) |
| Live BIP 360 integration trials in CI | Not wired into CI; unit tests run in `check-bip360` only |
| `shrincs` Cargo feature | Placeholder only in `lib/Cargo.toml` — no functionality |

### Human-only steps (prepared locally)

| Step | Reference |
|------|-----------|
| Review diff, **GPG-signed** commit, push `feature/bip360-cusf` | [`docs/UPSTREAM_PR.md`](./docs/UPSTREAM_PR.md) § Exact human commands |
| Open upstream PR | PR title/body in `docs/UPSTREAM_PR.md` |
| Live regtest demo walkthrough | [`docs/REGTEST_DEMO.md`](./docs/REGTEST_DEMO.md); `just demo-a` / `just demo-b` |
| Confirm overload model with BIP 360 authors | [`docs/BIP360_OVERLOAD_ADDENDUM.md`](./docs/BIP360_OVERLOAD_ADDENDUM.md) |
| Kellnr `bitcoinpqc` 0.4.0 publish | [`docs/KELLNR_PUBLISH.md`](./docs/KELLNR_PUBLISH.md) |
| Signet deployment / workshop | [`docs/SIGNET_WORKSHOP.md`](./docs/SIGNET_WORKSHOP.md) |
| Run live integration trials (31 registered) | `just setup-core`; `just it-all` or `just it <trial>` |

## Key paths

| Area | Path |
|------|------|
| Quantum validator | `lib/validator/quantum/` |
| Multi-leaf P2MR | `lib/validator/quantum/multi_leaf.rs` |
| Integration harness | `integration_tests/bip360_block.rs` |
| Trial registry | `integration_tests/integration_test.rs` |
| Design notes | `docs/MULTI_LEAF_P2MR.md`, `docs/CUSF-BIP360.md` |