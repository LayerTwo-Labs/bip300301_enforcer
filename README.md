# Feature matrix

| Feature              | Default  | Scope                                          |
| -------------------- | -------- | ---------------------------------------------- |
| `drivechain`         | yes      | BIP 300/301 sidechain rules (upstream default) |
| `bip360`             | no       | P2MR + PQC validation in `lib/validator/pqc/`  |
| `rustls` / `openssl` | `rustls` | TLS backend                                    |
| `shrincs`            | no       | Reserved placeholder — no implementation       |

| Build                | Command                                               |
| -------------------- | ----------------------------------------------------- |
| Drivechain (default) | `cargo build`                                         |
| BIP 360 CUSF only    | `cargo build --no-default-features --features bip360` |
| Both rule sets       | `cargo build --features "drivechain,bip360"`          |

Default `cargo build` and `just test-drivechain` / `just drivechain-smoke` pass
**without** `bip360` — optional PQC deps (`bitcoin-p2mr-pqc`, `bitcoinpqc`) are
not pulled in. `just drivechain-smoke` is **local-only** — CI wiring remains
**HITL** (Phase D delivered local `bip360-verify-full`).

See [docs/CUSF-BIP360.md](./docs/CUSF-BIP360.md) for BIP 360 activation height,
signature duck typing, and module layout.

Tier B CUSF mining: [docs/TIER_B_CUSF_MINER.md](./docs/TIER_B_CUSF_MINER.md)
(`just bip360-tier-b-cusf`). Optional inventory miner helper (TB-sidecar, not
stock mempool): crate `cusf_miner_sidecar` +
[docs/TIER_B_CUSF_SIDECAR.md](./docs/TIER_B_CUSF_SIDECAR.md)
(`just bip360-tier-b-cusf-sidecar`; not in the green 34-trial matrix).

**Bob mempool interop (opt-in):**
[docs/TIER_B_P2MR_MEMPOOL.md](./docs/TIER_B_P2MR_MEMPOOL.md)
(`just bip360-tier-b-mempool` — shapes 1 Schnorr + 2 Core hybrid + 3
kitchen-sink).

**On-disk block fidelity e2es (opt-in):**

| Recipe                        | What it checks                                                              |
| ----------------------------- | --------------------------------------------------------------------------- |
| `just drivechain-blk-dat-e2e` | Dual stock: Miner block == Alice `blk*.dat` (drivechain enforcer keeps tip) |
| `just bip360-blk-dat-e2e`     | Bob P2MR mines spend; Alice disk matches; bip360 enforcer keeps tip         |

**Architecture claim pins:** `just cusf-claims` (`testmempoolaccept` no-insert;
stock rejects P2MR spends).

Workspace research / residual (outside this git repo root when forked):  
[`cusf/FINAL_REPORT.md`](../FINAL_REPORT.md), [`cusf/FINISHED.md`](../FINISHED.md)
(closed residual archive), [`cusf/RESIDUAL.md`](../RESIDUAL.md) (open only),
[`cusf/STATUS.md`](../STATUS.md), [`cusf/HUMAN_REVIEW.md`](../HUMAN_REVIEW.md).

Contributors and agents: read [AGENTS.md](./AGENTS.md) for fork context, TDD
expectations, **workspace boundary** (stay within the opened workspace root;
external paths need permission and read-only access), integration-test
prerequisites, git signing policy, and **HITL guardrails** (human-only upstream
PR, live integration, Kellnr publish).

Day-to-day commands live in [Justfile](./Justfile) — run `just` from this
directory (or from the workspace root). Quick start: `just drivechain-smoke`
(upstream default), `just verify` (BIP 360 pre-submit), `just setup-core`,
`just demo-a`, `just demo-b`.

Requirements traceability: [`cusf/CRITERIA_AUDIT.md`](../CRITERIA_AUDIT.md);
canonical status: [`cusf/STATUS.md`](../STATUS.md).

