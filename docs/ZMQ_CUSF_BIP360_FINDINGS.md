# Bitcoin Core 31 ZMQ surface vs CUSF BIP 360 enforcement

**Date:** 2026-07-15 (revised after review)  
**Scope:** Stock Bitcoin Core **v31.0.0** ZMQ/RPC capabilities and whether they can enable full CUSF BIP 360 (P2MR) enforcement—including witness-v2 **spends**—on **unmodified** Core with **no configuration override flags** (`-acceptnonstdtxn`, `-acceptunknownwitness`, Knots-style `ignore_rejects`, binary patches, etc.).

### How to read this document

| Section | Purpose |
|---------|---------|
| **§1** | Complete stock ZMQ inventory (options, frames, topics, RPC, security notes) |
| **§2** | Major answer: can ZMQ ± RPC enable BIP 360 on stock Core? |
| **§3** | Combinatorial strategies + complex operator playbooks |
| **§4** | CUSF architecture map (`cusf-enforcer-mempool` + enforcer) |
| **§5** | Related policy flags (in/out of scope) |
| **§6** | Security / trust model for the recommended stack |
| **§7** | Practical recommendations |
| **§8** | Reproduce / re-verify commands |
| **§9** | Evidence of diligence and limits |

**Short answer (see §2 for rigor):** **No.** ZMQ is a write-only publish facility from Core’s perspective. Notifications fire only after Core has already accepted a tx into the mempool or a block into the chain view. Witness-v2 P2MR **spends** fail stock mempool admission in **two stages**—under defaults via `AreInputsStandard` (`WITNESS_UNKNOWN` prevouts), and even when standardness is relaxed via `SCRIPT_VERIFY_DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM`—and therefore never produce ZMQ mempool `A` / `hashtx` / `rawtx` events. ZMQ + JSON-RPC can still drive the existing **connect_block → invalidateblock** path for txs that enter via `submitblock` / P2P blocks, but that path is **fail-open** if the unauthenticated ZMQ control plane is suppressed or the companion dies. They cannot create a stock-Core **mempool** path for those spends without policy overrides or a patched Core.

---

## 1. Complete inventory of Core 31 ZMQ options / topics

### 1.1 Source and binary evidence

Line numbers in code fences below are for the **local Surmount/Knots-line tree** (sibling checkout typically `../bitcoin` relative to this workspace; treat as `$BITCOIN_SRC`). They were verified there. Stock **v31.0 behavior** was checked against the stock binary under `bip300301_enforcer/.integration-deps/bitcoin-stock-31.0/bitcoind` and against the five stock PUB topics / `getzmqnotifications` symbols. Stock **source** line numbers on `bitcoin/bitcoin@v31.0` often differ by **large offsets** (hundreds of lines in `validation.cpp` / `init.cpp`); when auditing upstream tags, prefer **symbol/function names** over local line numbers. Upstream reference: <https://github.com/bitcoin/bitcoin/tree/v31.0>.

| Artifact | Location (portable) | Role |
|----------|---------------------|------|
| Stock bitcoind | `bip300301_enforcer/.integration-deps/bitcoin-stock-31.0/bitcoind` | **v31.0.0** — authoritative option strings |
| Local Core-line tree | `$BITCOIN_SRC` (Surmount/Knots-line) | Readable ZMQ sources; **extra wallet topics** not in stock 31 |
| ZMQ docs | `$BITCOIN_SRC/doc/zmq.md` (local) / stock `doc/zmq.md` @ v31.0 | Multipart layout, sequence labels; see §1.4 for hash-order correction vs local Knots wording |
| Arg registration | `$BITCOIN_SRC/src/init.cpp` | `-zmqpub*` registration |
| Notification interface | `$BITCOIN_SRC/src/zmq/zmqnotificationinterface.{h,cpp}` | Validation signal hooks |
| Publish notifiers | `$BITCOIN_SRC/src/zmq/zmqpublishnotifier.{h,cpp}` | Topic strings, body encoding |
| Abstract notifier | `$BITCOIN_SRC/src/zmq/zmqabstractnotifier.{h,cpp}` | `DEFAULT_ZMQ_SNDHWM = 1000` |
| ZMQ RPC | `$BITCOIN_SRC/src/zmq/zmqrpc.cpp` | `getzmqnotifications` |

**Closed-world claim (stock 31):** the ten flags in §1.2 are the **only** `-zmq*` CLI options exposed by stock `bitcoind -help` on the integration binary (no wallet ZMQ; no other `-zmqpub*` names).

### 1.2 Every `-zmqpub*` option (stock Core 31)

| Option | Purpose | Default HWM |
|--------|---------|-------------|
| `-zmqpubhashblock=<address>` | Publish block tip hash | 1000 |
| `-zmqpubhashtx=<address>` | Publish **txid** (not wtxid) on mempool admit + block inclusion | 1000 |
| `-zmqpubrawblock=<address>` | Publish full serialized block at tip update | 1000 |
| `-zmqpubrawtx=<address>` | Publish full serialized tx **with witness** (mempool + block) | 1000 |
| `-zmqpubsequence=<address>` | Publish mempool A/R + block connect/disconnect sequence | 1000 |
| `-zmqpubhashblockhwm=<n>` | SNDHWM for hashblock | — |
| `-zmqpubhashtxhwm=<n>` | SNDHWM for hashtx | — |
| `-zmqpubrawblockhwm=<n>` | SNDHWM for rawblock | — |
| `-zmqpubrawtxhwm=<n>` | SNDHWM for rawtx | — |
| `-zmqpubsequencehwm=<n>` | SNDHWM for sequence | — |

Re-verify (stock binary):

```bash
STOCK=bip300301_enforcer/.integration-deps/bitcoin-stock-31.0/bitcoind
# Help lines are indented (two spaces); do not use '^-zmq' alone — it matches zero lines.
"$STOCK" -help 2>&1 | grep -E -- '-zmqpub'
# Expect exactly 10 lines, e.g.:
#   -zmqpubhashblock=<address>
#   -zmqpubhashblockhwm=<n>
#   … (hashtx, rawblock, rawtx, sequence + *hwm)
```

**Stock registration** (names only; stock v31.0 `init.cpp` registers these five topics + five HWM options—**no** wallet entries). Local Knots-line `$BITCOIN_SRC/src/init.cpp` also registers wallet options (see §1.2.1). Illustrative local non-wallet `AddArg` names (local line anchors; stock ~617–626 on `bitcoin/bitcoin@v31.0`):

