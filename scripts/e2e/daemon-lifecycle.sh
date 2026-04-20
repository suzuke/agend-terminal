#!/usr/bin/env bash
# End-to-end tests for the daemon-resident lifecycle (Stage 3.6).
#
# Scenarios (from docs/PLAN-daemon-resident.md §3.6):
#   1. start --detached → parent exits → daemon survives.
#   2. Second start with fleet hits the flock → exits non-zero.
#   3. app while daemon is live → connects as remote client (Stage 3.4).
#   4. stop tears down run dir; subsequent start cold-starts.
#
# Runs entirely under a temporary AGEND_HOME. No install, no touch to the
# user's real ~/agend. Requires python3 (PTY harness for Scenario 3).
#
# Exit 0 on success; non-zero on first failure.
set -euo pipefail

cd "$(dirname "$0")/../.."

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
info()  { printf '\033[36m[check]\033[0m %s\n' "$*"; }
fail()  { red "FAIL: $*"; exit 1; }

TEST_HOME="$(mktemp -d -t agend-lifecycle-XXXXXX)"
BIN="${AGEND_TERMINAL_BIN:-$PWD/target/debug/agend-terminal}"
APP_PID=""
DAEMON_PID=""

cleanup() {
    if [[ -n "$APP_PID" ]] && kill -0 "$APP_PID" 2>/dev/null; then
        kill -TERM "$APP_PID" 2>/dev/null || true
    fi
    # Best-effort daemon teardown via API — no-op if already stopped.
    AGEND_HOME="$TEST_HOME" "$BIN" stop >/dev/null 2>&1 || true
    # Hard-kill any lingering daemon we spawned.
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -KILL "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$TEST_HOME"
}
trap cleanup EXIT

info "build debug binary"
cargo build --quiet 2>&1 | tail -5
[[ -x "$BIN" ]] || fail "binary not found at $BIN"
green "  ok"

command -v python3 >/dev/null || fail "python3 required (PTY harness for Scenario 3)"

info "prepare isolated AGEND_HOME at $TEST_HOME"
cat > "$TEST_HOME/fleet.yaml" <<'YAML'
defaults:
  command: /bin/cat
instances:
  alpha: {}
  beta: {}
YAML
green "  ok"

wait_for_run_dir() {
    local tries=40
    while (( tries-- > 0 )); do
        if compgen -G "$TEST_HOME/run/*/.daemon" >/dev/null; then
            return 0
        fi
        sleep 0.25
    done
    return 1
}

get_daemon_pid() {
    local f
    f="$(find "$TEST_HOME/run" -name .daemon -type f 2>/dev/null | head -1 || true)"
    [[ -n "$f" ]] || return 1
    # .daemon is `pid:timestamp` (see src/daemon/mod.rs:write_daemon_id)
    tr -d '[:space:]' < "$f" | cut -d: -f1
}

# -------------------- Scenario 1: start --detached --------------------
info "Scenario 1: start --detached survives parent exit"
AGEND_HOME="$TEST_HOME" "$BIN" start --detached >/dev/null
wait_for_run_dir || fail "run_dir never appeared after start --detached"
DAEMON_PID="$(get_daemon_pid)" || fail "could not read daemon pid"
kill -0 "$DAEMON_PID" 2>/dev/null || fail "daemon pid $DAEMON_PID is not alive"
# Give it a moment, re-check — detached daemon must outlive this shell.
sleep 0.5
kill -0 "$DAEMON_PID" 2>/dev/null || fail "daemon died shortly after start"
# list must enumerate the fleet instances (up to a few seconds for agents to bind ports)
for _ in 1 2 3 4 5 6 7 8; do
    LIST="$(AGEND_HOME="$TEST_HOME" "$BIN" list --json)"
    if echo "$LIST" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if ("alpha" in d and "beta" in d) else 1)'; then
        break
    fi
    sleep 0.5
done
echo "$LIST" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if ("alpha" in d and "beta" in d) else 1)' \
    || fail "list should include alpha + beta, got: $LIST"
green "  ok (pid $DAEMON_PID, agents ready)"

# -------------------- Scenario 2: second start rejects --------------------
info "Scenario 2: second start hits the flock"
if AGEND_HOME="$TEST_HOME" "$BIN" start > "$TEST_HOME/second-start.out" 2> "$TEST_HOME/second-start.err"; then
    fail "second start unexpectedly succeeded (exit 0)"
fi
# Lock-held errors surface via anyhow — look for any hint of lock/running.
if ! grep -qiE "lock|already running|daemon" "$TEST_HOME/second-start.err" "$TEST_HOME/second-start.out" 2>/dev/null; then
    fail "second start error did not mention lock/running; see $TEST_HOME/second-start.err"
