#!/usr/bin/env bash
# Smoke-test gRPC server reflection end to end with real third-party clients.
#
# Spins up a throwaway regtest bitcoind + the enforcer, then verifies with
# `grpcurl` and `buf curl` (no local proto files — schemas are resolved via
# the reflection service itself) that:
#   1. ListServices enumerates every expected service,
#   2. reflection-resolved schemas suffice to call a real RPC (Ripemd160),
#      over gRPC and the Connect protocol, via both the v1 and the legacy
#      v1alpha reflection protocols.
#
# Requirements: bitcoind, grpcurl, buf, and a built enforcer binary.
#
# Overridable via environment:
#   BIP300301_ENFORCER  path to enforcer binary (default: target/debug/...)
#   BITCOIND            path to bitcoind (default: from PATH)

set -euo pipefail

ENFORCER="${BIP300301_ENFORCER:-target/debug/bip300301_enforcer}"
BITCOIND="${BITCOIND:-bitcoind}"

BITCOIND_RPC_PORT=18943
BITCOIND_ZMQ_PORT=18944
ENFORCER_GRPC_PORT=18945
GRPC_ADDR="127.0.0.1:$ENFORCER_GRPC_PORT"

for tool in "$BITCOIND" grpcurl buf "$ENFORCER"; do
    if ! command -v "$tool" > /dev/null; then
        echo "missing required tool: $tool" >&2
        exit 1
    fi
done

WORK_DIR="$(mktemp -d)"
BITCOIND_PID=""
ENFORCER_PID=""
cleanup() {
    [ -n "$ENFORCER_PID" ] && kill "$ENFORCER_PID" 2> /dev/null || true
    [ -n "$BITCOIND_PID" ] && kill "$BITCOIND_PID" 2> /dev/null || true
    wait 2> /dev/null || true
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT

wait_for_port() {
    local port="$1" name="$2"
    for _ in $(seq 1 100); do
        if (exec 3<> "/dev/tcp/127.0.0.1/$port") 2> /dev/null; then
            exec 3>&- 3<&-
            return 0
        fi
        sleep 0.2
    done
    echo "$name did not open port $port in time" >&2
    return 1
}

echo "==> starting bitcoind (regtest)"
mkdir -p "$WORK_DIR/bitcoind"
"$BITCOIND" \
    -regtest \
    -datadir="$WORK_DIR/bitcoind" \
    -rpcport="$BITCOIND_RPC_PORT" \
    -rpcuser=reflection \
    -rpcpassword=verify \
    -zmqpubsequence="tcp://127.0.0.1:$BITCOIND_ZMQ_PORT" \
    -listen=0 \
    -server=1 \
    -rest=1 \
    > "$WORK_DIR/bitcoind.log" 2>&1 &
BITCOIND_PID=$!
wait_for_port "$BITCOIND_RPC_PORT" bitcoind

echo "==> starting enforcer"
"$ENFORCER" \
    --data-dir "$WORK_DIR/enforcer" \
    --node-rpc-addr="127.0.0.1:$BITCOIND_RPC_PORT" \
    --node-rpc-user=reflection \
    --node-rpc-pass=verify \
    --node-zmq-addr-sequence="tcp://127.0.0.1:$BITCOIND_ZMQ_PORT" \
    --serve-grpc-addr="$GRPC_ADDR" \
    > "$WORK_DIR/enforcer.log" 2>&1 &
ENFORCER_PID=$!
if ! wait_for_port "$ENFORCER_GRPC_PORT" enforcer; then
    echo "--- enforcer log ---" >&2
    cat "$WORK_DIR/enforcer.log" >&2
    exit 1
fi

fail() {
    echo "FAIL: $1" >&2
    echo "--- enforcer log ---" >&2
    cat "$WORK_DIR/enforcer.log" >&2
    exit 1
}

echo "==> grpcurl: ListServices via reflection"
EXPECTED_SERVICES='cusf.crypto.v1.CryptoService
cusf.mainchain.v1.ValidatorService
cusf.mainchain.v1.WalletService
grpc.reflection.v1.ServerReflection
grpc.reflection.v1alpha.ServerReflection'
ACTUAL_SERVICES="$(grpcurl -plaintext "$GRPC_ADDR" list | sort)"
if [ "$ACTUAL_SERVICES" != "$EXPECTED_SERVICES" ]; then
    echo "expected services:" >&2
    echo "$EXPECTED_SERVICES" >&2
    echo "actual services:" >&2
    echo "$ACTUAL_SERVICES" >&2
    fail "grpcurl list returned unexpected services"
fi

echo "==> grpcurl: describe via reflection"
grpcurl -plaintext "$GRPC_ADDR" describe cusf.mainchain.v1.ValidatorService \
    | grep -q GetBlockHeaderInfo \
    || fail "grpcurl describe is missing GetBlockHeaderInfo"

# ripemd160("abc"), a fixed test vector
RIPEMD_REQUEST='{"msg":{"hex":"616263"}}'
RIPEMD_DIGEST='8eb208f7e05d987a9b044a8e98c6b087f15a0bfc'

echo "==> grpcurl: call Ripemd160 with reflection-resolved schema"
grpcurl -plaintext -d "$RIPEMD_REQUEST" \
    "$GRPC_ADDR" cusf.crypto.v1.CryptoService/Ripemd160 \
    | grep -q "$RIPEMD_DIGEST" \
    || fail "grpcurl Ripemd160 returned wrong digest"

echo "==> buf curl: call Ripemd160 over gRPC (v1 reflection)"
buf curl --protocol grpc --http2-prior-knowledge -d "$RIPEMD_REQUEST" \
    "http://$GRPC_ADDR/cusf.crypto.v1.CryptoService/Ripemd160" \
    | grep -q "$RIPEMD_DIGEST" \
    || fail "buf curl (grpc, v1 reflection) returned wrong digest"

echo "==> buf curl: call Ripemd160 over Connect (v1alpha reflection)"
buf curl --protocol connect --http2-prior-knowledge \
    --reflect-protocol grpc-v1alpha -d "$RIPEMD_REQUEST" \
    "http://$GRPC_ADDR/cusf.crypto.v1.CryptoService/Ripemd160" \
    | grep -q "$RIPEMD_DIGEST" \
    || fail "buf curl (connect, v1alpha reflection) returned wrong digest"

echo "OK: reflection verified with grpcurl + buf curl"
