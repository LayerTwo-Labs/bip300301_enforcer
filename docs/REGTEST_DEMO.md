# Regtest demo — BIP 360 CUSF divergence

Walkthrough demonstrating how **stock Bitcoin Core** accepts a block via
`submitblock`, while the **BIP 360 enforcer** rejects it and calls `invalidateblock`.

## Prerequisites

| Requirement | Notes |
|-------------|-------|
| Stock `bitcoind` + `bitcoin-cli` + `bitcoin-util` | No drivechain patches. Use `just setup-core` or a local Core build. |
| Enforcer built with `bip360` | See build commands below. |
| Rust 1.88+ (toolchain pins 1.96.0) | See `rust-toolchain.toml`. |
| Network for git deps | `bitcoin-p2mr-pqc` from `cryptoquick/rust-bitcoin` (p2mr rev); `bitcoinpqc` git pin |

### Build

```bash
cd bip300301_enforcer

just build-bip360
```

Optional: download stock Core into `integrationtests.env`:

```bash
just setup-core
```

## Quick automated path

```bash
just demo-a   # valid Schnorr spend retained
just demo-b   # empty witness → invalidateblock
```

If `integrationtests.env` is missing, recipes exit with instructions unless you pass
`yes` (e.g. `just demo-b yes` — downloads stock Core via `just setup-core`).

This runs the `bip360_invalid_block` integration trial end-to-end. Additional BIP 360
trials (valid Schnorr / ML-DSA / SLH spends, same-block and cross-block invalid sig /
pubkey / merkle-path / hybrid EC+SLH / kitchen-sink / multi-leaf matrix — 33 block-matrix trials + 1 P2P E2E + Tier A kitchen-sink) are registered in
`integration_tests/integration_test.rs` — run with
`cargo run --example integration_tests --features bip360 -- --exact <trial_name>`.
For a guided walkthrough with RPC transcripts, use the manual steps below or:

```bash
just demo-steps
```

## Tier A kitchen-sink demo (stock Core + cryptoquick P2MR)

Five rounds on one sat pile (wallet → Schnorr → hybrid → ML-DSA → **kitchen-sink**), with:

| Peer | Binary |
|------|--------|
| Alice | Stock Core 31 + bip360 enforcer (CUSF tip) |
| Bob | **jbride/bitcoin#2 head** (`cryptoquick:p2mr`, `BITCOIND_P2MR`) |

```bash
just setup              # electrs + stock Core
just setup-p2mr         # symlink cryptoquick build → BITCOIND_P2MR
just bip360-kitchen-sink-tier-a
# or: just bip360-kitchen-sink-tier-a yes
```

Spends try **Bob mempool first**, then submitblock fallback. See
[`TIER_A_P2MR_ALIGNMENT.md`](./TIER_A_P2MR_ALIGNMENT.md) (dual-valid dialects: overload vs OP_SUBSTR).

## Architecture (one diagram)

```
bitcoind (stock)                    bip300301_enforcer (--features bip360)
     │                                        │
     │  ZMQ sequence                          │
     ├──────────────────────────────────────►│ connect_block / validate_tx
     │                                        │
     │◄── submitblock (test harness / miner)  │
     │                                        │
     │  invalidateblock (on reject)           │
     │◄──────────────────────────────────────┤
     │                                        │
     └── active chain may briefly include ────┘ enforcer chain diverges
         block until invalidated
```

Reference: `docs/CUSF-BIP360.md` § Stock Bitcoin Core deployment model.

---

## Manual demo — start services

Ports below are examples; integration tests use random reserved ports. For a fixed
manual setup, pick non-conflicting values (e.g. RPC `19443`, ZMQ `19444`).

### 1. Start stock `bitcoind` (regtest)

```bash
export DEMO_DIR=/tmp/bip360-demo
mkdir -p "$DEMO_DIR/bitcoind"

bitcoind \
  -regtest \
  -datadir="$DEMO_DIR/bitcoind" \
  -rpcuser=demo \
  -rpcpassword=demo \
  -rpcport=19443 \
  -zmqpubsequence=tcp://127.0.0.1:19444 \
  -txindex \
  -acceptnonstdtxn \
  -server -rest -listen=0
```