- `-zmqpubhashblock`, `-zmqpubhashtx`, `-zmqpubrawblock`, `-zmqpubrawtx`, `-zmqpubsequence`
- `-zmqpubhashblockhwm`, `-zmqpubhashtxhwm`, `-zmqpubrawblockhwm`, `-zmqpubrawtxhwm`, `-zmqpubsequencehwm`

Factory map for **stock-relevant** notifiers (`CZMQNotificationInterface::Create` in `zmqnotificationinterface.cpp`; local body ~52–60 includes wallet factories that stock 31 does **not** register via CLI):

| Factory key | Notifier class | Stock 31 CLI? |
|-------------|----------------|---------------|
| `pubhashblock` | `CZMQPublishHashBlockNotifier` | Yes |
| `pubhashtx` | `CZMQPublishHashTransactionNotifier` | Yes |
| `pubrawblock` | `CZMQPublishRawBlockNotifier` | Yes |
| `pubrawtx` | `CZMQPublishRawTransactionNotifier` | Yes |
| `pubsequence` | `CZMQPublishSequenceNotifier` | Yes |
| `pubhashwallettx` | `CZMQPublishHashWalletTransactionNotifier` | **No** (Knots/local) |
| `pubrawwallettx` | `CZMQPublishRawWalletTransactionNotifier` | **No** (Knots/local) |

#### 1.2.1 Knots / local-only wallet ZMQ extras (not stock 31)

| Option | Topic string(s) |
|--------|-----------------|
| `-zmqpubhashwallettx=<address>` | `hashwallettx-mempool` / `hashwallettx-block` |
| `-zmqpubrawwallettx=<address>` | `rawwallettx-mempool` / `rawwallettx-block` |
| `-zmqpubhashwallettxhwm=<n>` | HWM for hash wallet topics |
| `-zmqpubrawwallettxhwm=<n>` | HWM for raw wallet topics |

### 1.3 Multipart frame layout (all PUB notifiers)

Every publish path uses three ZMQ frames via `SendZmqMessage` (local `$BITCOIN_SRC/src/zmq/zmqpublishnotifier.cpp`, function body ~207–221):

| Frame | Content |
|-------|---------|
| 0 | Topic / command string (no null terminator), e.g. `sequence`, `hashtx` |
| 1 | Payload (topic-specific; see below) |
| 2 | Per-notifier **uint32 message sequence**, **little-endian** (loss detection only—not authentication) |

Socket type is **ZMQ_PUB** only. Same address may be shared by multiple notifiers (socket reuse). TCP keepalive and conditional IPv6 are set at bind. Core may bind multiple addresses for the same notification; subscribers use `ZMQ_SUBSCRIBE` with a topic prefix.

**HWM semantics:** each option’s `*hwm` sets `ZMQ_SNDHWM` (default **1000**). When the high-water mark is exceeded, ZMQ **silently drops** outbound messages. For CUSF, a dropped `sequence` `'C'` is an **enforcement outage** (fail-open), not a soft observability glitch—see §6.

### 1.4 Topic strings, payloads, hash byte order

Topic constants live in `zmqpublishnotifier.cpp` (~43–51 local).

| Topic string | CLI option | Frame-1 body | Hash byte order on wire |
|--------------|------------|--------------|-------------------------|
| `hashblock` | `-zmqpubhashblock` | 32-byte block hash | **Display / RPC hex order** (internal `uint256` bytes reversed once) |
| `hashtx` | `-zmqpubhashtx` | 32-byte **txid** (from `GetHash()`, **not** wtxid) | Same display/RPC order |
| `rawblock` | `-zmqpubrawblock` | Serialized block bytes | N/A (**sensitive plaintext**—§6.3) |
| `rawtx` | `-zmqpubrawtx` | Serialized tx with witness (`TX_WITH_WITNESS`) | N/A (**sensitive plaintext**) |
| `sequence` | `-zmqpubsequence` | See §1.5 | Hash portion in display/RPC order |
| `hashwallettx-*` / `rawwallettx-*` | Knots/local only | As above for wallet txs | Same hash order / full witness tx |

Hash publish loop (hashtx; same reverse for hashblock and sequence hash prefix)—local ~235–243:

```cpp
// Conceptual: wire[i] = internal[31 - i]
// Result matches hex-decode of RPC / explorer hash strings.
// bytes(ZMQ hash) == unhexlify(rpc_txid)
```

**Correct wire-order rule (implementers):**

- Core stores `uint256` in internal `m_data` order.
- RPC `GetHex()` / explorer hex = reverse of internal bytes, then hex-encode.
- ZMQ does `data[31 - i] = hash.begin()[i]` → **same bytes as unhexlify(RPC hash string)**.
- Integers that **are** little-endian on the wire: frame-2 ZMQ sequence (`WriteLE32`) and sequence-topic mempool_seq (`WriteLE64`).

**Do not** call the hash payload “little-endian relative to RPC.” Local Knots-line `doc/zmq.md` sometimes says “Little Endian and not … RPC”; that wording is **inconsistent** with the reverse loop, Core functional tests (`hashtx` hex == `payment_txid`), and `cusf-enforcer-mempool` parsing. Prefer stock v31.0 framing: **display/RPC order on the wire**.

**Enforcer parse:** `cusf-enforcer-mempool/lib/zmq.rs` does `hash.reverse()` then `Txid::from_byte_array` / `BlockHash::from_byte_array` because rust-bitcoin expects **internal** order. That reverse is “display-order wire → internal,” **not** “undo LE-vs-RPC.”

### 1.5 `sequence` topic detail

Body layout (`SendSequenceMsg`, local function body ~287–295):

| Event | Label | Body size | Layout |
|-------|-------|-----------|--------|
| Block connected | `'C'` | 33 | `<32-byte display-order hash> C` |
| Block disconnected | `'D'` | 33 | `<32-byte display-order hash> D` |
| Mempool acceptance | `'A'` | 41 | `<32-byte display-order hash> A <8-byte LE mempool_seq>` |
| Mempool removal (non-block) | `'R'` | 41 | `<32-byte display-order hash> R <8-byte LE mempool_seq>` |

Plus frame 2: ZMQ uint32 sequence (little-endian; distinct from mempool sequence).

### 1.6 When each notification fires (validation hooks)

Hook table below is a **local-tree illustration** (`zmqnotificationinterface.cpp`). Stock v31.0 behavior matches; stock source line numbers differ. Prefer function names + stock `doc/zmq.md` for stock-facing audits.

