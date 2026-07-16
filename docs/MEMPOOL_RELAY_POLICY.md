# Mempool relay policy — stock Core vs CUSF companion

Why **stock Bitcoin Core** does **not** treat P2MR as a first-class program and will
**not** admit witness-v2 **spends** (or full CUSF-valid PQC packages that only make sense
as P2MR spends) into its mempool path the way a BIP 360 node would—and what the
**cusf-enforcer-mempool** companion does today in this prototype.

Pure v2 **funding** outputs are a different case: on stock Core 31 they are generally
mempool-admissible as `WITNESS_UNKNOWN` (see table below and
[`ZMQ_CUSF_BIP360_FINDINGS.md`](./ZMQ_CUSF_BIP360_FINDINGS.md) §2.3).

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

**Funding vs spending on stock Core 31** (see also
[`ZMQ_CUSF_BIP360_FINDINGS.md`](./ZMQ_CUSF_BIP360_FINDINGS.md) §2.3):

| Kind | Stock Core default mempool | Why |
|------|----------------------------|-----|
| **Funding** (pay *into* v2 `5220…`) | Generally **admissible** | Outputs of type `WITNESS_UNKNOWN` are standard on stock Core; Core does not treat them as P2MR, but also does not reject pure v2 outputs solely for version |
| **Spend** (spend a v2 prevout) | **Rejected** | Under defaults: `AreInputsStandard` rejects `WITNESS_UNKNOWN` inputs; even if standardness is relaxed, `SCRIPT_VERIFY_DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM` fails the spend. **No** ZMQ mempool `'A'` for the spend |

Additional reasons CUSF-valid packages may still fail stock mempool policy:

1. **Script validation for spends** — Core does not implement P2MR / PQC overload semantics; upgradable witness programs are discouraged in mempool script checks.
2. **Witness weight** — Large PQC signatures (~2.4 KB / ~7.8 KB) may hit standardness or policy size limits.
3. **Relay** — Without mempool admission, P2MR spends do not propagate on the ordinary tx relay path.

**Blocks** mined without an enforcer can include txs that violate BIP 360 rules; Core
will accept them under its own soft-fork-placeholder rules for unknown witness versions.

## Tier A dual stack (stock + P2MR peer)

Stock Core alone cannot demo mempool-path P2MR *spends*. Tier A pairs:

| Peer | Role |
|------|------|
| Stock Core 31 + enforcer | CUSF tip; funding mempool; spend path = block/`submitblock` |
| P2MR Core ([jbride/bitcoin#2](https://github.com/jbride/bitcoin/pull/2) head / `cryptoquick:p2mr`) | May admit v2 spends to mempool; mines when interop allows |

Kitchen-sink demo: `just bip360-kitchen-sink-tier-a`. Alignment / dual-valid dialects:
[`TIER_A_P2MR_ALIGNMENT.md`](./TIER_A_P2MR_ALIGNMENT.md).
The CUSF enforcer then **`invalidateblock`** on `connect_block` when violations are
detected (see [`REGTEST_DEMO.md`](./REGTEST_DEMO.md)).

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
`BlockHandler::validate_tx` → `pqc::validate_mempool_transaction`.

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
- PQC mempool hook: `lib/validator/task/mod.rs` (`validate_mempool_transaction`)
- Design overview: [`cusf/DESIGN.md`](../../DESIGN.md)
- Core 31 ZMQ inventory + spend-path limits: [`ZMQ_CUSF_BIP360_FINDINGS.md`](./ZMQ_CUSF_BIP360_FINDINGS.md)