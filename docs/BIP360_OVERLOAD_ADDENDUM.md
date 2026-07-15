# BIP 360 leaf-script addendum — CUSF overload model (draft)

**Status:** Draft for BIP 360 authors / upstream discussion. Documents what the
`bip300301_enforcer` prototype enforces today. Not a substitute for the main BIP text.

## Summary

The CUSF enforcer validates P2MR script-path spends using **overloaded** BIP 342
tapscript signature opcodes. Signature algorithm is selected by **witness signature
size** (duck typing), with a **pubkey size consistency** check at each site. The
ref-impl `OP_SUBSTR` (0x7f) PQ tag approach is **explicitly rejected**.

## Overloaded opcodes

No new opcode numbers. These existing tapscript opcodes are overloaded:

| Opcode | Byte | CUSF behavior |
|--------|------|---------------|
| `OP_CHECKSIG` | 0xac | Verify one witness sig against preceding pubkey push |
| `OP_CHECKSIGVERIFY` | 0xad | Same; script fails if invalid |
| `OP_CHECKSIGADD` | 0xba | **One sig site** (not BIP342 accumulator semantics) |
| `OP_CHECKMULTISIG` | 0xae | **N independent sites** (not Bitcoin M-of-N) |
| `OP_CHECKMULTISIGVERIFY` | 0xaf | Same as multisig; fails on invalid sig |

### Per-site verification steps

1. **Pubkey push** immediately precedes the opcode (`PUSH <pk> OP_CHECKSIG`).
2. **Witness sig** (in script order, bottom of stack first) classifies algorithm by size:
   - 64 B → BIP 340 Schnorr (secp256k1)
   - ~2420 B → ML-DSA-44 (FIPS 204)
   - ~7856 B → SLH-DSA-SHA2-128s
3. **Pubkey size** must match scheme: 32 B (Schnorr / SLH), 1312 B (ML-DSA-44).
4. Optional **trailing sighash byte** on witness sig; bare 64-byte Schnorr defaults to `SIGHASH_ALL`.
5. Recompute **tapscript sighash** and verify (Schnorr via secp256k1; PQC via `bitcoinpqc`).

## Rejection of `OP_SUBSTR` tag model

The BIP 360 ref-impl PQC construction vectors (`p2mr_pqc_construction.json`) tag leaves
with `OP_SUBSTR` (0x7f) before `OP_CHECKSIG`. The CUSF enforcer **rejects any leaf
containing `OP_SUBSTR` as an opcode** (`LeafScriptError::OpSubstrForbidden`).

Rationale:

- `OP_SUBSTR` is not a standard tapscript signature opcode; overloading existing
  `OP_CHECKSIG*` keeps script sizes smaller and avoids reserving a general-purpose opcode.
- Algorithm selection from witness sig size matches how verifiers already duck-type
  Schnorr vs PQC on the wire.
- Wallets and signers emit `PUSH <pk> OP_CHECKSIG` only; no separate on-chain algo tag.

Ref-impl vectors must be **converted** for CUSF testing: replace `OP_SUBSTR` + tagged
leaf patterns with `PUSH(32|1312) <pk> OP_CHECKSIG`.

Converted construction vectors and ref-impl id mapping:
[`OVERLOAD_VECTORS.md`](./OVERLOAD_VECTORS.md) (`test_vectors/p2mr_overload_construction.json`).

## Hybrid EC + PQ in one leaf

Multiple signature-check **sites** in one leaf — not one opcode verifying both keys:

```text
PUSH32 <ec_pk>   OP_CHECKSIG
PUSH32 <slh_pk>  OP_CHECKSIG
OP_BOOLAND OP_VERIFY
```

Witness (bottom → top): `[ec_sig, slh_sig, leaf_script, control_block]`.

**Algorithm exclusion** across schemes uses **different merkle leaves** (wallet builds
the tree; enforcer validates only the revealed leaf).

## BIP 342 deviations

### `OP_CHECKSIGADD`

BIP 342: pops `(pubkey, accumulator, sig)`, increments accumulator on success (MuSig2).

CUSF overload: each `OP_CHECKSIGADD` is a **standalone sig site** — one preceding pubkey
push and one witness signature, same as `OP_CHECKSIG`. Stack accumulator semantics are
not implemented.

### `OP_CHECKMULTISIG` / `OP_CHECKMULTISIGVERIFY`

Bitcoin classic: **M-of-N** — witness supplies M signatures for N keys.

CUSF overload: **N-site model** — `OP_0 PUSH pk₁ … PUSH pkₙ OP_N OP_CHECKMULTISIG`
requires **exactly N witness signatures**, one per pubkey, each verified independently
via size-gated duck typing. A 1-of-2 Bitcoin-style single-sig witness is **invalid**.

Example (2-site Schnorr):

```text
OP_0 PUSH32 <pk1> PUSH32 <pk2> OP_2 OP_CHECKMULTISIG
```

Witness sigs: `[sig1, sig2, …]` — two elements before `leaf_script` and `control_block`.

## Witness stack layout

P2MR script-path spend (bottom → top of witness stack):

```text
[signature(s)…]  <leaf_script>  <control_block>
```

- Signature count must equal sig-check site count in `leaf_script`.
- Control block: P2MR format (control byte + merkle path; no internal key / parity).
  - **Control byte:** `0xc1` for the single-leaf script-path used in demos and tests.
  - **Base padding:** 32 zero bytes after the control byte when the merkle branch is empty
    (33 bytes before any sibling hashes). Do not confuse this with the **leaf version**
    byte embedded in tapleaf hashes (`0xc0` / 192).
- Leaf version: `0xc0` (192) unless extended by annex/algo tags in a future spec.

## SHRINCs

SHRINCs / XMSS hybrid backup signatures are **out of scope** in this addendum. See
[`SHRINCS_DEFERRED.md`](./SHRINCS_DEFERRED.md).

## Implementation references

| Component | Location |
|-----------|----------|
| Leaf parser | `lib/validator/pqc/leaf_script.rs` |
| Scheme verification | `lib/validator/pqc/schemes.rs` |
| Spend validation | `lib/validator/pqc/spend.rs` |
| Enforcer docs | [`CUSF-BIP360.md`](./CUSF-BIP360.md) |
| External signer | [`P2MR_SIGNER.md`](./P2MR_SIGNER.md) |