#!/usr/bin/env bash
# Optional multiproc tip smoke: bip360 (+ optional drivechain) workers on UDS,
# hub framed ConnectBlock fail-closed paths, and optional live tip when BITCOIND
# is available.
#
# Autonomous default (no bitcoind): extends validate-rules-engine multiproc smoke
# with ConnectBlock + capability handshake. Full Core+hub tip e2e still residual
# (electrs / setup-p2mr / live invalidate path).
#
# Usage:
#   ./scripts/smoke-rules-workers-tip.sh
#   BITCOIND=/path/to/bitcoind ./scripts/smoke-rules-workers-tip.sh   # documents skip if incomplete
#
# Do NOT use cargo --all-features.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> smoke-rules-workers-tip: build workers"
cargo build -q -p cusf_rules_bip360 -p cusf_rules_drivechain
BIP360_BIN="$ROOT/target/debug/cusf_rules_bip360"
DC_BIN="$ROOT/target/debug/cusf_rules_drivechain"

SMOKE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/cusf-rules-tip-XXXXXX")"
cleanup() {
  for pidvar in WORKER_360_PID WORKER_DC_PID HUB_PID; do
    pid="${!pidvar:-}"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$SMOKE_DIR"
}
trap cleanup EXIT

SOCK_360="$SMOKE_DIR/bip360.sock"
SOCK_DC="$SMOKE_DIR/drivechain.sock"

"$BIP360_BIN" --uds "$SOCK_360" --activation-height 0 >/dev/null &
WORKER_360_PID=$!
"$DC_BIN" --uds "$SOCK_DC" >/dev/null &
WORKER_DC_PID=$!

for _ in $(seq 1 50); do
  if [[ -S "$SOCK_360" && -S "$SOCK_DC" ]]; then break; fi
  sleep 0.1
done
[[ -S "$SOCK_360" && -S "$SOCK_DC" ]] || {
  echo "workers did not bind sockets" >&2
  exit 1
}

python3 - <<'PY' "$SOCK_360" "$SOCK_DC"
import socket, struct, sys