### 2. Fund a wallet (human step)

```bash
bitcoin-cli -regtest -rpcport=19443 -rpcuser=demo -rpcpassword=demo \
  createwallet demo

ADDR=$(bitcoin-cli -regtest -rpcport=19443 -rpcuser=demo -rpcpassword=demo \
  getnewaddress)

bitcoin-cli -regtest -rpcport=19443 -rpcuser=demo -rpcpassword=demo \
  generatetoaddress 101 "$ADDR"
```

### 3. Start enforcer (BIP 360 validator only)

```bash
cargo build -p bip300301_enforcer --no-default-features --features bip360

./target/debug/bip300301_enforcer \
  --data-dir "$DEMO_DIR/enforcer" \
  --node-rpc-addr 127.0.0.1:19443 \
  --node-rpc-user demo \
  --node-rpc-pass demo \
  --node-zmq-addr-sequence tcp://127.0.0.1:19444 \
  --activation-height 0 \
  --pqc-verify-budget-ms 500 \
  --log-level info
```

Wait until gRPC responds:

```bash
curl -s -X POST -H "Content-Type: application/json" \
  http://localhost:50051/cusf.mainchain.v1.ValidatorService/GetChainTip
```

---

## Scenario A — valid P2MR spend block retained

**Goal:** Show a block with a valid same-block Schnorr P2MR script-path spend is accepted
by both Core and the enforcer.

### Automated (recommended)

```bash
export BIP300301_ENFORCER_INTEGRATION_TEST_ENV=integrationtests.env
just demo-a
# equivalent:
cargo run --example integration_tests --features bip360 -- --exact bip360_valid_schnorr_spend
```

### Manual — valid P2MR output block retained

**Goal:** Show a block that **creates** a valid P2MR output (no spend yet) is accepted
by both Core and the enforcer.

A P2MR `scriptPubKey` is witness v2: `0x52 0x20 <32-byte merkle root>`.

### Demo talking points

1. Core `getblockcount` advances after `submitblock`.
2. Enforcer `GetChainTip` height matches (or exceeds) the submitted block height.
3. `getchaintips` shows the block hash with status `"active"` (not `"invalid"`).
4. Enforcer logs contain **no** `BIP 360 validation failed` / `invalidateblock`.

### How to produce the block

The integration trial `test_bip360_invalid_block` builds a 3-tx block (coinbase,
P2MR funding tx, invalid spend). For Scenario A, submit only the **first two txs**
(coinbase + funding) — or mine normally with `generatetoaddress` and note that
non-P2MR blocks pass through the enforcer unchanged.

**Simplest live demo:** mine one block with `generatetoaddress` and confirm both
tips agree:

```bash
bitcoin-cli -regtest -rpcport=19443 -rpcuser=demo -rpcpassword=demo \
  generatetoaddress 1 "$ADDR"

# Core height
bitcoin-cli -regtest -rpcport=19443 -rpcuser=demo -rpcpassword=demo getblockcount

# Enforcer tip (Connect/gRPC)
curl -s -X POST -H "Content-Type: application/json" \
  http://localhost:50051/cusf.mainchain.v1.ValidatorService/GetChainTip
```

**P2MR-specific demo:** craft a block whose second tx pays to
`5220<32-byte-root>` (valid P2MR structure) using `getblocktemplate` + `grind` —
same pattern as `integration_tests/test_bip360_invalid_block.rs` but omit the
invalid spend tx. Expected: `submitblock` returns empty string (success); enforcer
retains the block.

**Valid P2MR spend via signer:** use [`P2MR_SIGNER.md`](./P2MR_SIGNER.md) to produce
`funding_tx_hex` and `signed_spend_tx_hex`, then submit a block containing coinbase +
funding + signed spend. Enforcer roundtrip coverage: `pqc::spend` tests
`p2mr_signer_roundtrip_*`.

```bash
cargo run --example p2mr_signer --no-default-features --features bip360 -- \
  --algorithm schnorr \
  --entropy-hex 1111111111111111111111111111111111111111111111111111111111111111 \
  | jq '{funding_tx_hex, signed_spend_tx_hex}'
```

---

