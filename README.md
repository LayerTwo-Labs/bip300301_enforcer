# Requirements

1. Bitcoin Core running in regtest mode at `localhost:18443` using cookie auth

# Getting started

Building/running:

```bash
# Compiles the project
$ cargo build

# Starts the gRPC server at localhost:50001
$ cargo run

# You should now be able to fetch data from the server!
$ buf curl  --schema 'https://github.com/LayerTwo-Labs/bip300301_enforcer_proto.git' \
            --http2-prior-knowledge --protocol grpc \
      http://localhost:50051/validator.Validator/GetMainBlockHeight
{
  "height": 2
}
```