# CUSF hub + modular rule workers

**Status:** architecture **locked**.  
**Shipped:** P1 in-process engine + feature ballots on mempool `validate_tx` **and** `connect_block`; P2 rules.v1 + `RuleBackend::{Local,Remote}` with **enforced** remote timeout; P3 worker bins (fail-closed, capped stdin + **UDS `--uds`** with RUL1 framing + nonblocking accept + I/O timeouts + **SIGTERM/SIGINT → shutdown flag**); P4 Just recipes + validate multiproc smoke + GH `check-rules-engine`; P5 `RULES_V1_VERSION` + timeout docs; hub CLI `--rules-worker` / `--rules-worker-timeout-ms`; parent prevouts on wire (`parent_txs_hex`); **chain P2MR UTXOs** on ConnectBlock (`chain_p2mr_utxos_hex`); **drivechain tip/sidechains/current-ctips/pending-M6ids** on ValidateTx/ConnectBlock (`drivechain_state`) with **independent SoftForkRule M8 tip + multi-active-OP_DRIVECHAIN + current-ctip structural M5/M6 + pending M6id/vote threshold + connect M7→accepted_bmm M8** (capability `drivechain-ctip-m8-pending-m6-checks-on-wire`); **capability negotiation**; **hot-path remote cutover only when validation is allowlisted real**; eager orphan reaper + **UDS mid-read abort** (`RuleTransport::abort_inflight` / socket shutdown on join-timeout); opt-in `CUSF_LIVE_TIP_E2E=1` live tip script + **HITL** `workflow_dispatch` job (does not gate PR CI); opt-in multiproc **bin SIGTERM e2e** (`just sigterm-rules-workers-e2e`, outside SCORE).  
**Not shipped yet (accepted residual honesty):** full historical `ctip_outpoint_to_value_seq` / M1–M4 DB-in-worker (Path B — not full-wire practical without dishonest caps or dual-AND-unsafe partials); **dynamic `RuleId`** (not unlocked — `&'static str` + `static_rule_id` allowlist only; freeform parse fails); automatic PR CI for live tip e2e (HITL `workflow_dispatch` only); worker mid-handler cancel + hub process SIGTERM e2e (needs bitcoind; worker bin SIGTERM e2e is opt-in outside SCORE).  
**Consent honesty:** Local feature ballots **always** run when the feature is on (mempool dual-AND). Remotes register **only** after successful handshake with an **allowlisted** real `validation` token; stub / unknown / unreachable → **not** registered, Local kept. Registered remote ballots are **AND**ed with Local. Local bip360 connect UTXO **diff** always runs. Timeout / Failure / Reject → aggregate Reject.  
**Hub today:** `bip300301_enforcer` owns Core I/O; **fat hub is default** (Local drivechain ON + optional remote); `--rules-worker` wires UDS remotes after allowlisted handshake; sends `drivechain_state` (tip + active slots + current ctips + pending M6ids + inclusion threshold) when tip DB is available.  
**Workers:** `cusf_rules_drivechain` (`validation=drivechain-ctip-m8-pending-m6-checks-on-wire` — allowlisted; independent M8 tip / multi-active-OP_DRIVECHAIN / current-ctip M5/M6 structure / pending M6id+votes / connect M7–M8 match; residual historical ctip + M1–M4 under Local dual-AND), `cusf_rules_bip360` (`validation=bip360-pqc-parents-and-chain-utxos-on-wire` — allowlisted).

## Target shape

```
bitcoind (untouched)
    │  ZMQ pubsequence + JSON-RPC / REST
    ▼
┌───────────────────────────────────────────────────┐
│  hub binary (today: bip300301_enforcer; name      │
│  cusf_hub is the architecture label only)         │
│  - ZMQ / mempool companion                        │
│  - invalidateblock on aggregate Reject            │
│  - GBT / Connect API (optional)                   │
│  - Worker registry + composition (AND)            │
└───────────┬───────────────────────────────────────┘
            │  IPC (UDS RUL1 frame / local Connect; stdin --once)
     ┌──────┴──────┬──────────────┐
     ▼             ▼              ▼
 cusf_rules_*   cusf_rules_*   cusf_rules_*
 (drivechain)   (bip360+PQC)   (future BIP)
```

