ARG RUST_VERSION=1

FROM rust:${RUST_VERSION}-slim-bookworm AS chef
RUN cargo install cargo-chef --locked --version 0.1.73
WORKDIR /workspace

# cargo-chef turns the workspace manifests into a stable dependency recipe.
# Source-only changes leave the `cook` layer below reusable.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /workspace/recipe.json recipe.json
RUN cargo chef cook --locked --release \
    --package bip300301_enforcer \
    --bin bip300301_enforcer \
    --recipe-path recipe.json
COPY . .
RUN cargo build --locked --release \
    --package bip300301_enforcer \
    --bin bip300301_enforcer

# Runtime stage
FROM debian:bookworm-slim

# Set automatically by docker buildx: amd64 or arm64
ARG TARGETARCH

RUN apt-get update && apt-get install -y curl

# Install grpc_health_probe, for usage in health checks
RUN GRPC_HEALTH_PROBE_VERSION=v0.4.37 && \
    curl -fsSL https://github.com/grpc-ecosystem/grpc-health-probe/releases/download/${GRPC_HEALTH_PROBE_VERSION}/grpc_health_probe-linux-${TARGETARCH} -o /bin/grpc_health_probe && \
    chmod +x /bin/grpc_health_probe

COPY --from=builder /workspace/target/release/bip300301_enforcer /bin/

# Verify we placed the binary in the right place, 
# and that it's executable.
RUN bip300301_enforcer --help

ENTRYPOINT ["bip300301_enforcer"]
