# Requirements

1. Bitcoin Core, with ZMQ support. This needs be be running with the following
   flags:

   ```
   user=user
   password=password
   signetblocktime=60
   signetchallenge=00141f61d57873d70d28bd28b3c9f9d6bf818b5a0d6a
   acceptnonstdtxn=1 # Important! Otherwise Core rejects OP_DRIVECHAIN TXs

   # this can also be set to a different address, as long
   # as you set the CLI arg for bip300301_enforcer
   zmqpubsequence=tcp://0.0.0.0:29000
   txindex=1
   ```

1. Rustc & Cargo, version 1.77.0 or higher. Installing via Rustup is
   recommended.

# Getting started

Building/running:

```bash
# Check out git submodules
$ git submodule update --init --recursive
# Compiles the project
$ cargo build

# See available options
$ cargo run -- --help

# Starts the gRPC server at localhost:50001
# Adjust these parameters to match your local Bitcoin
# Core instance
$ cargo run -- \
  --node-rpc-addr-=localhost:38332 \
  --node-rpc-user=user \
  --node-rpc-pass=password \
  --node-zmq-addr-sequence=tcp://0.0.0.0:29000

# You should now be able to fetch data from the server!
$ buf curl  --http2-prior-knowledge --protocol grpc \
        http://localhost:50051/cusf.validator.v1.ValidatorService/GetChainInfo
{
  "network": "NETWORK_SIGNET"
}
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

Code is generated with [protox](https://github.com/andrewhickman/protox), and
happens automatically as part of the build process.

Files are linted with [protolint](https://github.com/yoheimuta/protolint).

To lint the files, run:

```bash
$ protolint lint --fix proto/validator/v1/validator.proto
```

# Code formatting

Rust code is formatted with [rustfmt](https://github.com/rust-lang/rustfmt).

Markdown and YAML files are formatted with [Prettier](https://prettier.io/). The
easiest way to run it is to install the
[Prettier VSCode extension](https://marketplace.visualstudio.com/items?itemName=esbenp.prettier-vscode).

To run it from the command line, install the
[Prettier CLI](https://prettier.io/docs/en/cli.html) and run it from the root of
the repo:

```bash
$ prettier --write .
```
