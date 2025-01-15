# Stable Rust version, as of January 2015. If updating, also update
# the version in the Dockerfile.
FROM rust:1.84-slim-bookworm AS builder
WORKDIR /workspace
COPY . .
RUN cargo build --locked --release

# Runtime stage
FROM debian:bookworm-slim

COPY --from=builder /workspace/target/release/bip300301_enforcer /bin/

# Verify we placed the binary in the right place, 
# and that it's executable.
RUN bip300301_enforcer --help

ENTRYPOINT ["bip300301_enforcer"]
