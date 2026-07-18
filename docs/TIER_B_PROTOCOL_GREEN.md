# Tier B protocol green — shapes 1+2+3 + tip kitchen-sink

**Date:** 2026-07-17  
**Status:** Protocol interop **green**. TB-sendraw shapes **1+2+3** PASS on Bob
mempool; tip kitchen-sink PASS via CUSF mining. Green matrix still **34**.

This is a celebration of a **protocol** win, not a claim that stock Core mempool
admits witness-v2 spends.

---

## What was hard

**TB-sendraw was never a CUSF tip failure.** Soft-fork tip + mining already
worked on stock Core (`just bip360-tier-b-cusf`): enforcer-built spends go in
via `submitblock`; the enforcer keeps the tip with `invalidateblock` as needed.

TB-sendraw asks a different question: does **P2MR Core** (`BITCOIND_P2MR`,
cryptoquick) accept **enforcer-built** (or Core-interop) spends in its
**mempool** via `sendrawtransaction`? That is **two implementations agreeing on
script verify** — a protocol mismatch until both sides share control wire,
sighash rules, element size limits, and multi-`OP_CHECKSIG` dispatch.

Misleading diagnoses along the way:

- Treating Bob reject as “CUSF is broken”
- Fixing only control/sighash (3A/3C) while kitchen-sink still hit Core push
  caps
- Assuming OP_SUBSTR hybrid and overload kitchen-sink were the same shape

---

## Barriers crossed (ordered)

| #     | Barrier                        | What failed                                          | Resolution                                                                                     |
| ----- | ------------------------------ | ---------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| **1** | Control pad                    | Multi-byte / padded control vs Core single-leaf wire | Control block **`c1`** only                                                                    |
| **2** | Default vs All sighash         | Trailing `0x01` / wrong preimage for bare Schnorr    | Shape 1: **`TapSighashType::Default`** → bare **64 B**                                         |
| **3** | Stack / leaf **520**           | ML-DSA / SLH sigs and ML-DSA pk (1312) ≫ 520         | TAPSCRIPT uses `MAX_SCRIPT_ELEMENT_SIZE_P2MR` (**8000**) for initial stack **and** leaf pushes |
| **4** | Multi-`OP_CHECKSIG` size gates | Kitchen-sink never hit OP_SUCCESS127 stack raise     | `EvalChecksigTapscript` size-gates **Schnorr / ML-DSA / SLH**                                  |
| **5** | Multi-site index pairing       | Stack pairing alone mis-pairs multi-CHECKSIG sites   | Core multi-site path pairs witness sigs to CHECKSIG sites by **index**                         |

Core path: `/home/hunter/Projects/cryptoquick/bitcoin`  
(`script.h` `MAX_SCRIPT_ELEMENT_SIZE_P2MR`, `interpreter.cpp` TAPSCRIPT +
overload).

---

## Current scoreboard

| Check                                         | Status                                               | Command                                    |
| --------------------------------------------- | ---------------------------------------------------- | ------------------------------------------ |
| Shape **1** Schnorr Bob mempool               | **PASS**                                             | `just bip360-tier-b-mempool`               |
| Shape **2** Core hybrid OP_SUBSTR Bob mempool | **PASS** (interop; enforcer still rejects OP_SUBSTR) | same                                       |
| Shape **3** overload kitchen-sink Bob mempool | **PASS** (post Core 3B)                              | same                                       |
| Tip kitchen-sink CUSF mine                    | **PASS**                                             | `just bip360-tier-b-cusf`                  |
| Green matrix                                  | **34**                                               | `just it-all` / `just bip360-block-matrix` |

TB-factory / TB-sidecar remain optional PASS (not in the 34).

---

## Validate

```bash
cd bip300301_enforcer

# Unit goldens (fast)
cargo test -p bip300301_enforcer_lib --features bip360 --lib protocol_vectors
cargo test -p bip300301_enforcer_lib --features bip360 --lib kitchen_sink_3b
cargo test -p bip300301_enforcer_lib --features bip360 --lib core_interop
cargo test -p bip300301_enforcer_lib --features bip360 --lib published_vectors

# Protocol interop (needs BITCOIND_P2MR + electrs)
just bip360-tier-b-mempool   # shapes 1+2+3 hard green

# CUSF tip path (stock Core only)
just bip360-tier-b-cusf      # kitchen-sink on tip

# Full green matrix (long)
just bip360-block-matrix     # or just it-all
```

Both `just bip360-tier-b-cusf` and `just bip360-tier-b-mempool` must PASS.

---

## Terms people mix up (read this first)

This section exists because “mempool,” “Alice,” “Bob,” “CUSF,” and “green” got
blurred in discussion. Same words, different questions.

### Who is who

| Name      | Node                                             | Role                                                                               |
| --------- | ------------------------------------------------ | ---------------------------------------------------------------------------------- |
| **Alice** | **Stock** Bitcoin Core 31 (unchanged)            | Tip / chain for CUSF stories; usually has the bip360 **enforcer** watching her tip |
| **Bob**   | **P2MR** bitcoind (`BITCOIND_P2MR`, cryptoquick) | Protocol peer; can put our spends in **his** mempool after alignment               |

