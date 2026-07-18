#!/usr/bin/env bash
# Validate hub rules engine + workers (docs/MULTI_ENFORCER.md).
# Do NOT use cargo --all-features (enables reserved shrincs).
#
# SCORE / PR green matrix honesty:
#   * This script is what GH job `check-rules-engine` runs.
#   * No bitcoind, no CUSF_LIVE_TIP_E2E, no live-tip-rules-workers-e2e,
#     no sigterm-rules-workers-e2e (those stay HITL / opt-in outside SCORE).
#   * Lock tests: rules::pr_ci_live_tip_residual_honesty_score_lock
#     + rules::shutdown_residual_honesty_docs_inventory_lock
#   * Do NOT add BITCOIND requirements here without product unlock.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> cargo test -p bip300301_enforcer_lib rules::"
cargo test -p bip300301_enforcer_lib rules::

echo "==> cargo test -p bip300301_enforcer_lib --features bip360 rules::"
cargo test -p bip300301_enforcer_lib --features bip360 rules::

echo "==> cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 rules::"
cargo test -p bip300301_enforcer_lib --no-default-features --features bip360 rules::

echo "==> cargo check -p bip300301_enforcer --features drivechain,bip360,rustls"
cargo check -p bip300301_enforcer --features "drivechain,bip360,rustls"

echo "==> cargo check -p cusf_rules_drivechain"
cargo check -p cusf_rules_drivechain

echo "==> cargo check -p cusf_rules_bip360"
cargo check -p cusf_rules_bip360

echo "==> worker health smoke (capability tokens)"
out_dc="$(cargo run -q -p cusf_rules_drivechain -- --health)"
echo "$out_dc"
echo "$out_dc" | grep -q 'validation=drivechain-ctip-m8-pending-m6-checks-on-wire'
out_360="$(cargo run -q -p cusf_rules_bip360 -- --health)"
echo "$out_360"
echo "$out_360" | grep -q 'validation=bip360-pqc-parents-and-chain-utxos-on-wire'

echo "==> worker --once missing_tx fail-closed"
# rules.v1 ValidateTx without tx_hex must Reject missing_tx (not soft-Accept).
req='{"method":"ValidateTx","params":{"height":1,"label":"t"}}'
resp_dc="$(printf '%s' "$req" | cargo run -q -p cusf_rules_drivechain -- --once)"
echo "$resp_dc"
echo "$resp_dc" | grep -q 'missing_tx'
resp_360="$(printf '%s' "$req" | cargo run -q -p cusf_rules_bip360 -- --once)"
echo "$resp_360"
echo "$resp_360" | grep -q 'missing_tx'

echo "==> worker --once ConnectBlock fail-closed (bip360)"
# No block_hex → missing_block
req_cb='{"method":"ConnectBlock","params":{"height":1,"label":"t"}}'
resp_cb="$(printf '%s' "$req_cb" | cargo run -q -p cusf_rules_bip360 -- --once)"
echo "$resp_cb"
echo "$resp_cb" | grep -q 'missing_block'
# Valid regtest genesis block_hex, no chain_p2mr_utxos_hex → missing_chain_p2mr_utxos
# (bitcoin crate Network::Regtest genesis consensus hex)
GENESIS_HEX='0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4adae5494dffff7f20020000000101000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4d04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000'
req_chain="{\"method\":\"ConnectBlock\",\"params\":{\"height\":1,\"label\":\"t\",\"block_hex\":\"${GENESIS_HEX}\"}}"
resp_chain="$(printf '%s' "$req_chain" | cargo run -q -p cusf_rules_bip360 -- --once)"
echo "$resp_chain"
echo "$resp_chain" | grep -q 'missing_chain_p2mr_utxos'
echo "$resp_chain" | grep -qE '"accept"[[:space:]]*:[[:space:]]*false'