def recv_exact(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise EOFError(f"short read {len(data)}/{n}")
        data += chunk
    return data

def call(path, body: bytes) -> str:
    frame = b"RUL1" + struct.pack(">I", len(body)) + body
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(5)
    s.connect(path)
    s.sendall(frame)
    hdr = recv_exact(s, 8)
    assert hdr[:4] == b"RUL1", hdr
    (n,) = struct.unpack(">I", hdr[4:])
    data = recv_exact(s, n)
    s.close()
    return data.decode()

p360, pdc = sys.argv[1], sys.argv[2]

# Capability tokens
h360 = call(p360, b'{"method":"Health"}')
assert "bip360-pqc-parents-and-chain-utxos-on-wire" in h360, h360
hdc = call(pdc, b'{"method":"Health"}')
assert "drivechain-ctip-m8-pending-m6-checks-on-wire" in hdc, hdc

# ConnectBlock without block fail-closed
cb = call(p360, b'{"method":"ConnectBlock","params":{"height":1,"label":"tip-smoke"}}')
assert "missing_block" in cb and ("false" in cb), cb

# Valid genesis block_hex without chain set → missing_chain_p2mr_utxos
import json
GENESIS = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4adae5494dffff7f20020000000101000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4d04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000"
cb2 = call(
    p360,
    json.dumps(
        {
            "method": "ConnectBlock",
            "params": {"height": 1, "label": "tip-smoke", "block_hex": GENESIS},
        }
    ).encode(),
)
assert "missing_chain_p2mr_utxos" in cb2, cb2

# Drivechain ValidateTx without tx → missing_tx
vt = call(pdc, b'{"method":"ValidateTx","params":{"height":1,"label":"t"}}')
assert "missing_tx" in vt, vt

# Align with validate multiproc: drivechain_state Accept + independent M8/multi Reject paths.
PLAIN_TX = "020000000001000150c30000000000000000000000"
OP_DRIVECHAIN_TX = "0200000000010001e80300000000000004b401015100000000"
state = {"tip_height": 1, "tip_hash_hex": "00" * 32, "active_sidechain_numbers": []}
vt_ok = call(
    pdc,
    json.dumps(
        {
            "method": "ValidateTx",
            "params": {
                "height": 1,
                "label": "tip-smoke",
                "tx_hex": PLAIN_TX,
                "drivechain_state": state,
            },
        }
    ).encode(),
)
assert '"accept":true' in vt_ok or '"accept": true' in vt_ok, vt_ok
vt_miss = call(
    pdc,
    json.dumps(
        {
            "method": "ValidateTx",
            "params": {"height": 1, "label": "tip-smoke", "tx_hex": PLAIN_TX},
        }
    ).encode(),
)
assert "missing_drivechain_state" in vt_miss, vt_miss
vt_dc = call(
    pdc,
    json.dumps(
        {
            "method": "ValidateTx",
            "params": {
                "height": 1,
                "label": "tip-smoke",
                "tx_hex": OP_DRIVECHAIN_TX,
                "drivechain_state": state,
            },
        }
    ).encode(),
)
assert '"accept":true' in vt_dc or '"accept": true' in vt_dc, vt_dc
# Independent M8 tip mismatch Reject (subset of validate-rules-engine multiproc)
M8_TX = "02000000000100010000000000000000466a4400bf00010202020202020202020202020202020202020202020202020202020202020202030303030303030303030303030303030303030303030303030303030303030300000000"
vt_m8 = call(
    pdc,
    json.dumps(
        {
            "method": "ValidateTx",
            "params": {
                "height": 1,
                "label": "tip-smoke",
                "tx_hex": M8_TX,
                "drivechain_state": {
                    "tip_height": 1,
                    "tip_hash_hex": "aa" * 32,
                    "active_sidechain_numbers": [],
                },
            },
        }
    ).encode(),
)
assert "m8_prev_mainchain_mismatch" in vt_m8, vt_m8
# Multi-active OP_DRIVECHAIN independent Reject (align with validate-rules-engine)
MULTI_DC_TX = "0200000000010002e80300000000000004b4010751f40100000000000004b401075100000000"
vt_multi = call(
    pdc,
    json.dumps(
        {
            "method": "ValidateTx",
            "params": {
                "height": 1,
                "label": "tip-smoke",
                "tx_hex": MULTI_DC_TX,
                "drivechain_state": {
                    "tip_height": 1,
                    "tip_hash_hex": "00" * 32,
                    "active_sidechain_numbers": [7],
                },
            },
        }
    ).encode(),
)
assert "multiple_op_drivechain" in vt_multi, vt_multi
# Empty tip_hash_hex on M8 → m8_missing_tip_hash
vt_empty_tip = call(
    pdc,
    json.dumps(
        {
            "method": "ValidateTx",
            "params": {
                "height": 1,
                "label": "tip-smoke",
                "tx_hex": M8_TX,
                "drivechain_state": {
                    "tip_height": 1,
                    "tip_hash_hex": "",
                    "active_sidechain_numbers": [],
                },
            },
        }
    ).encode(),
)
assert "m8_missing_tip_hash" in vt_empty_tip, vt_empty_tip
# Independent ctip Reject paths (align with validate-rules-engine multiproc)
state_active = {
    "tip_height": 1,
    "tip_hash_hex": "00" * 32,
    "active_sidechain_numbers": [1],
    "ctips": [],
}
vt_miss_addr = call(
    pdc,
    json.dumps(
        {
            "method": "ValidateTx",
            "params": {
                "height": 1,
                "label": "tip-smoke",
                "tx_hex": OP_DRIVECHAIN_TX,
                "drivechain_state": state_active,
            },
        }
    ).encode(),
)
assert "missing_deposit_address" in vt_miss_addr, vt_miss_addr
# Non-palindromic ctip + OP_DRIVECHAIN with deposit address (no spend) → old_ctip_unspent
CTIP_TXID = "012345" + "00" * 27 + "abcd"  # 64 hex, non-palindromic
state_ctip = {
    "tip_height": 1,
    "tip_hash_hex": "00" * 32,
    "active_sidechain_numbers": [1],
    "ctips": [
        {
            "sidechain_number": 1,
            "outpoint_txid_hex": CTIP_TXID,
            "outpoint_vout": 0,
            "value_sats": 1000,
        }
    ],
}
OP_DRIVECHAIN_ADDR_TX = (
    "02000000"
    "0001"  # segwit marker/flag (same as OP_DRIVECHAIN_TX)
    "00"
    "02"
    "d007000000000000"
    "04b4010151"
    "0000000000000000"
    "066a0461646472"
    "00000000"
)
vt_old = call(
    pdc,
    json.dumps(
        {
            "method": "ValidateTx",
            "params": {
                "height": 1,
                "label": "tip-smoke",
                "tx_hex": OP_DRIVECHAIN_ADDR_TX,
                "drivechain_state": state_ctip,
            },
        }
    ).encode(),
)
assert "old_ctip_unspent" in vt_old, vt_old
# Pending M6 Reject paths (subset of validate-rules-engine multiproc)
M6_CTIP_TXID = "cdab" + "00" * 27 + "452301"
M6_TX = (
    "0200000001"
    + ("012345" + "00" * 27 + "abcd")
    + "0000000000ffffffff01900100000000000004b401015100000000"
)
M6ID = "0b3399365e861cd82d10faf52b4d3906e61253284174bb02a320cc3d9fcf93b7"
state_m6 = {
    "tip_height": 1,
    "tip_hash_hex": "00" * 32,
    "active_sidechain_numbers": [1],
    "ctips": [
        {
            "sidechain_number": 1,
            "outpoint_txid_hex": M6_CTIP_TXID,
            "outpoint_vout": 0,
            "value_sats": 1000,
        }
    ],
    "pending_m6ids": [],
    "withdrawal_bundle_inclusion_threshold": 5,
}
vt_m6_miss = call(
    pdc,
    json.dumps(
        {
            "method": "ValidateTx",
            "params": {
                "height": 1,
                "label": "tip-smoke",
                "tx_hex": M6_TX,
                "drivechain_state": state_m6,
            },
        }
    ).encode(),
)
assert "m6_missing_pending_withdrawal" in vt_m6_miss, vt_m6_miss
state_m6_low = dict(state_m6)
state_m6_low["pending_m6ids"] = [
    {
        "sidechain_number": 1,
        "m6id_hex": M6ID,
        "vote_count": 5,
        "proposal_height": 1,
    }
]
vt_m6_low = call(
    pdc,
    json.dumps(
        {
            "method": "ValidateTx",
            "params": {
                "height": 1,
                "label": "tip-smoke",
                "tx_hex": M6_TX,
                "drivechain_state": state_m6_low,
            },
        }
    ).encode(),
)
assert "m6_insufficient_vote_count" in vt_m6_low, vt_m6_low
# Parity with validate-rules-engine: M6 sufficient pending Accept
state_m6_ok = dict(state_m6)
state_m6_ok["pending_m6ids"] = [
    {
        "sidechain_number": 1,
        "m6id_hex": M6ID,
        "vote_count": 6,
        "proposal_height": 1,
    }
]
vt_m6_ok = call(
    pdc,
    json.dumps(
        {
            "method": "ValidateTx",
            "params": {
                "height": 1,
                "label": "tip-smoke",
                "tx_hex": M6_TX,
                "drivechain_state": state_m6_ok,
            },
        }
    ).encode(),
)
assert '"accept":true' in vt_m6_ok or '"accept": true' in vt_m6_ok, vt_m6_ok
cb_dc = call(
    pdc,
    json.dumps(
        {
            "method": "ConnectBlock",
            "params": {
                "height": 0,
                "label": "tip-smoke",
                "block_hex": GENESIS,
                "drivechain_state": state,
            },
        }
    ).encode(),
)
assert '"accept":true' in cb_dc or '"accept": true' in cb_dc, cb_dc
# Parity with validate-rules-engine: ConnectBlock M8 without M7 Reject
import struct

def encode_varint(n: int) -> bytes:
    if n < 0xFD:
        return bytes([n])
    if n <= 0xFFFF:
        return b"\xfd" + struct.pack("<H", n)
    if n <= 0xFFFFFFFF:
        return b"\xfe" + struct.pack("<I", n)
    return b"\xff" + struct.pack("<Q", n)

coinbase = (
    bytes.fromhex("02000000")
    + bytes([1])
    + bytes(32)
    + bytes.fromhex("ffffffff")
    + bytes([0])
    + bytes.fromhex("ffffffff")
    + bytes([1])
    + struct.pack("<Q", 50)
    + bytes([0])
    + bytes.fromhex("00000000")
)
m8_tx = bytes.fromhex(M8_TX)
header = (
    struct.pack("<I", 1)
    + bytes(32)
    + bytes(32)
    + struct.pack("<I", 0)
    + struct.pack("<I", 0)
    + struct.pack("<I", 0)
)
block_m7_hex = (header + encode_varint(2) + coinbase + m8_tx).hex()
state_m7 = {
    "tip_height": 0,
    "tip_hash_hex": "03" * 32,
    "active_sidechain_numbers": [],
    "ctips": [],
}
cb_m7 = call(
    pdc,
    json.dumps(
        {
            "method": "ConnectBlock",
            "params": {
                "height": 1,
                "label": "tip-smoke",
                "block_hex": block_m7_hex,
                "drivechain_state": state_m7,
            },
        }
    ).encode(),
)
assert "m8_not_accepted_by_miners" in cb_m7, cb_m7

print("smoke-rules-workers-tip multiproc: OK")
PY

if [[ "${CUSF_LIVE_TIP_E2E:-}" == "1" ]]; then
  echo "==> CUSF_LIVE_TIP_E2E=1: delegating to live-tip-rules-workers-e2e.sh"
  exec "$ROOT/scripts/live-tip-rules-workers-e2e.sh"
elif [[ -n "${BITCOIND:-}" ]]; then
  if [[ ! -x "$BITCOIND" ]]; then
    echo "BITCOIND set but not executable: $BITCOIND (skipping live tip)" >&2
  else
    echo "==> BITCOIND present at $BITCOIND"
    echo "    For opt-in live tip: CUSF_LIVE_TIP_E2E=1 BITCOIND=... just live-tip-rules-workers-e2e"
    echo "    Workers above are live on UDS; hub: --rules-worker bip360=$SOCK_360"
    echo "    --rules-worker drivechain=$SOCK_DC (real capability: drivechain-ctip-m8-pending-m6-checks-on-wire)."
    if [[ "${SMOKE_BUILD_HUB:-}" == "1" ]]; then
      cargo build -q -p bip300301_enforcer --features "drivechain,bip360,rustls"
      echo "    hub binary: $ROOT/target/debug/bip300301_enforcer"
    fi
  fi
else
  echo "==> BITCOIND / CUSF_LIVE_TIP_E2E not set: skipped live tip (multiproc UDS smoke passed)"
fi

echo "smoke-rules-workers-tip: OK"