| Process | Cargo features | Role |
|---------|----------------|------|
| **Hub** (`bip300301_enforcer` today) | Fat Local features until slim hub ships | Sole Core I/O; tip/mempool authority; registry; AND |
| **Rule worker** | That BIP’s features only | Pure policy: Accept / Reject (+ optional diff) |
| **BitWindow / UI** | n/a | Talks to Core + hub API — never a second tip veto |

Workers **never** open ZMQ sequence, never call `invalidateblock`, never own GBT.

## Registration and consent (normative)

Workers must **register** with the hub (coordinator). While registered, the hub **requires that worker’s consent** on every composition decision for rules that worker owns.

| Worker signal | Counts as | Hub logging |
|---------------|-----------|-------------|
| Explicit **Accept** (yes) | yes | normal |
| Explicit **Reject** (no) | no → aggregate **Reject** | normal (rule reason) |
| **No answer** in time (timeout / hang) | **no** | **error** |
| **Transport / process failure** (crash, disconnect mid-call) | **no** | **error** |
| **Not registered** | worker not in the set (does not vote) | info at config/load |
| **Deregister** | leave the set; no longer required | info |

**Fail-closed:** missing yes = no. Silence and failure are not soft-Accept.

Implications:

1. A registered bip360 worker that times out → hub rejects the tx/block (same as explicit reject for tip safety).
2. Operators who want “BIP360 not enforced” must **not register** that worker (or deregister it)—not leave a hung process registered.
3. Hub logs errors whenever consent is inferred as no due to timeout/failure (so outages are visible).

Composition default for all **registered** workers: **AND** (any no → Reject).

### Hub CLI (remote workers)

```bash
# Start workers first
cusf_rules_bip360 --uds /tmp/cusf-rules-bip360.sock --activation-height 0
cusf_rules_drivechain --uds /tmp/cusf-rules-drivechain.sock

# Hub: repeatable --rules-worker RULE_ID=PATH + shared timeout
bip300301_enforcer \
  --rules-worker bip360=/tmp/cusf-rules-bip360.sock \
  --rules-worker drivechain=/tmp/cusf-rules-drivechain.sock \
  --rules-worker-timeout-ms 5000 \
  ...
```

Supported rule ids (v0 static): `drivechain`, `bip360`.

**Capability negotiation:** Handshake/Health carry `validation`. Hub
`build_remote_backend_engine` **only registers** remotes after successful
handshake with an **exact allowlisted** token (`REAL_VALIDATION_CAPABILITIES` /
`is_real_validation_capability`). Stub, unknown spoof strings, and
**unreachable** workers are **not** registered (Local kept; log warn/error).
Fail-closed applies once a real remote is registered.

**Hot path:** `Validator::set_remote_rules` installs the engine used by
`BlockHandler` on every `validate_tx` / `connect_block`. Mempool: Local ballot
always + remote AND. Parent txs → `parent_txs_hex`. Connect always sends
`block_hex` + `chain_p2mr_utxos_hex` (empty map when no bip360 DB). Hub sends
`drivechain_state` (tip height/hash + active sidechain numbers + **pre-mutation**
current ctips + pending M6ids + inclusion threshold: mempool before Local M5/M6
apply; connect after coinbase / before `tx_diffs.apply`, `tip_hash` = prev tip
for M8) when tip DB is available. Post-apply ctips would false-Reject valid
deposits under dual-AND. SoftForkRule **connect** then folds ctips **and**
pending M6ids sequentially across block txs (mirror Local apply) so same-slot
chained M5/M6 in one block does not dual-AND brick. Hub still applies local
bip360 UTXO **diff** after Accept.

### Remote timeout (P5)

`RemoteRuleClient` exposes configurable `timeout` (default `DEFAULT_REMOTE_TIMEOUT` = 5s in `rules/ipc.rs`; hub flag `--rules-worker-timeout-ms`). **`0` is clamped to the default** (never infinite). When the join deadline is exceeded, the ballot is `VoteSource::Timeout` → **no**, logged at `tracing::error!` during aggregation.

**Dual deadline:** hub join wait **and** `UdsRuleTransport` UnixStream read/write timeouts (both set from the same operator value). Socket I/O timeout bounds hung peers; join deadline returns fail-closed immediately.