## Scenario B — invalid P2MR spend → `invalidateblock`

**Goal:** Core accepts the block via `submitblock`; enforcer rejects and invalidates it.

### Automated (recommended)

```bash
export BIP300301_ENFORCER_INTEGRATION_TEST_ENV=integrationtests.env
# or: export BIP300301_ENFORCER_INTEGRATION_TEST_ENV=integration_tests/example.env

cargo run --example integration_tests --features bip360 -- --exact bip360_invalid_block
```

The trial sets `bitcoind_kind: Unpatched` in `SetupOpts`, so it uses stock Core
(`BITCOIND_UNPATCHED` from the env file) — not the patched drivechain `bitcoind`.
Build the enforcer with `--no-default-features --features bip360` before running.

### What the trial does

From `integration_tests/test_bip360_invalid_block.rs`:

1. Starts stock `bitcoind` (`BitcoindKind::Unpatched`) + validator-only enforcer (`--activation-height=0`, `Mode::NoMempool`, ZMQ sync).
2. Builds a 3-tx block (coinbase + funding + spend). Non-coinbase txs:
   - **Tx index 1:** creates P2MR output (`5220` + 32-byte merkle root) — valid.
   - **Tx index 2:** spends that output with an **empty witness** — invalid under BIP 360.
3. `submitblock` → Core accepts (empty response).
4. Enforcer connects block, hits `BIP 360 validation failed`, calls `invalidateblock`.
5. Asserts `getchaintips` status `"invalid"` and log contains `BIP 360 validation failed`.

### RPC checks to demonstrate (after manual submit)

Replace `BLOCKHASH` with the submitted block hash.

```bash
# Core initially accepted submitblock (empty error string)
bitcoin-cli -regtest -rpcport=19443 -rpcuser=demo -rpcpassword=demo \
  submitblock "<hex>"   # → null / empty

# After enforcer processes the block:
bitcoin-cli -regtest -rpcport=19443 -rpcuser=demo -rpcpassword=demo \
  getchaintips | jq '.[] | select(.hash=="BLOCKHASH")'
# → "status": "invalid"

bitcoin-cli -regtest -rpcport=19443 -rpcuser=demo -rpcpassword=demo \
  getblock BLOCKHASH
# → "confirmations": -1  (no longer on active chain)

# Enforcer tip should be below the rejected block height
curl -s -X POST -H "Content-Type: application/json" \
  http://localhost:50051/cusf.mainchain.v1.ValidatorService/GetChainTip
```

### Expected enforcer log lines

Look for (substring match from integration test):

```
BIP 360 validation failed
```

Broader context may include witness / spend validation errors from `pqc::spend`.

---

## Comparison table (talking points)

| Step | Stock Core | BIP 360 enforcer |
|------|------------|------------------|
| `submitblock` (invalid P2MR spend) | Accepts into chain view | N/A (observes via ZMQ) |
| `connect_block` validation | Standard rules only | + P2MR / PQC rules |
| Violation | Ignored | `invalidateblock` |
| `getchaintips` | Block becomes `invalid` | Enforcer tip rewinds |
| Mempool (`--enable-mempool`) | Standard relay | Rejects non-compliant txs earlier |

---

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `bitcoin-p2mr-pqc` resolve error | Needs git access to `https://github.com/cryptoquick/rust-bitcoin` (p2mr rev in workspace `Cargo.toml`). |
| Integration test can't find `bitcoind` | Run `just setup-core` or set `BITCOIND_UNPATCHED` in env file. |
| Enforcer won't start (version check) | Use a supported Core version per `lib/version.rs`, or `--bitcoin-core-skip-version-check`. |
| `bip360_invalid_block` not in test list | Build with `--features bip360`. |
| Enforcer tip never advances | Check ZMQ port matches `bitcoind -zmqpubsequence`. |

---

## Integration trial names

