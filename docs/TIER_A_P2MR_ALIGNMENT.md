# Tier A — P2MR peer alignment (CUSF Core + cryptoquick P2MR)

## Roles

| Role             | Binary                                                                                                                      | Job                                                                                                                           |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| **Alice (CUSF)** | Stock Bitcoin Core 31 (`BITCOIND_UNPATCHED`) + `bip360` enforcer                                                            | Tip authority; funding mempool; rejects witness-v2 _spends_ in mempool                                                        |
| **Bob (P2MR)**   | **jbride/bitcoin#2 head** = [`cryptoquick/bitcoin`](https://github.com/cryptoquick/bitcoin) branch `p2mr` (`BITCOIND_P2MR`) | P2MR-capable peer; admits enforcer-format v2 spends only when protocol matches (today: usually reject → submitblock fallback) |

**Source of truth for Bob:**
[jbride/bitcoin PR #2](https://github.com/jbride/bitcoin/pull/2) **head**
(author cryptoquick), not jbride’s base `p2mr` until that PR is merged.

Setup:

```bash
just setup          # stock Core + electrs (if needed)
just setup-p2mr     # symlink cryptoquick build → .integration-deps/bitcoin-p2mr + BITCOIND_P2MR
just bip360-kitchen-sink-tier-a
```

## Competing P2MR protocols — both valid (for now)

Leaf-script / control-block rules were never fully finalized across
implementations. Call them **protocols** (Bitcoin script + commitment rules),
not informal “dialects.”

| Horizon           | Stance                                                                                                                                      |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| **Future P2MRv2** | Overloaded `OP_CHECKSIG` (size-gated PQC) is the intended direction                                                                         |
| **Today**         | **Both** overload (this enforcer) and OP_SUBSTR / OP_SUCCESS127-style leaves (some jbride hybrid paths) are **valid concurrent approaches** |

|                                 | Enforcer (this repo)                | cryptoquick/jbride P2MR Core                     |
| ------------------------------- | ----------------------------------- | ------------------------------------------------ |
| Kitchen-sink / multi-sig leaves | Triple overload `OP_CHECKSIG`       | May use `OP_SUBSTR` in combined hybrid encodings |
| OP_SUBSTR                       | Rejected here (`OpSubstrForbidden`) | Used in some hybrid leaf shapes                  |
| Product claim                   | CUSF validates **overload** spends  | Bob validates **its** P2MR rules                 |

This is not a correctness fight. Interop is empirical: same tip works when both
sides accept the encoding, or when stock Alice treats undefined v2 as
anyone-can-spend and the enforcer is the real check (classic CUSF).

## Demo policy

1. Kitchen-sink **climax** uses enforcer-format (overload) spends — CUSF
   product.
2. Tier A tries **Bob mempool first**; on reject, **submitblock fallback**
   (WARN) unless `BIP360_TIER_A_STRICT=1`.
3. Dual-stock P2P E2E (`just bip360-p2p-e2e` / `bip360_p2p_mempool_e2e`) stays
   dual-stock regression (submitblock spends).

## Interop matrix (live 2026-07-16)

Source binary: `cryptoquick/bitcoin` `p2mr` @ PR #2 era build with
**`WITH_ZMQ=ON`** (required for enforcer attachment).  
Trial: `just bip360-kitchen-sink-tier-a` → **PASS** (kitchen-sink weight
**12102** WU, algos schnorr+mldsa+slh).

| Round / shape                   | Enforcer-built | Bob mempool                                  | Path used     | Notes                                            |
| ------------------------------- | -------------- | -------------------------------------------- | ------------- | ------------------------------------------------ |
| 0 Funding → Schnorr P2MR        | wallet         | **accept** (Alice+Bob)                       | mempool       | Both peers                                       |
| 1 Schnorr spend → hybrid        | yes            | **reject** (`Witness program hash mismatch`) | `SubmitBlock` | Protocol / control-block mismatch                |
| 2 Hybrid EC+SLH → ML-DSA        | yes            | **reject** (`-26`)                           | `SubmitBlock` |                                                  |
| 3 ML-DSA → kitchen-sink out     | yes            | **reject** (`-26`)                           | `SubmitBlock` |                                                  |
| 4 Kitchen-sink triple-algo      | yes            | **reject** (`-26`)                           | `SubmitBlock` | Climax still green via CUSF tip                  |
| jbride SUBSTR hybrid → enforcer | n/a            | n/a                                          | n/a           | Expect enforcer reject — dual-valid, not “fixed” |

**Takeaway:** Tier A topology works (stock Alice + cryptoquick Bob, dual tip
retention). Enforcer-format spends still need **submitblock** on this Bob build
(both protocols valid; overload not yet mempool-admitted on Bob). Future
overload alignment on `cryptoquick:p2mr` can flip rounds to `P2mrMempool`
without harness rewrite.

**Tier B (split):**

- **TB-mine** CUSF mining (green):
  [`TIER_B_CUSF_MINER.md`](./TIER_B_CUSF_MINER.md) / `just bip360-tier-b-cusf`
- **TB-sendraw** Bob mempool **PASS** shapes 1 Schnorr + 2 Core hybrid + 3
  kitchen-sink: [`TIER_B_P2MR_MEMPOOL.md`](./TIER_B_P2MR_MEMPOOL.md) /
  `just bip360-tier-b-mempool`

Build note: default cmake had `WITH_ZMQ=OFF` — enforcer needs ZMQ. Rebuild:

```bash
cd ~/Projects/cryptoquick/bitcoin
cmake -B build -DWITH_ZMQ=ON
cmake --build build -j"$(nproc)" --target bitcoind bitcoin-cli
just setup-p2mr   # from bip300301_enforcer
```

Logs from `just bip360-kitchen-sink-tier-a` (or `--log-level info`) include
per-round `SpendConfirmPath` (`P2mrMempool` vs `SubmitBlock`).

## Related

- Trial: `bip360_kitchen_sink_tier_a`
- Harness: `integration_tests/bip360_dual_node.rs`,
  `test_bip360_kitchen_sink_tier_a.rs`
- Stock dual: `bip360_p2p_mempool_e2e` (not the green 34-count)
- Tier B green (TB-mine): [`TIER_B_CUSF_MINER.md`](./TIER_B_CUSF_MINER.md)
- Tier B TB-sendraw (**PASS** shapes 1 Schnorr + 2 Core hybrid + 3
  kitchen-sink): [`TIER_B_P2MR_MEMPOOL.md`](./TIER_B_P2MR_MEMPOOL.md)
- Mempool policy: [`MEMPOOL_RELAY_POLICY.md`](./MEMPOOL_RELAY_POLICY.md)
- ZMQ/CUSF: [`ZMQ_CUSF_BIP360_FINDINGS.md`](./ZMQ_CUSF_BIP360_FINDINGS.md)
