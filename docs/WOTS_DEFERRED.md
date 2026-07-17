# WOTS+ — deferred

**Status:** Not implemented. No local verifier, leaf-script path, or consensus
test vectors.

## Scope

WOTS+ (Winternitz one-time signature internals mentioned in BIP 360 /
quantum-hourglass discussions) is **out of scope** for the current
`bip300301_enforcer` prototype.

The enforcer today supports:

- BIP 340 Schnorr (secp256k1)
- ML-DSA-44
- SLH-DSA-SHA2-128s

via the overloaded `OP_CHECKSIG` family in `lib/validator/pqc/schemes.rs`.

WOTS+ was discussed as an **exposed internal** for tests and migration
experiments (alongside SLH-DSA as a stateless hash fallback). No separate WOTS+
duck-typing size class or pubkey layout is wired in the overload model.

## Why deferred

| Gap            | Detail                                                               |
| -------------- | -------------------------------------------------------------------- |
| Reference impl | No WOTS+ verifier in `bip-0360/ref-impl` or `libbitcoinpqc-bindings` |
| Test vectors   | No JSON construction / spend vectors for WOTS+ leaves                |
| Spec stability | WOTS+ role vs SLH-DSA / SHRINCs backup paths still exploratory       |
| Overload model | No sig-size / pubkey-size table entry for WOTS+ in `schemes.rs`      |

## Relationship to SLH-DSA

SLH-DSA-SHA2-128s is implemented as the **stateless hash** path in the prototype
scope as the practical fallback. WOTS+ exposure would require a distinct leaf
script layout and verifier — not a thin alias of SLH-DSA.

## When to revisit

1. WOTS+ reference implementation merged in BIP 360 ref-impl (Rust or C++).
2. Construction + spend JSON vectors published with stable sig/pubkey sizes.
3. `bitcoinpqc` (or successor crate) exposes WOTS+ verify API with stable ABI.
4. BIP text defines how WOTS+ leaves signal in tapscript (overload sizes or
   annex).

Then:

- Extend `schemes.rs` duck-typing table and `leaf_script.rs` walker rules.
- Add unit + integration tests before enabling by default in `bip360`.
- Document migration from WOTS+-only leaves to SLH-DSA / SHRINCs hybrid paths.

## References

- Criteria audit: [`cusf/CRITERIA_AUDIT.md`](../../CRITERIA_AUDIT.md) PQ-06
- SHRINCs deferral (related backup-sig path):
  [`SHRINCS_DEFERRED.md`](./SHRINCS_DEFERRED.md)
- Design overview: [`cusf/DESIGN.md`](../../DESIGN.md) § Deferred
- Artifact index: [`cusf/ARTIFACTS.md`](../../ARTIFACTS.md)
