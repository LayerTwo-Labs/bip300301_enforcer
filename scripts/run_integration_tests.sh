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
#   alphanet         the rolling build of ecash-com/bitcoin's alphanet branch
#   all              every flavor in the CI matrix, continuing past failures
#
# Remaining args go to the test runner. Missing dependencies are downloaded
# via setup_integration_tests.sh on first use.
#
# Some flavors skip tests that cannot pass on them (see skip_patterns below).
# Naming a specific FLAVOR *and* a filter that narrows the run overrides those
# skips: that combination is a deliberate "run exactly this, on exactly that
# build", and is how you reach a test the flavor would otherwise hide. A bare
# `--bitcoind FLAVOR` still skips, so CI is unaffected, and `--bitcoind all`
# mirrors CI and never overrides.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

flavor='bitcoin-patched'
flavor_explicit=''
rest=()
while [ $# -gt 0 ]; do
    case "$1" in
        --bitcoind=*) flavor="${1#*=}"; flavor_explicit=1; shift ;;
        --bitcoind) flavor="${2:?--bitcoind requires a value}"; flavor_explicit=1; shift 2 ;;
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
        if KEEP_FLAVOR_SKIPS=1 "${BASH_SOURCE[0]}" --bitcoind "$f" \
            ${rest[@]+"${rest[@]}"} 2>&1 | tee "$log"; then
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
        # Shared with the CI integration-test job, which applies the same
        # list; see the file for why each entry cannot run on stock.
        while IFS= read -r pattern; do
            skip_patterns+=("$pattern")
        done < <(grep -vE '^[[:space:]]*(#|$)' "$REPO_ROOT/scripts/stock-skip-patterns.txt")
        ;;
    drynet* | alphanet)
        # A pinned `drynetN` tag has to be named for setup to fetch that one;
        # `alphanet` is a fixed name that setup always fetches.
        case "$flavor" in drynet*) setup_env=("DRYNET_REVISION=$flavor") ;; esac
        ;;
    *)
        echo "unknown --bitcoind flavor '$flavor' (expected bitcoin-patched, unpatched, stock-X.Y, drynetN, alphanet, or all)" >&2
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
    # A caller-supplied `--list` would collide with the one added here, which
    # the runner rejects outright, so drop it from the args we reuse.
    list_args=()
    for arg in ${rest[@]+"${rest[@]}"}; do
        [ "$arg" = '--list' ] || list_args+=("$arg")
    done
    listing=$(run_tests --list ${list_args[@]+"${list_args[@]}"} | sed -n 's/: test$//p')
    # Did the caller narrow the run? Rather than re-parsing the test runner's
    # arguments here, ask it: a selection smaller than the whole suite means
    # its filter matched. That way `--exact`, a caller's own `--skip`, and any
    # future filtering flag are all accounted for by the parser that owns them.
    force=''
    if [ -n "$flavor_explicit" ] && [ -z "${KEEP_FLAVOR_SKIPS:-}" ] \
        && [ "$listing" != "$(run_tests --list | sed -n 's/: test$//p')" ]; then
        force=1
    fi
    for pattern in "${skip_patterns[@]}"; do
        matched=$(printf '%s\n' "$listing" | grep -aF "$pattern" || true)
        if [ -n "$force" ]; then
            # Say so, loudly: these normally do not run on this build, so a
            # failure here is expected rather than a regression.
            if [ -n "$matched" ]; then
                printf '%s\n' "$matched" \
                    | sed "s/^/forced (normally skipped on $flavor): /"
            fi
            continue
        fi
        skip_args+=('--skip' "$pattern")
        if [ -n "$matched" ]; then
            printf '%s\n' "$matched" | sed 's/^/skipped: /'
        fi
    done
fi

run_tests ${rest[@]+"${rest[@]}"} ${skip_args[@]+"${skip_args[@]}"}
