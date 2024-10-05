# Requirements

1. Bitcoin Core
1. Rust

# Getting started

Building/running:

```bash
# Compiles the project
$ cargo build

# See available options
$ cargo run -- --help

# Starts the gRPC server at localhost:50001
# Adjust these parameters to match your local Bitcoin
# Core instance
$ cargo run -- \
  --node-rpc-port=38332 \
  --node-rpc-user=user \
  --node-rpc-password=password

# You should now be able to fetch data from the server!
$ buf curl  --schema 'https://github.com/LayerTwo-Labs/bip300301_enforcer_proto.git' \
            --http2-prior-knowledge --protocol grpc \
      http://localhost:50051/validator.Validator/GetMainBlockHeight
{
  "height": 2
}
```

# Logging

The application uses the `env_logger` crate for logging. Logging is configured
through setting the `RUST_LOG` environment variable. Some examples:

```bash
# Prints ALL debug logs
$ RUST_LOG=debug cargo run ...

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
