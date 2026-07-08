# Mempool relay policy — stock Core vs CUSF companion

Why **stock Bitcoin Core** will not relay or accept P2MR/PQC transactions into its
mempool without a CUSF enforcer, and what the **cusf-enforcer-mempool** companion does
today in this prototype.

## Stock Core behavior

Unmodified Bitcoin Core validates transactions against **its** consensus and standardness
rules only:

| Check | Stock Core | BIP 360 CUSF enforcer |
|-------|------------|------------------------|
| Witness v2 `5220<32>` outputs | Not treated as standard P2MR | Valid P2MR `scriptPubKey` |
| P2MR script-path witness layout | No P2MR spend rules | `[sigs…] script control_block` |
| Overloaded `OP_CHECKSIG` (PQC sizes) | Tapscript assumes Schnorr-sized sigs | Duck-types ML-DSA / SLH / Schnorr |
| `OP_SUBSTR` PQ tags | N/A (non-standard / invalid in tapscript context) | **Rejected** (overload model) |
| Per-block PQC verify budget | N/A | Configurable wall-time cap |

A P2MR funding or spend transaction that is valid under CUSF rules will typically be
**rejected at mempool admission** on stock Core because:

1. **Output type** — v2 witness programs with 32-byte commitments are not recognized as
   standard pay-to-taproot/P2WPKH patterns Core expects.
2. **Script validation** — Even if relayed, tapscript execution does not implement PQC
   signature verification or the CUSF overload semantics for `OP_CHECKSIGADD` / multisig.
3. **Witness weight** — Large PQC signatures (~2.4 KB / ~7.8 KB) may hit standardness
   or policy limits before custom rules are considered.

**Blocks** mined without an enforcer can include txs that violate BIP 360 rules; Core
will accept them under its own rules. The CUSF enforcer then **`invalidateblock`** on
`connect_block` when violations are detected (see [`REGTEST_DEMO.md`](./REGTEST_DEMO.md)).

## CUSF mempool companion role

Architecture (from [`cusf/DESIGN.md`](../../DESIGN.md)):

```
bitcoind ──ZMQ sequence──► cusf-enforcer-mempool ──CusfEnforcer trait──► validator
                                    │
                                    ├── accept_tx (mempool admission)
                                    └── sync_to_tip / connect_block / disconnect_block
```

The companion (`cusf-enforcer-mempool`) subscribes to ZMQ, tracks mempool sequence, and
calls the enforcer's `CusfEnforcer` implementation:

| Method | Purpose |
|--------|---------|
| `accept_tx` | Mirror block rules at mempool admission; reject non-compliant txs early |
| `connect_block` | Validate block txs; return `RejectBlock` → `invalidateblock` |
| `disconnect_block` | Rewind on reorg |
| `sync_to_tip` | Catch up to chain tip |

In `bip300301_enforcer`, `Validator` implements `CusfEnforcer` in
`lib/validator/cusf_enforcer.rs`. With `bip360` enabled, `accept_tx` routes through
`BlockHandler::validate_tx` → `quantum::validate_mempool_transaction`.

### What `accept_tx` enforces today (BIP 360)

When `--activation-height` is reached:

- P2MR output `scriptPubKey` structure (witness v2, 32-byte merkle root)
- P2MR script-path spend witness layout and merkle path
- Overloaded signature verification (Schnorr / ML-DSA-44 / SLH-DSA-SHA2-128s)
- Leaf script rules (no `OP_SUBSTR`; sig site count matches witness)
- DoS limits (witness stack depth, per-input sig weight)

See [`CUSF-BIP360.md`](./CUSF-BIP360.md) for opcode and sighash details.

### Mempool vs block asymmetry

**Per-block PQC verify budget** (`--pqc-verify-budget-ms`) applies during
`connect_block` only. Mempool `accept_tx` does **not** accumulate the block budget
(`pqc_budget` is unset in `validate_mempool_transaction`).

A transaction can pass mempool validation and still cause **block rejection** when
batched with other PQC spends that exhaust the per-block verify budget.

### Relay to peers

The mempool companion filters what the **local** node treats as acceptable for block
template assembly (`getblocktemplate` path). It does **not** change Bitcoin Core's P2P
relay logic inside `bitcoind`. Other peers on the network still use stock relay policy
unless they also run a CUSF stack.

Practical effect for the prototype:

- **With enforcer + mempool mode:** locally mined blocks should only include CUSF-compliant
  txs the validator accepted.
- **Without enforcer:** txs may enter Core's mempool or blocks under legacy rules only;
  P2MR/PQC spends are not meaningfully supported end-to-end.

## Operational modes

| Mode | Mempool `accept_tx` | Block `connect_block` |
|------|---------------------|------------------------|
| Enforcer `NoMempool` | Not used | Validates; `invalidateblock` on fail |
| Enforcer with mempool sync | Validates | Validates + budget |

Integration tests often use `Mode::NoMempool` for simpler block-only trials
(`test_bip360_invalid_block`).

## Signing and relay workflow

1. Build P2MR output and signed spend with [`P2MR_SIGNER.md`](./P2MR_SIGNER.md).
2. Submit via `submitblock` (regtest/signet workshop) or local miner — not via ordinary
   `sendrawtransaction` on stock Core for PQC-sized witnesses.
3. Run enforcer with `bip360` so `connect_block` / `accept_tx` enforce overload rules.

## References

- Mempool crate: [`cusf-enforcer-mempool`](https://github.com/LayerTwo-Labs/cusf-enforcer-mempool)
- Enforcer trait impl: `lib/validator/cusf_enforcer.rs`
- Quantum mempool hook: `lib/validator/task/mod.rs` (`validate_mempool_transaction`)
- Design overview: [`cusf/DESIGN.md`](../../DESIGN.md)