# Overload-model P2MR construction vectors

CUSF-specific JSON v1 vectors mirroring BIP 360 ref-impl tree shapes with **overloaded**
`OP_CHECKSIG` leaves (no `OP_SUBSTR`).

| File | Tests |
|------|-------|
| `test_vectors/p2mr_overload_construction.json` | `lib/validator/quantum/overload_vectors.rs` (`quantum::overload_vectors::`) |

Run:

```bash
cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 quantum::overload_vectors
```

Regenerate golden `intermediary` / `expected` fields after editing `given.scriptTree`:

```bash
cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 dump_overload_golden -- --ignored --nocapture
```

## Ref-impl → overload mapping

| Ref-impl id | Overload id | Notes |
|-------------|-------------|-------|
| `p2mr_missing_leaf_script_tree_error` | `overload_missing_leaf_script_tree_error` | Error vector; empty `given` |
| `p2mr_single_leaf_script_tree` | `overload_single_slh_leaf` | `PUSH32 pk OP_SUBSTR` → `PUSH32 pk OP_CHECKSIG`; merkle root changes |
| `p2mr_two_leaf_same_version` | `overload_two_leaf_same_version` | SLH leaf converted; plain `Taproot` push leaf unchanged |
| `p2mr_three_leaf_complex` | `overload_three_leaf_complex` | Nested three-leaf tree; all SLH `OP_SUBSTR` tags converted |
| `p2mr_simple_lightning_contract` | `overload_simple_lightning_contract` | Tree shape preserved; see lightning notes below |
| — | `overload_single_mldsa_leaf` | **CUSF-native** single ML-DSA-44 leaf (`PUSHDATA2(1312) pk OP_CHECKSIG`); entropy `0x42×128` |
| `p2mr_different_version_leaves` | *(not included)* | Non-PQ / mixed leaf versions; defer until needed |
| `p2mr_three_leaf_alternative` | *(not included)* | Same conversion pattern as `three_leaf_complex`; omitted to limit suite size |

## JSON encoding (`given` vs `expected`)

The JSON file carries top-level metadata:

| Field | Meaning |
|-------|---------|
| `given_script_encoding` | `ref_impl_op_substr_tags` — `given.scriptTree` leaves mirror ref-impl hex (trailing `0x7f` where the ref-impl used `OP_SUBSTR`). |
| `expected_script_encoding` | `converted_op_checksig_at_test_load` — `intermediary` / `expected` merkle artifacts are computed from **converted** scripts (`0xac`). |
| `encoding_note` | Human-readable summary of the conversion-at-test-load pipeline. |

Do not expect `given.scriptTree` to already show overloaded scripts; tests call
`convert_substr_tag_to_checksig()` before `P2mrBuilder` construction.

## Conversion rules

1. **Simple SLH leaves** (`PUSH32 <32-byte-pk> OP_SUBSTR`): replace trailing `0x7f` with `0xac`
   (`OP_CHECKSIG`). Recompute merkle root, `scriptPubKey`, and control blocks — do not copy
   ref-impl intermediary values.
2. **ML-DSA leaves**: `PUSHDATA2(1312) <pk> OP_CHECKSIG` (no `OP_SUBSTR` in CUSF model).
3. **Merkle construction**: `P2mrBuilder::add_leaf_with_ver(depth, …)` using traversal `depth`
   (matches ref-impl `p2mr_pqc_construction.rs`, not the unused `modified_depth` local).
4. **Control blocks**: `P2mrControlBlock::serialize()` omits 32-byte base padding; enforcer
   `merkle::verify_merkle_path` expects `0xc1 || 32 zero bytes || merkle branch`. Tests expand
   serialized bytes via `control_block_bytes_for_enforcer()`. This serialize/decode layout gap is
   tracked as upstream alignment debt in `bitcoin-p2mr-pqc` (tests compensate locally; not an
   enforcer bug).

## Lightning contract notes

Ref-impl Bob leaf script accidentally encodes `OP_SUBSTR` bytes inside the 32-byte push
(ref-impl asm lists `OP_SUBSTR` but the serialized script ends inside the push — the final `0xac`
byte is **pubkey data**, not an `OP_CHECKSIG` opcode). The overload vector corrects this by
appending a real trailing `OP_CHECKSIG`:

```text
OP_SHA256 <hash> OP_EQUALVERIFY PUSH32 <pk> OP_CHECKSIG
```

(67 B serialized; pubkey push may still contain `0x7f` / `0xac` bytes inside push data.)

Alice leaf: CSV + DROP + SLH `OP_SUBSTR` converted to `OP_CHECKSIG`. Construction tests verify
merkle commitment; `parse_leaf_script` is **not** required to accept CSV/SHA256 leaves (enforcer
forbids those opcodes outside sig sites).

## Spend vectors

Ref-impl `priv_key` fields use `SLH_DSA_128S` (SHAKE); the CUSF enforcer verifies
`SLH_DSA_SHA2_128S`. Construction vectors retain ref-impl `priv_key` metadata for cross-reference
only — they are **not** wired into spend round-trip tests here (scheme naming mismatch). End-to-end
SLH spend round-trips live in `quantum::spend` (`slh_only_leaf_roundtrip`, integration
`bip360_valid_slh_spend`).

## Related docs

- [`BIP360_OVERLOAD_ADDENDUM.md`](./BIP360_OVERLOAD_ADDENDUM.md) — overload opcode semantics
- [`CUSF-BIP360.md`](./CUSF-BIP360.md) — enforcer architecture