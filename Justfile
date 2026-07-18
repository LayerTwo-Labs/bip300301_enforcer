# Tooling debt: this file is large (~1k lines). Quality gate (fmt-check → clippy →
# test) is intentional; setup/e2e/multiproc bulk is residual — see
# cusf/RESIDUAL.md ("Justfile sprawl → hermetic Nix"). Desired end state: idiomatic
# hermetic Nix flakes, not bash wrapped by Nix. Prefer scripts/ or flake checks
# over growing new long recipes here.
import? 'local.just'

env_file := env_var_or_default('BIP300301_ENFORCER_INTEGRATION_TEST_ENV', 'integrationtests.env')
enforcer_bin := env_var_or_default('BIP300301_ENFORCER', 'target/debug/bip300301_enforcer')

default:
    @just --list

# Ensure buf is on PATH for generate / lint-proto.
# Local: auto-installs to ~/.local/bin when missing (optional pre-install).
# CI: buf is already present via bufbuild/buf-action.
# fmt: optional — skips with a note if buf is absent (does not call this recipe).
_ensure_buf:
    #!/usr/bin/env bash
    set -euo pipefail
    export PATH="${HOME}/.local/bin:${PATH}"
    if command -v buf >/dev/null 2>&1; then
        exit 0
    fi
    BUF_VERSION="${BUF_VERSION:-1.50.0}"
    INSTALL_DIR="${HOME}/.local/bin"
    mkdir -p "$INSTALL_DIR"
    OS=$(uname -s)
    ARCH=$(uname -m)
    case "$OS" in
        Linux) BUF_OS=Linux ;;
        Darwin) BUF_OS=Darwin ;;
        *)
            echo "error: buf not on PATH and auto-install unsupported on OS=$OS" >&2
            echo "       install: https://buf.build/docs/installation" >&2
            echo "       or: put a buf binary on PATH / in ~/.local/bin" >&2
            exit 1
            ;;
    esac
    case "$ARCH" in
        x86_64) BUF_ARCH=x86_64 ;;
        aarch64|arm64)
            if [ "$BUF_OS" = Darwin ]; then BUF_ARCH=arm64; else BUF_ARCH=aarch64; fi
            ;;
        *)
            echo "error: buf not on PATH and unsupported arch $ARCH for auto-install" >&2
            echo "       install: https://buf.build/docs/installation" >&2
            exit 1
            ;;
    esac
    URL="https://github.com/bufbuild/buf/releases/download/v${BUF_VERSION}/buf-${BUF_OS}-${BUF_ARCH}"
    echo "buf not on PATH; installing ${BUF_VERSION} → ${INSTALL_DIR}/buf"
    if ! command -v curl >/dev/null 2>&1; then
        echo "error: curl required to auto-install buf" >&2
        echo "       install buf manually: https://buf.build/docs/installation" >&2
        exit 1
    fi
    if ! curl -fsSL -o "${INSTALL_DIR}/buf" "$URL"; then
        echo "error: failed to download buf from:" >&2
        echo "       $URL" >&2
        echo "       install manually: https://buf.build/docs/installation" >&2
        rm -f "${INSTALL_DIR}/buf"
        exit 1
    fi
    chmod +x "${INSTALL_DIR}/buf"
    "${INSTALL_DIR}/buf" --version

# Regenerate checked-in protobuf code (auto-installs buf if needed)
generate: _ensure_buf
    #!/usr/bin/env bash
    set -euo pipefail
    export PATH="${HOME}/.local/bin:${PATH}"
    buf generate --clean

# Lint protos under proto/ (auto-installs buf if needed)
lint-proto: _ensure_buf
    #!/usr/bin/env bash
    set -euo pipefail
    export PATH="${HOME}/.local/bin:${PATH}"
    buf lint proto

# Signet sync benchmark (arg: height or prior consensus-state.json path)
@sync-benchmark-signet target='0':
    #!/usr/bin/env bash
    set -euo pipefail
    target='{{target}}'
    if [[ -f "$target" ]]; then
        echo "Verifying consensus state against reference: $target"
        mode=(--verify-consensus-state "$target")
    else
        echo "Syncing to height: $target"
        mode=(--exit-after-sync="$target")
    fi
    datadir="$(mktemp -d "./datadir-sync-benchmark.XXXXXX")"
    echo "Using fresh data dir: $datadir"
    env RUST_BACKTRACE=1 cargo run --release -- \
        --data-dir "$datadir" \
        --node-rpc-addr=localhost:38332 \
        --node-rpc-user=user \
        --node-rpc-pass=password \
        "${mode[@]}"
    echo "Consensus state written to $datadir/consensus-state.json"

# --- Quality gate (fmt → clippy → tests; check-only, never --fix) ---
# Feature matrices match CI (no --all-features / no reserved shrincs).

# rustfmt --check via nightly (matches rustfmt.toml; no stable spam)
fmt-check:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "==> fmt-check: cargo +nightly fmt --all -- --check"
    cargo +nightly fmt --all -- --check

# Clippy *check* only — never --fix / --allow-dirty / --allow-staged.
# Combined product + bip360-only lib + bip360 integration_tests + nightly import lint.
_clippy-check:
    cargo clippy --all-targets --features "drivechain,bip360,rustls" -- --deny warnings
    cargo clippy -p bip300301_enforcer_lib --all-targets --no-default-features --features bip360 -- --deny warnings
    cargo clippy -p bip300301_enforcer_integration_tests --all-targets --features bip360 -- --deny warnings
    cargo +nightly clippy --features "drivechain,bip360,rustls" -- -A clippy::all -D unqualified_local_imports -Zcrate-attr="feature(unqualified_local_imports)"

# Standalone lint: format first, then clippy check (no --fix)
clippy: fmt-check _clippy-check

# Optional auto-fix (writes the tree). Never used by `just test` / CI / clippy.
clippy-fix:
    cargo clippy --all-targets --features "drivechain,bip360,rustls" --fix --allow-dirty --allow-staged -- --deny warnings

# Primary gate — single sequential body so order is undeniable and visible:
#   1) fmt-check  2) clippy check (no --fix)  3) cargo-nextest (all cargo unit tests)
# Requires cargo-nextest + nightly rustfmt.
# Bitcoind libtest-mimic example (`integration_tests` harness) is excluded via
# `-E 'not kind(example)'` — run those with `just it` / `it-all` (need Core env).
# Pass-through nextest args: `just test -- --no-fail-fast`
test *args='':
    #!/usr/bin/env bash
    set -euo pipefail
    root='{{ justfile_directory() }}'
    cd "$root"
    echo "==> [1/3] fmt-check (fail on incorrect formatting)"
    cargo +nightly fmt --all -- --check
    echo "==> [2/3] clippy (fail on warnings; check-only, never auto-fix)"
    # Invoke check body only — not `just clippy` (avoids double fmt-check).
    just _clippy-check
    if ! cargo nextest --version >/dev/null 2>&1; then
        echo "error: cargo-nextest required (cargo install cargo-nextest --locked)" >&2
        exit 1
    fi
    echo "==> [3/3] cargo-nextest (every cargo unit test; not bitcoind trials)"
    # Combined product features. Exclude kind(example): only the libtest-mimic
    # bitcoind harness is registered as an example-as-test (needs BITCOIND_*).
    cargo nextest run --workspace --all-targets --features "drivechain,bip360,rustls" \
        -E 'not kind(example)' {{ args }}
    # bip360-only lib matrix (drivechain off; catches cfg-gated paths).
    cargo nextest run -p bip300301_enforcer_lib --all-targets --no-default-features --features bip360 \
        -E 'not kind(example)' {{ args }}
    echo "==> just test: all stages passed"

