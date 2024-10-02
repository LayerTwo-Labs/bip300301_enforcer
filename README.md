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

# Working with the proto files

Code is generated with [protox](https://github.com/andrewhickman/protox), and
happens automatically as part of the build process.

Files are linted with [protolint](https://github.com/yoheimuta/protolint).

To lint the files, run:

```bash
$ protolint lint --fix proto/validator/v1/validator.proto
```