| Hook | Notifiers | Does **not** fire for |
|------|-----------|------------------------|
| `UpdatedBlockTip` | `NotifyBlock` → `hashblock` / `rawblock` | IBD; `pindexNew == pindexFork` (disconnect-only). `*block` topics only observe **active tip**—so they are **not issued** for assumeutxo **background** historical connects (tip-only design; see stock `doc/zmq.md`), even though this hook itself does not name background chainstate |
| `TransactionAddedToMempool` | `NotifyTransaction` + `NotifyTransactionAcceptance` | **Rejected** admissions; txs never in mempool |
| `TransactionRemovedFromMempool` | `NotifyTransactionRemoval` only | Removals **due to block inclusion** (non-block reasons only) |
| `BlockConnected` | Per-tx `NotifyTransaction`; then `NotifyBlockConnect` | `ChainstateRole::BACKGROUND` blocks (explicit early return) |
| `BlockDisconnected` | Per-tx `NotifyTransaction`; then `NotifyBlockDisconnect` | — |
| Wallet `TransactionAddedToWallet` | Wallet hash/raw (Knots/local CLI only) | Non-wallet txs; stock 31 has no wallet ZMQ CLI |

`hashtx` / `rawtx` fire on: (1) mempool acceptance, and (2) every tx in a connected or disconnected block (multiple publishes possible).

### 1.7 Effects on relay / mempool / consensus / CUSF

| Mechanism | Core relay? | Core mempool? | Core consensus? | Effect when CUSF companion + RPC attached |
|-----------|-------------|---------------|-----------------|-------------------------------------------|
| All `-zmqpub*` | **No** | **No** | **No** | **Drives** enforcer scheduling: may lead to `accept_tx`, `prioritisetransaction`, `invalidateblock`; leaks metadata if exposed |
| `getzmqnotifications` | No | No | No | Read-only discovery of active PUB endpoints |

From Core `doc/zmq.md`: the ZeroMQ socket is **write-only** from bitcoind’s perspective; PUB introduces no Core state. That does **not** mean ZMQ is operationally inert for CUSF—see §6.

Init registration: `CZMQNotificationInterface::Create` then `RegisterValidationInterface` (local ~2196–2204; stock ~1812–1823 on v31.0).

### 1.8 ZMQ RPC: `getzmqnotifications`

**Only** ZMQ-related JSON-RPC on Core (`RegisterZMQRPCCommands` / `zmqrpc.cpp`).

| Field | Type | Meaning |
|-------|------|---------|
| `type` | string | Notifier type, e.g. `pubsequence` |
| `address` | string | Bind address configured for that notifier |
| `hwm` | number | Outbound SNDHWM |

Example after enabling sequence on loopback:

```bash
bitcoin-cli getzmqnotifications
# [
#   { "type": "pubsequence", "address": "tcp://127.0.0.1:29000", "hwm": 1000 }
# ]
```

Empty array if no `-zmqpub*` options are active (or ZMQ not compiled in—stock 31 integration binary includes ZMQ).

### 1.9 Subscriber notes (operational protocol)

1. **txid vs wtxid:** hash topics publish **txid** (`GetHash()`), not wtxid.  
2. **HWM drops:** silent at PUB; detect via frame-2 sequence gaps (enforcer hard-errors on gap—fail-open for CUSF).  
3. **Multi-bind / multi-subscribe:** Core allows multiple addresses per notification; SUB must set `ZMQ_SUBSCRIBE` (prefix match).  
4. **No authentication / no encryption** on Core ZMQ (no CURVE in Core’s ZMQ path).  
5. **Bind vs connect:** Core **binds** the PUB address; the enforcer **connects** as SUB.  
   - Safe: Core `-zmqpubsequence=tcp://127.0.0.1:29000`; enforcer `--node-zmq-addr-sequence=tcp://127.0.0.1:29000`.  
   - Avoid `0.0.0.0` for either role in production (bind-any exposure / connect confusion).  
6. **Raw topics:** prefer `sequence` + RPC body fetch for CUSF; treat `rawtx` / `rawblock` as sensitive plaintext.

---

## 2. Major question — can ZMQ (± JSON-RPC) enable CUSF BIP 360 on stock Core?

### 2.1 Answer (clear)

| Capability | ZMQ alone | ZMQ + stock JSON-RPC, **no** policy override flags |
|------------|-----------|-----------------------------------------------------|
| Observe Core tip / mempool **after** Core accepts | Yes | Yes |
| Enforce BIP 360 on **confirmed** blocks (`connect_block` → `invalidateblock`) | Indirect (need RPC for block body + invalidate) | **Yes, with caveats** — today’s CUSF design; **unauthenticated ZMQ is the trigger plane**; fail-open on stream loss (§6) |
| Admit witness-v2 P2MR **spends** into Core mempool | **No** | **No** |
| Drive Core to relay BIP 360 spends on the P2P **mempool** path | **No** | **No** |
| Core’s own `getblocktemplate` includes BIP 360 spends | **No** | **No** (Core never mempooled them) |
| Enforcer-served GBT includes BIP 360 spends | N/A | **Only if** spend entered the enforcer shadow mempool via Core `'A'` **or** was attached offline / producer prefix (cooperative mining)—**not** via Core mempool admission of spends |
| Full mempool-path BIP 360 enforcement for P2MR spends | **No** | **No** without Core policy/consensus changes |

**Conclusion:** ZMQ ± RPC cannot enable **full** CUSF BIP 360 **mempool-path** enforcement for P2MR spends on unmodified stock Core without policy overrides that relax standardness and upgradable-witness script policy. The **block** path remains the viable functional model, subject to the security/availability model in §6.

### 2.2 Why: order of operations is irreversible via notify

```
Peer/RPC submits tx
    → Core PreChecks: if require_standard, IsStandardTx + AreInputsStandard + IsWitnessStandard
    → PolicyScriptChecks: STANDARD_SCRIPT_VERIFY_FLAGS (includes DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM)
    → ONLY if accepted: TransactionAddedToMempool → ZMQ hashtx/rawtx/sequence 'A'
    → Enforcer may then accept_tx / prioritise-evict
```

ZMQ subscribers never see **rejected** candidates. There is no pre-mempool ZMQ topic.

### 2.3 Funding vs spending (witness v2 / P2MR)

P2MR outputs use witness **version 2**, 32-byte program (`OP_2 OP_PUSHBYTES_32 <merkle_root>` → hex prefix `5220…`). Solver classifies this as `TxoutType::WITNESS_UNKNOWN` (`script/solver.cpp`).

