#!/usr/bin/env bash
# Opt-in live tip e2e: bip360 (+ drivechain) workers on UDS + hub --rules-worker
# against a regtest bitcoind when BITCOIND (or integrationtests.env) is available.
#
# Placement (SCORE / PR CI residual honesty):
#   * Not part of `just validate-rules-engine` / GH `check-rules-engine`.
#   * PR green matrix must stay free of bitcoind (docs/MULTI_ENFORCER.md).
#   * HITL only: workflow_dispatch input `run_live_tip_e2e` on
#     check_lint_build_release.yaml — never automatic on pull_request/push.
#   * Autonomous multiproc smoke (no bitcoind) stays in validate-rules-engine.sh
#     / smoke-rules-workers-tip.sh.
#   * Do NOT invent automatic PR CI bitcoind live tip without product unlock.
#
# Usage:
#   CUSF_LIVE_TIP_E2E=1 BITCOIND=/path/to/bitcoind ./scripts/live-tip-rules-workers-e2e.sh
#   CUSF_LIVE_TIP_E2E=1 just live-tip-rules-workers-e2e
#
# Exit policy:
#   * CUSF_LIVE_TIP_E2E unset / not 1 → skip exit 0 (soft-skip for CI default)
#   * CUSF_LIVE_TIP_E2E=1 + missing BITCOIND / bitcoind not ready / hub fail → exit non-zero
#
# Do NOT use cargo --all-features.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ "${CUSF_LIVE_TIP_E2E:-}" != "1" ]]; then
  echo "live-tip-rules-workers-e2e: skip (set CUSF_LIVE_TIP_E2E=1 to opt in)"
  exit 0
fi

# Load integrationtests.env if present (BITCOIND, etc.)
if [[ -f "$ROOT/integrationtests.env" ]]; then
  # shellcheck disable=SC1091
  set -a
  # shellcheck source=/dev/null
  source "$ROOT/integrationtests.env"
  set +a
fi

if [[ -z "${BITCOIND:-}" ]] || [[ ! -x "${BITCOIND}" ]]; then
  echo "live-tip-rules-workers-e2e: FAIL (CUSF_LIVE_TIP_E2E=1 but BITCOIND missing or not executable)" >&2
  echo "  set BITCOIND=/path/to/bitcoind (see integrationtests.env / example.env)" >&2
  exit 1
fi

echo "==> live-tip-rules-workers-e2e: build hub + workers"
cargo build -q -p cusf_rules_bip360 -p cusf_rules_drivechain
cargo build -q -p bip300301_enforcer --features "drivechain,bip360,rustls"

