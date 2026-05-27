default:
    @just --list

# Regenerate checked-in protobuf code under lib/proto/generated/ via buf.
# The proto source ref is pinned in buf.gen.yaml. Bump it there to upgrade.
generate:
    buf generate --clean

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