# Default developer build: both rule sets in one binary (same as combined product).
build *args='':
    cargo build --features "drivechain,bip360,rustls" {{ args }}

# --- Hub + rule workers (docs/MULTI_ENFORCER.md) ---
# Hub: bip300301_enforcer (Local feature ballots + optional --rules-worker UDS remotes on hot path).
# Workers: cusf_rules_drivechain / cusf_rules_bip360 (--health, --once, --uds RUL1).

# Unit + check for rules engine and worker crates (no --all-features).
validate-rules-engine:
    ./scripts/validate-rules-engine.sh

# Build hub (combined features) + both rule worker binaries.
build-hub-workers profile='debug':
    #!/usr/bin/env bash
    set -euo pipefail
    profile='{{profile}}'
    case "$profile" in debug|release) ;; *)
        echo "error: profile must be debug or release (got $profile)" >&2; exit 1 ;;
    esac
    out="{{justfile_directory()}}/target/${profile}"
    rel=()
    if [ "$profile" = release ]; then rel=(--release); fi
    cargo build -p bip300301_enforcer --no-default-features --features "drivechain,bip360,rustls" "${rel[@]}"
    cargo build -p cusf_rules_drivechain "${rel[@]}"
    cargo build -p cusf_rules_bip360 "${rel[@]}"
    ls -la "${out}/bip300301_enforcer" "${out}/cusf_rules_drivechain" "${out}/cusf_rules_bip360"
    echo "hub: bip300301_enforcer (Local always + remote AND when registered); workers: cusf_rules_drivechain, cusf_rules_bip360"
    echo "CI: .github/workflows/check_lint_build_release.yaml job check-rules-engine runs ./scripts/validate-rules-engine.sh (same as just validate-rules-engine)"

# Worker-only health smoke (no bitcoind).
rules-drivechain-health:
    cargo run -p cusf_rules_drivechain -- --health

rules-bip360-health:
    cargo run -p cusf_rules_bip360 -- --health

# Multiproc UDS tip smoke (no bitcoind required). Optional BITCOIND=… / CUSF_LIVE_TIP_E2E=1.
smoke-rules-workers-tip:
    ./scripts/smoke-rules-workers-tip.sh

# Opt-in live tip: bitcoind + hub --rules-worker bip360/drivechain.
# Soft-skip exit 0 only when CUSF_LIVE_TIP_E2E unset. With CUSF_LIVE_TIP_E2E=1,
# missing BITCOIND / hub failure → non-zero. Not part of validate-rules-engine
# / check-rules-engine (PR CI residual honesty — HITL workflow_dispatch only).
live-tip-rules-workers-e2e:
    ./scripts/live-tip-rules-workers-e2e.sh

# Process-level SIGTERM e2e: multiproc workers --uds → Health → kill -TERM →
# clean exit + socket unlink. Not part of validate-rules-engine (SCORE).
# Residual: accept-poll latency + mid-handler cancel (accept-loop only).
# Hub process SIGTERM not covered (needs bitcoind).
sigterm-rules-workers-e2e:
    ./scripts/sigterm-rules-workers-e2e.sh

# --- Transitional multi-feature artifacts (feature → named file) ---
# Prefer build-hub-workers for new deploys. These recipes still bake today's
# single-process feature variants (migration aid).
# Cargo features are package-scoped: one cargo build = one feature set.

# BIP 300/301 only (upstream default product name)
build-enforcer-drivechain profile='debug':
    #!/usr/bin/env bash
    set -euo pipefail
    profile='{{profile}}'
    case "$profile" in debug|release) ;; *)
        echo "error: profile must be debug or release (got $profile)" >&2; exit 1 ;;
    esac
    out="{{justfile_directory()}}/target/${profile}"
    flags=(--no-default-features --features "drivechain,rustls")
    if [ "$profile" = release ]; then flags+=(--release); fi
    cargo build -p bip300301_enforcer "${flags[@]}"
    # Cargo output name is already bip300301_enforcer
    echo "built ${out}/bip300301_enforcer (drivechain)"

# BIP 360 only (P2MR / PQC) → bip360_enforcer
build-enforcer-bip360 profile='debug':
    #!/usr/bin/env bash
    set -euo pipefail
    profile='{{profile}}'
    case "$profile" in debug|release) ;; *)
        echo "error: profile must be debug or release (got $profile)" >&2; exit 1 ;;
    esac
    out="{{justfile_directory()}}/target/${profile}"
    flags=(--no-default-features --features "bip360,rustls")
    if [ "$profile" = release ]; then flags+=(--release); fi
    cargo build -p bip300301_enforcer "${flags[@]}"
    cp -f "${out}/bip300301_enforcer" "${out}/bip360_enforcer"
    echo "built ${out}/bip360_enforcer (bip360)"

# Combined: drivechain + bip360 (AND of both rule sets) → cusf_enforcer
build-enforcer-combined profile='debug':
    #!/usr/bin/env bash
    set -euo pipefail
    profile='{{profile}}'
    case "$profile" in debug|release) ;; *)
        echo "error: profile must be debug or release (got $profile)" >&2; exit 1 ;;
    esac
    out="{{justfile_directory()}}/target/${profile}"
    flags=(--no-default-features --features "drivechain,bip360,rustls")
    if [ "$profile" = release ]; then flags+=(--release); fi
    cargo build -p bip300301_enforcer "${flags[@]}"
    cp -f "${out}/bip300301_enforcer" "${out}/cusf_enforcer"
    # After this recipe, cargo's bip300301_enforcer name is the combined binary.
    echo "built ${out}/cusf_enforcer (drivechain+bip360); cargo name bip300301_enforcer matches combined"