| Trial | Expect |
|-------|--------|
| `bip360_valid_schnorr_spend` | Accepted |
| `bip360_valid_mldsa_spend` | Accepted |
| `bip360_valid_slh_spend` | Accepted |
| `bip360_valid_cross_block_schnorr_spend` | Accepted |
| `bip360_valid_cross_block_mldsa_spend` | Accepted |
| `bip360_valid_cross_block_slh_spend` | Accepted |
| `bip360_valid_hybrid_ec_slh_spend` | Accepted (hybrid EC+SLH, same-block) |
| `bip360_valid_hybrid_ec_slh_cross_block_spend` | Accepted (hybrid EC+SLH, cross-block) |
| `bip360_invalid_block` | Rejected (empty witness) |
| `bip360_invalid_signature` | Rejected (tampered sig, same-block) |
| `bip360_invalid_pubkey_size` | Rejected (ML-DSA sig + 32-byte pubkey, same-block) |
| `bip360_invalid_merkle_path` | Rejected (bad control block, same-block) |
| `bip360_invalid_cross_block_signature_schnorr` | Rejected (tampered Schnorr sig) |
| `bip360_invalid_cross_block_signature_mldsa` | Rejected (tampered ML-DSA-44 sig) |
| `bip360_invalid_cross_block_signature_slh` | Rejected (tampered SLH-DSA sig) |
| `bip360_invalid_cross_block_merkle_path_schnorr` | Rejected (bad control block, Schnorr) |
| `bip360_invalid_cross_block_merkle_path_mldsa` | Rejected (bad control block, ML-DSA) |
| `bip360_invalid_cross_block_merkle_path_slh` | Rejected (bad control block, SLH) |
| `bip360_invalid_cross_block_pubkey_size_mldsa` | Rejected (ML-DSA sig + 32-byte pubkey) |
| `bip360_invalid_hybrid_ec_slh_tamper_ec_sig` | Rejected (tampered EC sig, hybrid) |
| `bip360_invalid_hybrid_ec_slh_tamper_slh_sig` | Rejected (tampered SLH sig, hybrid) |
| `bip360_invalid_hybrid_ec_slh_swap_sigs` | Rejected (swapped EC/SLH sig positions) |
| `bip360_valid_kitchen_sink_spend` | Accepted (kitchen-sink triple-algo leaf, same-block) |
| `bip360_invalid_kitchen_sink_tamper_ec_sig` | Rejected (tampered EC sig, kitchen-sink) |
| `bip360_p2p_mempool_e2e` | Accepted (dual-node P2P E2E; needs `just setup` + electrs) |
| `bip360_valid_multi_leaf_schnorr_spend` | Accepted (three-leaf tree, Schnorr leaf) |
| `bip360_valid_multi_leaf_mldsa_spend` | Accepted (three-leaf tree, ML-DSA leaf) |
| `bip360_valid_multi_leaf_slh_spend` | Accepted (three-leaf tree, SLH leaf) |
| `bip360_valid_multi_leaf_cross_block_mldsa_spend` | Accepted (multi-leaf fund N, ML-DSA spend N+1) |
| `bip360_invalid_multi_leaf_wrong_control_block` | Rejected (wrong control block, same-block) |
| `bip360_invalid_multi_leaf_cross_block_wrong_control_block` | Rejected (wrong control block, cross-block) |
| `bip360_valid_multi_leaf_cross_block_schnorr_spend` | Accepted (multi-leaf fund N, Schnorr spend N+1) |
| `bip360_valid_multi_leaf_cross_block_slh_spend` | Accepted (multi-leaf fund N, SLH spend N+1) |
| `bip360_invalid_multi_leaf_tampered_signature_mldsa` | Rejected (tampered ML-DSA sig, multi-leaf) |

**34 trials total** (33 block-matrix + 1 P2P E2E). Shared block builders: `integration_tests/bip360_block.rs`. Kitchen-sink and P2P harness: `integration_tests/bip360_dual_node.rs`. Multi-leaf design: `docs/MULTI_LEAF_P2MR.md`.

## References

- Integration trials: `integration_tests/test_bip360_*.rs`, `integration_tests/bip360_block.rs`
- Verdict helper: `integration_tests/block_verdict.rs`
- Enforcer rules: `docs/CUSF-BIP360.md`
- Task runner: `Justfile` (`just demo-a`, `just demo-b`, `just it-all`)
- P2MR signer: [`P2MR_SIGNER.md`](./P2MR_SIGNER.md)
- Upstream PR package: `docs/UPSTREAM_PR.md`