# Requirements

1. Bitcoin Core, with ZMQ support. For information on running this on the global
   signet, see [drivechain.info/dev.txt](https://drivechain.info/dev.txt)

1. Rustc & Cargo, version 1.88.0 or higher. Installing via Rustup is
   recommended.

## Supported Bitcoin Core versions

The enforcer supports running against the 3 latest major versions of Bitcoin
Core. `getnetworkinfo` is queried at startup and refuses to run against an
unsupported Bitcoin Core version. The supported set lives in
[`lib/version.rs`](./lib/version.rs); see `--help` for the override flags.

# Getting started

Building/running:

```bash
# Compiles the project
$ cargo build

# See available options
$ cargo run -- --help

# Starts the Connect RPC server at localhost:50001
# Adjust these parameters to match your local Bitcoin
# Core instance
$ cargo run -- \
  --node-rpc-addr=localhost:38332 \
  --node-rpc-user=user \
  --node-rpc-pass=password \
  --node-zmq-addr-sequence=tcp://0.0.0.0:29000

# You should now be able to fetch data from the server!
$ curl -H 'application/json' \
        http://localhost:50051/cusf.validator.v1.ValidatorService/GetChainInfo
{
  "network": "NETWORK_SIGNET"
}
```

# Interacting with the enforcer

The CUSF enforcer exposes multiple [Connect](https://connectrpc.com/) (gRPC)
services. These can be interacted with using either plain `curl` or a
Connect/gRPC client of your choice, for example
[`buf curl`](https://buf.build/docs/installation/) or
[`grpcurl`](https://github.com/fullstorydev/grpcurl).

Some examples of interacting with the enforcer using `curl`, assuming you expose
the server at the default address `localhost:50051`:

```bash
# Define an alias for ease of use
$ alias enforcer_curl='curl -X POST -H "Content-Type: application/json"'

# List all the available RPCs
$ buf_curl --list-methods http://localhost:50051
cusf.mainchain.v1.ValidatorService/GetBlockHeaderInfo
cusf.mainchain.v1.ValidatorService/GetChainInfo
cusf.mainchain.v1.ValidatorService/GetChainTip
cusf.mainchain.v1.ValidatorService/GetSidechains
cusf.mainchain.v1.WalletService/CreateNewAddress
cusf.mainchain.v1.WalletService/CreateSidechainProposal
... list continues

# Fetching data with a RPC that takes no input data
$ enforcer_curl http://localhost:50051/cusf.mainchain.v1.ValidatorService/GetChainInfo
{
  "network": "NETWORK_SIGNET"
}

# Fetching data with a RPC that takes input data
$ request='{"block_hash": {"hex": "000002a78fc54150bb2d4cdb0fb19bcf744f2877faf90a172972fca5daf5fe92"}}'
$ enforcer_curl -d "$request" http://localhost:50051/cusf.mainchain.v1.ValidatorService/GetBlockHeaderInfo
{
  "headerInfo": {
    "blockHash": {
      "hex": "000002a78fc54150bb2d4cdb0fb19bcf744f2877faf90a172972fca5daf5fe92"
    },
    "prevBlockHash": {
      "hex": "000002501d569e62a56ea175896d4348dd9cfef1d700e5b06250486df07c9225"
    },
    "height": 34998,
    "work": {
      "hex": "14d4490000000000000000000000000000000000000000000000000000000000"
    }
  }
}
```

# Regtest

By default, the enforcer runs against our custom signet. If you instead want to
run against a local regtest, you need to also run a local regtest Electrum
server. There are multiple implementations of Electrum servers, an easy-to-use
one is [`mempool/electrs`](https://github.com/mempool/electrs).

For complete instructions on how to do this, consult the
[official docs](https://github.com/mempool/electrs).

A quickstart (that might not work, in case you're missing some dependencies):

```bash
$ git clone https://github.com/mempool/electrs

$ cd electrs

$ cargo run --bin electrs --release -- \
    --network regtest \
    --cookie=user:password \
    --jsonrpc-import
```

# Logging

The application uses the `tracing` crate for logging. Logging is configured
through setting the `--log-level` argument. Some examples:

```bash
# Prints ALL debug logs
$ cargo run ... --log-level DEBUG
```

Logs can also be configured via env vars, which take precedence over CLI args.

```bash
# Prints logs at the "info" level and above, plus our logs the "debug" level and above
$ RUST_LOG=info,bip300301_enforcer=debug cargo run ...
```

# Working with the proto files

The proto definitions live in the upstream
[`LayerTwo-Labs/cusf_sidechain_proto`](https://github.com/LayerTwo-Labs/cusf_sidechain_proto)
repo. We pin a specific commit in [`buf.gen.yaml`](./buf.gen.yaml) and check the
generated Rust code into [`lib/proto/generated/`](./lib/proto/generated/).
Generation is performed by the remote
[`buf.build/anthropics/buffa`](https://buf.build/anthropics/buffa) (message
types) and
[`buf.build/anthropics/connect-rust`](https://buf.build/anthropics/connect-rust)
(Connect RPC service stubs) plugins.

To regenerate (after bumping the `ref:` in `buf.gen.yaml`):

```bash
$ just generate
```

# Code formatting

Rust code is formatted with [rustfmt](https://github.com/rust-lang/rustfmt). You
need to ensure you have a nightly version of Rust installed on your system. To
format the project files from the command line:

```bash
$ just fmt          # preferred: nightly rustfmt + prettier (npx/bunx) + buf if present
$ just fmt-check    # nightly rustfmt --check (no stable “unstable option” spam)
```

`rustfmt.toml` uses nightly-only options (`group_imports`,
`imports_granularity`). Always use **`cargo +nightly fmt`** (as `just fmt` /
`just fmt-check` do), not stable `cargo fmt`, if you want a clean check.

Markdown/YAML: `just fmt` runs Prettier via `bunx` or `npx` when available.
`buf format` is optional (skipped with a note if `buf` is not on `PATH`).

# Linting Rust code

Rust code is linted with Clippy. Prefer **`just clippy`** (real feature sets).

**Do not** pass Cargo `--all-features`: that enables the reserved `shrincs`
feature, which intentionally fails to compile (see
[docs/SHRINCS_DEFERRED.md](./docs/SHRINCS_DEFERRED.md)).

```bash
$ just clippy
# equivalent core sets:
$ cargo clippy --all-targets --features "drivechain,bip360,rustls" -- -D warnings
$ cargo clippy -p bip300301_enforcer_lib --all-targets --no-default-features --features bip360 -- -D warnings
$ cargo +nightly clippy --features "drivechain,bip360,rustls" -- \
    -A clippy::all -D unqualified_local_imports \
    -Zcrate-attr="feature(unqualified_local_imports)"
```

# Unit tests

```bash
$ cargo test -p bip300301_enforcer_lib --features "drivechain,bip360,rustls" --all-targets
$ cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 --all-targets
$ cargo test -p cusf_miner_sidecar --all-targets
```

# Integration tests

Integration tests can be run using

```bash
$ cargo run --example integration_tests --features bip360 -- <TEST ARGS>
# or: just it <trial_name>
# green matrix: just it-all
```

Requires `integrationtests.env` (see `just setup-core` / `just setup` /
`just setup-p2mr`).

# Profiling

```bash
# Generate a flamegraph for Rust code. This does NOT
# measure syscalls/IO wait
# https://github.com/flamegraph-rs/flamegraph
$ cargo install flamegraph
$ cargo flamegraph --  --data-dir ./datadir \
          --node-rpc-addr=localhost:38332 \
          --node-rpc-user=user \
          --node-rpc-pass=password \
          --enable-mempool --exit-after-sync 100000

# macOS only
$ just trace-macos
```
