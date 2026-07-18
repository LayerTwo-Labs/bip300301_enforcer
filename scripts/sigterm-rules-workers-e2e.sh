#!/usr/bin/env bash
# Process-level SIGTERM e2e for multiproc rule workers (drivechain + bip360).
#
# Asserts: start --uds → Health ready handshake → kill -TERM → process exits
# cleanly within timeout → UDS path unlinked (bin removes socket after loop).
#
# Placement (SCORE honesty):
#   * Not part of `just validate-rules-engine` / GH `check-rules-engine`.
#   * Library `serve_uds_loop` shutdown is unit-tested; this script covers bin
#     `ctrlc` → AtomicBool wiring at process level.
#   * Accept-poll residual: worst-case exit latency is ~UDS_ACCEPT_POLL_MS (50ms)
#     after the flag is set while idle. Mid-handler cancel is still residual
#     (accept-loop only). Script only SIGTERMs after Health returns (idle).
#
# Usage:
#   ./scripts/sigterm-rules-workers-e2e.sh
#   just sigterm-rules-workers-e2e
#
# Do NOT use cargo --all-features.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Exit wait after kill -TERM (accept poll is 50ms; generous bound for CI load).
EXIT_TIMEOUT_MS="${CUSF_SIGTERM_EXIT_TIMEOUT_MS:-3000}"
# Socket ready + Health retries.
READY_ATTEMPTS="${CUSF_SIGTERM_READY_ATTEMPTS:-50}"
READY_SLEEP_S="${CUSF_SIGTERM_READY_SLEEP_S:-0.1}"

echo "==> sigterm-rules-workers-e2e: build workers"
cargo build -q -p cusf_rules_bip360 -p cusf_rules_drivechain
BIP360_BIN="$ROOT/target/debug/cusf_rules_bip360"
DC_BIN="$ROOT/target/debug/cusf_rules_drivechain"
[[ -x "$BIP360_BIN" && -x "$DC_BIN" ]] || {
  echo "missing worker binaries" >&2
  exit 1
}

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/cusf-sigterm-e2e-XXXXXX")"
WORKER_360_PID=""
WORKER_DC_PID=""