# All three product artifacts (sequential; bip360/drivechain copies may be
# overwritten for the cargo name — named products bip360_enforcer + cusf_enforcer
# keep their last successful feature builds).
build-enforcers profile='debug':
    #!/usr/bin/env bash
    set -euo pipefail
    profile='{{profile}}'
    out="{{justfile_directory()}}/target/${profile}"
    # Build order: specialty products first, combined last so default cargo name
    # is the dual-rules binary. Preserve drivechain-only under an explicit name.
    flags_dc=(--no-default-features --features "drivechain,rustls")
    flags_360=(--no-default-features --features "bip360,rustls")
    flags_both=(--no-default-features --features "drivechain,bip360,rustls")
    if [ "$profile" = release ]; then
        flags_dc+=(--release); flags_360+=(--release); flags_both+=(--release)
    fi
    cargo build -p bip300301_enforcer "${flags_dc[@]}"
    cp -f "${out}/bip300301_enforcer" "${out}/bip300301_enforcer-drivechain"
    cargo build -p bip300301_enforcer "${flags_360[@]}"
    cp -f "${out}/bip300301_enforcer" "${out}/bip360_enforcer"
    cargo build -p bip300301_enforcer "${flags_both[@]}"
    cp -f "${out}/bip300301_enforcer" "${out}/cusf_enforcer"
    # Restore drivechain-only as the historical upstream binary name.
    cp -f "${out}/bip300301_enforcer-drivechain" "${out}/bip300301_enforcer"
    rm -f "${out}/bip300301_enforcer-drivechain"
    ls -la "${out}/bip300301_enforcer" "${out}/bip360_enforcer" "${out}/cusf_enforcer"
    echo "products: bip300301_enforcer (drivechain), bip360_enforcer, cusf_enforcer (both)"

# Format Rust (nightly), optional prettier + buf
fmt:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo +nightly fmt --all
    if command -v bunx >/dev/null 2>&1; then
        bunx prettier --write .
    elif command -v npx >/dev/null 2>&1; then
        npx --yes prettier --write .
    else
        echo "note: prettier skipped (install bunx or npx for md/yaml format)" >&2
    fi
    if command -v buf >/dev/null 2>&1; then
        buf format -w proto
    else
        echo "note: buf format skipped (buf not on PATH)" >&2
    fi

# Integration tests (pass trial names after --)
test-it *args='':
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build
    export BIP300301_ENFORCER_INTEGRATION_TEST_ENV='{{ justfile_directory() }}/integrationtests.env'
    cargo run --example integration_tests -- {{ args }}

# --- BIP 360 verification (AGENTS.md / CI check-bip360) ---

# pqc:: unit tests (bip360)
test-pqc:
    cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 pqc::

# Alias for test-pqc
test-quantum: test-pqc

# Drivechain default unit tests
test-drivechain:
    cargo test -p bip300301_enforcer_lib

# Minimal drivechain build + unit tests
drivechain-smoke:
    cargo build -p bip300301_enforcer
    just test-drivechain

# Clippy bip360 lib + integration tests (check-only; format first)
clippy-bip360: fmt-check
    cargo clippy -p bip300301_enforcer_lib --no-default-features --features bip360 -- -D warnings
    cargo clippy -p bip300301_enforcer_integration_tests --features bip360 -- -D warnings

# cargo check bip360 all targets
check-bip360:
    cargo check --no-default-features --features bip360 --all-targets

# cargo check integration_tests example
check-integration-build:
    cargo check --example integration_tests --features bip360

# p2mr_signer example smoke
p2mr-signer-smoke:
    cargo build --example p2mr_signer --no-default-features --features bip360
    cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 p2mr_signer_roundtrip -- --nocapture
    cargo run --example p2mr_signer --no-default-features --features bip360 -- \
        --algorithm schnorr \
        --entropy-hex 1111111111111111111111111111111111111111111111111111111111111111 \
        | grep -q signed_spend_tx_hex

# Local CI subset: fmt first, then checks/tests/clippy-bip360 (no nextest full suite)
verify: fmt-check check-bip360 test-pqc p2mr-signer-smoke test-drivechain clippy-bip360 check-integration-build

# Build bip360 enforcer + integration example
build-bip360:
    cargo build -p bip300301_enforcer --no-default-features --features bip360
    cargo build --example integration_tests --features bip360

