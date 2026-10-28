#!/usr/bin/env bash
# Fail if an entry of stock-skip-patterns.txt matches no registered test.
#
# Usage: INTEGRATION_TEST_RUNNER --list | check_stock_skip_patterns.sh
#
# Patterns are `--skip` substrings, so one that matches nothing skips nothing
# and never complains: a renamed or retired test would otherwise stay listed.

set -euo pipefail

patterns_file="$(dirname "${BASH_SOURCE[0]}")/stock-skip-patterns.txt"

listing=$(sed -n 's/: test$//p')
if [ -z "$listing" ]; then
    echo 'ERROR: no tests on stdin; pipe in the runner'"'"'s --list output' >&2
    exit 1
fi

stale=0
while IFS= read -r pattern; do
    if ! printf '%s\n' "$listing" | grep -aqF -- "$pattern"; then
        echo "ERROR: stock-skip-patterns.txt entry '$pattern' matches no registered test" >&2
        stale=1
    fi
done < <(grep -vE '^[[:space:]]*(#|$)' "$patterns_file")

if [ "$stale" -ne 0 ]; then
    echo 'Remove or rename the entries above in scripts/stock-skip-patterns.txt.' >&2
    exit 1
fi
