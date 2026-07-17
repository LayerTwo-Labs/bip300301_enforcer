# Tier B — CUSF mining path (TB-mine, **PASS**)

Soft-fork rules live in the **enforcer**. Stock Bitcoin Core stays unpatched.  
Enforcer-built P2MR spends are included with **block assembly +
`submitblock`**.  
The enforcer keeps the tip. Core’s mempool does not need to accept witness-v2
spends.

This is the **CUSF-facing Tier B** (see
[CUSF paper](https://bip300cusf.com/cusf.pdf): separate activator, tip via
`invalidateblock` / keep tip).

**Last verified:** 2026-07-16 — `just bip360-tier-b-cusf` **PASS** (stock Core
31);  
`just bip360-tier-b-cusf-factory` **PASS** (dual stock Alice+Miner).

## Run

```bash
just bip360-tier-b-cusf
# or: just it bip360_tier_b_cusf_miner
```

Also included in green block matrix: `just it-all` / `just bip360-block-matrix`.

Trial: **`bip360_tier_b_cusf_miner`**  
Harness: `integration_tests/test_bip360_tier_b_cusf_miner.rs`

Needs stock Core (`just setup-core` / `BITCOIND_UNPATCHED`). No `BITCOIND_P2MR`.

## Pass criteria

1. One stock Core 31 + bip360 enforcer (**no** `BITCOIND_P2MR`).
2. Fund P2MR outputs (block-harness coinbase funding is OK; distinct mature
   coinbases for the two spends).
3. Enforcer-format **Schnorr** + **kitchen-sink** spends included via stock Core
   `submitblock` / `build_block_with_coinbase`.
4. Stock tip retained; enforcer accepts (no `invalidateblock`); kitchen-sink
   weight **> 10_000** WU and **three** algos.
5. Log path `cusf_mine` (not Bob / P2MR Core mempool).

## What this does not claim

- Stock Core **mempool** admits v2 spends (it does not; that is policy, not CUSF
  failure).
- That every enforcer leaf format is also Bob-mempool-legal — TB-sendraw
  ([`TIER_B_P2MR_MEMPOOL.md`](./TIER_B_P2MR_MEMPOOL.md) /
  `just bip360-tier-b-mempool`) is **PASS** for shapes 1 Schnorr + 2 Core hybrid
  `OP_SUBSTR` + **3** overload kitchen-sink (post-Core 3B). See
  [`TIER_B_PROTOCOL_GREEN.md`](./TIER_B_PROTOCOL_GREEN.md).

## Dual-process factory (TB-factory, optional)

Same pass shape as TB-mine, but blocks are produced by a **second stock
bitcoind** (the Miner). Alice remains the user-facing tip node with the bip360
enforcer.

| Role      | Process                       | Job                                                    |
| --------- | ----------------------------- | ------------------------------------------------------ |
| **Alice** | Stock Core + bip360 enforcer  | Tip authority; enforcer keeps tip (`Expect::Accepted`) |
| **Miner** | Second stock Core (Unpatched) | Block factory only: GBT + assemble + `submitblock`     |

```bash
just bip360-tier-b-cusf-factory
```

Primary recipe rebuilds the enforcer with `drivechain,bip360` and preflights
stock Core.  
Raw `just it bip360_tier_b_cusf_factory` also works when `integrationtests.env`
already points at a bip360-capable enforcer binary and stock
`BITCOIND_UNPATCHED` (no electrs required).

Trial: **`bip360_tier_b_cusf_factory`**  
Harness: `integration_tests/test_bip360_tier_b_cusf_factory.rs`  
Log path: `cusf_mine_factory`

Needs stock Core (`just setup-core` / `BITCOIND_UNPATCHED`). **No** electrs,
**no** `BITCOIND_P2MR`.  
Both peers use `Mode::NoMempool`. Alice runs bip360 (`--activation-height=0`) as
tip gate; Miner’s structural enforcer uses `--activation-height=1000000` so it
never tip-gates during the trial (factory-only: GBT + assemble +
`submitblock`).  
**Not** in `just it-all` / block matrix (dual-process is slower; single-node
TB-mine covers green CI).

### Pass criteria (factory)

1. Two peered regtest stock bitcoinds: Alice (tip + enforcer) and Miner
   (factory).
2. Same spends as TB-mine: enforcer-format **Schnorr** then **kitchen-sink**
   (weight > 10k, 3 algos, witness depth 5).
3. Blocks produced/submitted by **Miner** (not Alice assembling alone); Alice
   receives tip via P2P.
4. Alice `getbestblockhash` matches submitted block before and after enforcer
   `Expect::Accepted` (no invalidate).

Funding coinbases mature on Alice; funding txs are wallet-signed on Alice; Miner
only mines.

Does **not** claim stock mempool admits v2 spends (same as TB-mine).

## Related

| Item                                                     | Role                                                                                                                        |
| -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| `just bip360-tier-b-cusf` / `it-all`                     | Single-process TB-mine (green; in block matrix)                                                                             |
| `just bip360-tier-b-cusf-factory`                        | Dual-process factory (Alice tip + Miner factory; not in `it-all`)                                                           |
| `just bip360-tier-b-cusf-sidecar`                        | TB-sidecar miner helper (inventory → `submitblock`; not in `it-all`) — [`TIER_B_CUSF_SIDECAR.md`](./TIER_B_CUSF_SIDECAR.md) |
| `just bip360-tier-b-mempool`                             | TB-sendraw Bob mempool **PASS** shapes 1+2+**3** (kitchen-sink green post-Core 3B)                                          |
| `just bip360-kitchen-sink-tier-a`                        | Dual-stack demo (stock + P2MR Bob; often submitblock fallback)                                                              |
| `just bip360-p2p-e2e`                                    | Dual stock P2P E2E (`bip360_p2p_mempool_e2e`; not the green 34-count)                                                       |
| [`TIER_B_CUSF_SIDECAR.md`](./TIER_B_CUSF_SIDECAR.md)     | TB-mine as a service (localhost HTTP / library)                                                                             |
| [`TIER_B_P2MR_MEMPOOL.md`](./TIER_B_P2MR_MEMPOOL.md)     | Red twin + community bounty note                                                                                            |
| [`TIER_A_P2MR_ALIGNMENT.md`](./TIER_A_P2MR_ALIGNMENT.md) | Tier A dual-stack roles / interop matrix                                                                                    |
| [`MEMPOOL_RELAY_POLICY.md`](./MEMPOOL_RELAY_POLICY.md)   | Why stock Core rejects v2 _spends_ in mempool                                                                               |
