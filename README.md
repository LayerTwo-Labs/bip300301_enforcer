# Requirements

1. Local installation of `buf`: https://buf.build/docs/installation
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

Regenerating Protobuf/gRPC code:

```bash
$ buf generate 
```

# Notes on generated Protobuf/gRPC code

A typical setup when working with Protobuf/gRPC in Rust is to have an extremely
complicated set of build scripts and crates that generate code at build-time,
_without_ having that code checked in to Git.

This is a bad way of doing things:

1. Navigating the complex maze of crates, concepts and nomenclature is a
   guaranteed way of getting a head-ache.
1. Building locally is never as simple as `cargo build`.
1. This requires a local installation of `protoc`, and potentially `protoc`
   plugins. This again requires keeping versions in check among multiple
   different dev machines. Always goes wrong, some way or another!

Instead, we rely on Buf and and its
[remote plugin](https://buf.build/docs/generate/remote-plugins) and
[code generation](https://buf.build/docs/generate/remote-plugins) features.
Users only need `buf` installed locally! `buf` is very mindful of breaking
changes, and version mismatches are not an issue.

Generated code is checked into Git. This makes it easy to see precisely what
code we're dealing with! With the typical build-time generation this is not
possible without having a fully configured Rust IDE. This also means that a
brand new dev can jump into coding, without having to deal with generating code
from Protobuf definitions.