echo "==> multiproc UDS smoke (Health + ValidateTx + ConnectBlock fail-closed)"
SMOKE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/cusf-rules-uds-XXXXXX")"
cleanup_smoke() {
  if [[ -n "${WORKER_PID:-}" ]] && kill -0 "$WORKER_PID" 2>/dev/null; then
    kill "$WORKER_PID" 2>/dev/null || true
    wait "$WORKER_PID" 2>/dev/null || true
  fi
  if [[ -n "${WORKER_DC_PID:-}" ]] && kill -0 "$WORKER_DC_PID" 2>/dev/null; then
    kill "$WORKER_DC_PID" 2>/dev/null || true
    wait "$WORKER_DC_PID" 2>/dev/null || true
  fi
  rm -rf "$SMOKE_DIR"
}
trap cleanup_smoke EXIT

SOCK="$SMOKE_DIR/bip360.sock"
SOCK_DC="$SMOKE_DIR/drivechain.sock"
cargo build -q -p cusf_rules_bip360 -p cusf_rules_drivechain
WORKER_BIN="$ROOT/target/debug/cusf_rules_bip360"
WORKER_DC_BIN="$ROOT/target/debug/cusf_rules_drivechain"
"$WORKER_BIN" --uds "$SOCK" --activation-height 0 >/dev/null &
WORKER_PID=$!
"$WORKER_DC_BIN" --uds "$SOCK_DC" >/dev/null &
WORKER_DC_PID=$!

for _ in $(seq 1 50); do
  if [[ -S "$SOCK" && -S "$SOCK_DC" ]]; then break; fi
  sleep 0.1
done
if [[ ! -S "$SOCK" ]]; then
  echo "UDS smoke: bip360 worker socket did not appear: $SOCK" >&2
  exit 1
fi
if [[ ! -S "$SOCK_DC" ]]; then
  echo "UDS smoke: drivechain worker socket did not appear: $SOCK_DC" >&2
  exit 1
fi

# Framed requests with full header/body read loops (partial-read safe).
python3 - <<'PY' "$SOCK" "$SOCK_DC"
import socket, struct, sys, json

