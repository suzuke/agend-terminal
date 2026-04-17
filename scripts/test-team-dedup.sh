#!/usr/bin/env bash
# End-to-end test for fix/team-tab-missing:
#   1. regression — `create_team` successfully creates tabs + all panes attach
#   2. dedup — re-creating a team with the same name is rejected, not silently
#      overwriting the registry (which previously orphaned PTY subscriptions)
#
# Uses an isolated AGEND_HOME under /tmp. Never touches the real one.
# Exits 0 only if every assertion passes. Prints the filtered app.log on
# failure for debugging.

set -euo pipefail

BACKEND="${BACKEND:-bash}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/debug/agend-terminal"

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
info()  { printf '\033[36m[test]\033[0m %s\n' "$*"; }

fail() {
    red "FAIL: $*"
    if [[ -f "${LOG:-/nope}" ]]; then
        echo "---- app.log (team-related) ----"
        grep -E "CREATE_TEAM|TeamCreated|handle_team_created" "$LOG" || true
    fi
    exit 1
}

if [[ ! -x "$BIN" ]]; then
    info "building debug binary..."
    (cd "$ROOT" && cargo build --quiet) || fail "cargo build failed"
fi

AGEND_HOME="/tmp/agend-dedup-test-$$"
rm -rf "$AGEND_HOME"
mkdir -p "$AGEND_HOME"
export AGEND_HOME
SESSION="agend-dedup-$$"
LOG="$AGEND_HOME/app.log"

cleanup() {
    tmux kill-session -t "$SESSION" 2>/dev/null || true
    rm -rf "$AGEND_HOME"
}
trap cleanup EXIT

info "starting app in tmux (AGEND_HOME=$AGEND_HOME)"
tmux new-session -d -s "$SESSION" -x 120 -y 40 \
    "env AGEND_HOME='$AGEND_HOME' '$BIN' app"

# Wait for api.sock
SOCK=""
for _ in $(seq 1 50); do
    SOCK="$(find "$AGEND_HOME/run" -name api.sock 2>/dev/null | head -1 || true)"
    [[ -n "$SOCK" && -S "$SOCK" ]] && break
    sleep 0.2
done
[[ -z "$SOCK" ]] && fail "api.sock never appeared"
sleep 1

api_call() {
    python3 - "$SOCK" "$1" <<'PY'
import json, socket, sys
sock_path, payload = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(10)
s.connect(sock_path)
s.sendall((payload + "\n").encode())
print(s.makefile().readline().strip())
PY
}

json_ok() { python3 -c "import json,sys; d=json.loads(sys.argv[1]); sys.exit(0 if d.get('ok') is True else 1)" "$1"; }
json_field() { python3 -c "import json,sys; d=json.loads(sys.argv[1]); print(d.get(sys.argv[2], ''))" "$1" "$2"; }

# --- regression: happy path ---
info "create_team gemini count=3 (first call, should succeed)"
R1="$(api_call '{"method":"create_team","params":{"name":"gemini","count":3,"backend":"'"$BACKEND"'"}}')"
json_ok "$R1" || fail "first create_team returned ok=false: $R1"
SPAWNED1="$(python3 -c "import json,sys; print(len(json.loads(sys.argv[1]).get('spawned',[])))" "$R1")"
[[ "$SPAWNED1" == "3" ]] || fail "expected 3 spawned, got $SPAWNED1: $R1"
green "  first call spawned 3 members"

sleep 1

info "create_team kiro count=3 (different team, should succeed)"
R2="$(api_call '{"method":"create_team","params":{"name":"kiro","count":3,"backend":"'"$BACKEND"'"}}')"
json_ok "$R2" || fail "kiro create_team returned ok=false: $R2"
green "  kiro team spawned"

sleep 2

# Confirm tabs exist in tmux capture
CAPTURE="$(tmux capture-pane -t "$SESSION" -p)"
echo "$CAPTURE" | head -1 | grep -q "gemini" || fail "gemini tab missing from tab bar: $(echo "$CAPTURE" | head -1)"
echo "$CAPTURE" | head -1 | grep -q "kiro" || fail "kiro tab missing from tab bar"
green "  both team tabs visible in tab bar"

# Verify attach_pane counts via log
grep -q "handle_team_created end team=\"gemini\" expected=3 attached=3" "$LOG" || fail "gemini: not all 3 panes attached"
grep -q "handle_team_created end team=\"kiro\" expected=3 attached=3" "$LOG" || fail "kiro: not all 3 panes attached"
green "  all team panes attached (per app.log)"

# --- dedup: same-name rejection ---
info "create_team gemini count=3 (same name, should fail with all members rejected)"
R3="$(api_call '{"method":"create_team","params":{"name":"gemini","count":3,"backend":"'"$BACKEND"'"}}')"
if json_ok "$R3"; then
    fail "re-creating gemini unexpectedly succeeded — dedup check missing: $R3"
fi
echo "$R3" | grep -q "already exists" || fail "expected 'already exists' error, got: $R3"
green "  re-create rejected with dedup error"

# --- dedup partial: one member exists, two new ---
# Create team "alpha" with 2 members, then re-create "alpha" with count=3.
# Members 1,2 should be rejected (exist), member 3 should be accepted (new).
info "create_team alpha count=2"
api_call '{"method":"create_team","params":{"name":"alpha","count":2,"backend":"'"$BACKEND"'"}}' >/dev/null
sleep 1

info "create_team alpha count=3 (partial overlap: 1,2 exist, 3 new)"
R4="$(api_call '{"method":"create_team","params":{"name":"alpha","count":3,"backend":"'"$BACKEND"'"}}')"
json_ok "$R4" || fail "partial-overlap create returned ok=false: $R4"
SPAWNED4="$(python3 -c "import json,sys; d=json.loads(sys.argv[1]); print(len(d.get('spawned',[])))" "$R4")"
FAILED4="$(python3 -c "import json,sys; d=json.loads(sys.argv[1]); print(len(d.get('failed',[])))" "$R4")"
[[ "$SPAWNED4" == "1" ]] || fail "expected 1 new spawn for alpha re-create, got $SPAWNED4: $R4"
[[ "$FAILED4" == "2" ]] || fail "expected 2 dedup rejections for alpha re-create, got $FAILED4: $R4"
green "  partial overlap handled: 1 new spawn, 2 rejections"

# --- dedup at SPAWN level ---
info "spawn duplicate name (should fail)"
api_call '{"method":"spawn","params":{"name":"gemini-1","backend":"'"$BACKEND"'"}}' > /tmp/spawn-dup-$$.out 2>&1
R5="$(cat /tmp/spawn-dup-$$.out)"
rm -f /tmp/spawn-dup-$$.out
if json_ok "$R5"; then
    fail "spawn duplicate unexpectedly succeeded: $R5"
fi
echo "$R5" | grep -q "already exists" || fail "expected 'already exists' from SPAWN dedup, got: $R5"
green "  SPAWN dedup rejects duplicate name"

info "shutdown"
api_call '{"method":"shutdown","params":{}}' >/dev/null 2>&1 || true

echo
green "All dedup + regression assertions passed."