**Orphan helper + mid-read abort:** on join timeout the hub sets `cancel_requested`, calls **`RuleTransport::abort_inflight`** (UDS: generation-scoped active stream + `shutdown(Both)` so mid-read fails quickly), and spawns an **eager background reaper** so the next call is not blocked. Active abort handles are generation-scoped (orphaned clear cannot wipe a newer call). Ballot still fail-closed immediately as Timeout — never soft-Accept. Residual: worker-side mid-handler cancel (accept-loop only).

**Worker UDS shutdown:** library `serve_uds_loop` uses nonblocking `accept` + poll; worker bins install SIGTERM/SIGINT → `AtomicBool` and pass `Some(&flag)` via `ctrlc` with the **`termination` feature** (SIGINT alone is the crate default; without `termination`, `kill -TERM` is default-disposition exit 143 and skips socket unlink). Mid-flight request handling on the worker is still not cancelled mid-handler. **Process SIGTERM e2e (shipped, opt-in):** `just sigterm-rules-workers-e2e` / `./scripts/sigterm-rules-workers-e2e.sh` starts multiproc drivechain + bip360 `--uds`, Health ready handshake (idle accept), `kill -TERM`, asserts exit 0 within timeout and socket unlink. **Not** in `validate-rules-engine` / GH `check-rules-engine` (SCORE) — keeps PR green free of process-signal flakiness; residual is accept-poll exit latency (~`UDS_ACCEPT_POLL_MS`) and mid-handler cancel (accept-loop only). Hub process SIGTERM not covered (needs bitcoind; use live-tip cleanup paths).

**Worker auth residual (intentional):** UDS relies on filesystem permissions (`0600` best-effort); no mutual auth / MAC on the wire. Deploy only on trusted local hosts.

Operators disable a rule by deregistering — not by ignoring a hung worker.

Handshake must use `RULES_V1_VERSION` (currently `1`); version skew is a handshake failure (fail-closed until the worker is fixed or deregistered).

## Why not N full enforcers

| Approach | Problem |
|----------|---------|
| Each BIP = full enforcer with ZMQ | Tip races, double `invalidateblock` |
| One mega-binary, all features | Does not scale; PQC deps infect everything; N-way feature matrix |
| **Hub + feature-compiled workers** | One I/O owner; rebuild only the worker that changed; heavy deps stay in that worker |

## Wire protocol (hub ↔ worker) — rules.v1 (P2)

Frozen serde JSON schema in `lib/validator/rules/ipc.rs` (`RULES_V1_VERSION = 1`).

### UDS framing

```
[4-byte magic FRAME_MAGIC = b"RUL1"][4-byte BE body length][JSON body]
```

Body length capped at `MAX_IPC_BODY_BYTES` (16 MiB). Stdin `--once` uses raw JSON only (no frame).

| RPC | Purpose |
|-----|---------|
| `Handshake` | rule id + version (`RULES_V1_VERSION`) |
| `ValidateTx` | mempool path → Accept / Reject{reason}; optional `parent_txs_hex` |
| `ConnectBlock` | tip path → Accept / Reject |
| `Health` | liveness |

Transport: Local `SoftForkRule` in-process, or `RemoteRuleClient` over `RuleTransport` (`UdsRuleTransport` for production UDS; mock in unit tests). `RemoteRuleClient::timeout` is enforced with a join deadline → `VoteSource::Timeout` (structured `TransportError`, not string matching). On timeout the hub sets `cancel_requested`, calls **`RuleTransport::abort_inflight`** (UDS: shutdown active socket so mid-read fails quickly), and spawns an **eager background reaper** for the helper; ballot still fail-closed immediately as Timeout.

Worker bins:

* `--once` — one JSON request on stdin (body capped at `MAX_IPC_BODY_BYTES`)
* `--uds PATH` — accept framed connections on a Unix socket
* `--health` — capability token on stdout

`tx_hex` / `block_hex` are sent when present; missing payload → Reject. SoftForkRule has **no** production label soft-Accept (wire cannot bypass policy).

### Parent prevouts (`parent_txs_hex`)

`ValidateTx` may include `parent_txs_hex`: map of txid hex → consensus-serialized parent tx hex. Remote bip360 runs real `pqc::validate_mempool_transaction` with those parents. Absent/empty map → empty parents (active P2MR spends fail-closed without prevouts).

### Chain P2MR UTXOs (`chain_p2mr_utxos_hex`)

