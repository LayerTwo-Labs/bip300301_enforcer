# Tier B — P2MR Core mempool / protocol match (known FAIL)

**Not** “CUSF is broken.” CUSF tip + mining works without this — see [`TIER_B_CUSF_MINER.md`](./TIER_B_CUSF_MINER.md).

**This check:** whether **P2MR Core** (`BITCOIND_P2MR`, cryptoquick/jbride) accepts **enforcer-built** spends in its **mempool** via `sendrawtransaction`. That requires both sides to implement the **same P2MR protocol** for leaf scripts, control blocks, and witness program commitment.

**Status:** Intentionally **red** until that protocol matches.  
**Not** part of green CI, `just it-all`, or `just bip360-verify-full`.

## Reproduce (expected FAIL)

```bash
cd bip300301_enforcer
just setup              # stock Core + electrs
just setup-p2mr         # BITCOIND_P2MR, WITH_ZMQ=ON
just bip360-tier-b-mempool
# or: BIP360_TIER_B=1 just it bip360_tier_b_p2mr_mempool
```

Trial: **`bip360_tier_b_p2mr_mempool`**  
Without `BIP360_TIER_B=1`, the trial no-ops (pass) so full-suite runs stay green.

## Pass criteria

1. Stock Alice + P2MR Bob (Tier A layout).
2. Fund P2MR (already works on both mempools).
3. Enforcer-format Schnorr spend: Bob `sendrawtransaction` + `getmempoolentry` OK.
4. Enforcer-format kitchen-sink on a new UTXO: same.
5. No `submitblock` substitute for those spends.

## Failure seen (2026-07-16)

| Shape | Bob mempool | Typical reject |
|-------|-------------|----------------|
| Enforcer Schnorr spend | **reject** | `mempool-script-verify-flag-failed (Witness program hash mismatch)` |
| Enforcer kitchen-sink / hybrid / ML-DSA | **reject** | RPC `-26` / script verify |
| Funding into v2 | **accept** | — |

**Witness program hash mismatch** means the witness (script + control block path) does not recommit to the output’s 32-byte witness program under Core’s rules—leaf bytes, merkle path, or control-block serialization disagree with what the enforcer built.

## Fix lanes (bounty-shaped)

| Lane | Who moves | Goal |
|------|-----------|------|
| **A** | Enforcer builders / `bitcoin-p2mr-pqc` | Emit what P2MR Core already verifies |
| **B** | P2MR Core (`cryptoquick:p2mr`) | Verify what the enforcer already emits |
| **C** | Shared protocol vectors | Canonical hex both CI suites accept |

## What CUSF does *not* require

- Stock Core mempool admission of v2 spends.
- This trial green before claiming tip enforcement works.

## Related

| Item | Role |
|------|------|
| [`TIER_B_CUSF_MINER.md`](./TIER_B_CUSF_MINER.md) | Green CUSF mining path |
| [`TIER_A_P2MR_ALIGNMENT.md`](./TIER_A_P2MR_ALIGNMENT.md) | Dual-stack demo |
| [`MEMPOOL_RELAY_POLICY.md`](./MEMPOOL_RELAY_POLICY.md) | Stock mempool policy |
| [`ZMQ_CUSF_BIP360_FINDINGS.md`](./ZMQ_CUSF_BIP360_FINDINGS.md) | ZMQ cannot inject into Core mempool |

## Bounty note

Turning `just bip360-tier-b-mempool` green (with a short note on A vs B vs C) is a clean acceptance test for **P2MR protocol alignment** between enforcer and P2MR Core—not for “soft-fork Bitcoin.”
