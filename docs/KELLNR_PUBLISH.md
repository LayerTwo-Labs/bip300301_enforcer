# Kellnr publish checklist — `bitcoinpqc` 0.4.0

Human steps to publish the `bitcoinpqc` Rust crate to the Kellnr registry at
`crates.denver.space`. The enforcer workspace currently pins `bitcoinpqc` from
**git** rather than a published version.

## Current workspace pin

From `bip300301_enforcer/Cargo.toml`:

```toml
[workspace.dependencies.bitcoinpqc]
git = "https://github.com/cryptoquick/libbitcoinpqc-bindings.git"
rev = "5ef70672649fe21937318652e7ee7ad78f8e9405"
```

Source tree (local): sibling checkout of
[`libbitcoinpqc-bindings`](https://github.com/cryptoquick/libbitcoinpqc-bindings)
— e.g. `$LIBBITCOINPQC_BINDINGS` or `../libbitcoinpqc-bindings` next to this
workspace.

Published registry today (per `ARTIFACTS.md`): `bitcoinpqc` **0.3.0** on Kellnr.

Target: **`bitcoinpqc` 0.4.0** on Kellnr, aligned with the git pin above.

## Pre-publish verification

Run in the bindings repo:

```bash
cd "${LIBBITCOINPQC_BINDINGS:-../libbitcoinpqc-bindings}"

# Full test suite
cargo test

# Example smoke test
cargo run --example basic

# Confirm crate version in Cargo.toml matches intended publish (0.4.0)
grep '^version' Cargo.toml
```

Checklist:

- [ ] `CHANGELOG` / release notes for 0.4.0 (ML-DSA-44, SLH-DSA-SHA2-128s,
      Schnorr bindings)
- [ ] `SECP256K1_SCHNORR` keygen/sign paths tested (32-byte entropy, 32-byte
      message hash)
- [ ] Native `libbitcoinpqc` builds cleanly on Linux (CMake + bindgen)
- [ ] No accidental `ide` feature in release builds
- [ ] License / repository metadata correct in `Cargo.toml`

## Version bump (if not already 0.4.0)

```bash
cd "${LIBBITCOINPQC_BINDINGS:-../libbitcoinpqc-bindings}"
# Edit Cargo.toml: version = "0.4.0"
cargo test
git tag -a v0.4.0 -m "bitcoinpqc 0.4.0"
```

## Kellnr publish (human)

Kellnr host: **https://crates.denver.space**

Typical flow (adjust for your Kellnr credentials / CI):

1. **Log in** to Kellnr (web UI or `cargo login` with registry token).
2. Ensure **`.cargo/config.toml`** in the publish environment points at Kellnr:

   ```toml
   [registries.kellnr-denver-space]
   index = "sparse+https://crates.denver.space/api/v1/crates/"

   [registry]
   default = "kellnr-denver-space"   # or use --registry flag explicitly
   ```

3. **Dry-run** (optional):

   ```bash
   cargo publish --dry-run --allow-dirty
   ```

4. **Publish**:

   ```bash
   cargo publish --registry kellnr-denver-space
   ```

5. **Verify** on Kellnr UI: crate `bitcoinpqc` version `0.4.0` appears and
   downloads.

## Post-publish — update consumers

After Kellnr has `bitcoinpqc` 0.4.0:

1. **`bip300301_enforcer/Cargo.toml`** — switch from git pin to registry
   (optional but recommended):

   ```toml
   [workspace.dependencies.bitcoinpqc]
   version = "0.4.0"
   registry = "kellnr-denver-space"
   ```

2. **`bip-0360/ref-impl/rust/Cargo.toml`** — align PQC crate version if the
   ref-impl should consume 0.4.0 from Kellnr.

3. Run enforcer verification:

   ```bash
   cd bip300301_enforcer
   cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 pqc::
   cargo clippy -p bip300301_enforcer_lib --no-default-features --features bip360 -- -D warnings
   ```

## Related Kellnr crates

These are already on Kellnr and should stay version-compatible:

| Crate              | Version (today)     | Notes                       |
| ------------------ | ------------------- | --------------------------- |
| `bitcoin-p2mr-pqc` | 0.32.6-p2mr-pqc.1   | P2MR types + merkle helpers |
| `miniscript`       | 13.0.0-p2mr-pqc-1.0 | P2MR-aware miniscript fork  |
| `bitcoinpqc`       | 0.3.0 → **0.4.0**   | This checklist              |

## Human-only reminder

Publishing to Kellnr and tagging the upstream git repo are **manual** steps.
This document does not automate credentials, registry ACLs, or cross-repo
version bumps.
