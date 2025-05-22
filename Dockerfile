ARG RUST_VERSION=1

FROM rust:${RUST_VERSION}-slim-bookworm AS builder
WORKDIR /workspace
COPY . .
RUN cargo build --locked --release

# Runtime stage
FROM debian:bookworm-slim

# Install dependencies needed for signet mining
# TODO: git is needed for fetching the Bitcoin Core mining script. Arguably 
# it'd be a lot better to just include that script as part of the build process!
RUN apt-get update && apt-get install -y \
    python3 curl git

# Download and extract Bitcoin Core binaries
RUN cd /tmp \
    && curl -L https://bitcoincore.org/bin/bitcoin-core-28.0/bitcoin-28.0-x86_64-linux-gnu.tar.gz | tar xz \
    && cp bitcoin-28.0/bin/bitcoin-cli /bin/ \
    && cp bitcoin-28.0/bin/bitcoin-util /bin/ \
    && rm -rf bitcoin-28.0 

# Install grpc_health_probe, for usage in health checks
RUN GRPC_HEALTH_PROBE_VERSION=v0.4.37 && \
    curl -fsSL https://github.com/grpc-ecosystem/grpc-health-probe/releases/download/${GRPC_HEALTH_PROBE_VERSION}/grpc_health_probe-linux-amd64 -o /bin/grpc_health_probe && \
    chmod +x /bin/grpc_health_probe

COPY --from=builder /workspace/target/release/bip300301_enforcer /bin/

# Verify we placed the binary in the right place, 
# and that it's executable.
RUN bip300301_enforcer --help

ENTRYPOINT ["bip300301_enforcer"]