# Download stock bitcoind only; write integrationtests.env
setup-core:
    #!/usr/bin/env bash
    set -euo pipefail
    REPO_ROOT="$(pwd)"
    GIT_COMMON_DIR="$(git rev-parse --git-common-dir)"
    case "$GIT_COMMON_DIR" in
        /*) ;;
        *) GIT_COMMON_DIR="$REPO_ROOT/$GIT_COMMON_DIR" ;;
    esac
    DEPS_ROOT="$(cd "$GIT_COMMON_DIR/.." && pwd)"
    DEPS_DIR="$DEPS_ROOT/.integration-deps"
    VERSION_FILE="$REPO_ROOT/lib/version.rs"
    ALL_BITCOIN_VERSIONS="$(grep -oE '"[0-9]+\.[0-9]+"' "$VERSION_FILE" | tr -d '"' || true)"
    if [ -z "$ALL_BITCOIN_VERSIONS" ]; then
        echo "Could not parse CI_BITCOIN_CORE_VERSIONS from $VERSION_FILE" >&2
        exit 1
    fi
    BITCOIN_VERSION="${ALL_BITCOIN_VERSIONS%%$'\n'*}"
    UNPATCHED_DIR="$DEPS_DIR/bitcoin-stock-$BITCOIN_VERSION"
    OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
    ARCH="$(uname -m)"
    case "$OS-$ARCH" in
        linux-x86_64)  STOCK_TARGET="x86_64-linux-gnu" ;;
        darwin-x86_64) STOCK_TARGET="x86_64-apple-darwin" ;;
        darwin-arm64)  STOCK_TARGET="arm64-apple-darwin" ;;
        *) echo "Unsupported platform: $OS-$ARCH" >&2; exit 1 ;;
    esac
    mkdir -p "$DEPS_DIR"
    if [ ! -x "$UNPATCHED_DIR/bitcoind" ]; then
        echo "Downloading stock Bitcoin Core $BITCOIN_VERSION ($STOCK_TARGET)..."
        TMP=$(mktemp -d)
        trap 'rm -rf "$TMP"' EXIT
        TARBALL="bitcoin-$BITCOIN_VERSION-$STOCK_TARGET.tar.gz"
        curl -# -fL "https://bitcoincore.org/bin/bitcoin-core-$BITCOIN_VERSION/$TARBALL" -o "$TMP/$TARBALL"
        tar -C "$TMP" -xf "$TMP/$TARBALL"
        rm -rf "$UNPATCHED_DIR"
        mv "$TMP/bitcoin-$BITCOIN_VERSION/bin" "$UNPATCHED_DIR"
        chmod +x "$UNPATCHED_DIR"/bitcoind "$UNPATCHED_DIR"/bitcoin-cli "$UNPATCHED_DIR"/bitcoin-util
        rm -rf "$TMP"
        trap - EXIT
    else
        echo "Stock bitcoin: cached"
    fi
    ENV_FILE="$REPO_ROOT/integrationtests.env"
    {
        echo "BIP300301_ENFORCER='target/debug/bip300301_enforcer'"
        echo "BITCOIND_UNPATCHED='$UNPATCHED_DIR/bitcoind'"
        echo "BITCOIN_CLI='$UNPATCHED_DIR/bitcoin-cli'"
        echo "BITCOIN_UTIL='$UNPATCHED_DIR/bitcoin-util'"
    } > "$ENV_FILE"
    echo
    echo "Wrote $ENV_FILE"
    echo "Deps cache: $DEPS_DIR"
    echo "Run BIP 360 trials with: just demo-a / just it <trial_name>"

# Full bootstrap: patched + stock bitcoind, electrs, signet miner
setup:
    #!/usr/bin/env bash
    set -euo pipefail
    REPO_ROOT="$(pwd)"
    GIT_COMMON_DIR="$(git rev-parse --git-common-dir)"
    case "$GIT_COMMON_DIR" in
        /*) ;;
        *) GIT_COMMON_DIR="$REPO_ROOT/$GIT_COMMON_DIR" ;;
    esac
    DEPS_ROOT="$(cd "$GIT_COMMON_DIR/.." && pwd)"
    DEPS_DIR="$DEPS_ROOT/.integration-deps"
    VERSION_FILE="$REPO_ROOT/lib/version.rs"
    ALL_BITCOIN_VERSIONS="$(grep -oE '"[0-9]+\.[0-9]+"' "$VERSION_FILE" | tr -d '"' || true)"
    if [ -z "$ALL_BITCOIN_VERSIONS" ]; then
        echo "Could not parse CI_BITCOIN_CORE_VERSIONS from $VERSION_FILE" >&2
        exit 1
    fi
    BITCOIN_VERSION="${ALL_BITCOIN_VERSIONS%%$'\n'*}"
    ELECTRS_VERSION="v3.2.0"
    PATCHED_REVISION="latest"
    PATCHED_DIR="$DEPS_DIR/bitcoin-patched-$PATCHED_REVISION"
    UNPATCHED_DIR="$DEPS_DIR/bitcoin-stock-$BITCOIN_VERSION"
    SIGNET_REPO_DIR="$DEPS_DIR/bitcoin-patched-repo"
    ELECTRS_DIR="$DEPS_DIR/electrs-$ELECTRS_VERSION"
    OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
    ARCH="$(uname -m)"
    case "$OS-$ARCH" in
        linux-x86_64)  STOCK_TARGET="x86_64-linux-gnu";    PATCHED_TARGET="x86_64-unknown-linux-gnu" ;;
        darwin-x86_64) STOCK_TARGET="x86_64-apple-darwin"; PATCHED_TARGET="x86_64-apple-darwin" ;;
        darwin-arm64)  STOCK_TARGET="arm64-apple-darwin";  PATCHED_TARGET="x86_64-apple-darwin" ;;
        *) echo "Unsupported platform: $OS-$ARCH (no patched binary published)" >&2; exit 1 ;;
    esac
    mkdir -p "$DEPS_DIR"
    if [ ! -x "$PATCHED_DIR/bitcoind" ]; then
        echo "Downloading patched bitcoin ($PATCHED_TARGET)..."
        TMP=$(mktemp -d)
        trap 'rm -rf "$TMP"' EXIT
        ZIP="L1-bitcoin-patched-$PATCHED_REVISION-$PATCHED_TARGET.zip"
        curl -# -fL "https://releases.drivechain.info/$ZIP" -o "$TMP/patched.zip"
        unzip -q "$TMP/patched.zip" -d "$TMP"
        rm -rf "$PATCHED_DIR"
        mv "$TMP/L1-bitcoin-patched-$PATCHED_REVISION-$PATCHED_TARGET" "$PATCHED_DIR"
        chmod +x "$PATCHED_DIR"/bitcoind "$PATCHED_DIR"/bitcoin-cli "$PATCHED_DIR"/bitcoin-util
        rm -rf "$TMP"
        trap - EXIT
    else
        echo "Patched bitcoin: cached"
    fi
    if [ ! -x "$UNPATCHED_DIR/bitcoind" ]; then
        echo "Downloading stock Bitcoin Core $BITCOIN_VERSION ($STOCK_TARGET)..."
        TMP=$(mktemp -d)
        trap 'rm -rf "$TMP"' EXIT
        TARBALL="bitcoin-$BITCOIN_VERSION-$STOCK_TARGET.tar.gz"
        curl -# -fL "https://bitcoincore.org/bin/bitcoin-core-$BITCOIN_VERSION/$TARBALL" -o "$TMP/$TARBALL"
        tar -C "$TMP" -xf "$TMP/$TARBALL"
        rm -rf "$UNPATCHED_DIR"
        mv "$TMP/bitcoin-$BITCOIN_VERSION/bin" "$UNPATCHED_DIR"
        chmod +x "$UNPATCHED_DIR"/bitcoind "$UNPATCHED_DIR"/bitcoin-cli "$UNPATCHED_DIR"/bitcoin-util
        rm -rf "$TMP"
        trap - EXIT
    else
        echo "Stock bitcoin: cached"
    fi
    if [ ! -f "$SIGNET_REPO_DIR/contrib/signet/miner" ]; then
        echo "Cloning bitcoin-patched for signet miner script..."
        rm -rf "$SIGNET_REPO_DIR"
        git clone --depth 1 https://github.com/LayerTwo-Labs/bitcoin-patched.git "$SIGNET_REPO_DIR"
    else
        echo "Signet miner repo: cached"
    fi
    ELECTRS_BIN="$ELECTRS_DIR/target/release/electrs"
    if [ ! -x "$ELECTRS_BIN" ]; then
        echo "Building electrs $ELECTRS_VERSION (a few minutes on a cold build)..."
        if [ ! -d "$ELECTRS_DIR" ]; then
            git clone --branch "$ELECTRS_VERSION" --depth 1 \
                https://github.com/mempool/electrs.git "$ELECTRS_DIR"
            printf '\n[workspace]\n' >> "$ELECTRS_DIR/Cargo.toml"
        fi
        # GCC 15+/16: vendored RocksDB 8.1.1 needs <cstdint> (facebook/rocksdb#13365).
        # Same workaround as AUR electrs PKGBUILD; scoped to this subshell only.
        (cd "$ELECTRS_DIR" && \
            export CXXFLAGS="${CXXFLAGS:-} -include cstdint" && \
            cargo build --locked --release)
    else
        echo "electrs: cached"
    fi
    ENV_FILE="$REPO_ROOT/integrationtests.env"
    {
        echo "BIP300301_ENFORCER='target/debug/bip300301_enforcer'"
        echo "BITCOIND='$PATCHED_DIR/bitcoind'"
        echo "BITCOIND_UNPATCHED='$UNPATCHED_DIR/bitcoind'"
        echo "BITCOIN_CLI='$PATCHED_DIR/bitcoin-cli'"
        echo "BITCOIN_UTIL='$PATCHED_DIR/bitcoin-util'"
        echo "ELECTRS='$ELECTRS_BIN'"
        echo "SIGNET_MINER='$SIGNET_REPO_DIR/contrib/signet/miner'"
    } > "$ENV_FILE"
    echo
    echo "Wrote $ENV_FILE"
    echo "Deps cache: $DEPS_DIR"
    echo "Run integration trials with: just it <trial_name>"

demo-check:
    #!/usr/bin/env bash
    set -euo pipefail
    ENV_FILE="{{env_file}}"
    ENFORCER="{{enforcer_bin}}"
    info() { echo "==> $*"; }
    warn() { echo "WARN: $*" >&2; }
    info "checking Rust toolchain"
    command -v cargo >/dev/null
    command -v rustup >/dev/null
    if [ ! -f .cargo/config.toml ]; then
        warn ".cargo/config.toml missing — Kellnr registry needed for bitcoin-p2mr-pqc"
    fi
    info "checking enforcer binary ($ENFORCER)"
    if [ ! -x "$ENFORCER" ]; then
        warn "enforcer not built yet — run: just build-bip360"
    fi
    if [ -f "$ENV_FILE" ]; then
        info "env file: $ENV_FILE"
        set -a
        # shellcheck disable=SC1090
        source "$ENV_FILE"
        set +a
    else
        warn "env file $ENV_FILE not found — run: just setup-core"
    fi
    CORE_BIN="${BITCOIND_UNPATCHED:-${BITCOIND:-}}"
    if [ -n "$CORE_BIN" ] && [ -x "$CORE_BIN" ]; then
        info "bitcoind: $CORE_BIN ($("$CORE_BIN" -version | head -1))"
    else
        warn "BITCOIND_UNPATCHED not set or not executable — run: just setup-core"
    fi
    command -v jq >/dev/null 2>&1 || warn "jq not installed — optional for pretty RPC output"
    info "prereq check complete"

demo-steps:
    @echo "Full walkthrough: docs/REGTEST_DEMO.md"
    @echo ""
    @echo "Quick path:"
    @echo "  just setup-core"
    @echo "  just build-bip360"
    @echo "  just demo-a          # valid Schnorr spend retained"
    @echo "  just demo-b          # empty witness → invalidateblock"
    @echo "  just it <trial>      # single integration trial"
    @echo "  just it-all          # all 34 block-only BIP 360 trials (incl. TB-mine)"

demo-a auto='':
    @just _run-it bip360_valid_schnorr_spend {{auto}}

demo-b auto='':
    @just _run-it bip360_invalid_block {{auto}}

it trial auto='':
    @just _run-it {{trial}} {{auto}}

# Alias for it-all (34 green block-only trials)
bip360-block-matrix auto='': (it-all auto)

# Dual-node P2P E2E (needs electrs: just setup)
bip360-p2p-e2e auto='':
    #!/usr/bin/env bash
    set -euo pipefail
    ENV_FILE="{{env_file}}"
    if [ -f "$ENV_FILE" ]; then
        set -a
        # shellcheck disable=SC1090
        source "$ENV_FILE"
        set +a
    fi
    if [ -z "${ELECTRS:-}" ] || [ ! -x "${ELECTRS}" ]; then
        if [ "{{auto}}" = "yes" ] || [ "{{auto}}" = "1" ]; then
            echo "WARN: ELECTRS missing — running just setup" >&2
            just setup
        else
            echo "ELECTRS not set or not executable — P2P E2E needs wallet+mempool (run: just setup)" >&2
            echo "or re-run with: just bip360-p2p-e2e yes" >&2
            exit 1
        fi
    fi
    cargo build -p bip300301_enforcer --features "drivechain,bip360"
    cargo build --example integration_tests --features bip360
    export BIP300301_ENFORCER_INTEGRATION_TEST_ENV="{{env_file}}"
    export BIP300301_ENFORCER="{{enforcer_bin}}"
    # Keep drivechain+bip360 binary: `_run-it` would otherwise rebuild bip360-only.
    export BIP360_SKIP_REBUILD=1
    just _run-it bip360_p2p_mempool_e2e "{{auto}}"

# Point BITCOIND_P2MR at cryptoquick/jbride P2MR bitcoind (ZMQ required)
setup-p2mr:
    #!/usr/bin/env bash
    set -euo pipefail
    REPO_ROOT="$(pwd)"
    DEPS_DIR="$REPO_ROOT/.integration-deps/bitcoin-p2mr"
    ENV_FILE="$REPO_ROOT/integrationtests.env"
    BUILD_BIN="${CRYPTOQUICK_BITCOIN_BUILD:-$HOME/Projects/cryptoquick/bitcoin/build/bin}"
    mkdir -p "$DEPS_DIR"
    if [ ! -x "$BUILD_BIN/bitcoind" ]; then
        echo "No P2MR bitcoind at $BUILD_BIN/bitcoind" >&2
        echo "Build jbride/bitcoin#2 head (cryptoquick:p2mr), e.g.:" >&2
        echo "  git clone https://github.com/cryptoquick/bitcoin.git && cd bitcoin && git checkout p2mr" >&2
        echo "  cmake -B build -DWITH_ZMQ=ON && cmake --build build -j\"\$(nproc)\" --target bitcoind bitcoin-cli" >&2
        echo "Or: CRYPTOQUICK_BITCOIN_BUILD=/path/to/build/bin just setup-p2mr" >&2
        exit 1
    fi
    if ! ldd "$BUILD_BIN/bitcoind" 2>/dev/null | grep -qi zmq; then
        echo "WARN: $BUILD_BIN/bitcoind does not link libzmq — enforcer will fail." >&2
        echo "Rebuild with: cmake -B build -DWITH_ZMQ=ON && cmake --build build -j\"\$(nproc)\" --target bitcoind" >&2
    fi
    for b in bitcoind bitcoin-cli bitcoin-tx bitcoin-util bitcoin-wallet; do
        if [ -x "$BUILD_BIN/$b" ]; then
            ln -sfn "$BUILD_BIN/$b" "$DEPS_DIR/$b"
        fi
    done
    echo "P2MR bitcoind: $DEPS_DIR/bitcoind -> $(readlink -f "$DEPS_DIR/bitcoind")"
    "$DEPS_DIR/bitcoind" -version | head -1
    # Merge BITCOIND_P2MR into env file without clobbering other keys.
    touch "$ENV_FILE"
    if grep -q '^BITCOIND_P2MR=' "$ENV_FILE" 2>/dev/null; then
        # portable in-place replace
        tmp=$(mktemp)
        sed "s|^BITCOIND_P2MR=.*|BITCOIND_P2MR='$DEPS_DIR/bitcoind'|" "$ENV_FILE" > "$tmp"
        mv "$tmp" "$ENV_FILE"
    else
        echo "BITCOIND_P2MR='$DEPS_DIR/bitcoind'" >> "$ENV_FILE"
    fi
    echo "Updated $ENV_FILE (BITCOIND_P2MR)"
    echo "Run: just bip360-kitchen-sink-tier-a"

# Tier A kitchen-sink dual-node demo (stock Alice + P2MR Bob; needs electrs)
bip360-kitchen-sink-tier-a auto='':
    #!/usr/bin/env bash
    set -euo pipefail
    ENV_FILE="{{env_file}}"
    if [ -f "$ENV_FILE" ]; then
        set -a
        # shellcheck disable=SC1090
        source "$ENV_FILE"
        set +a
    fi
    if [ -z "${BITCOIND_P2MR:-}" ] || [ ! -x "${BITCOIND_P2MR}" ]; then
        if [ "{{auto}}" = "yes" ] || [ "{{auto}}" = "1" ]; then
            echo "WARN: BITCOIND_P2MR missing — running just setup-p2mr" >&2
            just setup-p2mr
            set -a
            # shellcheck disable=SC1090
            source "$ENV_FILE"
            set +a
        else
            echo "BITCOIND_P2MR not set or not executable — run: just setup-p2mr" >&2
            echo "Source of truth: jbride/bitcoin#2 head (cryptoquick:p2mr)" >&2
            echo "or re-run with: just bip360-kitchen-sink-tier-a yes" >&2
            exit 1
        fi
    fi
    if [ -z "${ELECTRS:-}" ] || [ ! -x "${ELECTRS}" ]; then
        if [ "{{auto}}" = "yes" ] || [ "{{auto}}" = "1" ]; then
            echo "WARN: ELECTRS missing — running just setup" >&2
            just setup
            set -a
            # shellcheck disable=SC1090
            source "$ENV_FILE"
            set +a
        else
            echo "ELECTRS not set or not executable — Tier A needs wallet+mempool (run: just setup)" >&2
            echo "or re-run with: just bip360-kitchen-sink-tier-a yes" >&2
            exit 1
        fi
    fi
    echo "==> Tier A: Alice stock + Bob P2MR ($BITCOIND_P2MR)"
    cargo build -p bip300301_enforcer --features "drivechain,bip360"
    cargo build --example integration_tests --features bip360
    export BIP300301_ENFORCER_INTEGRATION_TEST_ENV="{{env_file}}"
    export BIP300301_ENFORCER="{{enforcer_bin}}"
    export BIP360_SKIP_REBUILD=1
    just _run-it bip360_kitchen_sink_tier_a "{{auto}}"

# TB-mine: CUSF tip via submitblock (stock Core; in it-all)
bip360-tier-b-cusf auto='':
    #!/usr/bin/env bash
    set -euo pipefail
    echo "==> Tier B CUSF mining (expect PASS)"
    just _run-it bip360_tier_b_cusf_miner "{{auto}}"

# TB-factory: dual stock Miner + Alice tip (not in it-all)
bip360-tier-b-cusf-factory auto='':
    #!/usr/bin/env bash
    set -euo pipefail
    echo "==> Tier B CUSF dual-process factory (expect PASS): Miner submitblock → Alice tip"
    cargo build -p bip300301_enforcer --features "drivechain,bip360"
    cargo build --example integration_tests --features bip360
    export BIP300301_ENFORCER_INTEGRATION_TEST_ENV="{{env_file}}"
    export BIP300301_ENFORCER="{{enforcer_bin}}"
    export BIP360_SKIP_REBUILD=1
    just _run-it bip360_tier_b_cusf_factory "{{auto}}"

# TB-sidecar: inventory miner helper (not in it-all)
bip360-tier-b-cusf-sidecar auto='':
    #!/usr/bin/env bash
    set -euo pipefail
    echo "==> Tier B CUSF miner sidecar (expect PASS): inventory → submitblock → tip retained"
    cargo build -p bip300301_enforcer --features "drivechain,bip360"
    cargo build -p cusf_miner_sidecar
    cargo build --example integration_tests --features bip360
    export BIP300301_ENFORCER_INTEGRATION_TEST_ENV="{{env_file}}"
    export BIP300301_ENFORCER="{{enforcer_bin}}"
    export BIP360_SKIP_REBUILD=1
    just _run-it bip360_tier_b_cusf_sidecar "{{auto}}"

# TB-sendraw: Bob mempool shapes 1+2+3 (opt-in; needs BITCOIND_P2MR)
bip360-tier-b-mempool auto='':
    #!/usr/bin/env bash
    set -euo pipefail
    ENV_FILE="{{env_file}}"
    if [ -f "$ENV_FILE" ]; then
        set -a
        # shellcheck disable=SC1090
        source "$ENV_FILE"
        set +a
    fi
    if [ -z "${BITCOIND_P2MR:-}" ] || [ ! -x "${BITCOIND_P2MR}" ]; then
        if [ "{{auto}}" = "yes" ] || [ "{{auto}}" = "1" ]; then
            just setup-p2mr
            set -a
            # shellcheck disable=SC1090
            source "$ENV_FILE"
            set +a
        else
            echo "BITCOIND_P2MR required — run: just setup-p2mr" >&2
            exit 1
        fi
    fi
    if [ -z "${ELECTRS:-}" ] || [ ! -x "${ELECTRS}" ]; then
        if [ "{{auto}}" = "yes" ] || [ "{{auto}}" = "1" ]; then
            just setup
            set -a
            # shellcheck disable=SC1090
            source "$ENV_FILE"
            set +a
        else
            echo "ELECTRS required — run: just setup" >&2
            exit 1
        fi
    fi
    echo "==> Tier B TB-sendraw: Bob mempool interop (shapes 1 Schnorr + 2 Core hybrid + 3 kitchen-sink)"
    echo "    Expect PASS all three hard green gates. Green tip twin: just bip360-tier-b-cusf."
    echo "    Docs: docs/TIER_B_P2MR_MEMPOOL.md"
    cargo build -p bip300301_enforcer --features "drivechain,bip360"
    cargo build --example integration_tests --features bip360
    export BIP300301_ENFORCER_INTEGRATION_TEST_ENV="{{env_file}}"
    export BIP300301_ENFORCER="{{enforcer_bin}}"
    export BIP360_SKIP_REBUILD=1
    export BIP360_TIER_B=1
    just _run-it bip360_tier_b_p2mr_mempool "{{auto}}"

# Bob-mined P2MR block vs Alice blk*.dat (needs P2MR + electrs)
bip360-blk-dat-e2e auto='':
    #!/usr/bin/env bash
    set -euo pipefail
    ENV_FILE="{{env_file}}"
    if [ -f "$ENV_FILE" ]; then
        set -a
        # shellcheck disable=SC1090
        source "$ENV_FILE"
        set +a
    fi
    if [ -z "${BITCOIND_P2MR:-}" ] || [ ! -x "${BITCOIND_P2MR}" ]; then
        if [ "{{auto}}" = "yes" ] || [ "{{auto}}" = "1" ]; then
            just setup-p2mr
            set -a
            # shellcheck disable=SC1090
            source "$ENV_FILE"
            set +a
        else
            echo "BITCOIND_P2MR required — run: just setup-p2mr" >&2
            exit 1
        fi
    fi
    if [ -z "${ELECTRS:-}" ] || [ ! -x "${ELECTRS}" ]; then
        if [ "{{auto}}" = "yes" ] || [ "{{auto}}" = "1" ]; then
            just setup
            set -a
            # shellcheck disable=SC1090
            source "$ENV_FILE"
            set +a
        else
            echo "ELECTRS required — run: just setup" >&2
            exit 1
        fi
    fi
    echo "==> E2E blk.dat: Bob-mined block vs Alice on-disk (enforcer keeps tip)"
    cargo build -p bip300301_enforcer --features "drivechain,bip360"
    cargo build --example integration_tests --features bip360
    export BIP300301_ENFORCER_INTEGRATION_TEST_ENV="{{env_file}}"
    export BIP300301_ENFORCER="{{enforcer_bin}}"
    export BIP360_SKIP_REBUILD=1
    just _run-it bip360_blk_dat_e2e "{{auto}}"

# Dual stock: Miner block == Alice blk*.dat (drivechain enforcer)
drivechain-blk-dat-e2e auto='':
    #!/usr/bin/env bash
    set -euo pipefail
    ENV_FILE="{{env_file}}"
    if [ -f "$ENV_FILE" ]; then
        set -a
        # shellcheck disable=SC1090
        source "$ENV_FILE"
        set +a
    fi
    if [ -z "${BITCOIND_UNPATCHED:-}" ] || [ ! -x "${BITCOIND_UNPATCHED}" ]; then
        if [ "{{auto}}" = "yes" ] || [ "{{auto}}" = "1" ]; then
            just setup-core
            set -a
            # shellcheck disable=SC1090
            source "$ENV_FILE"
            set +a
        else
            echo "BITCOIND_UNPATCHED required — run: just setup-core" >&2
            exit 1
        fi
    fi
    echo "==> E2E blk.dat (drivechain): Miner block vs Alice on-disk (enforcer keeps tip)"
    cargo build -p bip300301_enforcer --features drivechain
    cargo build --example integration_tests --features drivechain
    export BIP300301_ENFORCER_INTEGRATION_TEST_ENV="{{env_file}}"
    export BIP300301_ENFORCER="{{enforcer_bin}}"
    export BIP360_SKIP_REBUILD=1
    just _run-it drivechain_blk_dat_e2e "{{auto}}"

# Claim pins: testmempoolaccept no-insert; stock rejects P2MR spend
cusf-claims auto='':
    #!/usr/bin/env bash
    set -euo pipefail
    ENV_FILE="{{env_file}}"
    if [ -f "$ENV_FILE" ]; then
        set -a
        # shellcheck disable=SC1090
        source "$ENV_FILE"
        set +a
    fi
    cargo build -p bip300301_enforcer --features "drivechain,bip360"
    cargo build --example integration_tests --features bip360
    export BIP300301_ENFORCER_INTEGRATION_TEST_ENV="{{env_file}}"
    export BIP300301_ENFORCER="{{enforcer_bin}}"
    export BIP360_SKIP_REBUILD=1
    just _run-it cusf_claim_testmempoolaccept_no_insert "{{auto}}"
    just _run-it cusf_claim_stock_rejects_p2mr_spend "{{auto}}"

# Full local verify stack (just verify + optional e2es; pass yes to auto-setup)
bip360-verify-full auto='':
    just drivechain-smoke
    just verify
    just build-bip360
    just bip360-block-matrix {{auto}}
    just bip360-p2p-e2e {{auto}}

it-all auto='':
    #!/usr/bin/env bash
    set -euo pipefail
    just build-bip360
    export BIP360_SKIP_REBUILD=1
    trials=(
        bip360_valid_schnorr_spend bip360_valid_mldsa_spend bip360_valid_slh_spend
        bip360_valid_cross_block_schnorr_spend bip360_valid_cross_block_mldsa_spend
        bip360_valid_cross_block_slh_spend bip360_valid_hybrid_ec_slh_spend
        bip360_valid_hybrid_ec_slh_cross_block_spend bip360_invalid_block
        bip360_invalid_signature bip360_invalid_pubkey_size bip360_invalid_merkle_path
        bip360_invalid_cross_block_signature_schnorr bip360_invalid_cross_block_signature_mldsa
        bip360_invalid_cross_block_signature_slh bip360_invalid_cross_block_merkle_path_schnorr
        bip360_invalid_cross_block_merkle_path_mldsa bip360_invalid_cross_block_merkle_path_slh
        bip360_invalid_cross_block_pubkey_size_mldsa bip360_invalid_hybrid_ec_slh_tamper_ec_sig
        bip360_invalid_hybrid_ec_slh_tamper_slh_sig bip360_invalid_hybrid_ec_slh_swap_sigs
        bip360_valid_kitchen_sink_spend bip360_invalid_kitchen_sink_tamper_ec_sig
        bip360_valid_multi_leaf_schnorr_spend bip360_valid_multi_leaf_mldsa_spend
        bip360_valid_multi_leaf_slh_spend bip360_valid_multi_leaf_cross_block_mldsa_spend
        bip360_invalid_multi_leaf_wrong_control_block
        bip360_invalid_multi_leaf_cross_block_wrong_control_block
        bip360_valid_multi_leaf_cross_block_schnorr_spend
        bip360_valid_multi_leaf_cross_block_slh_spend
        bip360_invalid_multi_leaf_tampered_signature_mldsa
        bip360_tier_b_cusf_miner
    )
    for trial in "${trials[@]}"; do
        echo "==> $trial"
        just _run-it "$trial" "{{auto}}"
    done

[private]
_run-it trial auto_setup='':
    #!/usr/bin/env bash
    set -euo pipefail
    ENV_FILE="{{env_file}}"
    ENFORCER="{{enforcer_bin}}"
    TRIAL="{{trial}}"
    AUTO_SETUP="{{auto_setup}}"
    if [ ! -f "$ENV_FILE" ]; then
        if [ "$AUTO_SETUP" = "yes" ] || [ "$AUTO_SETUP" = "1" ]; then
            echo "WARN: env file $ENV_FILE missing — running just setup-core" >&2
            just setup-core
            ENV_FILE="integrationtests.env"
        else
            echo "env file $ENV_FILE not found — run: just setup-core" >&2
            echo "or re-run with: just it $TRIAL yes" >&2
            exit 1
        fi
    fi
    set -a
    # shellcheck disable=SC1090
    source "$ENV_FILE"
    set +a
    if [ "${BIP360_SKIP_REBUILD:-}" != "1" ]; then
        # drivechain-smoke / default `cargo build` leaves a drivechain-only enforcer
        # that lacks bip360 CLI flags (`--activation-height`, etc.).
        echo "==> building bip360 binaries"
        just build-bip360
    fi
    export BIP300301_ENFORCER_INTEGRATION_TEST_ENV="$ENV_FILE"
    export BIP300301_ENFORCER="$ENFORCER"
    if [ -n "${BITCOIND_UNPATCHED:-}" ] && [ -x "${BITCOIND_UNPATCHED}" ]; then
        export BITCOIND="${BITCOIND_UNPATCHED}"
        export BITCOIN_CLI="$(dirname "$BITCOIND_UNPATCHED")/bitcoin-cli"
        export BITCOIN_UTIL="$(dirname "$BITCOIND_UNPATCHED")/bitcoin-util"
        echo "==> using stock bitcoind: $BITCOIND"
    elif [ -z "${BITCOIND:-}" ] || [ ! -x "$BITCOIND" ]; then
        echo "BITCOIND_UNPATCHED not set or not executable — run: just setup-core" >&2
        exit 1
    fi
    echo "==> running integration trial: $TRIAL"
    cargo run --example integration_tests --features bip360 -- --exact "$TRIAL"

verify-reflection:
    #!/usr/bin/env bash
    set -euo pipefail
    ENFORCER="{{enforcer_bin}}"
    BITCOIND="${BITCOIND:-bitcoind}"
    BITCOIND_RPC_PORT=18943
    BITCOIND_ZMQ_PORT=18944
    ENFORCER_GRPC_PORT=18945
    GRPC_ADDR="127.0.0.1:$ENFORCER_GRPC_PORT"
    for tool in grpcurl buf; do
        command -v "$tool" >/dev/null || { echo "missing required command: $tool" >&2; exit 1; }
    done
    if [ ! -x "$BITCOIND" ] && ! command -v "$BITCOIND" >/dev/null; then
        echo "missing or not executable bitcoind: $BITCOIND" >&2
        exit 1
    fi
    if [ ! -x "$ENFORCER" ]; then
        echo "missing or not executable enforcer: $ENFORCER" >&2
        exit 1
    fi
    WORK_DIR="$(mktemp -d)"
    BITCOIND_PID=""
    ENFORCER_PID=""
    cleanup() {
        [ -n "$ENFORCER_PID" ] && kill "$ENFORCER_PID" 2>/dev/null || true
        [ -n "$BITCOIND_PID" ] && kill "$BITCOIND_PID" 2>/dev/null || true
        wait 2>/dev/null || true
        rm -rf "$WORK_DIR"
    }
    trap cleanup EXIT
    wait_for_port() {
        local port="$1" name="$2"
        for _ in $(seq 1 100); do
            if (exec 3<> "/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
                exec 3>&- 3<&-
                return 0
            fi
            sleep 0.2
        done
        echo "$name did not open port $port in time" >&2
        return 1
    }
    fail() {
        echo "FAIL: $1" >&2
        cat "$WORK_DIR/enforcer.log" >&2
        exit 1
    }
    mkdir -p "$WORK_DIR/bitcoind"
    "$BITCOIND" -regtest -datadir="$WORK_DIR/bitcoind" -rpcport="$BITCOIND_RPC_PORT" \
        -rpcuser=reflection -rpcpassword=verify \
        -zmqpubsequence="tcp://127.0.0.1:$BITCOIND_ZMQ_PORT" -listen=0 -server=1 -rest=1 \
        >"$WORK_DIR/bitcoind.log" 2>&1 &
    BITCOIND_PID=$!
    wait_for_port "$BITCOIND_RPC_PORT" bitcoind
    "$ENFORCER" --data-dir "$WORK_DIR/enforcer" \
        --node-rpc-addr="127.0.0.1:$BITCOIND_RPC_PORT" --node-rpc-user=reflection \
        --node-rpc-pass=verify --node-zmq-addr-sequence="tcp://127.0.0.1:$BITCOIND_ZMQ_PORT" \
        --serve-grpc-addr="$GRPC_ADDR" >"$WORK_DIR/enforcer.log" 2>&1 &
    ENFORCER_PID=$!
    wait_for_port "$ENFORCER_GRPC_PORT" enforcer || { cat "$WORK_DIR/enforcer.log" >&2; exit 1; }
    EXPECTED_SERVICES="$(printf '%s\n' \
        'cusf.crypto.v1.CryptoService' \
        'cusf.mainchain.v1.ValidatorService' \
        'cusf.mainchain.v1.WalletService' \
        'cusf.sidechain.v1.SidechainService' \
        'grpc.reflection.v1.ServerReflection' \
        'grpc.reflection.v1alpha.ServerReflection')"
    ACTUAL_SERVICES="$(grpcurl -plaintext "$GRPC_ADDR" list | sort)"
    [ "$ACTUAL_SERVICES" = "$EXPECTED_SERVICES" ] || fail "grpcurl list returned unexpected services"
    grpcurl -plaintext "$GRPC_ADDR" describe cusf.mainchain.v1.ValidatorService \
        | grep -q GetBlockHeaderInfo || fail "grpcurl describe is missing GetBlockHeaderInfo"
    RIPEMD_REQUEST='{"msg":{"hex":"616263"}}'
    RIPEMD_DIGEST='8eb208f7e05d987a9b044a8e98c6b087f15a0bfc'
    grpcurl -plaintext -d "$RIPEMD_REQUEST" "$GRPC_ADDR" cusf.crypto.v1.CryptoService/Ripemd160 \
        | grep -q "$RIPEMD_DIGEST" || fail "grpcurl Ripemd160 returned wrong digest"
    buf curl --protocol grpc --http2-prior-knowledge -d "$RIPEMD_REQUEST" \
        "http://$GRPC_ADDR/cusf.crypto.v1.CryptoService/Ripemd160" \
        | grep -q "$RIPEMD_DIGEST" || fail "buf curl (grpc) returned wrong digest"
    buf curl --protocol connect --http2-prior-knowledge --reflect-protocol grpc-v1alpha \
        -d "$RIPEMD_REQUEST" "http://$GRPC_ADDR/cusf.crypto.v1.CryptoService/Ripemd160" \
        | grep -q "$RIPEMD_DIGEST" || fail "buf curl (connect) returned wrong digest"
    echo "OK: reflection verified with grpcurl + buf curl"

# Optional upstream dev tools (scripts/ retained from upstream).

analyze-sync log:
    uv run scripts/analyze_sync_logs.py {{log}}

trace-macos:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$(uname -s)" != "Darwin" ]; then
        echo "trace-macos is macOS-only (uses dtrace)" >&2
        exit 1
    fi
    exec ./scripts/trace_enforcer_macos.sh
