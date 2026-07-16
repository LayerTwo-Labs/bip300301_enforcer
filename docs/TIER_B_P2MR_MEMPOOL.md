# Tier B — P2MR mempool interop (open problem / bounty target)

**Status:** Intentionally **red** until someone fixes Core↔enforcer spend admission.  
**Not** part of green CI, `just it-all`, or `just bip360-verify-full`.

## One-sentence problem

Enforcer-built **overload-model** P2MR *spends* (CUSF product format) are rejected from the **cryptoquick / jbride P2MR Core** mempool, so a pure mempool+mine path for BIP 360 CUSF dual-stack does not work yet.

Tier A still proves CUSF tip enforcement via `submitblock`. Tier B is the missing mempool half.

## Reproduce (expected FAIL)

```bash
cd bip300301_enforcer
just setup              # stock Core + electrs
just setup-p2mr         # BITCOIND_P2MR = cryptoquick:p2mr (jbride#2 head), WITH_ZMQ=ON
just bip360-tier-b-mempool
# or: BIP360_TIER_B=1 just it bip360_tier_b_p2mr_mempool
```

Trial name: **`bip360_tier_b_p2mr_mempool`**  
Harness: `integration_tests/test_bip360_tier_b_p2mr_mempool.rs`

Without `BIP360_TIER_B=1`, the trial **no-ops (pass)** so default full-suite runs stay green.

## Success criteria (when the test turns green)

On Tier A topology (Alice = stock Core 31 + bip360 enforcer, Bob = `BITCOIND_P2MR`):

1. Fund a Schnorr P2MR UTXO (already works — both mempools).
2. Build an **enforcer-format** Schnorr script-path spend (`signer_dev` / overload leaf).
3. `sendrawtransaction` to **Bob** → **accept**; `getmempoolentry` succeeds.
4. Same for a **kitchen-sink** (Schnorr + ML-DSA-44 + SLH-DSA) spend from a fresh pile.
5. Stock Alice may still reject the spend (that is the CUSF gap, not Tier B).

No `submitblock` fallback counts as success.

## Observed failure (2026-07-16)

| Shape | Bob mempool | Typical reject |
|-------|-------------|----------------|
| Enforcer Schnorr spend | **reject** | `mempool-script-verify-flag-failed (Witness program hash mismatch)` |
| Enforcer hybrid / ML-DSA / kitchen-sink | **reject** | RPC `-26` / script verify |
| Funding into v2 | **accept** | — |

Likely causes (research pointers, not prescriptions):

- Control-block / tapleaf / merkle serialization mismatch between enforcer (`bitcoin-p2mr-pqc` git pin) and P2MR Core (`libbitcoinpqc` 0.4 path).
- Leaf script dialect: enforcer **overload `OP_CHECKSIG`** vs Core hybrid paths that still use **`OP_SUBSTR` / OP_SUCCESS127-style** encodings in places (both treated as valid concurrent approaches for now — see [`TIER_A_P2MR_ALIGNMENT.md`](./TIER_A_P2MR_ALIGNMENT.md)).
- Sighash / annex / leaf version (`0xc0`) edge cases.

## Architecture reminder

```
Alice: stock bitcoind ──ZMQ──► bip360 enforcer   (CUSF tip authority)
Bob:   P2MR bitcoind   (should hold P2MR spends in mempool for Tier B)
```

CUSF does **not** require stock Core to mempool spends. It *does* want a practical path for miners/relays that understand P2MR — Tier B is that path for the dual-stack demo.

## Bounty / collaboration note

This is a good **chip-away** problem for people who enjoy Bitcoin Core script/mempool and BIP 360:

- Failing test is checked in; green CI does not depend on it.
- Fix may land in **P2MR Core** (accept overload leaves), **enforcer builders** (match Core’s accepted serialization), or both — dual-valid dialects mean either side can move.
- Kitchen-sink + Schnorr both required so a Schnorr-only hack is not enough.

**Suggested ask for Paul Sztorc / CUSF community:** whether a modest bounty for turning `just bip360-tier-b-mempool` green (with a short design note) would attract attention. This doc + the trial are the acceptance tests.

Contact / PR target for enforcer work: LayerTwo-Labs `bip300301_enforcer` BIP 360 branch / fork; for Core: [jbride/bitcoin#2](https://github.com/jbride/bitcoin/pull/2) head (`cryptoquick:p2mr`).

## Related

| Doc / recipe | Role |
|--------------|------|
| [`TIER_A_P2MR_ALIGNMENT.md`](./TIER_A_P2MR_ALIGNMENT.md) | Working dual-stack + submitblock fallback |
| [`MEMPOOL_RELAY_POLICY.md`](./MEMPOOL_RELAY_POLICY.md) | Stock vs CUSF mempool policy |
| [`ZMQ_CUSF_BIP360_FINDINGS.md`](./ZMQ_CUSF_BIP360_FINDINGS.md) | Why ZMQ alone cannot inject spends into Core |
| `just bip360-kitchen-sink-tier-a` | Green demo (block path) |
| `just bip360-tier-b-mempool` | This open problem |
