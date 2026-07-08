# Security & design notes — BIP 360 CUSF prototype

Short security and design notes for the BIP 360 CUSF prototype. Complements [`CUSF-BIP360.md`](./CUSF-BIP360.md)
and [`cusf/DESIGN.md`](../../DESIGN.md).

## Why these schemes

| Scheme | Class | Role in prototype |
|--------|-------|-------------------|
| BIP 340 Schnorr | Elliptic curve | Transitional / hybrid EC leg; familiar tapscript path |
| ML-DSA-44 (FIPS 204) | Module lattice | Primary **lattice** option in prototype scope |
| SLH-DSA-SHA2-128s (FIPS 205) | Stateless hash | Primary **hash** option; practical fallback without stateful XMSS |

The prototype scope calls for lattice **and** hash coverage with multiple QR-relevant signature options.
The prototype delivers three live paths (Schnorr + ML-DSA + SLH) on the overloaded
`OP_CHECKSIG` model. **SHRINCs** and **WOTS+** are deferred — see below.

## SHRINCs state handling

**Not applicable in this prototype.** SHRINCs (XMSS normal path + SPHINCS+ backup) requires
reference verifiers, annex/backup sig layout, and consensus vectors none of which exist locally.
See [`SHRINCS_DEFERRED.md`](./SHRINCS_DEFERRED.md).

Design notes reserve a **30 000-byte** backup sig ceiling with annex flag `0x50` for a future
`shrincs` feature — documented only; **no code enforces it today**.

## WOTS+ internals

**Not implemented.** WOTS+ exposure was discussed as a test/migration hook; SLH-DSA covers the
stateless hash role for the prototype. See [`WOTS_DEFERRED.md`](./WOTS_DEFERRED.md).

## Migration thoughts (non-normative)

1. **Algorithm exclusion via merkle leaves** — Wallets can commit to Schnorr, ML-DSA, and SLH
   leaves in one P2MR tree; spenders reveal one leaf. Enforcer validates only the revealed path
   (`multi_leaf.rs`, Phase C integration trials).

2. **Hybrid EC+PQ in one leaf** — Multiple `OP_CHECKSIG` sites + `OP_BOOLAND OP_VERIFY` require
   both signatures (EC+SLH demonstrated in integration trials). Supports gradual migration:
   retain EC leg while adding PQ leg before EC-only deprecation.

3. **Overload vs ref-impl `OP_SUBSTR`** — CUSF uses witness sig-size duck typing on existing
   tapscript opcodes. Ref-impl vectors are **converted** in `test_vectors/p2mr_overload_construction.json`.
   BIP author confirmation pending ([`BIP360_OVERLOAD_ADDENDUM.md`](./BIP360_OVERLOAD_ADDENDUM.md)).

4. **Stock Core relay** — P2MR/PQC txs are not standard on unmodified Core; CUSF mempool
   companion + enforcer required for meaningful end-to-end flows
   ([`MEMPOOL_RELAY_POLICY.md`](./MEMPOOL_RELAY_POLICY.md)).

5. **Activation height** — `--activation-height` gates rules; default `0` is appropriate for
   regtest/signet prototype only. Production deployments need explicit height coordination.

## Open items for support window

- BIP author review of overload wording (human)
- Live regtest execution of 31 integration trials (human env setup)
- SHRINCs / WOTS+ when reference vectors land (deferred)

## References

- Criteria traceability: [`cusf/CRITERIA_AUDIT.md`](../../CRITERIA_AUDIT.md)
- Canonical status: [`cusf/STATUS.md`](../../STATUS.md)
- Upstream PR package: [`UPSTREAM_PR.md`](./UPSTREAM_PR.md)