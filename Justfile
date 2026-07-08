import? 'local.just'

env_file := env_var_or_default('BIP300301_ENFORCER_INTEGRATION_TEST_ENV', 'integrationtests.env')
enforcer_bin := env_var_or_default('BIP300301_ENFORCER', 'target/debug/bip300301_enforcer')

default:
    @just --list

# Regenerate checked-in protobuf code under lib/proto/generated/ via buf.
# The proto source ref is pinned in buf.gen.yaml. Bump it there to upgrade.
#
# NB: no `--include-imports`/`--include-wkt`: well-known types come from the
# `buffa-types` crate, not from generated code.
generate:
    buf generate --clean

# Benchmark a from-scratch signet sync. Each run creates a brand-new, isolated
# data dir with a random suffix (./datadir-sync-benchmark.XXXXXX). Logs stats
# and a consensus-state digest on exit, and writes the full consensus state
# to <data-dir>/consensus-state.json so runs are easy to diff.
#
# The single argument (default 0) is either:
#   - a block height to sync to (0 = the chain tip), or
#   - a path to a consensus-state.json file from a previous run: we sync to that
#     file's tip height and then verify our consensus state matches it, exiting
#     non-zero on any mismatch.
#
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

clippy:
    cargo clippy --all-targets --all-features --fix --allow-dirty --allow-staged -- --deny warnings
    cargo +nightly clippy -- -A clippy::all -D unqualified_local_imports -Zcrate-attr="feature(unqualified_local_imports)"

build *args='':
    cargo build --all-features {{ args }}

fmt:
    cargo +nightly fmt --all
    bunx prettier --write .

# Run integration tests (drivechain default; pass trial names / flags after --).
test-it *args='':
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build
    export BIP300301_ENFORCER_INTEGRATION_TEST_ENV='{{ justfile_directory() }}/integrationtests.env'
    cargo run --example integration_tests -- {{ args }}

# --- BIP 360 verification (AGENTS.md / CI check-bip360) ---

test-quantum:
    cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 quantum::

test-drivechain:
    cargo test -p bip300301_enforcer_lib

clippy-bip360:
    cargo clippy -p bip300301_enforcer_lib --no-default-features --features bip360 -- -D warnings
    cargo clippy -p bip300301_enforcer_integration_tests --features bip360 -- -D warnings

fmt-check:
    cargo fmt --all -- --check

check-bip360:
    cargo check --no-default-features --features bip360 --all-targets

check-integration-build:
    cargo check --example integration_tests --features bip360

p2mr-signer-smoke:
    cargo build --example p2mr_signer --no-default-features --features bip360
    cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 p2mr_signer_roundtrip -- --nocapture
    cargo run --example p2mr_signer --no-default-features --features bip360 -- \
        --algorithm schnorr \
        --entropy-hex 1111111111111111111111111111111111111111111111111111111111111111 \
        | grep -q signed_spend_tx_hex

verify: check-bip360 test-quantum p2mr-signer-smoke test-drivechain clippy-bip360 fmt-check check-integration-build

build-bip360:
    cargo build -p bip300301_enforcer --no-default-features --features bip360
    cargo build --example integration_tests --features bip360

# Download stock bitcoind only; write integrationtests.env for BIP 360 live trials.
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

# Full upstream bootstrap: patched + stock bitcoind, electrs, signet miner.
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
        (cd "$ELECTRS_DIR" && cargo build --locked --release)
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
    @echo "  just it-all          # all 31 BIP 360 trials"

demo-a auto='':
    @just _run-it bip360_valid_schnorr_spend {{auto}}

demo-b auto='':
    @just _run-it bip360_invalid_block {{auto}}

it trial auto='':
    @just _run-it {{trial}} {{auto}}

it-all auto='':
    #!/usr/bin/env bash
    set -euo pipefail
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
        bip360_valid_multi_leaf_schnorr_spend bip360_valid_multi_leaf_mldsa_spend
        bip360_valid_multi_leaf_slh_spend bip360_valid_multi_leaf_cross_block_mldsa_spend
        bip360_invalid_multi_leaf_wrong_control_block
        bip360_invalid_multi_leaf_cross_block_wrong_control_block
        bip360_valid_multi_leaf_cross_block_schnorr_spend
        bip360_valid_multi_leaf_cross_block_slh_spend
        bip360_invalid_multi_leaf_tampered_signature_mldsa
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
    if [ ! -x "$ENFORCER" ] || [ ! -x "target/debug/examples/integration_tests" ]; then
        echo "==> building bip360 binaries"
        just build-bip360
    else
        echo "==> bip360 binaries already built — skipping rebuild"
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
