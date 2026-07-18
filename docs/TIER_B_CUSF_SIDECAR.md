# Tier B — CUSF miner sidecar (TB-sidecar, **PASS**)

Hold enforcer-format raw transactions in process memory, assemble a block
(coinbase + inventory), and `submitblock` to **stock** Bitcoin Core. The bip360
enforcer still gates the tip (`invalidateblock` / keep). This is **TB-mine as a
service** — not a mempool, not a greening of TB-sendraw.

```
builders ──HTTP──► cusf-miner-sidecar ──submitblock──► stock bitcoind
                         │
                         └── enforcer tip check (existing trial pattern)
```

**Last verified:** 2026-07-16 — `just bip360-tier-b-cusf-sidecar` **PASS**
(stock Core 31).

## What this is / is not

| Is                                        | Is not                                       |
| ----------------------------------------- | -------------------------------------------- |
| Inventory of raw enforcer-format txs      | Stock Core **mempool** for witness-v2 spends |
| Block build + `submitblock` to stock Core | Greening `sendrawtransaction` / TB-sendraw   |
| Local demo / builder helper               | Public multi-tenant mining API               |
| Localhost bind by default                 | Safe to expose on `0.0.0.0` without auth     |

Soft-fork rules still live in the **enforcer**. Core stays unpatched.

## Run (integration trial)

```bash
just bip360-tier-b-cusf-sidecar
# or: just it bip360_tier_b_cusf_sidecar
```

Trial: **`bip360_tier_b_cusf_sidecar`**  
Harness: `integration_tests/test_bip360_tier_b_cusf_sidecar.rs`  
Library: `cusf_miner_sidecar` (in-process against harness bitcoind)

Needs stock Core (`just setup-core` / `BITCOIND_UNPATCHED`). No `BITCOIND_P2MR`,
no electrs.  
**Not** in `just it-all` / green block matrix (matrix stays **34** = classic +
TB-mine).

### Pass criteria

1. One stock Core + bip360 enforcer (`Mode::NoMempool`, Unpatched).
2. Enforcer-format **Schnorr** then **kitchen-sink** spends loaded into sidecar
   inventory.
3. Sidecar mines each pair (funding + spend) via GBT + assemble + `submitblock`.
4. Stock tip retained before and after enforcer `Expect::Accepted` (no
   invalidate).
5. Kitchen-sink: witness depth 5, three algos, weight **> 10_000** WU.
6. Log path `cusf_sidecar`.

## HTTP binary (local only)

```bash
cargo run -p cusf_miner_sidecar -- \
  --bitcoind-rpc 127.0.0.1:18443 \
  --rpc-user USER --rpc-pass PASS \
  --coinbase-address bcrt1q... \
  --port 18450
```

Default bind is **`127.0.0.1`**. Non-loopback requires `--allow-non-loopback`
(unauthenticated inventory — do not expose).

| Method   | Path                  | Body / notes                                                          |
| -------- | --------------------- | --------------------------------------------------------------------- |
| `GET`    | `/health`             | `{"ok":true}`                                                         |
| `POST`   | `/tx`                 | `{"hex":"..."}` or raw hex → store; **duplicate txid → 400**          |
| `GET`    | `/tx` or `/inventory` | list txids                                                            |
| `DELETE` | `/inventory`          | clear                                                                 |
| `POST`   | `/mine` or `/submit`  | build + `submitblock`; snapshot via `take_all` (concurrent adds kept) |

### Inventory rules

| Rule                 | Behavior                                                                                                                                 |
| -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| Duplicate `txid`     | **Error** (`DuplicateTxid` / HTTP 400) — not silently ignored                                                                            |
| Max txs              | Default **100** (`DEFAULT_MAX_TXS`)                                                                                                      |
| Max serialized bytes | Default **4 MiB** (`DEFAULT_MAX_BYTES`)                                                                                                  |
| Coinbase value       | **Inventory-only**: GBT `coinbasevalue` minus fees of dropped `template.transactions` (subsidy-only; never overclaim)                    |
| Concurrent mine/add  | `mine` takes a snapshot; concurrent adds after snapshot survive; failed mine restores taken set with caps re-enforced (overflow dropped) |

## Library API (preferred in tests)

```rust
use cusf_miner_sidecar::{MinerSidecar, RpcConfig, connect_bitcoind};

let client = connect_bitcoind(&RpcConfig::new(addr, user, pass))?;
let sidecar = MinerSidecar::new(client, coinbase_spk);
sidecar.add_tx_hex(&hex)?; // Err on duplicate / cap
let result = sidecar.mine().await?; // block_hash, height, tx_count
```

## Related

| Item                                                   | Role                                                                               |
| ------------------------------------------------------ | ---------------------------------------------------------------------------------- |
| [`TIER_B_CUSF_MINER.md`](./TIER_B_CUSF_MINER.md)       | TB-mine single-process + TB-factory dual-process                                   |
| `just bip360-tier-b-cusf`                              | Green CUSF mining path (also in `it-all`)                                          |
| `just bip360-tier-b-cusf-sidecar`                      | This helper (not in `it-all`)                                                      |
| [`TIER_B_P2MR_MEMPOOL.md`](./TIER_B_P2MR_MEMPOOL.md)   | TB-sendraw Bob mempool **PASS** shapes 1+2+**3** (kitchen-sink green post-Core 3B) |
| [`MEMPOOL_RELAY_POLICY.md`](./MEMPOOL_RELAY_POLICY.md) | Why stock mempool rejects v2 spends                                                |