#### Creating / funding a v2 output (stock defaults, no overrides)

- Creating an output does **not** execute the unknown witness program.
- On **stock Core 31**, pure `WITNESS_UNKNOWN` **outputs** are treated as standard `IsStandard` shapes (stock has **no** `-acceptunknownwitness` / `scriptpubkey-unknown-witnessversion` gate in help or binary surface). Large packages may still fail other limits (size, fees, etc.).
- **Evidence for no-override funding:** stock policy / solver / `IsStandard` behavior above—not the dual-node harness alone.
- **Secondary observation:** dual-node / regtest harnesses also see funding enter mempool, but those nodes are started with **`-acceptnonstdtxn`** (`integration_tests/util.rs`)—an **out-of-scope override**, cited only as secondary confirmation, not as proof under this document’s constraint.

#### Spending a v2 output — **two-stage** mempool gate

**Stage 1 — default stock policy (`require_standard=true`):**  
`AreInputsStandard` rejects prevouts of type `WITNESS_UNKNOWN` early (PreChecks, before script verification). **No ZMQ `'A'`.**

| Tree | Typical stage-1 reject reason string |
|------|--------------------------------------|
| **Stock Core v31.0 (primary for this doc)** | **`bad-txns-nonstandard-inputs`** when `AreInputsStandard` fails (fixed PreChecks reason; stock binary has no `witness-unknown` string) |
| Knots / local tree (secondary) | Often prefixed form such as `bad-txns-input-witness-unknown` (`AreInputsStandard` `MaybeReject("witness-unknown")` under a `bad-txns-input-` prefix) |

Prefer the **stock** string when re-verifying with the integration `bitcoind` v31.0.0.

**Stage 2 — even if standardness is relaxed** (e.g. `-acceptnonstdtxn` on regtest, which only flips `require_standard` via mempool args—**out of scope** for deployment claims, but useful as a residual gate):  
`MemPoolAccept::PolicyScriptChecks` still applies script flags that include `SCRIPT_VERIFY_DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM`.

- **Stock Core v31.0 shape:** flags are effectively fixed to `STANDARD_SCRIPT_VERIFY_FLAGS` (no `ignore_rejects` strip path in stock).  
- **Local Knots-line shape:** `PolicyScriptVerifyFlags(ignore_rejects)` can strip upgradable-witness discourage flags when configured—**non-stock**, excluded by scope.

`VerifyWitnessProgram` for non-v0 / non-v1-taproot / non-anchor programs:

```text
if (flags & SCRIPT_VERIFY_DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM)
    → SCRIPT_ERR_DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM
else
    → success (soft-fork placeholder)
```

**Harness quote** (`integration_tests/bip360_dual_node.rs`, spend helper)—**out-of-scope override, cited only to show it is still insufficient for spends:**

> Spending a witness-v2 (P2MR) output trips Core's `DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM` mempool script flag **even with `-acceptnonstdtxn`**. Funding (paying *into* v2) still works via mempool. Spend rounds therefore use GBT coinbase + spend assembly and **`submitblock`**.

#### Block / consensus path

`GetBlockScriptFlags` sets P2SH / WITNESS / TAPROOT and deployment flags (DERSIG, CLTV, CSV, NULLDUMMY, …). It does **not** add `SCRIPT_VERIFY_DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM`. Unknown witness versions therefore **succeed as soft-fork placeholders** at block connect. Core accepts blocks containing P2MR spends **without** BIP 360 semantics—why CUSF `connect_block` + `invalidateblock` works.

#### Reconciliation with [`MEMPOOL_RELAY_POLICY.md`](./MEMPOOL_RELAY_POLICY.md)

Older wording there that **funding** is “typically rejected” for “output type” is **overstated for stock Core 31**. Correct split:

| Tx kind | Stock Core 31 default mempool | Notes |
|---------|--------------------------------|-------|
| **Funding** (pay *into* v2 `5220…`) | Generally **admissible** as `WITNESS_UNKNOWN` output | Still subject to fee/size/other policy; enforcer may add CUSF checks via `accept_tx` when Core admits |
| **Spend** (spend v2 prevout) | **Rejected** (stage 1 and/or stage 2) | No ZMQ mempool events; no Core relay |
| Large PQC packages | May fail weight / standardness limits independently | Orthogonal to v2 version gate |

### 2.4 Flag dataflow (checkable chain)

```
-acceptnonstdtxn (out of scope)
    → mempool_opts.require_standard = !flag   (node/mempool_args.cpp)
    → if require_standard: IsStandardTx / AreInputsStandard / IsWitnessStandard in PreChecks

Always (stock defaults; independent of require_standard once past PreChecks):
    PolicyScriptChecks
    → script flags = STANDARD_SCRIPT_VERIFY_FLAGS
         (includes SCRIPT_VERIFY_DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM)
    → VerifyWitnessProgram else-branch for v2 spends
```

Expected experimental oracles (sample shapes; exact `reject-reason` text can wrap under `mandatory-script-verify-flag-failed` / `non-mandatory-script-verify-flag` depending on path and Core version):

| Case | Primary oracle (stock 31) | Notes |
|------|---------------------------|-------|
| Default spend of v2 | **`bad-txns-nonstandard-inputs`** | Stage 1 / `AreInputsStandard`. Knots-local may instead show `bad-txns-input-witness-unknown`. |
| Spend with standardness relaxed (`-acceptnonstdtxn`, out of scope) | Script-verify failure mentioning **upgradable witness program** / discourage | Stage 2 / `DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM` |
| Funding pure v2 output | Often **accepted** (`"allowed": true`) on stock defaults | Not a reject oracle |

Abbreviated `testmempoolaccept` shapes (illustrative—not a captured live log):

```json
// Stage 1 (stock defaults): spend of confirmed v2 prevout
[{ "txid": "…", "allowed": false, "reject-reason": "bad-txns-nonstandard-inputs" }]

// Stage 2 (regtest with -acceptnonstdtxn only): same spend class
// reject-reason typically mentions non-mandatory/mandatory script-verify and
// upgradable witness program / discourage (exact wrapping varies by Core build)
[{ "txid": "…", "allowed": false, "reject-reason": "non-mandatory-script-verify-flag (…upgradable witness…)" }]
```

### 2.5 Rigorous restatement

