# Tier B — CUSF mining path (expect PASS)

Soft-fork rules live in the **enforcer**. Stock Bitcoin Core stays unpatched.  
Enforcer-built P2MR spends are included with **block assembly + `submitblock`**.  
The enforcer keeps the tip. Core’s mempool does not need to accept witness-v2 spends.

This is the CUSF-facing Tier B (see [CUSF paper](https://bip300cusf.com/cusf.pdf): separate activator, tip via `invalidateblock` / keep tip).

## Run

```bash
just bip360-tier-b-cusf
# or: just it bip360_tier_b_cusf_miner
```

Trial: **`bip360_tier_b_cusf_miner`**  
Harness: `integration_tests/test_bip360_tier_b_cusf_miner.rs`

Needs stock Core (`just setup-core` / `BITCOIND_UNPATCHED`). No `BITCOIND_P2MR`.

## Pass criteria

1. Stock Core 31 + bip360 enforcer.
2. Enforcer-format **Schnorr** spend in a block via `submitblock` → tip retained.
3. Enforcer-format **kitchen-sink** (Schnorr + ML-DSA + SLH) spend same way → tip retained.
4. Log path `cusf_mine` (not P2MR Core mempool).

## What this does not claim

- Stock Core **mempool** admits v2 spends (it does not; that is policy, not CUSF failure).
- P2MR Core `sendrawtransaction` accepts the same spends — that is a separate check:  
  [`TIER_B_P2MR_MEMPOOL.md`](./TIER_B_P2MR_MEMPOOL.md) / `just bip360-tier-b-mempool` (known FAIL until both implement the same P2MR protocol for those witnesses).

## Related

| Item | Role |
|------|------|
| `just bip360-tier-b-cusf` | This trial (green) |
| `just bip360-tier-b-mempool` | P2MR Core mempool protocol (red until fixed) |
| `just bip360-kitchen-sink-tier-a` | Dual-stack demo (stock + P2MR Bob; often submitblock fallback) |
| `just bip360-p2p-e2e` | Dual stock #34 |