BIP360_BIN="$ROOT/target/debug/cusf_rules_bip360"
DC_BIN="$ROOT/target/debug/cusf_rules_drivechain"
HUB_BIN="$ROOT/target/debug/bip300301_enforcer"

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/cusf-live-tip-XXXXXX")"
cleanup() {
  for pidvar in HUB_PID WORKER_360_PID WORKER_DC_PID BITCOIND_PID; do
    pid="${!pidvar:-}"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

SOCK_360="$WORK_DIR/bip360.sock"
SOCK_DC="$WORK_DIR/drivechain.sock"
BTC_DIR="$WORK_DIR/bitcoind"
HUB_DATA="$WORK_DIR/hub"
mkdir -p "$BTC_DIR" "$HUB_DATA"

# Minimal regtest bitcoind (rest=1 + zmq sequence for hub; no electrs required for this smoke).
# Honor CUSF_LIVE_TIP_*_PORT env overrides; only auto-pick free ports when unset.
if command -v python3 >/dev/null 2>&1; then
  read -r AUTO_RPC AUTO_P2P AUTO_ZMQ < <(python3 - <<'PY'
import socket
def free():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p
print(free(), free(), free())
PY
)
else
  AUTO_RPC=18443
  AUTO_P2P=18444
  AUTO_ZMQ=28332
fi
RPC_PORT="${CUSF_LIVE_TIP_RPC_PORT:-$AUTO_RPC}"
P2P_PORT="${CUSF_LIVE_TIP_P2P_PORT:-$AUTO_P2P}"
ZMQ_PORT="${CUSF_LIVE_TIP_ZMQ_PORT:-$AUTO_ZMQ}"

cat >"$BTC_DIR/bitcoin.conf" <<EOF
regtest=1
server=1
txindex=1
rest=1
acceptnonstdtxn=1
rpcuser=user
rpcpassword=pass
rpcport=${RPC_PORT}
port=${P2P_PORT}
rpcbind=127.0.0.1
rpcallowip=127.0.0.1
listen=0
fallbackfee=0.0001
zmqpubsequence=tcp://127.0.0.1:${ZMQ_PORT}
EOF

echo "==> starting bitcoind (regtest rpc=${RPC_PORT} zmq=${ZMQ_PORT})"
"$BITCOIND" -datadir="$BTC_DIR" -conf="$BTC_DIR/bitcoin.conf" -daemon=0 \
  >"$WORK_DIR/bitcoind.log" 2>&1 &
BITCOIND_PID=$!

# Wait for RPC
BTC_CLI=()
if command -v bitcoin-cli >/dev/null 2>&1; then
  BTC_CLI=(bitcoin-cli -regtest -rpcuser=user -rpcpassword=pass -rpcport="$RPC_PORT")
elif [[ -x "$(dirname "$BITCOIND")/bitcoin-cli" ]]; then
  BTC_CLI=("$(dirname "$BITCOIND")/bitcoin-cli" -regtest -rpcuser=user -rpcpassword=pass -rpcport="$RPC_PORT")
fi

rpc_ready=0
if [[ ${#BTC_CLI[@]} -gt 0 ]]; then
  for _ in $(seq 1 80); do
    if "${BTC_CLI[@]}" getblockchaininfo >/dev/null 2>&1; then
      rpc_ready=1
      break
    fi
    sleep 0.25
  done
else
  # No bitcoin-cli: poll TCP open on RPC port (not mere process aliveness).
  if command -v python3 >/dev/null 2>&1; then
    for _ in $(seq 1 80); do
      if python3 - "$RPC_PORT" <<'PY'
import socket, sys
p = int(sys.argv[1])
s = socket.socket(); s.settimeout(0.3)
try:
    s.connect(("127.0.0.1", p)); s.close(); sys.exit(0)
except OSError:
    sys.exit(1)
PY
      then
        rpc_ready=1
        break
      fi
      sleep 0.25
    done
  else
    sleep 3
    if kill -0 "$BITCOIND_PID" 2>/dev/null; then
      rpc_ready=1
    fi
  fi
fi

if [[ "$rpc_ready" -ne 1 ]]; then
  echo "bitcoind RPC not ready; log:" >&2
  tail -n 40 "$WORK_DIR/bitcoind.log" >&2 || true
  echo "live-tip-rules-workers-e2e: FAIL (bitcoind did not become ready under CUSF_LIVE_TIP_E2E=1)" >&2
  exit 1
fi
echo "    bitcoind RPC ok"

echo "==> starting rules workers"
"$BIP360_BIN" --uds "$SOCK_360" --activation-height 0 >"$WORK_DIR/bip360.log" 2>&1 &
WORKER_360_PID=$!
"$DC_BIN" --uds "$SOCK_DC" >"$WORK_DIR/drivechain.log" 2>&1 &
WORKER_DC_PID=$!

for _ in $(seq 1 50); do
  if [[ -S "$SOCK_360" && -S "$SOCK_DC" ]]; then break; fi
  sleep 0.1
done
[[ -S "$SOCK_360" && -S "$SOCK_DC" ]] || {
  echo "workers did not bind" >&2
  exit 1
}

# Capability sanity (registration path; recv_exact for RUL1 header)
python3 - <<'PY' "$SOCK_360" "$SOCK_DC"
import socket, struct, sys

def recv_exact(sock, n: int) -> bytes:
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
    n = struct.unpack(">I", hdr[4:])[0]
    data = recv_exact(s, n)
    s.close()
    return data.decode()

p360, pdc = sys.argv[1], sys.argv[2]
assert "bip360-pqc-parents-and-chain-utxos-on-wire" in call(p360, b'{"method":"Health"}')
assert "drivechain-ctip-m8-pending-m6-checks-on-wire" in call(pdc, b'{"method":"Health"}')
print("live-tip worker capabilities: OK")
PY

echo "==> starting hub with --rules-worker bip360 + drivechain (real CLI flags)"
# Real hub flags from lib/cli.rs (not --mainchain-rpc-url).
"$HUB_BIN" \
  --data-dir "$HUB_DATA" \
  --node-rpc-addr "127.0.0.1:${RPC_PORT}" \
  --node-rpc-user user \
  --node-rpc-pass pass \
  --node-zmq-addr-sequence "tcp://127.0.0.1:${ZMQ_PORT}" \
  --rules-worker "bip360=${SOCK_360}" \
  --rules-worker "drivechain=${SOCK_DC}" \
  --rules-worker-timeout-ms 5000 \
  >"$WORK_DIR/hub.log" 2>&1 &
HUB_PID=$!

# Brief settle: hub must stay up (not brick on worker handshake).
sleep 3
if ! kill -0 "$HUB_PID" 2>/dev/null; then
  echo "hub exited early; last log lines:" >&2
  tail -n 80 "$WORK_DIR/hub.log" >&2 || true
  echo "live-tip-rules-workers-e2e: FAIL (hub exited under CUSF_LIVE_TIP_E2E=1)" >&2
  exit 1
fi

# Fail-closed: clap/usage errors must not soft-skip under opt-in.
if grep -qiE 'error: unexpected argument|error: unrecognized|Usage: bip300301_enforcer' "$WORK_DIR/hub.log" 2>/dev/null; then
  echo "hub CLI error under opt-in:" >&2
  tail -n 40 "$WORK_DIR/hub.log" >&2 || true
  exit 1
fi

echo "    hub still running with remote workers (tip path not bricked by handshake)"
echo "live-tip-rules-workers-e2e: OK (workers+hub+bitcoind smoke; full invalidate residual)"
echo "  SCORE note: not in validate-rules-engine / check-rules-engine (HITL / opt-in only)"