1. ZMQ is **outbound-only** (PUB); it cannot rewrite Core policy or push txs into Core.  
2. Events are **post-acceptance** only.  
3. BIP 360 spend validity is not Core’s default mempool validity (two-stage reject).  
4. An external enforcer using only stock interfaces can **filter / invalidate after the fact**, and maintain a **parallel** mempool for mining templates, for txs Core stored or that the enforcer obtained out-of-band and included via **block submission**.  
5. ZMQ integrity is a **control-plane** dependency of CUSF side effects, not mere observability (§6).

---

## 3. Combinatorial strategies (stock interfaces only)

Assumptions for every strategy:

- Unmodified stock Core 31 binary  
- No `-acceptnonstdtxn`, `-acceptunknownwitness`, Knots `ignore_rejects`, or similar  
- Allowed: ZMQ topics above, standard JSON-RPC, optional second stock node, enforcer process  
- RPC/ZMQ isolated to loopback or private IPC (§6)

### 3.1 Sequence A/R + `getrawtransaction` + enforcer `accept_tx`  
**(current `cusf-enforcer-mempool` design)**

| Step | Mechanism |
|------|-----------|
| Subscribe | `-zmqpubsequence` → enforcer `subscribe_sequence` |
| On `'A'` | `getrawtransaction` (+ parent fee resolution) → `CusfEnforcer::accept_tx` |
| On reject | `prioritisetransaction` with large negative fee delta |
| On `'C'`/`'D'` | `getblock` → `connect_block` / `disconnect_block`; reject → `invalidateblock` |

| Works | Fails for BIP 360 spends | Residual risk |
|-------|--------------------------|---------------|
| Filters **Core-admitted** txs (funding, ordinary txs, drivechain M8, …) | Spends never generate `'A'`; never reach `accept_tx` via ZMQ | ZMQ unauthenticated trigger; gap/HWM → **CUSF fail-open**; forged `'A'` + `txindex` can still `getrawtransaction` historical txs and drive prioritise/shadow insert (**today’s companion does not re-check mempool membership**—§4.1/§6.4 recommend `getmempoolentry` as hardening, not current behavior); `prioritisetransaction` **cannot** admit a never-accepted spend |

### 3.2 `rawtx` / `hashtx` + dual nodes

| Variant | Works | Fails | Residual risk |
|---------|-------|-------|---------------|
| rawtx alone | Full body without txindex | No A/R/C/D semantics; removal vs block-inclusion ambiguous | Raw plaintext leak if non-loopback |
| hashtx/rawtx without sequence | Observe some admits/blocks | Lose mempool sequence ordering and connect/disconnect labels | Incomplete reorg handling |
| Dual stock nodes, ZMQ on both | Dual-tip observation for **blocks** and ordinary txs | Neither admits v2 spends to mempool | More PUB endpoints to isolate |
| Dual nodes + submitblock spend + P2P block | **Block path works** (dual-node spend rounds) | No mempool spend path | Reorg races; both enforcers should enforce |

### 3.3 `submitblock` / `getblocktemplate` / `invalidateblock`

| Path | Works | Fails | Residual risk |
|------|-------|-------|---------------|
| Enforcer GBT + offline assembly + `submitblock` | **Primary working path** for valid spends | Must not rely on Core GBT for spends | High-privilege mining APIs—authenticate & loopback only |
| Core GBT only | Ordinary Core-mempool txs | No P2MR spends in template | Accidental non-CUSF mining |
| Reactive `invalidateblock` only (no enforcer GBT) | Can still reject bad tips after connect | Purely reactive; does not help produce valid BIP 360 blocks | Fail-open if ZMQ `'C'` missed |
| `invalidateblock` then `reconsiderblock` | Recovery / policy flip experiments | Tip thrash if enforcer and operators disagree | Privilege to reorg local tip |

### 3.4 P2P inject + ZMQ observation

| Step | Outcome |
|------|---------|
| Send `tx` for P2MR spend | Same mempool gates → **reject**; no ZMQ `'A'` |
| Send full `block` with spend | May accept (consensus soft-fork path) → sequence `'C'` → enforcer validates |

### 3.5 Shadow enforcer mempool without Core mempool

| Design | Works | Fails | Residual risk |
|--------|-------|-------|---------------|
| Accept spends only into enforcer-side pool (RPC to enforcer) | Mining via enforcer GBT + submitblock | Core unaware; no ZMQ for those spends | **Authenticate, rate-limit, full BIP 360 validate** before insert—never trust OOB feeds like ZMQ |
| External feed into shadow pool | Same | Not “ZMQ-enabled Core enforcement” | Feed compromise = template poison |

### 3.6 Mining with only coinbase via GBT + offline block assembly

Proven pattern in dual-node harness (`build_block_from_template` + spend): enforcer GBT `coinbasetxn`, attach multi-leaf / hybrid / kitchen-sink spends offline, `submitblock`.

### 3.7 Package / diagnostic RPCs (still same gates)

| RPC | Use | Admits v2 spend? |
|-----|-----|------------------|
| `testmempoolaccept` | Prove reject without broadcast | **No** — same standardness/script gates; good oracle, not an admission bypass |
| `sendrawtransaction` | Primary stock inject | **No** for v2 spends under this scope |
| `submitpackage` | Package relay attempts | **No** — same admission gates per tx |
| `prioritisetransaction` | Fee delta only | **Cannot** force admission of a rejected spend; useless as spend unlock |

### 3.8 Complex BIP 360 operator playbooks

| Playbook | ZMQ | RPCs | Funding | Multi-leaf / hybrid / kitchen-sink **spend** | Residual risk |
|----------|-----|------|---------|-----------------------------------------------|---------------|
| A. Diagnose spend reject | optional sequence | `testmempoolaccept` / `sendrawtransaction` | N/A | Rejected; **no** `'A'` | Use expected reject oracle (§2.4) |
| B. Fund pile on stock defaults | sequence | wallet/`sendrawtransaction` | Often `'A'` → enforcer `accept_tx` can check outputs | N/A | Fee/size limits |
| C. Offline multi-spend block | sequence for `'C'` | enforcer GBT + `submitblock` | Confirmed earlier | **Works** on block path; Core soft-fork NOP | Must use enforcer template or offline attach |
| D. `generateblock` with explicit tx list (regtest) | sequence | `generateblock` | — | Same as submitblock family if Core accepts block | Regtest-oriented; still no mempool spend |
| E. sequence + rawtx (avoid txindex) | sequence + rawtx | optional getblock | Bodies on ZMQ | Spends still never mempool-notify | **rawtx plaintext**; prefer loopback only |
| F. Invalid BIP 360 block | sequence `'C'` | `getblock`, `invalidateblock` | — | Enforcer rejects → invalidate | Fail-open if `'C'` dropped |
| G. Tip recovery | sequence C/D | `invalidateblock` / `reconsiderblock` | — | Operator/enforcer policy flips | High privilege |
| H. Compact blocks / mempool-less peers | — | full block download | — | Peers without spend in mempool need full block | Propagation depends on block relay, not tx relay |
| I. PQC verify budget | sequence | connect_block path | — | Mempool budget **not** applied; block budget can reject valid-looking batches | Documented mempool/block asymmetry |
| J. Dual enforcer mining | dual sequence | dual GBT/submitblock | — | Both must accept tip | Split-brain if only one enforces |

