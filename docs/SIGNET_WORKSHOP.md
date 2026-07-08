# Signet workshop pointer — P2MR / PQC

High-level guide for running a **signet** workshop with BIP 360 P2MR artifacts. Full
signet infrastructure is **not** built in this repo; this doc links existing materials
and notes what the CUSF enforcer would need.

## Prerequisites

The workshop walkthrough lives in the **BIP 360 ref-impl** tree, not in this enforcer repo.
Clone or check out the ref-impl (e.g. `bip-0360/ref-impl` from the
[p2mr-v2 BIP branch](https://github.com/bitcoin/bips/tree/p2mr-v2/bip-0360)) before following
`p2mr-signet-workshop.adoc`.

## Existing artifacts

From [`cusf/ARTIFACTS.md`](../../ARTIFACTS.md) (BIP 360 ref-impl tree):

| Asset | Path (on ref-impl machine) |
|-------|----------------------------|
| End-to-end walkthrough | `bip-0360/ref-impl/rust/docs/p2mr-end-to-end.adoc` |
| **Signet workshop** | `bip-0360/ref-impl/rust/docs/p2mr-signet-workshop.adoc` |
| Signet miner loop | `bip-0360/ref-impl/common/utils/signet_miner_loop.sh` |
| Docker image | `quay.io/jbride2000/bitcoin-cli:p2mr-pqc-0.0.1` |

Public BIP branch: https://github.com/bitcoin/bips/tree/p2mr-v2/bip-0360

## What the workshop covers (ref-impl)

The `p2mr-signet-workshop.adoc` document (ref-impl) typically walks through:

- Custom signet parameters with P2MR-aware tooling
- Funding P2MR outputs on signet
- Script-path spends with PQC signatures (ref-impl `OP_SUBSTR` model)
- Miner loop for block production on a private signet

**CUSF note:** The ref-impl workshop may use `OP_SUBSTR`-tagged leaves. This enforcer
uses the **overload model** (`PUSH <pk> OP_CHECKSIG`). For CUSF demos, convert leaf
scripts per [`BIP360_OVERLOAD_ADDENDUM.md`](./BIP360_OVERLOAD_ADDENDUM.md) and sign with
[`p2mr_signer`](./P2MR_SIGNER.md).

## Running signet with this enforcer (high level)

Stock Bitcoin Core on signet does **not** enforce P2MR/PQC rules. To mirror the regtest
prototype on signet:

```
signet bitcoind ──ZMQ──► bip300301_enforcer (--features bip360)
                              │
                              ├── connect_block / validate_tx (P2MR + PQC)
                              └── invalidateblock on violation
```

### Prerequisites

| Component | Notes |
|-----------|-------|
| Signet `bitcoind` | Workshop signet definition or custom signet params from ref-impl docs |
| `electrs` | Same pattern as regtest integration harness (wallet / indexer) |
| Enforcer binary | `cargo build -p bip300301_enforcer --no-default-features --features bip360` |
| Kellnr registry | `.cargo/config.toml` for `bitcoin-p2mr-pqc` / `bitcoinpqc` |
| Signet miner | `signet_miner_loop.sh` or workshop miner; produces blocks for `submitblock` / ZMQ |

### Enforcer flags (signet)

```bash
bip300301_enforcer \
  --network signet \
  --activation-height <n> \
  --pqc-verify-budget-ms 500 \
  # … standard RPC / ZMQ / electrs flags from integration harness
```

Set `--activation-height` to the signet height where P2MR rules should begin (often `0`
for a dedicated experimental signet).

### Mempool companion (optional)

For tx relay before block inclusion, run with mempool mode enabled and the
[`cusf-enforcer-mempool`](https://github.com/LayerTwo-Labs/cusf-enforcer-mempool) sync
loop so `accept_tx` rejects non-compliant txs early. See
[`MEMPOOL_RELAY_POLICY.md`](./MEMPOOL_RELAY_POLICY.md).

### Signing spends

Use the external signer CLI to produce witness stacks compatible with the overload model:

```bash
cargo run --example p2mr_signer --no-default-features --features bip360 -- \
  --algorithm slh --entropy-hex "<256 hex chars>"
```

## What is **not** in scope here

- Building or hosting a **public** P2MR signet (DNS seeds, fixed challenge, community faucet)
- Patching Bitcoin Core for native P2MR validation
- Automating the full workshop in CI (human-driven workshop remains P3)
- SHRINCs / XMSS paths ([`SHRINCS_DEFERRED.md`](./SHRINCS_DEFERRED.md))

## Suggested workshop flow (CUSF variant)

1. Start signet `bitcoind` per `p2mr-signet-workshop.adoc`.
2. Start enforcer with `bip360` feature and `--activation-height 0`.
3. Mine / submit a funding tx with P2MR output (`5220` + merkle root).
4. Use `p2mr_signer` JSON output to assemble a valid script-path spend.
5. Submit spend block — enforcer retains valid spends; demo invalid spend (empty witness)
   to show `invalidateblock` (same pattern as [`REGTEST_DEMO.md`](./REGTEST_DEMO.md)).

## References

- Regtest demo (automated): [`REGTEST_DEMO.md`](./REGTEST_DEMO.md)
- Enforcer rules: [`CUSF-BIP360.md`](./CUSF-BIP360.md)
- Artifact index: [`cusf/ARTIFACTS.md`](../../ARTIFACTS.md)