`ConnectBlock` may include `chain_p2mr_utxos_hex`: map of `txid:vout` → consensus-serialized TxOut hex. Remote bip360 SoftForkRule runs `pqc::validate_and_diff_block_transactions` with that set (diff not applied in worker). Absent → Reject `missing_chain_p2mr_utxos` (fail-closed). Hub still applies its local UTXO diff after Accept.

## Migration

| Phase | What | Status |
|-------|------|--------|
| **P0** | This doc; transitional multi-feature binaries | done |
| **P1** | In-process `SoftForkRule` + `RuleEngine`; drivechain/bip360 adapters; mempool **and connect** AND via ballots | done |
| **P2** | `Local` / `Remote` backends + rules.v1 schema + unit tests | done |
| **P3** | Worker packages `cusf_rules_*`; hub = enforcer app for now | bins + fail-closed + UDS |
| **P4** | Hub `--rules-worker` CLI, UDS loop, parent prevouts, validate multiproc smoke, hot-path remote cutover | done |
| **P4b** | Capability policy (stub ≠ consent), chain UTXOs on connect, nonblocking accept, GH validate-rules-engine | done |
| **P4 residual** | Historical `ctip_outpoint_to_value_seq` + M1–M4 DB-in-worker (Path B residual honesty — not full-wire practical); dynamic RuleId (accepted residual honesty — not unlocked) | open (accepted residual) |
| **Drivechain state on wire** | `drivechain_state` + `drivechain-ctip-m8-pending-m6-checks-on-wire`; independent M8 tip + multi-active-OP_DRIVECHAIN + current-ctip M5/M6 + pending M6id/votes + connect M7/M8; Local AND for historical/M1–M4 residual | done (Path A: ctips + pending M6) |
| **UDS mid-read abort** | `abort_inflight` + socket shutdown on join-timeout (+ eager reaper) | done |
| **Worker signal shutdown** | SIGTERM/SIGINT → AtomicBool in bins | done |
| **Bin SIGTERM process e2e** | `just sigterm-rules-workers-e2e` multiproc kill -TERM + socket unlink (outside SCORE) | done (opt-in) |
| **HITL live tip CI** | `workflow_dispatch` job only (not PR green matrix); SCORE lock tests pin no bitcoind in `check-rules-engine` | done (HITL); automatic PR live tip = residual |

### Builds

**Preferred (hub + workers):**

```bash
just build-hub-workers           # hub (fat) + cusf_rules_drivechain + cusf_rules_bip360
just validate-rules-engine       # unit tests + cargo check + UDS multiproc smoke (SCORE)
just sigterm-rules-workers-e2e   # opt-in: kill -TERM multiproc workers (not SCORE)
just rules-drivechain-health
just rules-bip360-health
```

**Transitional single-process products** (still available; **not** recommended multi-attach):

```bash
just build-enforcer-drivechain   # → bip300301_enforcer (drivechain only)
just build-enforcer-bip360       # → bip360_enforcer
just build-enforcer-combined     # → cusf_enforcer (both in one process)
just build-enforcers             # all three artifacts
```

Combined process today = in-process AND (preview of hub aggregation). Do **not** attach two full enforcer binaries to one bitcoind.

### Ballot composition (mempool + connect)

**Local always + remote AND (mempool dual-path):**

1. Feature-gated **Local** ballots **always** run when the feature is on (bip360/drivechain mempool cannot be sole-replaced by remote).
2. Remotes register only with allowlisted real `validation` after successful handshake.
3. Stub / unreachable / unknown capability → **not** registered (Local-only composition for those ids).
4. All local + remote ballots composed with `RuleEngine::decide` (**AND**): any non-consent → aggregate Reject.
5. Mempool remotes receive `tx_hex` + optional `parent_txs_hex` (key must match `compute_txid()`).
6. Connect: always send `block_hex` + `chain_p2mr_utxos_hex`; send `drivechain_state` when tip DB available; local bip360 UTXO **diff** always runs; Local connect ballots AND with remotes.