### 3.9 Strategy matrix summary

| Strategy | Funding mempool | Spend mempool | P2P mempool **relay** of spend | Spend in blocks | BIP 360 enforcement |
|----------|-----------------|---------------|--------------------------------|-----------------|---------------------|
| ZMQ-only | Observe only | **Impossible** | **No** | Observe only | **None** |
| Sequence + RPC accept_tx | If Core admits | **No** | **No** | Via connect_block | Blocks + non-spend mempool |
| hashtx/rawtx without sequence | Partial | **No** | **No** | Ambiguous events | Weak |
| Dual ZMQ nodes | Observe | **No** | **No** | If submitblock/P2P block | Redundant block path |
| submitblock + invalidateblock | N/A | N/A | N/A | **Yes** | **Yes** (if companion alive + events delivered) |
| Shadow mempool + enforcer GBT | Optional parallel | **Enforcer-only** | **No** (Core) | Yes if mined that way | Yes if miner cooperates |
| Core GBT alone | Ordinary txs | **No spends** | N/A | Ordinary only | Reactive invalidate only if companion runs |
| testmempoolaccept / sendrawtx spend | N/A | **No** | **No** | No | Diagnostic only |
| prioritisetransaction without admit | N/A | **No** | **No** | No | Cannot unlock spends |

---

## 4. CUSF architecture map

### 4.1 How `cusf-enforcer-mempool` uses `zmqpubsequence`

Implementation: [`cusf-enforcer-mempool/lib/zmq.rs`](../../cusf-enforcer-mempool/lib/zmq.rs).

| Piece | Behavior |
|-------|----------|
| `subscribe_sequence` | SUB connect + subscribe `"sequence"` (15s timeout) |
| `parse_sequence_message` | Frame0 `sequence`; **reverse display-order wire hash into rust-bitcoin internal order**; labels `C`/`D`/`A`/`R`; LE mempool_seq + LE zmq_seq |
| `check_seq_numbers` | Detects gaps/duplicates (**loss detection, not authentication**) |
| `initial_sync` (`cusf_enforcer.rs`) | Subscribe + chase tip on `BlockHash` C/D; **intentionally drops `TxHash` A/R** until tip stable |
| After sync | `mempool/sync/task.rs` fully applies A/R/C/D |

Sketch — **descriptive of today’s companion** (simplified; unfiltered mempool / parent fetch / abandoned pool omitted):

```
bitcoind  -zmqpubsequence=tcp://127.0.0.1:29000   # BIND (loopback only)
enforcer  --node-zmq-addr-sequence=tcp://127.0.0.1:29000  # CONNECT

subscribe_sequence ──► SequenceMessage { BlockHash C/D | TxHash A/R }
        │
        ├─ initial_sync: use C/D only to reach tip; ignore A/R
        ├─ A [today]: getrawtransaction → accept_tx
        │      → insert shadow Mempool | Reject → prioritisetransaction
        │   (RequestItem::Tx(txid, true) treats the ZMQ path as in-mempool;
        │    no getmempoolentry re-check is implemented in the companion)
        ├─ R: remove from unfiltered + shadow mempool
        ├─ C: getblock → connect_block → Accept | Reject→invalidateblock
        └─ D: disconnect_block
```

**Recommended hardening (not implemented in current `cusf-enforcer-mempool`):** after `'A'`, call `getmempoolentry` / sequence-aware `getrawmempool` before `accept_tx` or `prioritisetransaction`—see §6.4. Residual prioritise/shadow-insert risk under a compromised ZMQ peer + `txindex` remains for **current code** (§3.1).

README requirements: RPC on, **ZMQ sequence** on, **`txindex`** on (expands blast radius for forged `'A'`—historical txs remain fetchable).

### 4.2 RPC surface the enforcer stack already needs

| RPC | Used for | Privilege note |
|-----|----------|----------------|
| `getbestblockhash` / `getblockheader` | Initial sync tip chase | Read |
| `getblock` | Block connect validation | Read (authoritative body vs ZMQ hash) |
| `getrawtransaction` | Mempool/tx bodies after `'A'` | Read; with txindex, not proof of current mempool membership |
| `getrawmempool` / sequence variants | Initial mempool mirror | Read |
| `prioritisetransaction` | Soft-evict enforcer-rejected txs from Core | **Write** mempool policy |
| `invalidateblock` | CUSF block reject | **Write** tip / reorg local chain |
| Enforcer `getblocktemplate` / `submitblock` | Mining path | **High-privilege** mining control plane |

`CusfEnforcer` trait: `sync_to_tip`, `connect_block`, `disconnect_block`, `accept_tx`.  
BIP 360: `lib/validator/cusf_enforcer.rs` → `validate_tx` / block connect with feature `bip360`.

### 4.3 Gap analysis: full BIP 360 mempool-path vs connect_block-path

| Concern | Mempool path (`accept_tx`) | Block path (`connect_block`) |
|---------|----------------------------|------------------------------|
| Ingress of P2MR spends into Core | **Blocked** (stage 1/2) | Allowed (soft-fork true) via submitblock / mined blocks |
| Enforcer sees spend? | Only if Core accepted (won’t for spends under defaults) | Yes after block connect (if ZMQ `'C'` delivered) |
| Early reject of invalid spends | Implemented but **unreachable** for v2 spends via Core mempool | Yes → `invalidateblock` |
| PQC per-block budget | Not applied in mempool | Applied |
| Relay | Core will not mempool-relay spends | Block relay only |
| GBT inclusion of spends | Enforcer GBT if Core `'A'` **or** offline/OOB insert | Offline attach works without Core mempool |
| Funding P2MR outputs | Often Core-admitted → `accept_tx` can check outputs | Also checked in blocks |

