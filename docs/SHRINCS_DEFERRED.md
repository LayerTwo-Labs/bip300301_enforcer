# SHRINCs — deferred

**Status:** Not implemented. No local reference implementation or consensus test
vectors.

## Scope

SHRINCs (and related XMSS hybrid backup signature paths mentioned in BIP 360
discussions) are **out of scope** for the current `bip300301_enforcer`
prototype.

The enforcer today supports:

- BIP 340 Schnorr (secp256k1)
- ML-DSA-44
- SLH-DSA-SHA2-128s

via the overloaded `OP_CHECKSIG` family in `lib/validator/pqc/schemes.rs`.

## Why deferred

| Gap            | Detail                                                                 |
| -------------- | ---------------------------------------------------------------------- |
| Reference impl | No SHRINCs verifier in `bip-0360/ref-impl` or `libbitcoinpqc-bindings` |
| Test vectors   | No JSON construction / spend vectors for SHRINCs leaves                |
| Spec stability | Backup sig format, annex flags, and size limits still exploratory      |
| Related work   | SLH-DSA + XMSS research repos (not SHRINCs) — no local verifier wired  |

Per [`DESIGN.md`](../../DESIGN.md) prototype limits, a SHRINCs backup signature
might allow up to **30 000 bytes** when annex flag `0x50` is present — that
limit is documented in design notes only; **no code enforces it**.

## Future feature flag

A reserved Cargo feature exists on `bip300301_enforcer_lib`:

```toml
# lib/Cargo.toml
shrincs = []   # placeholder — no dependencies wired
```

`lib/validator/pqc/mod.rs` notes that a future `shrincs` feature may gate
optional backup-signature verification when reference vectors land.

**Do not enable `shrincs` expecting functionality** — it is a naming reservation
only.

## When to revisit

1. SHRINCs reference implementation merged in BIP 360 ref-impl (Rust or C++).
2. Construction + spend JSON vectors published.
3. `bitcoinpqc` (or successor crate) exposes verify API with stable ABI.
4. BIP text defines leaf script + annex layout for backup sigs.

Then:

- Add `shrincs` feature dependencies and `pqc/shrincs.rs` (or extend
  `schemes.rs`).
- Wire activation height / annex parsing in `spend.rs`.
- Add unit + integration tests before enabling by default in `bip360`.

## References

- Design limits: [`cusf/DESIGN.md`](../../DESIGN.md) § Weight / DoS limits
- Overload model (current):
  [`BIP360_OVERLOAD_ADDENDUM.md`](./BIP360_OVERLOAD_ADDENDUM.md)
- Artifact index: [`cusf/ARTIFACTS.md`](../../ARTIFACTS.md)
