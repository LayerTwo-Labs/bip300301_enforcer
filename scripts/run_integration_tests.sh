#!/usr/bin/env bash
# Run the integration tests against a chosen Bitcoin Core build.
#
# Usage: run_integration_tests.sh [--bitcoind FLAVOR] [test-runner args...]
#
# FLAVOR is one of (default: bitcoin-patched):
#   bitcoin-patched  LayerTwo-Labs bitcoin-patched
#   unpatched        newest stock Bitcoin Core release
#   stock-X.Y        a specific stock release from CI_BITCOIN_CORE_VERSIONS
#   drynetN          the ecash-com/bitcoin drynet fork at that tag
#   all              every flavor in the CI matrix, continuing past failures
#
# Remaining args go to the test runner. Missing dependencies are downloaded
# via setup_integration_tests.sh on first use.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

flavor='bitcoin-patched'
rest=()
while [ $# -gt 0 ]; do
    case "$1" in
        --bitcoind=*) flavor="${1#*=}"; shift ;;
        --bitcoind) flavor="${2:?--bitcoind requires a value}"; shift 2 ;;
        *) rest+=("$1"); shift ;;
    esac
done

if [ "$flavor" = 'all' ]; then
    # Mirror the CI matrix. Like CI's fail-fast: false, run every flavor,
    # then print a per-flavor summary. Passing is the base case — only
    # failed and skipped tests are listed by name.
    flavors=($("$REPO_ROOT/scripts/setup_integration_tests.sh" --print-flavors))
    if [ ${#flavors[@]} -eq 0 ]; then
        echo 'setup_integration_tests.sh --print-flavors returned nothing' >&2
        exit 1
    fi
    logdir=$(mktemp -d)
    trap 'rm -rf "$logdir"' EXIT
    summary="$logdir/summary"
    : > "$summary"
    overall=0
    for f in "${flavors[@]}"; do
        echo "=== integration tests (--bitcoind $f) ==="
        log="$logdir/$f.log"
        if "${BASH_SOURCE[0]}" --bitcoind "$f" ${rest[@]+"${rest[@]}"} 2>&1 | tee "$log"; then
            status=0
        else
            status=1
            overall=1
        fi
        # Summarize from the run's output: libtest's result line, per-test
        # FAILED lines, and the `skipped: ` lines emitted below.
        passed=$(grep -aoE '[0-9]+ passed' "$log" | tail -1 || true)
        n_skipped=$(grep -ac '^skipped: ' "$log" || true)
        n_failed=$(grep -acE ' \.\.\. FAILED$' "$log" || true)
        line="$f: ${passed:-0 passed}"
        [ "$n_skipped" -gt 0 ] && line="$line, $n_skipped skipped"
        if [ "$status" -ne 0 ]; then
            if [ "$n_failed" -gt 0 ]; then
                line="$line, $n_failed FAILED"
            else
                line="$line, FAILED without test results (see its output above)"
            fi
        fi
        echo "$line" >> "$summary"
        # `[^ ] *` trims the padding libtest inserts between name and dots.
        sed -n 's/^test \(.*[^ ]\) *\.\.\. FAILED$/    FAILED: \1/p' "$log" >> "$summary"
        sed -n 's/^skipped: \(.*\)/    skipped: \1/p' "$log" >> "$summary"
    done
    echo
    echo '=== flavor summary ==='
    cat "$summary"
    exit "$overall"
fi

setup_env=()
skip_patterns=()
env_file="integrationtests.$flavor.env"
case "$flavor" in
    bitcoin-patched) env_file='integrationtests.env' ;;
    unpatched | stock-*)
        # Stock Bitcoin Core lacks the drivechain consensus rules
        # (BIP300 opcodes), matching the stock CI matrix entries.
        skip_patterns=('deposit_withdraw_roundtrip')
        ;;
    drynet*)
        setup_env=("DRYNET_REVISION=$flavor")
        if [ -n "$(DRYNET_REVISION="$flavor" \
            "$REPO_ROOT/scripts/setup_integration_tests.sh" --print-drynet-magic)" ]; then
            skip_patterns=('peer_bmm_request')
        fi
        ;;
    *)
        echo "unknown --bitcoind flavor '$flavor' (expected bitcoin-patched, unpatched, stock-X.Y, drynetN, or all)" >&2
        exit 1
        ;;
esac

# The env files use paths relative to the repo root (see
# setup_integration_tests.sh), and the tests expect to run from there.
cd "$REPO_ROOT"

if [ ! -f "$env_file" ]; then
    env ${setup_env[@]+"${setup_env[@]}"} "$REPO_ROOT/scripts/setup_integration_tests.sh"
fi
cargo build

run_tests() {
    env BIP300301_ENFORCER_INTEGRATION_TEST_ENV="$REPO_ROOT/$env_file" \
        cargo run --example integration_tests -- "$@"
}

skip_args=()
if [ ${#skip_patterns[@]} -gt 0 ]; then
    # Name the tests this flavor's skip patterns exclude (on top of any
    # user-supplied filter); `--skip` is substring matching, so a listing
    # filtered by the pattern is exactly the excluded set. The `skipped: `
    # prefix is what the `--bitcoind all` summary parses.
    listing=$(run_tests --list ${rest[@]+"${rest[@]}"} | sed -n 's/: test$//p')
    for pattern in "${skip_patterns[@]}"; do
        skip_args+=('--skip' "$pattern")
        printf '%s\n' "$listing" | grep -aF "$pattern" | sed 's/^/skipped: /' || true
    done
fi

run_tests ${rest[@]+"${rest[@]}"} ${skip_args[@]+"${skip_args[@]}"}
