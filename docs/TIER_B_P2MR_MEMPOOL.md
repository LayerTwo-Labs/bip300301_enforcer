# Tier B — TB-sendraw: P2MR Bob mempool interop

**Milestone:** protocol green —
[`TIER_B_PROTOCOL_GREEN.md`](./TIER_B_PROTOCOL_GREEN.md).

**Not** “CUSF is broken.” Soft-fork tip + mining already work on stock Core
without this check — see the green path:
[`TIER_B_CUSF_MINER.md`](./TIER_B_CUSF_MINER.md) (`just bip360-tier-b-cusf`,
TB-mine).

**This check (TB-sendraw):** whether **P2MR Core** (`BITCOIND_P2MR`,
cryptoquick/jbride) accepts **enforcer-built** (or Core-interop) spends in
**Bob’s mempool** via `sendrawtransaction` + `getmempoolentry`. That is a **P2MR
protocol** question—two implementations agreeing on script verify—not whether
the CUSF enforcer keeps a valid tip, and **not** whether **stock Alice’s**
mempool holds the spend.

**Alice (stock) vs Bob (P2MR), Test A vs Test B:** plain-language table and
common mix-ups are in
[`TIER_B_PROTOCOL_GREEN.md` § Terms people mix up](./TIER_B_PROTOCOL_GREEN.md#terms-people-mix-up-read-this-first).

**Not** part of green CI, `just it-all`, or `just bip360-verify-full`.

## Reproduce

```bash
cd bip300301_enforcer
just setup              # stock Core + electrs
just setup-p2mr         # BITCOIND_P2MR, WITH_ZMQ=ON
just bip360-tier-b-mempool   # primary; recipe exports BIP360_TIER_B=1; shapes 1+2+3
# raw it only: BIP360_TIER_B=1 just it bip360_tier_b_p2mr_mempool
# BIP360_TIER_B_KITCHEN_SINK=1 is redundant (kitchen-sink always required post-Core 3B)
```

Trial: **`bip360_tier_b_p2mr_mempool`**  
Not in `just it-all`. Without `BIP360_TIER_B=1`, the trial no-ops (pass) so
accidental full-suite runs stay green.

Default path runs shapes **1+2+3** (Schnorr, Core hybrid OP_SUBSTR, overload
kitchen-sink). All three are hard green gates after Core 3B multi-barrier fix.

## Shapes (product)

| Shape                             | Leaf                                                                                     | Where                  | Status intent                              |
| --------------------------------- | ---------------------------------------------------------------------------------------- | ---------------------- | ------------------------------------------ |
| **1. Schnorr**                    | `PUSH32 <pk> OP_CHECKSIG`, control `c1`, TapSighash **Default** bare 64 B                | Bob mempool            | **PASS** (green gate)                      |
| **2. Core hybrid OP_SUBSTR**      | `PUSH32 <schnorr_pk> OP_CHECKSIG PUSH32 <slh_pk> OP_SUBSTR OP_BOOLAND OP_VERIFY` (~70 B) | Bob mempool            | **PASS** (interop; not enforcer-canonical) |
| **3. Full overload kitchen-sink** | Schnorr + ML-DSA + SLH, all `OP_CHECKSIG`                                                | Bob mempool + tip/CUSF | **PASS** (green gate post-Core 3B)         |

Builders for shape 2 live in `lib/validator/pqc/core_interop.rs` (labeled
**interop / not enforcer-canonical**). Enforcer verify still rejects `OP_SUBSTR`
(`OpSubstrForbidden`) — shape 2 asserts Bob `sendraw` + `getmempoolentry` only,
not Alice tip keep of that spend.

Shape 3 fixture: `lib/validator/pqc/kitchen_sink_3b.rs`. Core accepts via
TAPSCRIPT element size 8000 + size-gated OP_CHECKSIG (Schnorr / ML-DSA-44 /
SLH-DSA).

Shared Schnorr hex (lane **3C**): `lib/validator/pqc/protocol_vectors.rs`
(`SCHNORR_VECTOR_ENTROPY = [0x11; 32]`).

## Pass criteria

1. Stock Alice + P2MR Bob (Tier A layout).
2. Fund P2MR (works on both mempools).
3. **Shape 1** enforcer Schnorr spend: Bob `sendrawtransaction` +
   `getmempoolentry` OK.
4. **Shape 2** Core hybrid OP_SUBSTR spend on a matching funded UTXO: same.
5. **Shape 3** full overload kitchen-sink spend: same.
6. No `submitblock` substitute for those spends.

## History / failure stages

### Before 3A control-block fix

| Shape                  | Bob mempool | Typical reject                  |
| ---------------------- | ----------- | ------------------------------- |
| Enforcer Schnorr spend | **reject**  | `Witness program hash mismatch` |
| Funding into v2        | **accept**  | —                               |

**Cause:** legacy `0xc1 \|\| 32 zeros` pad treated as a merkle sibling by Core
(`P2MR_CONTROL_BASE_SIZE = 1`).

### After 3A control + sighash Default/All (2026-07-16)

| Shape                  | Bob mempool | Result                                               |
| ---------------------- | ----------- | ---------------------------------------------------- |
| Enforcer Schnorr spend | **accept**  | control `c1`; bare 64 = Default                      |
| Kitchen-sink overload  | **reject**  | `Push value size limit exceeded` (ML-DSA pk ≫ 520 B) |

**Root cause of “Invalid Schnorr signature” (fixed):** bare 64-byte Schnorr
means `SIGHASH_DEFAULT`; signing with `All` preimage without trailing `0x01`
mismatched Core.

### After residual P0 (shape split + Core hybrid) — live 2026-07-16

| Shape                         | Bob mempool | Notes                                                          |
| ----------------------------- | ----------- | -------------------------------------------------------------- |
| Shape 1 Schnorr               | **PASS**    | Vectors in `protocol_vectors`                                  |
| Shape 2 Core hybrid OP_SUBSTR | **PASS**    | `core_interop`; enforcer still rejects OP_SUBSTR on tip verify |
| Trial overall (pre-3B Core)   | **PASS**    | Kitchen-sink not yet required                                  |

### After Core 3B multi-barrier — live 2026-07-16

| Shape                         | Bob mempool | Notes                                               |
| ----------------------------- | ----------- | --------------------------------------------------- |
| Shape 1 Schnorr               | **PASS**    | unchanged                                           |
| Shape 2 Core hybrid OP_SUBSTR | **PASS**    | unchanged                                           |
| Shape 3 overload kitchen-sink | **PASS**    | TAPSCRIPT element size 8000 + multi-OP_CHECKSIG PQC |
| Trial overall                 | **PASS**    | shapes 1+2+3 hard green                             |

## 3B — kitchen-sink Bob mempool (**closed** on Core)

**Status:** green on Bob mempool after Core multi-barrier fix. Fixture retained.

### Historical barriers (pre-fix) → fix

Cryptoquick P2MR Core (`script.h` / `interpreter.cpp`):

| Constant                                   |    Value | Pre-fix                     | Post-fix (TAPSCRIPT)                       |
| ------------------------------------------ | -------: | --------------------------- | ------------------------------------------ |
| `MAX_SCRIPT_ELEMENT_SIZE`                  |  **520** | stack + leaf pushes         | BASE / WITNESS_V0 only                     |
| `MAX_SCRIPT_ELEMENT_SIZE_SLHDSA` / `_P2MR` | **8000** | stack only if OP_SUCCESS127 | TAPSCRIPT stack **and** leaf pushes always |

Overload kitchen-sink witness (no `OP_SUBSTR`):

```text
[ec_sig ~65, mldsa_sig ~2421, slh_sig ~7857, leaf 1387, control c1]
```

Leaf (enforcer overload model — all `OP_CHECKSIG`):

```text
PUSH32 <schnorr_pk> OP_CHECKSIG
OP_PUSHDATA2 <mldsa_pk 1312 B> OP_CHECKSIG
PUSH32 <slh_pk> OP_CHECKSIG
OP_BOOLAND OP_BOOLAND OP_VERIFY
```

| #     | Barrier                     | Pre-fix reject                           | Core fix                                 |
| ----- | --------------------------- | ---------------------------------------- | ---------------------------------------- |
| **1** | Initial witness stack ≤ 520 | ML-DSA/SLH sigs → `SCRIPT_ERR_PUSH_SIZE` | TAPSCRIPT max element **8000**           |
| **2** | Leaf script pushes ≤ 520    | ML-DSA pk 1312                           | `EvalScript` TAPSCRIPT push max **8000** |
| **3** | multi-OP_CHECKSIG           | only Schnorr pk32                        | size-gated SLH + ML-DSA verify           |

### Fixture export (enforcer)

Module: `lib/validator/pqc/kitchen_sink_3b.rs`

| Field              | Golden / fact                                                   |
| ------------------ | --------------------------------------------------------------- |
| entropy            | EC `[0x33;32]`, ML-DSA `[0x22;128]`, SLH `[0x88;128]` (harness) |
| control            | `c1` (after 3A)                                                 |
| leaf_script_len    | 1387                                                            |
| max_leaf_push_size | 1312 (ML-DSA pk)                                                |
| witness sigs       | ML-DSA ~2421, SLH ~7857 (both > classic 520; fit under 8000)    |
| spk / root         | pinned hex in module                                            |

```bash
cargo test -p bip300301_enforcer_lib --features bip360 --lib kitchen_sink_3b
just bip360-tier-b-mempool   # shape 3 hard accept
```

Tip/CUSF full kitchen-sink remains green: `just bip360-tier-b-cusf`.

## Protocol dump (how to read a TB-sendraw failure)

On `sendrawtransaction` reject **or** `getmempoolentry` failure after sendraw
ok, `assert_bob_p2mr_mempool_accepts_spend` appends a structured **TB-sendraw
protocol dump** (implementation: `lib/validator/pqc/protocol_dump.rs`). Labels
match the printed dump exactly.

| Field (as printed)                    | Meaning                                                                          |
| ------------------------------------- | -------------------------------------------------------------------------------- |
| `prevout_script_pubkey`               | Full P2MR output script (`OP_2` + push 32 + program) being spent                 |
| `prevout_witness_program (32 B)`      | The 32-byte program Bob hashes against                                           |
| `leaf_script`                         | Full hex of script-path leaf (second-to-last witness element); **not truncated** |
| `leaf_script_len`                     | Leaf length in bytes                                                             |
| `control_block`                       | Full hex of control block (last element); first byte should be `0xc1`            |
| `control_byte` / `control_block_len`  | First control byte and length                                                    |
| `enforcer_leaf_version`               | Leaf version the enforcer uses for tapleaf / merkle (`0xc0`)                     |
| `tapleaf_hash`                        | Enforcer `TapLeafHash::from_script(leaf, leaf_version)`                          |
| `recomputed_merkle_root (enforcer)`   | Root from leaf + control under **enforcer** merkle rules                         |
| `recomputed_script_pubkey (enforcer)` | `OP_2 PUSH32 <recomputed_root>` the enforcer would fund                          |
| `root_matches_prevout_program`        | Whether enforcer leaf+control recommits to the funded UTXO                       |
| `note`                                | Optional merged diagnostics (short witness, non-P2MR prevout, etc.)              |

**How to use the dump**

1. If `root_matches_prevout_program=true` but Bob still rejects, Core’s leaf /
   control / sig policy differs from enforcer commitment self-check (classic 3A
   vs 3B).
2. If `root_matches_prevout_program=false`, funding spk ≠ spend leaf tree — fix
   funding first. Shape 2 funds a **Core hybrid** spk (not a Schnorr pile).
3. Leaf and control hex are printed in full for 3C vector porting.

Witness stack layout assumed: `[signatures…] <leaf_script> <control_block>`.

### Control block layout (3A — fixed to Core)

```text
control_block = 0xc1 || (32 * N merkle branch nodes)
```

- **Single-leaf:** exactly **1 byte** `0xc1` (empty branch).
- **Multi-leaf:** `0xc1` plus 32 bytes per sibling.

### Core hybrid leaf (shape 2)

```text
PUSH32 <schnorr_pk> OP_CHECKSIG PUSH32 <slh_pk> OP_SUBSTR OP_BOOLAND OP_VERIFY  (70 B)
witness: [ec_sig 64 B Default bare, slh_sig 7856 B All-preimage bare, leaf, c1]
```

Core OP_SUCCESS127 pre-scan matches stack sizes `64` + `7856` and verifies SLH
under `SIGHASH_ALL` when the SLH element has no trailing type byte.

## Fix lanes

| Lane   | Who moves         | Goal                                                                                                                                        |
| ------ | ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| **3A** | Enforcer builders | Emit what P2MR Core verifies (control, Default bare Schnorr — **done** for shape 1)                                                         |
| **3B** | P2MR Core         | Large-push / multi-algo kitchen-sink on Bob — **closed** (TAPSCRIPT element size 8000 + size-gated multi-`OP_CHECKSIG`; shape 3 hard green) |
| **3C** | Shared vectors    | Schnorr pack in `protocol_vectors`; hybrid export via unit pins                                                                             |

Remaining Core polish (optional): quieter SLH DEBUG logs; published 3C/3B hex
JSON for Core CI; pin bumps.

## Community / bounty note

CUSF’s claim is stock Core + a **separate** activator on the tip. That path is
green (TB-mine). Protocol alignment for Bob kitchen-sink (**3B**) is closed on
cryptoquick P2MR Core; remaining bounty/polish is fixture export / pin hygiene —
not “make stock Core mempool accept witness-v2 spends.”

## What CUSF does _not_ require

- Stock Core mempool admission of v2 spends.
- This trial green before claiming tip enforcement works.
- Enforcer tip acceptance of OP_SUBSTR hybrid leaves (intentionally rejected).

## Related

| Item                                                     | Role                                                              |
| -------------------------------------------------------- | ----------------------------------------------------------------- |
| [`TIER_B_PROTOCOL_GREEN.md`](./TIER_B_PROTOCOL_GREEN.md) | Celebration / status: shapes 1+2+3 + tip kitchen-sink green       |
| [`TIER_B_CUSF_MINER.md`](./TIER_B_CUSF_MINER.md)         | Green CUSF mining path (TB-mine)                                  |
| [`TIER_A_P2MR_ALIGNMENT.md`](./TIER_A_P2MR_ALIGNMENT.md) | Dual-stack demo (often `submitblock` fallback)                    |
| [`MEMPOOL_RELAY_POLICY.md`](./MEMPOOL_RELAY_POLICY.md)   | Stock mempool policy (funding vs spend)                           |
| `lib/validator/pqc/protocol_vectors.rs`                  | 3C fixed-entropy Schnorr hex                                      |
| `lib/validator/pqc/core_interop.rs`                      | Shape 2 Core hybrid builders                                      |
| `lib/validator/pqc/kitchen_sink_3b.rs`                   | 3B kitchen-sink golden fixture (Bob hard green gate post-Core 3B) |
| `lib/validator/pqc/data/`                                | Published 3C / hybrid / 3B JSON for Core CI                       |
