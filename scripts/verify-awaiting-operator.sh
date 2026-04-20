#!/usr/bin/env bash
# End-to-end smoke test for the AwaitingOperator feature.
#
# Reproduces the codex-style "stuck on interactive prompt" scenario with a
# silent backend (`cat`), verifies the state machine flips to
# `awaiting_operator` after ~30s of stdout silence, and confirms that an
# API `inject` with `raw: true` writes bytes straight to the agent's stdin
# without inbox wrapping.
#
# Runs entirely in an isolated AGEND_HOME under /tmp. No install, no touch
# to the user's real ~/.codex or fleet.yaml.
#
# Exit 0 on success; non-zero on first failure.
set -euo pipefail

cd "$(dirname "$0")/.."

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
info()  { printf '\033[36m[check]\033[0m %s\n' "$*"; }
fail()  { red "FAIL: $*"; exit 1; }

TEST_HOME="$(mktemp -d -t agend-await-XXXXXX)"
BIN="$PWD/target/debug/agend-terminal"
DAEMON_PID=""

cleanup() {
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -TERM "$DAEMON_PID" 2>/dev/null || true
        for _ in 1 2 3 4 5; do
            kill -0 "$DAEMON_PID" 2>/dev/null || break
            sleep 0.3
        done
        kill -KILL "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$TEST_HOME"
}
trap cleanup EXIT

info "cargo build (debug)"
cargo build --quiet 2>&1 | tail -5
[[ -x "$BIN" ]] || fail "debug binary not found at $BIN"
green "  ok"

info "start daemon with silent backend (cat) in $TEST_HOME"
mkdir -p "$TEST_HOME/workspace"
AGEND_HOME="$TEST_HOME" "$BIN" daemon silent:cat \
    > "$TEST_HOME/daemon.log" 2>&1 &
DAEMON_PID=$!
# Suppress bash's "Terminated:" job-control message when cleanup kills the daemon.
disown "$DAEMON_PID" 2>/dev/null || true

# Wait up to 3s for the API port file to appear (run dir is keyed by pid).
API_PORT_FILE=""
for _ in 1 2 3 4 5 6; do
    API_PORT_FILE="$(find "$TEST_HOME/run" -name api.port -type f 2>/dev/null | head -1 || true)"
    [[ -n "$API_PORT_FILE" ]] && break
    sleep 0.5
done
[[ -n "$API_PORT_FILE" ]] || fail "api.port never appeared (daemon crashed? see $TEST_HOME/daemon.log)"
API_PORT="$(cat "$API_PORT_FILE")"
API_COOKIE_FILE="$(dirname "$API_PORT_FILE")/api.cookie"
[[ -f "$API_COOKIE_FILE" ]] || fail "api.cookie missing at $API_COOKIE_FILE (Stage 8 auth regression?)"
green "  ok (api port $API_PORT)"

# Helper: send a single NDJSON request after the Stage 8 cookie handshake.
# Usage: rpc '{"method":"list"}'
rpc() {
    python3 - "$API_PORT" "$API_COOKIE_FILE" "$1" <<'PY'
import json, socket, sys
port = int(sys.argv[1])
cookie_path = sys.argv[2]
req = sys.argv[3]
with open(cookie_path, "rb") as f:
    cookie_bytes = f.read()
cookie_hex = cookie_bytes.hex()
s = socket.create_connection(("127.0.0.1", port), timeout=3.0)
s.settimeout(3.0)
def send_line(payload):
    s.sendall((payload + "\n").encode())
def recv_line():
    buf = b""
    while b"\n" not in buf:
        chunk = s.recv(4096)
        if not chunk:
            break
        buf += chunk
    return buf.decode().splitlines()[0] if buf else ""
# Stage 8 handshake: first line auths the whole session.
send_line(json.dumps({"auth": cookie_hex}))
auth_resp = recv_line()
if json.loads(auth_resp).get("ok") is not True:
    print(auth_resp)
    sys.exit(0)
send_line(req)
print(recv_line())
s.close()
PY
}

info "initial state == starting"
INITIAL="$(rpc '{"method":"list"}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["agents"][0]["agent_state"])')"
[[ "$INITIAL" == "starting" ]] || fail "expected 'starting', got '$INITIAL'"
green "  ok"

info "wait for silence detection (tick period is 10s, silence threshold 30s)"
AFTER=""
for i in $(seq 1 50); do
    sleep 1
    AFTER="$(rpc '{"method":"list"}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["agents"][0]["agent_state"])')"
    [[ "$AFTER" == "awaiting_operator" ]] && { green "  ok (flipped after ${i}s)"; break; }
done
[[ "$AFTER" == "awaiting_operator" ]] || fail "expected 'awaiting_operator' within 50s, got '$AFTER'"

info "inject raw bytes — should reach cat's stdin unwrapped"
RESP="$(rpc '{"method":"inject","params":{"name":"silent","data":"HELLO-RAW\n","raw":true}}')"
OK="$(echo "$RESP" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ok"))')"
[[ "$OK" == "True" ]] || fail "raw inject failed: $RESP"
green "  ok"

info "verify cat echoed the raw bytes back through its PTY"
# Agent TUI socket streams raw PTY output (ANSI-escaped). Pull a snapshot
# and check the literal 'HELLO-RAW' appears — no '[telegram:xxx]' wrapping.
AGENT_PORT_FILE="$(find "$TEST_HOME/run" -name silent.port -type f 2>/dev/null | head -1 || true)"
[[ -n "$AGENT_PORT_FILE" ]] || fail "agent TUI port file not found"
AGENT_PORT="$(cat "$AGENT_PORT_FILE")"
PTY_DUMP="$(python3 - "$AGENT_PORT" "$API_COOKIE_FILE" <<'PY'
import socket, sys
port = int(sys.argv[1])
with open(sys.argv[2], "rb") as f:
    cookie = f.read()
s = socket.create_connection(("127.0.0.1", port), timeout=2.0)
# TUI socket auth: 32 raw cookie bytes before any stream reads.
s.sendall(cookie)
s.settimeout(1.5)
buf = b""
try:
    while True:
        d = s.recv(8192)
        if not d:
            break
        buf += d
except socket.timeout:
    pass
s.close()
sys.stdout.buffer.write(buf)
PY
)"
if ! echo "$PTY_DUMP" | grep -q "HELLO-RAW"; then
    fail "PTY output did not contain 'HELLO-RAW' — raw inject may not have reached stdin"
fi
if echo "$PTY_DUMP" | grep -qE '\[(telegram|from):'; then
    fail "PTY output contains source-wrapping prefix — raw path should bypass inbox formatting"
fi
green "  ok"

info "state stays awaiting_operator until ready_pattern matches"
FINAL="$(rpc '{"method":"list"}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["agents"][0]["agent_state"])')"
[[ "$FINAL" == "awaiting_operator" ]] || fail "expected state to persist at 'awaiting_operator', got '$FINAL' (cat has no ready_pattern, so lift is not expected here)"
green "  ok"

echo
green "AwaitingOperator end-to-end smoke test passed."