SoftForkRule shells never soft-Accept without required context (`missing_tx` / `missing_chain_p2mr_utxos` / `missing_drivechain_state`). Drivechain with state present: **Reject** M8 tip mismatch / multi-active-OP_DRIVECHAIN / current-ctip structural M5/M6 failures (old ctip unspent, treasury spent without replace, zero-diff, missing deposit address, bad M6 shape) / M6 missing pending or insufficient votes / connect M8 without matching coinbase M7; **Accept** plain and inactive OP_DRIVECHAIN (Local treats inactive as anyone-can-spend). Local dual-AND remains authority for residual: historical `ctip_outpoint_to_value_seq` (unbounded over chain life — not on wire), full M1–M4 proposal state.

### Slim-hub operator path (residual honesty)

| Mode | Drivechain Local | Drivechain remote | What is enforced |
|------|------------------|-------------------|------------------|
| **Fat hub (default)** | ON | optional allowlisted remote | Full Local BlockHandler M1–M8 **and** remote independent Path A snapshot checks |
| **Remote-only (slim)** | feature off / not registered Local | allowlisted remote only | Path A independent checks only — **not** full production M1–M8 authority (see dual table) |
| **Local-only** | ON | none | Full BlockHandler; no remote ballot |

### Dual residual honesty — remote SoftForkRule can vs cannot

Independent checks require only `drivechain_state` (Path A) or block-local coinbase data. Residual paths stay Local dual-AND by design (Path B) — remote **Accept** on those markers is intentional, not a soft-Accept of incomplete required context.

| BIP 300/301 check | Remote SoftForkRule (allowlisted) | Authority under dual-AND |
|-------------------|-----------------------------------|--------------------------|
| Missing `drivechain_state` / missing tx/block | **Reject** `missing_*` (fail-closed) | same |
| M8 `prev_mainchain_block_hash` vs tip | **Reject** independently (Path A) | Local also enforces |
| Multi OP_DRIVECHAIN same *active* slot | **Reject** independently (Path A) | Local also enforces |
| Current-ctip structural M5/M6 (spend/replace, zero-diff, deposit address, M6 shape) | **Reject** independently (Path A `ctips`) | Local also enforces |
| Pending M6id match + vote threshold | **Reject** independently (Path A `pending_m6ids` + threshold) | Local also enforces |
| Connect M7 → accepted_bmm M8 | **Reject** independently (block-local M7) | Local also enforces |
| Sequential same-slot chained M5/M6 in one block | **Accept** when fold ok (Path A sequential ctip+pending fold; no dual-AND brick) | Local apply order mirrored |
| Inactive-slot OP_DRIVECHAIN | **Accept** (mainchain anyone-can-spend) | Local ignores as CTIP |
| Historical `ctip_outpoint_to_value_seq` spends (non-current treasury outpoints) | **Path B residual Accept** — full map **not** on wire | **Local** Reject authority |
| M1–M4 coinbase proposal / ack / bundle DB | **Path B residual Accept** — proposal/ack DB **not** on wire | **Local** Reject authority |

**Path A shipped (bounded wire):** `tip_height` / `tip_hash_hex` + `active_sidechain_numbers` + current `ctips` (≤1 per active slot) + `pending_m6ids` (active proposals only) + `withdrawal_bundle_inclusion_threshold` + connect M7 from coinbase. Capability: `drivechain-ctip-m8-pending-m6-checks-on-wire` (allowlisted). Legacy tokens (`drivechain-ctip-m8-checks-on-wire`, `drivechain-m8-tip-check-on-wire`, `drivechain-tip-and-sidechains-on-wire`) **not** allowlisted.

**Path B residual decision (this loop — no dishonest extract):**

| Residual surface | Why not Path A wire extract |
|------------------|-----------------------------|
| Historical `ctip_outpoint_to_value_seq` | Unbounded over chain life (every past treasury outpoint). Full map is not practical on every ValidateTx/ConnectBlock. Cap/pagination would omit rows → remote would soft-Accept Local-Reject historical spends (dishonest independent authority) or require a new capability that still cannot claim full M5–M8. Fat-hub dual-AND already covers via Local. |
| M1–M4 proposal/ack DB | Needs live proposal windows, ack scores, pending-bundle maps, prior M4 upvotes, activation thresholds. Partial extract dual-AND-unsafe (hard-Reject Local-Accept or soft-Accept Local-Reject). No bounded correct subset for independent SoftForkRule that matches Local without near-full LMDB. |

