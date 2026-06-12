import? 'local.just'

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

# Run integration tests
test-it *args='':
    #!/usr/bin/env bash
    cargo build
    env BIP300301_ENFORCER_INTEGRATION_TEST_ENV='{{ justfile_directory() }}/integrationtests.env' \
        cargo run --example integration_tests -- {{ args }}
