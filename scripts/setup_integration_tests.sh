#!/usr/bin/env bash
# Set up integration-test dependencies and write integrationtests.env.
# Idempotent. Re-running re-uses cached artifacts.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPS_DIR="$REPO_ROOT/.integration-deps"

BITCOIN_VERSION="30.2"
ELECTRS_VERSION="v3.2.0"
PATCHED_REVISION="latest"

PATCHED_DIR="$DEPS_DIR/bitcoin-patched-$PATCHED_REVISION"
UNPATCHED_DIR="$DEPS_DIR/bitcoin-stock-$BITCOIN_VERSION"
SIGNET_REPO_DIR="$DEPS_DIR/bitcoin-patched-repo"
ELECTRS_DIR="$DEPS_DIR/electrs-$ELECTRS_VERSION"

# `releases.drivechain.info` only publishes patched bitcoin for
# x86_64-{linux,darwin,windows}. arm64 falls back to the x86_64 darwin
# build (Rosetta). Stock Bitcoin Core has native arm64 builds.
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$OS-$ARCH" in
    linux-x86_64)  STOCK_TARGET="x86_64-linux-gnu";    PATCHED_TARGET="x86_64-unknown-linux-gnu" ;;
    darwin-x86_64) STOCK_TARGET="x86_64-apple-darwin"; PATCHED_TARGET="x86_64-apple-darwin" ;;
    darwin-arm64)  STOCK_TARGET="arm64-apple-darwin";  PATCHED_TARGET="x86_64-apple-darwin" ;;
    *) echo "Unsupported platform: $OS-$ARCH (no patched binary published)" >&2; exit 1 ;;
esac

mkdir -p "$DEPS_DIR"

# Render an absolute path as relative to REPO_ROOT.
relpath() {
    python3 -c 'import os, sys; print(os.path.relpath(sys.argv[1], sys.argv[2]))' "$1" "$REPO_ROOT"
}

# --- Patched Bitcoin Core ---
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

# --- Stock Bitcoin Core (BITCOIND_UNPATCHED) ---
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

# --- bitcoin-patched repo (signet miner script only) ---
if [ ! -f "$SIGNET_REPO_DIR/contrib/signet/miner" ]; then
    echo "Cloning bitcoin-patched for signet miner script..."
    rm -rf "$SIGNET_REPO_DIR"
    git clone --depth 1 https://github.com/LayerTwo-Labs/bitcoin-patched.git "$SIGNET_REPO_DIR"
else
    echo "Signet miner repo: cached"
fi

# --- electrs (built from source) ---
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

# --- Write integrationtests.env ---
ENV_FILE="$REPO_ROOT/integrationtests.env"
cat > "$ENV_FILE" <<EOF
BIP300301_ENFORCER='target/debug/bip300301_enforcer'
BITCOIND='$(relpath "$PATCHED_DIR/bitcoind")'
BITCOIND_UNPATCHED='$(relpath "$UNPATCHED_DIR/bitcoind")'
BITCOIN_CLI='$(relpath "$PATCHED_DIR/bitcoin-cli")'
BITCOIN_UTIL='$(relpath "$PATCHED_DIR/bitcoin-util")'
ELECTRS='$(relpath "$ELECTRS_BIN")'
SIGNET_MINER='$(relpath "$SIGNET_REPO_DIR/contrib/signet/miner")'
EOF

echo
echo "Wrote $(relpath "$ENV_FILE")"
echo "Run integration tests with: just test-it"