**Gap statement:** software implements mempool-path BIP 360 validation, but on stock Core 31 without policy overrides that path is **unreachable for witness-v2 spends**. Production CUSF BIP 360 on stock Core is **block-enforcement-primary**, with mempool filtering limited to transactions Core stores, plus optional cooperative shadow-mempool mining.

---

## 5. Related Core policy notes (not ZMQ, but gate ZMQ)

| Flag / mechanism | Stock Core 31 | Effect on v2 | In “no override” scope? |
|------------------|---------------|--------------|-------------------------|
| Default `require_standard` + `AreInputsStandard` | Yes | Spends fail stage 1 | Default — in scope |
| `STANDARD_SCRIPT_VERIFY_FLAGS` / DISCOURAGE | Yes | Spends fail stage 2 if stage 1 skipped | Default — in scope |
| `-acceptnonstdtxn` | Exists; **not supported on all chains** (binary string: `acceptnonstdtxn is not currently supported for %s chain`—typically disallowed on mainnet-class chains; used on regtest/harness) | Clears stage 1 only; **not** DISCOURAGE | **Out of scope** |
| `-acceptunknownwitness` | Knots/local; **not** stock 31 help | Funding-oriented | Override / non-stock |
| Knots `ignore_rejects` / `PolicyScriptVerifyFlags` | Local tree only | Can strip DISCOURAGE | **Excluded** |
| Binary patch / soft-fork BIP 360 in Core | N/A | Root-cause fix | Excluded |

---

## 6. Security / trust model (required reading for operators)

### 6.1 Network placement

1. Bind ZMQ **only** to loopback or private UNIX/IPC (`tcp://127.0.0.1:…` or `ipc://…` / `unix:…` as Core accepts).  
2. **Never** advertise ZMQ on public interfaces.  
3. Treat ZMQ and JSON-RPC as the **same-trust-domain local control plane**.  
4. Core ZMQ has **no** CURVE/plain auth—firewall and bind address are the control.  
5. Prefer patterns in [`REGTEST_DEMO.md`](./REGTEST_DEMO.md) (`127.0.0.1`) over any `0.0.0.0` examples elsewhere in the ecosystem.  
6. If using IPC/UNIX sockets: path must be **owner-restricted** (directory + socket permissions); do **not** place sockets in world-writable directories (shared-host multi-user exposure).

### 6.2 Unauthenticated ZMQ as CUSF control plane

- ZMQ is the **trigger** for which hashes the companion processes; **bodies** come from RPC (`getblock` / `getrawtransaction`).  
- RPC payloads are authoritative for **content**; ZMQ still controls **whether** enforcement runs.  
- **Suppression** of `'C'` for a BIP-360-invalid tip → **fail-open** (Core keeps tip; no `invalidateblock`).  
- **Injection** of `'C'`/`'A'` can drive validation and high-privilege RPCs if the peer is attacker-controlled.  
- **Forged `'D'` / `'R'`** can desync companion tip or shadow-mempool state relative to Core (false disconnect / eviction of still-live Core txs) even when bodies are later RPC-fetched—same unauthenticated control plane, usually lower severity than missed `'C'` fail-open.  
- Sequence checks detect **loss/duplication**, not publisher authenticity; gap → stream/task error while Core continues.  
- Treat ZMQ endpoint compromise as **loss of CUSF block enforcement**, not mere missing logs.

### 6.3 Confidentiality

- `sequence` / hash topics leak txids, block hashes, timing, and mempool activity metadata.  
- `rawtx` / `rawblock` leak **full transactions and blocks** (including large PQC witnesses once mined/relayed). Prefer sequence + RPC for CUSF; restrict raw topics to short-lived localhost debug.

### 6.4 Mempool mutation hygiene

- Do not treat a ZMQ hash alone as authorization to `prioritisetransaction` or to trust `getrawtransaction` under `txindex` as “currently in mempool.”  
- **Recommended (not implemented in today’s companion):** re-validate membership (`getmempoolentry` / sequence-aware `getrawmempool`) before accept/evict side effects—see §4.1 descriptive vs prescriptive split.

### 6.5 Availability = enforcement

- HWM overflow, subscriber lag, process crash, or sequence gap ⇒ **CUSF fail-open** until resync.  
- Monitor sequence gap errors as high severity; size HWM for load; recover via restart + `sync_to_tip` / tip re-scan before calling the node “enforcing.”

### 6.6 Privileged RPC / mining APIs

Authenticate RPC, loopback/VPN only, distinct credentials, no public enforcer GBT/submitblock. Distrust external shadow-mempool feeds: authenticate, rate-limit, full BIP 360 validate before insert.

---

## 7. Practical recommendations

1. **Do not** expect ZMQ (or ZMQ+RPC alone) to unlock P2MR **spend** mempool admission on stock Core 31.  
2. **Do** run: stock bitcoind + **loopback** `-zmqpubsequence` + enforcer companion + **enforcer GBT / submitblock** for spends + `invalidateblock` on bad blocks.  
3. Treat mempool `accept_tx` as valuable for **txs Core admits** (including many funding constructions) and as future-proofing—not as a present-day spend gate.  
4. Public P2P **mempool** relay of P2MR spends requires Core consensus/policy changes (or accepted local overrides outside this finding’s constraints).  
5. Apply §6 hardening before calling a deployment “CUSF-enforcing.”

---

## 8. Reproduce / re-verify

Paths relative to workspace `cusf/`. Stock binary: `bip300301_enforcer/.integration-deps/bitcoin-stock-31.0/`.

### 8.1 Stock ZMQ option surface (closed world)

```bash
STOCK=bip300301_enforcer/.integration-deps/bitcoin-stock-31.0/bitcoind
"$STOCK" -version    # Bitcoin Core daemon version v31.0.0
# Help options are indented; match the flag substring (not ^-zmq alone):
"$STOCK" -help 2>&1 | grep -E -- '-zmqpub'
# Exactly 10 lines: 5 topics + 5 *hwm; no hashwallettx/rawwallettx
# Equivalent: grep -E '^[[:space:]]*-zmq'
strings "$STOCK" | grep -E -- '-zmqpub' | sort -u
```

### 8.2 Runtime notifier list

```bash
# After starting bitcoind with e.g. -zmqpubsequence=tcp://127.0.0.1:29000
bitcoin-cli getzmqnotifications
# Expect type/address/hwm objects for each active pub*
```

### 8.3 Mempool reject of v2 spend (no ZMQ `'A'`)

