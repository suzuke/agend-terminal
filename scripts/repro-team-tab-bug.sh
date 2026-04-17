#!/usr/bin/env bash
# Reproduce the "create team doesn't show tab" investigation flow.
#
# Safety: uses an isolated AGEND_HOME under /tmp by default. Your real
# ~/.agend-terminal (or whatever AGEND_HOME you export) is NOT touched
# unless you explicitly pass --real-home.
#
# Usage:
#   ./scripts/repro-team-tab-bug.sh [--real-home] [--keep] [--backend CMD]
#     --real-home   use $AGEND_HOME from env instead of a fresh tmp dir
#                   (destructive — will add teams to your actual fleet)
#     --keep        keep tmp AGEND_HOME + tmux session after run
#     --backend CMD spawn command for team members (default: bash;
#                   use "claude"/"gemini"/"kiro" for real backends)
#
# What it does:
#   1. build agend-terminal debug binary if missing
#   2. start `agend-terminal app` in a detached tmux session
#   3. wait for api.sock
#   4. create team "gemini" (count=3), then team "kiro" (count=3) via API
#   5. query list, capture tmux pane screen, tail app.log
#   6. shutdown via api, kill tmux session
#
# Emits to stdout: API responses, tmux capture, filtered log lines.
# Full log path is printed at the end for deeper inspection.

set -euo pipefail

REAL_HOME=0
KEEP=0
BACKEND="bash"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --real-home) REAL_HOME=1; shift ;;
        --keep) KEEP=1; shift ;;
        --backend) BACKEND="$2"; shift 2 ;;
        -h|--help) sed -n '1,30p' "$0"; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/target/debug/agend-terminal"

if [[ ! -x "$BIN" ]]; then
    echo ">> building debug binary..."
    (cd "$ROOT" && cargo build) >&2
fi

if [[ $REAL_HOME -eq 1 ]]; then
    : "${AGEND_HOME:?--real-home requires AGEND_HOME to be set}"
    echo ">> using REAL AGEND_HOME=$AGEND_HOME (DESTRUCTIVE)"
else
    AGEND_HOME="/tmp/agend-repro-$$"
    rm -rf "$AGEND_HOME"
    mkdir -p "$AGEND_HOME"
    export AGEND_HOME
    echo ">> using isolated AGEND_HOME=$AGEND_HOME"
fi

SESSION="agend-repro-$$"
cleanup() {
    local ec=$?
    if [[ $KEEP -eq 0 ]]; then
        tmux kill-session -t "$SESSION" 2>/dev/null || true
        [[ $REAL_HOME -eq 0 ]] && rm -rf "$AGEND_HOME"
    else
        echo ">> KEEP set — tmux session '$SESSION', AGEND_HOME=$AGEND_HOME retained"
    fi
    return $ec
}
trap cleanup EXIT

echo ">> starting tmux session '$SESSION' (120x40)"
tmux new-session -d -s "$SESSION" -x 120 -y 40 \
    "env AGEND_HOME='$AGEND_HOME' '$BIN' app"

echo ">> waiting for api.port..."
API_PORT=""
for _ in $(seq 1 50); do
    PORT_FILE="$(find "$AGEND_HOME/run" -name api.port 2>/dev/null | head -1 || true)"
    if [[ -n "$PORT_FILE" && -f "$PORT_FILE" ]]; then
        API_PORT="$(cat "$PORT_FILE" 2>/dev/null || true)"
        [[ -n "$API_PORT" ]] && break
    fi
    sleep 0.2
done
if [[ -z "$API_PORT" ]]; then
    echo "!! api.port never appeared under $AGEND_HOME/run" >&2
    echo "-- tmux pane dump:" >&2
    tmux capture-pane -t "$SESSION" -p >&2 || true
    exit 1
fi
echo ">> api.port=$API_PORT"
sleep 1  # let app fully init

api_call() {
    local payload="$1"
    python3 - "$API_PORT" "$payload" <<'PY'
import socket, sys
port, payload = int(sys.argv[1]), sys.argv[2]
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.connect(("127.0.0.1", port))
s.sendall((payload + "\n").encode())
line = s.makefile().readline().strip()
print(line)
PY
}

echo ">> create_team gemini count=3 backend=$BACKEND"
api_call '{"method":"create_team","params":{"name":"gemini","count":3,"backend":"'"$BACKEND"'"}}'
sleep 1

echo ">> create_team kiro count=3 backend=$BACKEND"
api_call '{"method":"create_team","params":{"name":"kiro","count":3,"backend":"'"$BACKEND"'"}}'
sleep 2

echo ">> list"
api_call '{"method":"list","params":{}}'

echo ""
echo "-- tmux capture (last frame) --"
tmux capture-pane -t "$SESSION" -p || true

echo ""
echo "-- requesting shutdown --"
api_call '{"method":"shutdown","params":{}}' || true
sleep 0.5

LOG="$AGEND_HOME/app.log"
if [[ -f "$LOG" ]]; then
    echo ""
    echo "-- app.log filtered --"
    grep -E "CREATE_TEAM|TeamCreated|handle_team_created|handle_instance_created|InstanceCreated|attach_pane|add_tab|spawn" "$LOG" || true
    echo ""
    echo "-- full log: $LOG (lines: $(wc -l < "$LOG")) --"
else
    echo "!! no app.log at $LOG"
fi