Do **not** invent partial historical/M1–M4 SoftForkRule Rejects that soft-Accept invalid Local-Reject paths or hard-Reject Local-Accept paths. Capability token stays `drivechain-ctip-m8-pending-m6-checks-on-wire` (no wire expansion → no rename this loop).

**Dynamic `RuleId` (accepted residual honesty — not unlocked this loop):**

| Surface | Lock (current) |
|---------|----------------|
| Type | `RuleId = &'static str` only — no `String` / freeform owned ids |
| CLI / endpoint mapping | `ipc::static_rule_id` allowlists **only** `drivechain` / `bip360` |
| Freeform / dynamic names | Fail mapping at parse (`RulesWorkerEndpoint::parse` / `--rules-worker`); **no** freeform registration path |
| Tests | `static_rule_id_allowlist_only_drivechain_bip360`, `rules_worker_endpoint_rejects_freeform_dynamic_rule_ids` |
| Unlock | Requires product decision + type/registration change — do **not** invent this loop |

**PR CI live tip residual honesty (HITL only — not unlocked for automatic PR):**

| Surface | Lock (current) |
|---------|----------------|
| SCORE / PR green | `just validate-rules-engine` + GH job `check-rules-engine` — **no** bitcoind, **no** live tip, **no** SIGTERM e2e |
| Opt-in script | `CUSF_LIVE_TIP_E2E=1 BITCOIND=… just live-tip-rules-workers-e2e` (env unset → soft-skip 0; env set + missing prereqs → fail-closed non-zero) |
| HITL CI | `workflow_dispatch` input `run_live_tip_e2e` on `check_lint_build_release.yaml` job `live-tip-rules-workers-e2e-hitl` only |
| Automatic PR | **Does not** run live tip — `pull_request` / `push` keep green matrix free of bitcoind |
| Tests | `pr_ci_live_tip_residual_honesty_score_lock` (workflow + validate script + Justfile), `shutdown_residual_honesty_docs_inventory_lock` |
| Unlock automatic PR live tip | Requires product decision + bitcoind in PR matrix — do **not** invent this loop |

**Shutdown residual honesty (prefer docs over SCORE-risky invent):** worker mid-handler cancel remains residual (accept-loop only; mid-handler cancel is large / dual-AND-risky / can race SCORE). Hub process SIGTERM e2e needs bitcoind (not invented). Worker bin SIGTERM multiproc e2e is **shipped opt-in** (`just sigterm-rules-workers-e2e`, outside SCORE).

## Operator summary

1. Run **one hub** per bitcoind (`bip300301_enforcer` today).  
2. List workers with `--rules-worker RULE_ID=PATH` or use fat Local features.  
3. Only allowlisted healthy remotes join consent; silence after register = **no**. Stub/unreachable = Local kept (safe to list a down socket).  
4. Drivechain worker is allowlisted (`drivechain-ctip-m8-pending-m6-checks-on-wire`); bip360 allowlisted. Registering drivechain remote is safe under dual-AND (remote may independent-Reject invalid M8/multi-DC/current-ctip structure/pending M6/connect M7–M8; Local enforces Path B residual historical-ctip + M1–M4 full DB). Slim-hub remote-only is **not** full M1–M8 authority — see dual residual honesty table.  
5. Omit CLI flag to stop requiring a remote.  
6. CI: `just validate-rules-engine` (GH job `check-rules-engine` — **no** bitcoind/live tip/SIGTERM e2e). Optional: `just smoke-rules-workers-tip`. Opt-in SIGTERM multiproc: `just sigterm-rules-workers-e2e` (not SCORE). Opt-in live tip: `CUSF_LIVE_TIP_E2E=1 BITCOIND=… just live-tip-rules-workers-e2e` (fail-closed when env set but prereqs missing; soft-skip exit 0 only when env unset). **HITL CI:** `workflow_dispatch` input `run_live_tip_e2e` on `check_lint_build_release.yaml` (manual; does not break PR green matrix).

## See also

- Plan: session plan “Central CUSF hub + modular rule worker binaries”  
- BIP 360 rules: [`CUSF-BIP360.md`](./CUSF-BIP360.md)  
- Mempool vs tip: [`MEMPOOL_RELAY_POLICY.md`](./MEMPOOL_RELAY_POLICY.md)  
- Features: [`AGENTS.md`](../AGENTS.md), [`README.md`](../README.md)  