fi
kill -0 "$DAEMON_PID" 2>/dev/null || fail "daemon died during second-start attempt"
green "  ok"

# -------------------- Scenario 3: app attaches (Stage 3.4) --------------------
info "Scenario 3: app attaches to live daemon via PTY (post-3.4)"
# Spawn the app with a pseudo-tty so ratatui::init succeeds. The python
# wrapper reports the child pid on its stdout, sleeps a few seconds to let
# the app reach bootstrap::prepare + build remote panes, then SIGTERMs it.
python3 - "$BIN" "$TEST_HOME" > "$TEST_HOME/app-pid.out" 2>&1 <<'PY' &
import os, pty, sys, time
bin_path = sys.argv[1]
test_home = sys.argv[2]
pid, _fd = pty.fork()
if pid == 0:
    os.environ["AGEND_HOME"] = test_home
    try:
        os.execvp(bin_path, [bin_path, "app"])
    except Exception as e:
        print(f"execvp failed: {e}", file=sys.stderr)
        os._exit(127)
print(pid, flush=True)
time.sleep(3)
try:
    os.kill(pid, 15)  # SIGTERM
except ProcessLookupError:
    pass
# Reap up to 3s
for _ in range(30):
    try:
        wpid, _ = os.waitpid(pid, os.WNOHANG)
        if wpid == pid:
            break
    except ChildProcessError:
        break
    time.sleep(0.1)
PY
WRAPPER_PID=$!
for _ in 1 2 3 4 5 6 7 8; do
    [[ -s "$TEST_HOME/app-pid.out" ]] && break
    sleep 0.2
done
APP_PID="$(head -1 "$TEST_HOME/app-pid.out" | awk '{print $1}' | grep -E '^[0-9]+$' || true)"
[[ -n "$APP_PID" ]] || fail "python wrapper did not print app pid; output: $(cat "$TEST_HOME/app-pid.out")"
# Let it settle, then confirm it's alive (i.e. it didn't fail-fast).
sleep 1.5
if ! kill -0 "$APP_PID" 2>/dev/null; then
    fail "app exited early (pid $APP_PID) — Stage 3.4 regression? See $TEST_HOME/app.log"
fi
# Positive assertion: app.log shows the Attached branch ran.
if ! grep -q "attached to existing daemon" "$TEST_HOME/app.log" 2>/dev/null; then
    fail "app.log missing 'attached to existing daemon' trace; see $TEST_HOME/app.log"
fi
# And no Stage 3.3 fail-fast message (defense in depth — shouldn't even be
# possible with the pre-TUI bail removed, but cheap to assert).
if grep -qi "another agend-terminal daemon is already running" "$TEST_HOME/app.log" 2>/dev/null; then
    fail "app.log still contains pre-3.4 fail-fast message"
fi
# Let the wrapper finish (it SIGTERMs the app after 3s).
wait "$WRAPPER_PID" 2>/dev/null || true
APP_PID=""
# The daemon must survive the app's exit — app is a client, not an owner.
kill -0 "$DAEMON_PID" 2>/dev/null || fail "daemon died when app was shut down (should have survived)"
green "  ok (attach traced; daemon survived app exit)"

# -------------------- Scenario 4: stop → cold start --------------------
info "Scenario 4: stop tears down run dir; next start cold-starts"
AGEND_HOME="$TEST_HOME" "$BIN" stop >/dev/null
for _ in $(seq 1 20); do
    compgen -G "$TEST_HOME/run/*/.daemon" >/dev/null || break
    sleep 0.3
done
compgen -G "$TEST_HOME/run/*/.daemon" >/dev/null && fail "run_dir still present after stop"
for _ in 1 2 3 4 5 6 7 8 9 10; do
    kill -0 "$DAEMON_PID" 2>/dev/null || break
    sleep 0.3
done
kill -0 "$DAEMON_PID" 2>/dev/null && fail "old daemon pid $DAEMON_PID still alive after stop"
OLD_PID="$DAEMON_PID"
DAEMON_PID=""
# Cold start — new PID, new run_dir.
AGEND_HOME="$TEST_HOME" "$BIN" start --detached >/dev/null
wait_for_run_dir || fail "cold start failed to create run_dir"
DAEMON_PID="$(get_daemon_pid)" || fail "cold start has no pid"
kill -0 "$DAEMON_PID" 2>/dev/null || fail "cold-started daemon $DAEMON_PID not alive"
[[ "$DAEMON_PID" != "$OLD_PID" ]] || fail "new daemon reuses old pid $OLD_PID (should be impossible)"
green "  ok (new pid $DAEMON_PID)"

echo
green "Daemon lifecycle end-to-end tests passed."