**Primary re-verify path for spend/block claims:** use the integration harness recipes in §8.5 (`just bip360-block-matrix` / `just bip360-p2p-e2e`). Those assemble funding + spends and exercise `submitblock` (with harness `-acceptnonstdtxn`).

**Manual procedure** (requires BIP 360 builders—outline, not a full standalone script):

1. **Fund** a confirmed v2 output under stock defaults (funding generally allowed).  
2. **Build a script-path spend hex:**  
   - Example CLI: `cargo run --example p2mr_signer --no-default-features --features bip360 -- …`  
     (see [`P2MR_SIGNER.md`](./P2MR_SIGNER.md); unit coverage `p2mr_signer_roundtrip`).  
   - Or reuse harness helpers under `integration_tests/bip360_block.rs` / `bip360_dual_node.rs`.  
3. **Reject oracle:**  
   ```bash
   bitcoin-cli testmempoolaccept "[\"$SPEND_HEX\"]"
   # Stock defaults: allowed=false, reject-reason contains bad-txns-nonstandard-inputs (§2.4)
   # Optional out-of-scope: restart with -acceptnonstdtxn → stage-2 discourage-upgradable-witness family
   ```  
4. **No ZMQ `'A'` for the spend** — subscribe while step 3 runs:  
   ```bash
   # From a Core source tree with contrib/zmq (or any SUB client):
   #   bitcoind … -zmqpubsequence=tcp://127.0.0.1:29000
   python3 contrib/zmq/zmq_sub.py   # or equivalent SUB to tcp://127.0.0.1:29000 topic "sequence"
   # After testmempoolaccept/sendrawtransaction of the spend: no sequence body with label 'A'
   # for that txid. (Funding admits may still emit 'A'.)
   ```  
5. Optionally repeat with regtest **`-acceptnonstdtxn`** (**override, out of scope**): still expect no successful mempool admit for spends.

### 8.4 Block path emits `'C'`

**Primary:** harness block-matrix / dual-node spend path (§8.5) — `confirm_p2mr_spend_via_submitblock` builds a block and submits it; Core retains tip; enforcers sync without invalidate for valid spends.

**Manual outline:**

1. Assemble block (enforcer GBT coinbase + spend, or harness `build_block_from_template`).  
2. `submitblock` → Core accepts under soft-fork placeholder rules.  
3. ZMQ `sequence` shows `'C'` for the new tip; enforcer `connect_block` runs; invalid BIP 360 → `invalidateblock`.

### 8.5 Integration harness recipes (recommended for spend/block)

From `bip300301_enforcer/` (paths relative to that crate):

| Goal | Prerequisites | Command |
|------|---------------|---------|
| Stock bitcoind env only | — | `just setup-core` → writes `integrationtests.env` / `BITCOIND_UNPATCHED` |
| Full P2P stack (electrs) | electrs build | `just setup` (not `setup-core` alone) |
| 33 block-matrix trials (submitblock spends) | `just setup-core` | `just bip360-block-matrix` or `just bip360-block-matrix yes` |
| Dual-node P2P E2E (uses dual-node spend helper) | `just setup` | `just bip360-p2p-e2e` or `just bip360-p2p-e2e yes` |
| Signer smoke | bip360 feature | `cargo run --example p2mr_signer --no-default-features --features bip360 -- …` — [`P2MR_SIGNER.md`](./P2MR_SIGNER.md) |

Code anchors:

- `integration_tests/bip360_dual_node.rs` — `confirm_p2mr_spend_via_submitblock` (spend via submitblock; documents DISCOURAGE even with `-acceptnonstdtxn`)  
- Block-matrix trials registered in integration test modules / `just bip360-block-matrix`

Harness nodes pass **`-acceptnonstdtxn`** (`util.rs`). Use for:

- spend-via-submitblock / dual-node tip retention,  
- “DISCOURAGE even with acceptnonstdtxn,”  

**not** as sole proof of default-policy funding (use stock policy reasoning in §2.3 for that).

---

## 9. Evidence of diligence

### 9.1 Inspected artifacts

| Item | What was checked |
|------|------------------|
| Stock `bitcoind` v31.0.0 | `-version`, `-help` ZMQ closed world, `strings` for `-zmqpub*`, absence of wallet ZMQ / stock `acceptunknownwitness` help |
| `$BITCOIN_SRC/src/zmq/*` | Notifier stack, defaults, factories, `getzmqnotifications` |
| `$BITCOIN_SRC/src/init.cpp` | Arg registration; validation interface wire-up |
| `doc/zmq.md` (local + stock framing) | Multipart layout; write-only; corrected hash-order interpretation |
| `script/interpreter.cpp` | `VerifyWitnessProgram` DISCOURAGE branch |
| `policy/policy.{h,cpp}` | STANDARD flags; `AreInputsStandard` `WITNESS_UNKNOWN`; `IsStandard` funding |
| `validation.cpp` | PreChecks standardness vs `PolicyScriptChecks`; `GetBlockScriptFlags` |
| `cusf-enforcer-mempool` | Sequence parse, initial_sync vs task apply, RPC methods |
| Integration harness | dual-node spend comments; `-acceptnonstdtxn` in `util.rs` |

### 9.2 Honest limits

- Local line numbers are Knots-line anchors; stock **source** offsets can differ by large amounts—use symbols when auditing `bitcoin/bitcoin@v31.0`.  
- This revision did **not** re-run a full live ZMQ capture; §8 gives operators a recipe. Spend conclusions rest on stock/local policy source + harness comments.  
- Network-wide enforcement and miner incentives are out of scope.  
- A future Core soft-fork defining witness v2 would change both mempool and block semantics.

---

## References (workspace)

- [`CUSF-BIP360.md`](./CUSF-BIP360.md) — enforcer rules and stock deployment model  
- [`MEMPOOL_RELAY_POLICY.md`](./MEMPOOL_RELAY_POLICY.md) — companion role (see §2.3 reconciliation for funding wording)  
- [`REGTEST_DEMO.md`](./REGTEST_DEMO.md) — localhost ZMQ/RPC patterns  
- [`../../STATUS.md`](../../STATUS.md) — project status  
- [`../../DESIGN.md`](../../DESIGN.md) — insertion points  
- [`../../cusf-enforcer-mempool/lib/zmq.rs`](../../cusf-enforcer-mempool/lib/zmq.rs) — sequence client  
- Upstream Bitcoin Core v31.0: <https://github.com/bitcoin/bitcoin/tree/v31.0>