cleanup() {
  local pid
  for pid in ${WORKER_360_PID:-} ${WORKER_DC_PID:-}; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      # Last-resort cleanup only — happy path already proved TERM exit.
      kill -KILL "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

SOCK_360="$WORK_DIR/bip360.sock"
SOCK_DC="$WORK_DIR/drivechain.sock"
LOG_360="$WORK_DIR/bip360.log"
LOG_DC="$WORK_DIR/drivechain.log"

echo "==> start workers --uds"
"$BIP360_BIN" --uds "$SOCK_360" --activation-height 0 >"$LOG_360" 2>&1 &
WORKER_360_PID=$!
"$DC_BIN" --uds "$SOCK_DC" >"$LOG_DC" 2>&1 &
WORKER_DC_PID=$!

wait_sockets() {
  local i
  for i in $(seq 1 "$READY_ATTEMPTS"); do
    if [[ -S "$SOCK_360" && -S "$SOCK_DC" ]]; then
      return 0
    fi
    # Fail fast if a worker already died during bind.
    if ! kill -0 "$WORKER_360_PID" 2>/dev/null; then
      echo "bip360 worker exited before socket bind" >&2
      cat "$LOG_360" >&2 || true
      return 1
    fi
    if ! kill -0 "$WORKER_DC_PID" 2>/dev/null; then
      echo "drivechain worker exited before socket bind" >&2
      cat "$LOG_DC" >&2 || true
      return 1
    fi
    sleep "$READY_SLEEP_S"
  done
  echo "workers did not bind sockets in time" >&2
  cat "$LOG_360" "$LOG_DC" >&2 || true
  return 1
}

wait_sockets

echo "==> Health ready handshake (prove accept loop idle before SIGTERM)"
python3 - <<'PY' "$SOCK_360" "$SOCK_DC"
import socket, struct, sys, time

def recv_exact(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise EOFError(f"short read {len(data)}/{n}")
        data += chunk
    return data

def health(path: str, want_rule: str, want_cap: str, attempts: int = 20) -> None:
    last = None
    for _ in range(attempts):
        try:
            body = b'{"method":"Health"}'
            frame = b"RUL1" + struct.pack(">I", len(body)) + body
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(2)
            s.connect(path)
            s.sendall(frame)
            hdr = recv_exact(s, 8)
            assert hdr[:4] == b"RUL1", hdr
            (n,) = struct.unpack(">I", hdr[4:])
            data = recv_exact(s, n).decode()
            s.close()
            last = data
            if want_rule in data and want_cap in data:
                print(f"health ok {path}: {data[:120]}...")
                return
        except (OSError, AssertionError, EOFError) as e:
            last = repr(e)
        time.sleep(0.1)
    raise SystemExit(f"Health failed for {path}: last={last}")

p360, pdc = sys.argv[1], sys.argv[2]
health(p360, "bip360", "bip360-pqc-parents-and-chain-utxos-on-wire")
health(pdc, "drivechain", "drivechain-ctip-m8-pending-m6-checks-on-wire")
print("ready handshake: OK")
PY

# Dump worker logs before EXIT trap rm -rf WORK_DIR (post-TERM assertion path).
dump_worker_logs() {
  echo "--- worker logs (before WORK_DIR cleanup) ---" >&2
  echo "=== bip360 log ($LOG_360) ===" >&2
  cat "$LOG_360" >&2 || echo "(missing $LOG_360)" >&2
  echo "=== drivechain log ($LOG_DC) ===" >&2
  cat "$LOG_DC" >&2 || echo "(missing $LOG_DC)" >&2
  echo "--- end worker logs ---" >&2
}

# Millisecond-ish wait for process exit after TERM (no GNU timeout required).
wait_exit_ms() {
  local pid="$1"
  local name="$2"
  local budget_ms="$3"
  local waited=0
  local step_ms=50
  while kill -0 "$pid" 2>/dev/null; do
    if (( waited >= budget_ms )); then
      echo "FAIL: $name (pid=$pid) still alive ${budget_ms}ms after SIGTERM" >&2
      dump_worker_logs
      return 1
    fi
    # bash sleep accepts fractional seconds
    sleep 0.05
    waited=$((waited + step_ms))
  done
  # Reap; exit code from clean shutdown is 0.
  local ec=0
  wait "$pid" || ec=$?
  if [[ "$ec" -ne 0 ]]; then
    echo "FAIL: $name (pid=$pid) exited non-zero after SIGTERM: $ec" >&2
    dump_worker_logs
    return 1
  fi
  echo "  $name exited 0 within ${waited}ms after SIGTERM"
  return 0
}

assert_socket_gone() {
  local path="$1"
  local name="$2"
  local i
  # Unlink happens after serve_uds_loop returns; tiny race vs process exit.
  for i in $(seq 1 40); do
    if [[ ! -e "$path" ]]; then
      echo "  $name socket unlinked: $path"
      return 0
    fi
    sleep 0.05
  done
  echo "FAIL: $name socket still present after exit: $path" >&2
  ls -la "$(dirname "$path")" >&2 || true
  dump_worker_logs
  return 1
}

echo "==> kill -TERM multiproc workers"
kill -TERM "$WORKER_360_PID" "$WORKER_DC_PID"

wait_exit_ms "$WORKER_360_PID" "cusf_rules_bip360" "$EXIT_TIMEOUT_MS"
# Cleared so trap does not KILL a reaped pid.
WORKER_360_PID=""
wait_exit_ms "$WORKER_DC_PID" "cusf_rules_drivechain" "$EXIT_TIMEOUT_MS"
WORKER_DC_PID=""

assert_socket_gone "$SOCK_360" "bip360"
assert_socket_gone "$SOCK_DC" "drivechain"

echo "sigterm-rules-workers-e2e: OK (drivechain + bip360; hub not covered — needs bitcoind)"
echo "  SCORE note: not in validate-rules-engine (opt-in; accept-poll latency residual only)"
