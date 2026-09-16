#!/usr/bin/env bash
# Issue #3661: keep an attached TUI responsive while its daemon is restarted.
#
# The scenario uses an isolated AGEND_HOME and a real PTY. It starts a daemon,
# attaches the app, kills only that daemon, measures the outage, starts a new
# daemon, and verifies the existing app reconnects before cleanup.
set -euo pipefail

cd "$(dirname "$0")/../.."

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
info()  { printf '\033[36m[check]\033[0m %s\n' "$*"; }
fail()  {
    red "FAIL: $*"
    find "$TEST_HOME" -maxdepth 1 -type f -name 'app*' -print -exec tail -20 {} \; 2>/dev/null || true
    exit 1
}

TEST_HOME="$(mktemp -d -t agend-event-reconnect-XXXXXX)"
BIN="${AGEND_TERMINAL_BIN:-$PWD/target/debug/agend-terminal}"
APP_PID=""
DAEMON_PID=""
WRAPPER_PID=""

cleanup() {
    if [[ -n "$APP_PID" ]] && kill -0 "$APP_PID" 2>/dev/null; then
        kill -TERM "$APP_PID" 2>/dev/null || true
    fi
    if [[ -n "$WRAPPER_PID" ]] && kill -0 "$WRAPPER_PID" 2>/dev/null; then
        kill -TERM "$WRAPPER_PID" 2>/dev/null || true
    fi
    AGEND_HOME="$TEST_HOME" "$BIN" stop >/dev/null 2>&1 || true
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -KILL "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$TEST_HOME"
}
trap cleanup EXIT INT TERM

[[ -x "$BIN" ]] || fail "binary not found at $BIN (run cargo build first)"
command -v python3 >/dev/null || fail "python3 is required for the PTY harness"

cat > "$TEST_HOME/fleet.yaml" <<'YAML'
defaults:
  command: /bin/cat
instances:
  alpha: {}
YAML

wait_for_run_dir() {
    local tries=60
    while (( tries-- > 0 )); do
        if compgen -G "$TEST_HOME/run/*/.daemon" >/dev/null; then
            return 0
        fi
        sleep 0.25
    done
    return 1
}

get_daemon_pid() {
    local daemon_file
    local daemon_pid
    daemon_file="$(find "$TEST_HOME/run" -name .daemon -type f 2>/dev/null | head -1 || true)"
    [[ -n "$daemon_file" ]] || return 1
    daemon_pid="$(tr -d '[:space:]' < "$daemon_file" | cut -d: -f1)"
    [[ "$daemon_pid" =~ ^[0-9]+$ ]] || return 1
    echo "$daemon_pid"
}

wait_for_live_list() {
    local tries=40
    local list
    while (( tries-- > 0 )); do
        list="$(AGEND_HOME="$TEST_HOME" "$BIN" list --json 2>/dev/null || true)"
        if grep -q '"mode"[[:space:]]*:[[:space:]]*"live"' <<<"$list"; then
            return 0
        fi
        sleep 0.25
    done
    return 1
}

wait_for_pid() {
    local pid="$1"
    local tries=20
    while (( tries-- > 0 )); do
        kill -0 "$pid" 2>/dev/null && return 0
        sleep 0.25
    done
    return 1
}

info "start isolated daemon"
AGEND_HOME="$TEST_HOME" "$BIN" start >/dev/null
wait_for_run_dir || fail "daemon run directory did not appear"
DAEMON_PID="$(get_daemon_pid)" || fail "could not read daemon pid"
wait_for_pid "$DAEMON_PID" || fail "daemon $DAEMON_PID is not alive"
wait_for_live_list || fail "initial daemon did not reach live list mode"
green "  daemon live (pid $DAEMON_PID)"

info "start attached app in a real PTY"
python3 - "$BIN" "$TEST_HOME" > "$TEST_HOME/app-pid.out" 2> "$TEST_HOME/app-pty.err" <<'PY' &
import fcntl
import os
import pty
import select
import struct
import sys
import termios
import time

