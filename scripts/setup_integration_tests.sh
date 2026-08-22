#!/usr/bin/env bash
# Set up integration-test dependencies and write integrationtests.env.
# Idempotent. Re-running re-uses cached artifacts.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Deps live at the primary worktree so all worktrees of this clone share them.
# `git rev-parse --git-common-dir` returns the primary worktree's .git regardless
# of which worktree we're invoked from.
GIT_COMMON_DIR="$(cd "$REPO_ROOT" && git rev-parse --git-common-dir)"
case "$GIT_COMMON_DIR" in
    /*) ;;
    *)  GIT_COMMON_DIR="$REPO_ROOT/$GIT_COMMON_DIR" ;;
esac
DEPS_ROOT="$(cd "$GIT_COMMON_DIR/.." && pwd)"
DEPS_DIR="$DEPS_ROOT/.integration-deps"

# Stock Bitcoin Core version to download as BITCOIND_UNPATCHED. Derived from
# CI_BITCOIN_CORE_VERSIONS in lib/version.rs.
VERSION_FILE="$REPO_ROOT/lib/version.rs"
ALL_BITCOIN_VERSIONS="$(grep -oE '"[0-9]+\.[0-9]+"' "$VERSION_FILE" | tr -d '"' || true)"
if [ -z "$ALL_BITCOIN_VERSIONS" ]; then
    echo "Could not parse CI_BITCOIN_CORE_VERSIONS from $VERSION_FILE" >&2
    exit 1
fi
# CI_BITCOIN_CORE_VERSIONS is sorted newest-first; take the newest.
BITCOIN_VERSION="${ALL_BITCOIN_VERSIONS%%$'\n'*}"
ELECTRS_VERSION="v3.2.0"
PATCHED_REVISION="latest"
# Drivechain-patched Bitcoin Core fork from ecash-com/bitcoin. Bump when a
# new drynet tag is published on releases.drivechain.info. CI parses this
# literal out of this script (see compute-integration-matrix in
# .github/workflows/check_lint_build_release.yaml) — keep the
# `DRYNET_DEFAULT_REVISION="..."` shape. Override with the DRYNET_REVISION
# env var (`just test-it --bitcoind drynetN` does this) to fetch a
# different tag.
DRYNET_DEFAULT_REVISION="drynet4"
DRYNET_REVISION="${DRYNET_REVISION:-$DRYNET_DEFAULT_REVISION}"
# The rolling build of ecash-com/bitcoin's `alphanet` branch, republished on
# every push to it. Tested alongside the pinned tag so that L1 changes which
# break the enforcer surface here, rather than when the next tag is cut.
ALPHANET_REVISION="alphanet"
ECASH_REVISIONS="$DRYNET_REVISION $ALPHANET_REVISION"

# Regtest P2P magic of an ecash build, empty when it uses the stock bytes.
ecash_regtest_magic() {
    case "$1" in
        drynet4)  echo "eca5d434" ;;
        alphanet) echo "eca5a134" ;;
        *)        echo "" ;;
    esac
}
DRYNET_REGTEST_MAGIC="$(ecash_regtest_magic "$DRYNET_REVISION")"

# Print the `--bitcoind` flavor of each CI matrix entry, one per line, and
# exit. Lets run_integration_tests.sh (`--bitcoind all`) reuse this
# script's parsing instead of re-grepping the constants.
if [ "${1:-}" = '--print-flavors' ]; then
    echo 'bitcoin-patched'
    echo "$DRYNET_DEFAULT_REVISION"
    echo "$ALPHANET_REVISION"
    for v in $ALL_BITCOIN_VERSIONS; do
        echo "stock-$v"
    done
    exit 0
fi

# Print an ecash build's regtest magic (empty if it uses the stock bytes) and
# exit, so CI and run_integration_tests.sh read it from here rather than
# repeating the literal. Takes the flavor as an argument, defaulting to
# DRYNET_REVISION like the rest of the script.
if [ "${1:-}" = '--print-regtest-magic' ]; then
    ecash_regtest_magic "${2:-$DRYNET_REVISION}"
    exit 0
fi

PATCHED_DIR="$DEPS_DIR/bitcoin-patched-$PATCHED_REVISION"
ecash_dir() { echo "$DEPS_DIR/bitcoin-ecash-$1"; }
UNPATCHED_DIR="$DEPS_DIR/bitcoin-stock-$BITCOIN_VERSION"
SIGNET_REPO_DIR="$DEPS_DIR/bitcoin-patched-repo"
ELECTRS_DIR="$DEPS_DIR/electrs-$ELECTRS_VERSION"
# Pre-mined signet chain, reused across runs. Not built here (it needs the
# integration-test binary); `just test-it` mines it on first use.
SIGNET_CHAIN_DIR="$DEPS_DIR/signet-chain"
# The chain is a datadir written by one specific bitcoind, and its block files
# carry that build's magic. bitcoin-patched and stock derive the same magic from
# the signet challenge, so they can share a chain. A build that rebrands the
# magic cannot read theirs and needs its own. CI keys its signet-chain cache 
# by bitcoind build for this same reason.
ecash_signet_chain_dir() {
    if [ -n "$(ecash_regtest_magic "$1")" ]; then
        echo "$SIGNET_CHAIN_DIR-$1"
    else
        echo "$SIGNET_CHAIN_DIR"
    fi
}

# `releases.drivechain.info` only publishes patched bitcoin for
# x86_64-{linux,darwin,windows}. arm64 falls back to the x86_64 darwin
# build (Rosetta). Stock Bitcoin Core and the ecash drynet builds have
# native arm64 darwin builds.
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$OS-$ARCH" in
    linux-x86_64)  STOCK_TARGET="x86_64-linux-gnu";    PATCHED_TARGET="x86_64-unknown-linux-gnu"; DRYNET_TARGET="x86_64-unknown-linux-gnu" ;;
    darwin-x86_64) STOCK_TARGET="x86_64-apple-darwin"; PATCHED_TARGET="x86_64-apple-darwin";      DRYNET_TARGET="x86_64-apple-darwin" ;;
    darwin-arm64)  STOCK_TARGET="arm64-apple-darwin";  PATCHED_TARGET="x86_64-apple-darwin";      DRYNET_TARGET="aarch64-apple-darwin" ;;
    *) echo "Unsupported platform: $OS-$ARCH (no patched binary published)" >&2; exit 1 ;;
esac

mkdir -p "$DEPS_DIR"

# Download `$1.zip` from releases.drivechain.info (the zip unpacks to a
# directory named after its basename) and install the bitcoin binaries at
# `$2`.
fetch_drivechain_zip() {
    local zip_name="$1" dest_dir="$2"
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    curl -# -fL "https://releases.drivechain.info/$zip_name.zip" -o "$TMP/bins.zip"
    unzip -q "$TMP/bins.zip" -d "$TMP"
    rm -rf "$dest_dir"
    mv "$TMP/$zip_name" "$dest_dir"
    chmod +x "$dest_dir"/bitcoind "$dest_dir"/bitcoin-cli "$dest_dir"/bitcoin-util
    rm -rf "$TMP"
    trap - EXIT
}

# Print the published identity of `$1.zip` on releases.drivechain.info (its
# ETag, falling back to Last-Modified), empty if the request fails. Lets a
# rolling artifact be cached without going stale: one HEAD request tells us
# whether what we hold is still what is published.
remote_zip_version() {
    curl -sfI "https://releases.drivechain.info/$1.zip" \
        | tr -d '\r' \
        | awk 'tolower($1) == "etag:" || tolower($1) == "last-modified:" { $1 = ""; print }' \
        | tr -d ' "' \
        | head -1
}

# Download the stock Bitcoin Core `$1` release tarball from bitcoincore.org
# and install its bin/ at `$2`.
fetch_stock_tarball() {
    local version="$1" dest_dir="$2"
    TMP=$(mktemp -d)
    trap 'rm -rf "$TMP"' EXIT
    local tarball="bitcoin-$version-$STOCK_TARGET.tar.gz"
    curl -# -fL "https://bitcoincore.org/bin/bitcoin-core-$version/$tarball" -o "$TMP/$tarball"
    tar -C "$TMP" -xf "$TMP/$tarball"
    rm -rf "$dest_dir"
    mv "$TMP/bitcoin-$version/bin" "$dest_dir"
    chmod +x "$dest_dir"/bitcoind "$dest_dir"/bitcoin-cli "$dest_dir"/bitcoin-util
    rm -rf "$TMP"
    trap - EXIT
}

# --- Patched Bitcoin Core ---
if [ ! -x "$PATCHED_DIR/bitcoind" ]; then
    echo "Downloading patched bitcoin ($PATCHED_TARGET)..."
    fetch_drivechain_zip "L1-bitcoin-patched-$PATCHED_REVISION-$PATCHED_TARGET" "$PATCHED_DIR"
else
    echo "Patched bitcoin: cached"
fi

# --- ecash Bitcoin Core, one per ECASH_REVISIONS entry ---
# `drynetN` is a pinned tag and never changes; `alphanet` is rebuilt on every
# push to that branch, so a cached copy of it goes stale silently. Both are
# checked against what is published rather than assumed fresh, which costs one
# HEAD request each and keeps a single code path.
for rev in $ECASH_REVISIONS; do
    ECASH_DIR="$(ecash_dir "$rev")"
    ECASH_ZIP="L1-ecash-bitcoin-$rev-$DRYNET_TARGET"
    PUBLISHED="$(remote_zip_version "$ECASH_ZIP" || true)"
    CACHED="$(cat "$ECASH_DIR/.remote-version" 2>/dev/null || true)"
    if [ ! -x "$ECASH_DIR/bitcoind" ] ||
        { [ -n "$PUBLISHED" ] && [ "$PUBLISHED" != "$CACHED" ]; }; then
        echo "Downloading ecash bitcoin $rev ($DRYNET_TARGET)..."
        fetch_drivechain_zip "$ECASH_ZIP" "$ECASH_DIR"
        printf '%s\n' "$PUBLISHED" > "$ECASH_DIR/.remote-version"
    else
        echo "ecash bitcoin $rev: cached"
    fi
done

# --- Stock Bitcoin Core, one per CI_BITCOIN_CORE_VERSIONS entry ---
# The newest doubles as BITCOIND_UNPATCHED for the drivechain-patched
# flavors; the rest let `just test-it --bitcoind stock-<version>` (and
# `--bitcoind all`) mirror the CI matrix locally.
for v in $ALL_BITCOIN_VERSIONS; do
    STOCK_DIR="$DEPS_DIR/bitcoin-stock-$v"
    if [ ! -x "$STOCK_DIR/bitcoind" ]; then
        echo "Downloading stock Bitcoin Core $v ($STOCK_TARGET)..."
        fetch_stock_tarball "$v" "$STOCK_DIR"
    else
        echo "Stock bitcoin $v: cached"
    fi
done

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

# --- Write env files, one per --bitcoind flavor of `just test-it` ---
# Deps paths are absolute (shared across worktrees); the enforcer binary stays
# relative since `target/` is per-worktree and tests run with cwd at the worktree root.
# The files differ in which builds the BITCOIND* vars point at and in
# BITCOIND_HAS_DRIVECHAIN (see `BitcoindKind::accept_nonstd_txns` in
# integration_tests/setup.rs for the standardness rationale). The stock
# flavors use the same binary for BITCOIND and BITCOIND_UNPATCHED,
# mirroring the stock CI matrix entries.
write_env_file() {
    local env_file="$1" bins_dir="$2" unpatched_dir="$3" has_drivechain="$4"
    local regtest_magic="${5:-}"
    local signet_chain_dir="${6:-$SIGNET_CHAIN_DIR}"
    cat > "$env_file" <<EOF
BIP300301_ENFORCER='target/debug/bip300301_enforcer'
BITCOIND='$bins_dir/bitcoind'
BITCOIND_HAS_DRIVECHAIN='$has_drivechain'
BITCOIND_REGTEST_MAGIC='$regtest_magic'
BITCOIND_UNPATCHED='$unpatched_dir/bitcoind'
BITCOIN_CLI='$bins_dir/bitcoin-cli'
BITCOIN_UTIL='$bins_dir/bitcoin-util'
ELECTRS='$ELECTRS_BIN'
SIGNET_MINER='$SIGNET_REPO_DIR/contrib/signet/miner'
SIGNET_CHAIN_DIR='$signet_chain_dir'
EOF
    echo "Wrote $env_file"
}

echo
write_env_file "$REPO_ROOT/integrationtests.env" "$PATCHED_DIR" "$UNPATCHED_DIR" 1
write_env_file "$REPO_ROOT/integrationtests.unpatched.env" "$UNPATCHED_DIR" "$UNPATCHED_DIR" 0
for rev in $ECASH_REVISIONS; do
    write_env_file "$REPO_ROOT/integrationtests.$rev.env" "$(ecash_dir "$rev")" "$UNPATCHED_DIR" 1 \
        "$(ecash_regtest_magic "$rev")" "$(ecash_signet_chain_dir "$rev")"
done
for v in $ALL_BITCOIN_VERSIONS; do
    STOCK_DIR="$DEPS_DIR/bitcoin-stock-$v"
    write_env_file "$REPO_ROOT/integrationtests.stock-$v.env" "$STOCK_DIR" "$STOCK_DIR" 0
done

echo "Deps cache: $DEPS_DIR"
echo "Run integration tests with: just test-it [--bitcoind bitcoin-patched|unpatched|stock-X.Y|drynetN|alphanet|all]"
