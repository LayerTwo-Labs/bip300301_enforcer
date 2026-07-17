# Published P2MR protocol vectors (3C / hybrid / 3B)

Static JSON for Core CI and external interop. **Source of truth** remains the
Rust builders; unit tests in `published_vectors.rs` fail if these files drift.

| File                   | Shape                                                | Builder            |
| ---------------------- | ---------------------------------------------------- | ------------------ |
| `schnorr_3c.json`      | 1 — Schnorr, control `c1`, Default bare 64           | `protocol_vectors` |
| `core_hybrid.json`     | 2 — Core OP_SUBSTR hybrid (interop only)             | `core_interop`     |
| `kitchen_sink_3b.json` | 3 — overload kitchen-sink (Bob accepts post-Core 3B) | `kitchen_sink_3b`  |

## Regenerate

```bash
cd bip300301_enforcer
PUBLISHED_VECTORS_DUMP=1 cargo test -p bip300301_enforcer_lib --features bip360 --lib dump_published_vectors_if_env -- --nocapture
cargo test -p bip300301_enforcer_lib --features bip360 --lib published_vectors
```

## Notes

- Shape 2 is **not** enforcer-canonical (`enforcer_canonical: false`); enforcer
  rejects `OP_SUBSTR` on tip verify.
- Kitchen-sink JSON notes `bob_accepts_post_core_3b: true` after TAPSCRIPT
  element size 8000 + size-gated multi-`OP_CHECKSIG`.