def recv_exact(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise EOFError(f"short read {len(data)}/{n}")
        data += chunk
    return data

def call(path, body: bytes) -> bytes:
    frame = b"RUL1" + struct.pack(">I", len(body)) + body
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(5)
    s.connect(path)
    s.sendall(frame)
    hdr = recv_exact(s, 8)
    assert hdr[:4] == b"RUL1", hdr
    (n,) = struct.unpack(">I", hdr[4:])
    assert 0 < n <= 16 * 1024 * 1024, n
    data = recv_exact(s, n)
    s.close()
    return data

path_360, path_dc = sys.argv[1], sys.argv[2]

# Health — bip360 real capability
text = call(path_360, b'{"method":"Health"}').decode()
print("bip360 Health:", text)
assert '"status":"HealthOk"' in text or '"status": "HealthOk"' in text, text
assert "bip360" in text, text
assert "bip360-pqc-parents-and-chain-utxos-on-wire" in text, text

# Health — drivechain real capability (ctip + M8 + pending M6 checks on wire)
text = call(path_dc, b'{"method":"Health"}').decode()
print("drivechain Health:", text)
assert "drivechain" in text, text
assert "drivechain-ctip-m8-pending-m6-checks-on-wire" in text, text

# ValidateTx without tx_hex → missing_tx (fail-closed, never soft-Accept)
text = call(
    path_360,
    b'{"method":"ValidateTx","params":{"height":1,"label":"t"}}',
).decode()
print("ValidateTx:", text)
assert "missing_tx" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

# ConnectBlock without block_hex → missing_block
text = call(
    path_360,
    b'{"method":"ConnectBlock","params":{"height":1,"label":"t"}}',
).decode()
print("ConnectBlock missing_block:", text)
assert "missing_block" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

# ConnectBlock with valid block_hex but no chain_p2mr_utxos_hex → missing_chain_p2mr_utxos
# bitcoin crate Network::Regtest genesis (consensus hex) — keep as one string.
GENESIS = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4adae5494dffff7f20020000000101000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4d04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000"
body = json.dumps(
    {
        "method": "ConnectBlock",
        "params": {"height": 1, "label": "t", "block_hex": GENESIS},
    }
).encode()
text = call(path_360, body).decode()
print("ConnectBlock missing_chain_p2mr_utxos:", text)
assert "missing_chain_p2mr_utxos" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

# ConnectBlock with empty block_hex invalid → invalid_block_hex
text = call(
    path_360,
    b'{"method":"ConnectBlock","params":{"height":1,"label":"t","block_hex":"00"}}',
).decode()
print("ConnectBlock bad hex:", text)
assert '"accept":false' in text or '"accept": false' in text, text

# Handshake capability negotiation surface
text = call(
    path_360,
    b'{"method":"Handshake","params":{"version":1,"rule_id":"bip360"}}',
).decode()
print("bip360 Handshake:", text)
assert "HandshakeOk" in text, text
assert "bip360-pqc-parents-and-chain-utxos-on-wire" in text, text

text = call(
    path_dc,
    b'{"method":"Handshake","params":{"version":1,"rule_id":"drivechain"}}',
).decode()
print("drivechain Handshake:", text)
assert "HandshakeOk" in text, text
assert "drivechain-ctip-m8-pending-m6-checks-on-wire" in text, text

# Drivechain ValidateTx with plain tx + drivechain_state → Accept (not soft-stub)
import json
# bitcoin crate consensus encode: v2, 0 inputs, 1 empty-script 50_000 sat output, locktime 0
PLAIN_TX = "020000000001000150c30000000000000000000000"
state = {"tip_height": 1, "tip_hash_hex": "00" * 32, "active_sidechain_numbers": []}
body = json.dumps(
    {
        "method": "ValidateTx",
        "params": {
            "height": 1,
            "label": "t",
            "tx_hex": PLAIN_TX,
            "drivechain_state": state,
        },
    }
).encode()
text = call(path_dc, body).decode()
print("drivechain plain ValidateTx Accept:", text)
assert '"accept":true' in text or '"accept": true' in text, text

# Drivechain ValidateTx with tx but no state → missing_drivechain_state
body2 = json.dumps(
    {
        "method": "ValidateTx",
        "params": {"height": 1, "label": "t", "tx_hex": PLAIN_TX},
    }
).encode()
text = call(path_dc, body2).decode()
print("drivechain missing_drivechain_state:", text)
assert "missing_drivechain_state" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

# ConnectBlock drivechain: valid regtest genesis hex + state policy
body3 = json.dumps(
    {
        "method": "ConnectBlock",
        "params": {
            "height": 0,
            "label": "t",
            "block_hex": GENESIS,
        },
    }
).encode()
text = call(path_dc, body3).decode()
print("drivechain ConnectBlock missing_drivechain_state:", text)
assert "missing_drivechain_state" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

body4 = json.dumps(
    {
        "method": "ConnectBlock",
        "params": {
            "height": 0,
            "label": "t",
            "block_hex": GENESIS,
            "drivechain_state": state,
        },
    }
).encode()
text = call(path_dc, body4).decode()
print("drivechain ConnectBlock with state Accept:", text)
assert '"accept":true' in text or '"accept": true' in text, text

# M8 with prev != tip_hash_hex → independent remote Reject
M8_TX = "02000000000100010000000000000000466a4400bf00010202020202020202020202020202020202020202020202020202020202020202030303030303030303030303030303030303030303030303030303030303030300000000"
state_m8_bad = {
    "tip_height": 1,
    "tip_hash_hex": "aa" * 32,
    "active_sidechain_numbers": [],
}
body_m8 = json.dumps(
    {
        "method": "ValidateTx",
        "params": {
            "height": 1,
            "label": "t",
            "tx_hex": M8_TX,
            "drivechain_state": state_m8_bad,
        },
    }
).encode()
text = call(path_dc, body_m8).decode()
print("drivechain M8 tip mismatch Reject:", text)
assert "m8_prev_mainchain_mismatch" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

# M8 with matching tip → Accept (M7 commitment still Local-only)
state_m8_ok = {
    "tip_height": 1,
    "tip_hash_hex": "03" * 32,
    "active_sidechain_numbers": [],
}
body_m8_ok = json.dumps(
    {
        "method": "ValidateTx",
        "params": {
            "height": 1,
            "label": "t",
            "tx_hex": M8_TX,
            "drivechain_state": state_m8_ok,
        },
    }
).encode()
text = call(path_dc, body_m8_ok).decode()
print("drivechain M8 tip match Accept:", text)
assert '"accept":true' in text or '"accept": true' in text, text

# M8 with empty tip_hash_hex → fail-closed m8_missing_tip_hash
body_m8_empty = json.dumps(
    {
        "method": "ValidateTx",
        "params": {
            "height": 1,
            "label": "t",
            "tx_hex": M8_TX,
            "drivechain_state": {
                "tip_height": 1,
                "tip_hash_hex": "",
                "active_sidechain_numbers": [],
            },
        },
    }
).encode()
text = call(path_dc, body_m8_empty).decode()
print("drivechain M8 empty tip Reject:", text)
assert "m8_missing_tip_hash" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

# multi OP_DRIVECHAIN on active sidechain → independent Reject
# consensus encode (segwit marker/flag) of 2x OP_DRIVECHAIN sidechain 7
MULTI_DC_TX = "0200000000010002e80300000000000004b4010751f40100000000000004b401075100000000"
state_multi = {
    "tip_height": 1,
    "tip_hash_hex": "00" * 32,
    "active_sidechain_numbers": [7],
}
body_multi = json.dumps(
    {
        "method": "ValidateTx",
        "params": {
            "height": 1,
            "label": "t",
            "tx_hex": MULTI_DC_TX,
            "drivechain_state": state_multi,
        },
    }
).encode()
text = call(path_dc, body_multi).decode()
print("drivechain multi OP_DRIVECHAIN Reject:", text)
assert "multiple_op_drivechain" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

# OP_DRIVECHAIN marker + drivechain_state → Accept (inactive; Local dual-AND residual paths)
# consensus: v2, 0 ins, 1 out 1000 sat, script OP_NOP5 OP_PUSHBYTES_1 0x01 OP_TRUE
OP_DRIVECHAIN_TX = "0200000000010001e80300000000000004b401015100000000"
body5 = json.dumps(
    {
        "method": "ValidateTx",
        "params": {
            "height": 1,
            "label": "t",
            "tx_hex": OP_DRIVECHAIN_TX,
            "drivechain_state": state,
        },
    }
).encode()
text = call(path_dc, body5).decode()
print("drivechain OP_DRIVECHAIN + state Accept:", text)
assert '"accept":true' in text or '"accept": true' in text, text

# Active first deposit without address → independent missing_deposit_address Reject
state_active = {
    "tip_height": 1,
    "tip_hash_hex": "00" * 32,
    "active_sidechain_numbers": [1],
    "ctips": [],
}
body6 = json.dumps(
    {
        "method": "ValidateTx",
        "params": {
            "height": 1,
            "label": "t",
            "tx_hex": OP_DRIVECHAIN_TX,
            "drivechain_state": state_active,
        },
    }
).encode()
text = call(path_dc, body6).decode()
print("drivechain active OP_DRIVECHAIN missing address Reject:", text)
assert "missing_deposit_address" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

# old_ctip_unspent: active slot with non-palindromic ctip, OP_DRIVECHAIN + address
# without spending old ctip (old_ctip_unspent takes precedence over missing addr)
# Non-palindromic display hex (LE/Display mixup would fail unit match paths)
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
# OP_DRIVECHAIN value 2000 + OP_RETURN address, no spend of CTIP_TXID
# Segwit-encoded (marker/flag) like OP_DRIVECHAIN_TX: v2, 0 ins, 2 outs
OP_DRIVECHAIN_ADDR_TX = (
    "02000000"  # version
    "0001"  # segwit marker/flag
    "00"  # 0 inputs
    "02"  # 2 outputs
    "d007000000000000"  # 2000 sats
    "04b4010151"  # OP_DRIVECHAIN PUSH1 0x01 OP_TRUE
    "0000000000000000"  # 0 sats
    "066a0461646472"  # OP_RETURN push "addr"
    "00000000"  # locktime (no witness blobs for 0 inputs)
)
body7 = json.dumps(
    {
        "method": "ValidateTx",
        "params": {
            "height": 1,
            "label": "t",
            "tx_hex": OP_DRIVECHAIN_ADDR_TX,
            "drivechain_state": state_ctip,
        },
    }
).encode()
text = call(path_dc, body7).decode()
print("drivechain old_ctip_unspent Reject:", text)
assert "old_ctip_unspent" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

# Path A pending M6: non-palindromic ctip spend (M6 shape) + pending_m6ids on wire
# CTIP internal LE 01 23 45 … AB CD → display hex below; M6 spends it → new treasury 400.
M6_CTIP_TXID = "cdab" + "00" * 27 + "452301"
# Non-segwit consensus encode: v2, 1 in (non-palindromic ctip), 1 OP_DRIVECHAIN out 400
M6_TX = (
    "0200000001"
    + ("012345" + "00" * 27 + "abcd")
    + "0000000000ffffffff01900100000000000004b401015100000000"
)
# Blinded m6id display (compute_m6id with old treasury 1000)
M6ID = "0b3399365e861cd82d10faf52b4d3906e61253284174bb02a320cc3d9fcf93b7"
state_m6_base = {
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
body_m6_miss = json.dumps(
    {
        "method": "ValidateTx",
        "params": {
            "height": 1,
            "label": "t",
            "tx_hex": M6_TX,
            "drivechain_state": state_m6_base,
        },
    }
).encode()
text = call(path_dc, body_m6_miss).decode()
print("drivechain M6 missing pending Reject:", text)
assert "m6_missing_pending_withdrawal" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

state_m6_low = dict(state_m6_base)
state_m6_low["pending_m6ids"] = [
    {
        "sidechain_number": 1,
        "m6id_hex": M6ID,
        "vote_count": 5,  # <= threshold 5
        "proposal_height": 1,
    }
]
body_m6_low = json.dumps(
    {
        "method": "ValidateTx",
        "params": {
            "height": 1,
            "label": "t",
            "tx_hex": M6_TX,
            "drivechain_state": state_m6_low,
        },
    }
).encode()
text = call(path_dc, body_m6_low).decode()
print("drivechain M6 insufficient votes Reject:", text)
assert "m6_insufficient_vote_count" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

state_m6_ok = dict(state_m6_base)
state_m6_ok["pending_m6ids"] = [
    {
        "sidechain_number": 1,
        "m6id_hex": M6ID,
        "vote_count": 6,  # > threshold 5
        "proposal_height": 1,
    }
]
body_m6_ok = json.dumps(
    {
        "method": "ValidateTx",
        "params": {
            "height": 1,
            "label": "t",
            "tx_hex": M6_TX,
            "drivechain_state": state_m6_ok,
        },
    }
).encode()
text = call(path_dc, body_m6_ok).decode()
print("drivechain M6 sufficient pending Accept:", text)
assert '"accept":true' in text or '"accept": true' in text, text

# ConnectBlock M7: block with M8 but no coinbase M7 → m8_not_accepted_by_miners
# (M8_TX prev_mainchain = 03*32; tip_hash must match so tip check passes first.)
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
    bytes.fromhex("02000000")  # version
    + bytes([1])  # 1 input
    + bytes(32)  # null prev txid
    + bytes.fromhex("ffffffff")  # coinbase index
    + bytes([0])  # empty scriptsig
    + bytes.fromhex("ffffffff")  # sequence
    + bytes([1])  # 1 output
    + struct.pack("<Q", 50)  # 50 sats
    + bytes([0])  # empty scriptPubKey
    + bytes.fromhex("00000000")  # locktime
)
m8_tx = bytes.fromhex(M8_TX)
header = (
    struct.pack("<I", 1)
    + bytes(32)  # prev
    + bytes(32)  # merkle (unchecked by SoftForkRule)
    + struct.pack("<I", 0)
    + struct.pack("<I", 0)
    + struct.pack("<I", 0)
)
block_hex = (header + encode_varint(2) + coinbase + m8_tx).hex()
state_m7 = {
    "tip_height": 0,
    "tip_hash_hex": "03" * 32,
    "active_sidechain_numbers": [],
    "ctips": [],
}
body_m7 = json.dumps(
    {
        "method": "ConnectBlock",
        "params": {
            "height": 1,
            "label": "t",
            "block_hex": block_hex,
            "drivechain_state": state_m7,
        },
    }
).encode()
text = call(path_dc, body_m7).decode()
print("drivechain ConnectBlock M8 without M7 Reject:", text)
assert "m8_not_accepted_by_miners" in text, text
assert '"accept":false' in text or '"accept": false' in text, text

# Path B residual SoftForkRule Accept locks over UDS (not full M1–M8 authority).
# Historical non-current spend (outpoint not in current ctips) → Accept.
# Design: Local dual-AND still owns historical map when present in LMDB.
HIST_TX = (
    "0200000001"
    + ("deadbeef" + "00" * 24 + "cafebabe")  # 32-byte txid LE wire
    + "0000000000ffffffff01"
    + "8403000000000000"  # 900 sats
    + "00"  # empty spk
    + "00000000"
)
state_hist = {
    "tip_height": 1,
    "tip_hash_hex": "00" * 32,
    "active_sidechain_numbers": [1],
    "ctips": [
        {
            "sidechain_number": 1,
            "outpoint_txid_hex": "012345" + "00" * 27 + "abcd",
            "outpoint_vout": 0,
            "value_sats": 1000,
        }
    ],
}
body_hist = json.dumps(
    {
        "method": "ValidateTx",
        "params": {
            "height": 1,
            "label": "t",
            "tx_hex": HIST_TX,
            "drivechain_state": state_hist,
        },
    }
).encode()
text = call(path_dc, body_hist).decode()
print("drivechain Path B historical residual ValidateTx Accept:", text)
assert '"accept":true' in text or '"accept": true' in text, text

# M1 coinbase ConnectBlock residual Accept (proposal DB not on wire).
# M1 script: OP_RETURN push TAG D5E0C4AF + sidechain 01 + desc 00 78
m1_spk = bytes.fromhex("6a07d5e0c4af010078")
coinbase_m1 = (
    bytes.fromhex("02000000")
    + bytes([1])
    + bytes(32)
    + bytes.fromhex("ffffffff")
    + bytes([0])
    + bytes.fromhex("ffffffff")
    + bytes([1])
    + struct.pack("<Q", 50)
    + bytes([len(m1_spk)])
    + m1_spk
    + bytes.fromhex("00000000")
)
header_m1 = (
    struct.pack("<I", 1)
    + bytes(32)
    + bytes(32)
    + struct.pack("<I", 0)
    + struct.pack("<I", 0)
    + struct.pack("<I", 0)
)
block_m1_hex = (header_m1 + encode_varint(1) + coinbase_m1).hex()
body_m1 = json.dumps(
    {
        "method": "ConnectBlock",
        "params": {
            "height": 1,
            "label": "t",
            "block_hex": block_m1_hex,
            "drivechain_state": state_hist,
        },
    }
).encode()
text = call(path_dc, body_m1).decode()
print("drivechain Path B M1 residual ConnectBlock Accept:", text)
assert '"accept":true' in text or '"accept": true' in text, text

print("UDS multiproc smoke: OK")
PY

echo "validate-rules-engine: OK"