bin_path, test_home = sys.argv[1:]
pid, fd = pty.fork()
if pid == 0:
    os.environ["AGEND_HOME"] = test_home
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    os.execvp(bin_path, [bin_path, "app"])
print(pid, flush=True)
deadline = time.time() + 20
while time.time() < deadline:
    ready, _, _ = select.select([fd], [], [], 0.1)
    if ready:
        try:
            os.read(fd, 65536)
        except OSError:
            pass
    try:
        waited, _ = os.waitpid(pid, os.WNOHANG)
    except ChildProcessError:
        break
    if waited == pid:
        break
    time.sleep(0.1)
try:
    os.kill(pid, 15)
except ProcessLookupError:
    pass
try:
    os.waitpid(pid, 0)
except ChildProcessError:
    pass
os.close(fd)
PY
WRAPPER_PID="$!"
for _ in $(seq 1 30); do
    [[ -s "$TEST_HOME/app-pid.out" ]] && break
    sleep 0.1
done
APP_PID="$(head -1 "$TEST_HOME/app-pid.out" | grep -E '^[0-9]+$' || true)"
[[ -n "$APP_PID" ]] || fail "PTY harness did not report app pid"
sleep 5
kill -0 "$APP_PID" 2>/dev/null || fail "app exited before daemon outage"

info "kill only the attached daemon and sample the outage"
kill -KILL "$DAEMON_PID"
PEAK_CPU="0"
SAMPLE_END=$((SECONDS + 3))
while (( SECONDS < SAMPLE_END )); do
    cpu="$(ps -p "$APP_PID" -o %cpu= 2>/dev/null | tr -d ' ' || true)"
    if [[ "$cpu" =~ ^[0-9]+([.][0-9]+)?$ ]] && awk "BEGIN {exit !($cpu > $PEAK_CPU)}"; then
        PEAK_CPU="$cpu"
    fi
    sleep 0.25
done
kill -0 "$APP_PID" 2>/dev/null || fail "existing app exited during daemon outage"

info "start successor daemon without relaunching the app"
AGEND_HOME="$TEST_HOME" "$BIN" start >/dev/null
wait_for_run_dir || fail "successor daemon run directory did not appear"
SUCCESSOR_PID="$(get_daemon_pid)" || fail "could not read successor daemon pid"
[[ "$SUCCESSOR_PID" != "$DAEMON_PID" ]] || fail "daemon pid was not replaced"
wait_for_pid "$SUCCESSOR_PID" || fail "successor daemon $SUCCESSOR_PID is not alive"
wait_for_live_list || fail "successor daemon did not reach live list mode"

for _ in $(seq 1 40); do
    connected="$(grep -h -c 'daemon event stream connected' "$TEST_HOME"/app.*.log 2>/dev/null || true)"
    if (( connected >= 2 )); then
        break
    fi
    sleep 0.25
done
connected="$(grep -h -c 'daemon event stream connected' "$TEST_HOME"/app.*.log 2>/dev/null || true)"
[[ "$connected" -ge 2 ]] || fail "existing app did not reconnect (connected transitions: $connected)"
kill -0 "$APP_PID" 2>/dev/null || fail "existing app exited after successor startup"

warnings="$(grep -h -c 'daemon event stream unavailable' "$TEST_HOME"/app.*.log 2>/dev/null || true)"
[[ "$warnings" -le 3 ]] || fail "event disconnect warnings were not bounded: $warnings"
if grep -qi 'panic' "$TEST_HOME"/app.*.log 2>/dev/null; then
    fail "app panicked during reconnect smoke"
fi

green "  app stayed alive; connected transitions=$connected; outage warnings=$warnings; peak cpu=${PEAK_CPU}%"
echo "runtime evidence: old_pid=$DAEMON_PID successor_pid=$SUCCESSOR_PID connected=$connected warnings=$warnings peak_cpu=${PEAK_CPU}%"
