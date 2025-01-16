# Stable Rust version, as of January 2015. If updating, also update
# the version in the Dockerfile.
FROM rust:1.84-slim-bookworm AS builder
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

COPY --from=builder /workspace/target/release/bip300301_enforcer /bin/

# Verify we placed the binary in the right place, 
# and that it's executable.
RUN bip300301_enforcer --help

ENTRYPOINT ["bip300301_enforcer"]