The enforcer is **not** Alice’s mempool. It checks soft-fork rules on **tips**
(and may `invalidateblock`). It does **not** force stock Alice to accept a P2MR
spend into her mempool.

### What “in the mempool” means here

Mempool is more than relay. It also means: pending, queryable (`getmempoolentry`
/ index), fee-ranked, available for a normal block template.

| Whose mempool?    | Kitchen-sink / P2MR **spend** in mempool?     |
| ----------------- | --------------------------------------------- |
| **Alice (stock)** | **No** — policy still rejects those spends    |
| **Bob (P2MR)**    | **Yes** after TB-sendraw green (shapes 1–2–3) |

**Without changing stock Bitcoin:** you still can **mine** those spends by
building a block yourself and `submitblock` (what most CUSF green tests do). You
do **not** get them into **Alice’s** mempool that way.

### Test A vs test B

|                       | **Test A — Tier A / dual tip demo**                                    | **Test B — TB-sendraw (this doc’s mempool story)**         |
| --------------------- | ---------------------------------------------------------------------- | ---------------------------------------------------------- |
| **Question**          | Do Alice and Bob end up on the **same tip** after the spends?          | Does **Bob’s mempool** accept the spend bytes?             |
| **Pass**              | Tips match; spend is **in a block**                                    | `sendrawtransaction` on Bob + **`getmempoolentry` on Bob** |
| **Alice mempool**     | Not required                                                           | Not required                                               |
| **Bob mempool**       | Optional nice path; if it fails → fall back to **block + submitblock** | **Required** — no submitblock substitute                   |
| **What went green**   | Dual tips / confirm (often via **block**)                              | Bob pending pool for shapes **1 + 2 + 3**                  |
| **Recipe (examples)** | `just bip360-kitchen-sink-tier-a`                                      | `just bip360-tier-b-mempool`                               |

**CUSF tip twin (not A, not B mempool):** `just bip360-tier-b-cusf` — stock
only, Schnorr then kitchen-sink via **`submitblock`**, enforcer keeps tip.
Proves **mined + tip**, not Alice mempool.

### Confusions we hit (and the straight answer)

| Confused claim                                           | Straight answer                                                                     |
| -------------------------------------------------------- | ----------------------------------------------------------------------------------- |
| “Green mempool means stock Core holds the spend”         | **No.** Green mempool means **Bob** holds it.                                       |
| “CUSF green means stock mempool works”                   | **No.** CUSF green means **block + tip + enforcer**.                                |
| “TB-sendraw tested gossip → enforcer injects into Alice” | **No.** It tested **RPC `sendraw` on Bob** + mempool entry.                         |
| “Kitchen-sink tests need stock mempool”                  | **No.** Most use **assemble block + submitblock**.                                  |
| “Mempool is only about relay”                            | **No.** Index, inspect, template inclusion matter too — still on **whose** mempool. |

### One-line map

- **Alice mempool:** stock; still empty of these spends; not our green bar.
- **Bob mempool:** P2MR; shapes 1–2–3 green.
- **Test A:** same tip (often block path).
- **Test B:** Bob’s pending pool.
- **CUSF mine:** stock block + enforcer tip.

---

## Related docs and modules

| Link                                                 | Role                                        |
| ---------------------------------------------------- | ------------------------------------------- |
| [`TIER_B_P2MR_MEMPOOL.md`](./TIER_B_P2MR_MEMPOOL.md) | TB-sendraw narrative, shapes 1+2+3          |
| [`TIER_B_CUSF_MINER.md`](./TIER_B_CUSF_MINER.md)     | TB-mine / tip kitchen-sink green path       |
| [`../../RESIDUAL.md`](../../RESIDUAL.md)             | Residual scoreboard / polish tracker        |
| `lib/validator/pqc/kitchen_sink_3b.rs`               | 3B kitchen-sink golden fixture              |
| `lib/validator/pqc/protocol_vectors.rs`              | 3C Schnorr fixed-entropy pack               |
| `lib/validator/pqc/core_interop.rs`                  | Shape 2 Core hybrid builders (interop only) |
| `lib/validator/pqc/data/`                            | Published 3C / hybrid / 3B JSON for Core CI |

---

## What polish remains (optional)

- Human **GPG-signed** commits, push, upstream PRs (not autonomous)
- Sidecar auth / HTTP e2e maturity
- Bump `bitcoin-p2mr-pqc` pin past `9093253` (decode base size 1 natively)
- Core unit coverage and quieter PQC logs (this polish pass)
- Kellnr / signet / BIP author review

**Explicit non-goals:** patch stock Alice mempool for v2 spends; treat sendraw
green as the definition of CUSF success; claim enforcer tip accepts OP_SUBSTR
hybrid.
