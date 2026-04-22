default:
    @just --list

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
    env BIP300301_ENFORCER_INTEGRATION_TEST_ENV='{{ justfile_directory() }}/integrationstests.env' \
        cargo run --example integration_tests -- {{ args }}
