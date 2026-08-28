#!/usr/bin/env bash
# tool-cli Phase 0b eval harness — isolated sandbox lifecycle (SPEC.txt §2).
#
# One sandbox = one AGEND_HOME + one daemon + one seeded scenario, living
# entirely under $TMPDIR.  The user's real ~/.agend-terminal is never read or
# written: every child process gets PATH=DIR/bin:$PATH, AGEND_HOME=DIR/home,
# AGEND_INSTANCE_NAME=<identity> and NO other AGEND_* variable.
#
#   sandbox.sh up   DIR SCENARIO ARM PAIR
#   sandbox.sh seed DIR SCENARIO PAIR
#   sandbox.sh down DIR
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"

# ── isolation ────────────────────────────────────────────────────────────────
# The operator's real AGEND home ships shims in its bin/ (notably `git`, which
# needs AGEND_REAL_GIT to work).  Drop that directory from PATH *before* the
# AGEND_* scrub, so neither this script nor any sandbox child can reach into
# ~/.agend-terminal through PATH.
_real_home="${AGEND_HOME:-$HOME/.agend-terminal}"
PATH="$(_RH="$_real_home" python3 -c '
import os
rh = os.environ["_RH"]
bad = {os.path.realpath(rh), os.path.realpath(os.path.join(rh, "bin"))}
print(":".join(p for p in os.environ["PATH"].split(":")
               if p and os.path.realpath(p) not in bad))
')"
export PATH
unset _real_home
# Drop every inherited AGEND_* before we set our own.
while read -r _v; do [ -n "$_v" ] && unset "$_v"; done < <(
  env | sed -n 's/^\(AGEND_[A-Za-z0-9_]*\)=.*/\1/p'
)

die() { echo "sandbox.sh: $*" >&2; exit 1; }

realdir() { python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$1"; }

# Refuse to operate anywhere but under $TMPDIR (mirrors the hermetic_home
# assertion in tests/tool_cli_phase0a_real_red.rs).
assert_under_tmp() {
  local dir="$1" tmp
  tmp="$(realdir "${TMPDIR:-/tmp}")"
  local real
  real="$(realdir "$dir")"
  case "$real/" in
    "$tmp"/*) : ;;
    *) die "refusing to use '$dir': sandboxes must live under \$TMPDIR ($tmp)" ;;
  esac
  case "$real" in
    */.agend-terminal|*/.agend-terminal/*) die "refusing to touch a real AGEND_HOME: $real" ;;
  esac
}

FLEET_YAML='instances:
  probe:
    command: /bin/cat
    role_kind: implementer
  ane-review:
    command: /bin/cat
    role_kind: reviewer
  lead:
    command: /bin/cat
    role_kind: orchestrator
'
RUNTIME_CONFIG='{"schema_version":1,"experimental":{"tool_cli_enabled":true}}'

# ── up ───────────────────────────────────────────────────────────────────────
cmd_up() {
  local dir="$1" scenario="$2" arm="$3" pair="$4"
  case "$arm" in mcp|cli) : ;; *) die "arm must be mcp or cli, got '$arm'" ;; esac
  [ -d "$HERE/scenarios/$scenario" ] || die "unknown scenario: $scenario"
  mkdir -p "$dir"
  assert_under_tmp "$dir"

  mkdir -p "$dir/home" "$dir/cwd" "$dir/bin"
  # Fence ON in both arms (SPEC §2) — written BEFORE the daemon boots.
  printf '%s\n' "$RUNTIME_CONFIG" > "$dir/home/runtime-config.json"
  printf '%s' "$FLEET_YAML" > "$dir/home/fleet.yaml"
  # The daemon rewrites home/fleet.yaml on boot (it stamps generated instance
  # ids), so keep the exact template as the reproducibility pin.
  printf '%s' "$FLEET_YAML" > "$dir/fleet.template.yaml"

  for bin in agend-terminal agend-mcp-bridge; do
    [ -x "$REPO/target/release/$bin" ] || die "missing release binary: target/release/$bin"
    ln -sf "$REPO/target/release/$bin" "$dir/bin/$bin"
  done

  printf '%s' "$arm" > "$dir/arm"
  printf '%s' "$pair" > "$dir/pair"
  printf '%s' "$scenario" > "$dir/scenario"

  start_daemon "$dir"
  cmd_seed "$dir" "$scenario" "$pair"
}

start_daemon() {
  local dir="$1"
  local pid
  # setsid/setpgrp so `down` can take the whole daemon process group with it
  # (the boot-spawned /bin/cat stubs are children of the daemon).
  if command -v setsid >/dev/null 2>&1; then
    env PATH="$dir/bin:$PATH" AGEND_HOME="$dir/home" \
        setsid "$dir/bin/agend-terminal" start --foreground \
        >"$dir/daemon.log" 2>&1 &
    pid=$!
  else
    env PATH="$dir/bin:$PATH" AGEND_HOME="$dir/home" \
        perl -e 'setpgrp(0,0); exec @ARGV or die' \
        "$dir/bin/agend-terminal" start --foreground \
        >"$dir/daemon.log" 2>&1 &
    pid=$!
  fi
  printf '%s' "$pid" > "$dir/daemon.pid"

  # Ready = run/<pid>/api.port AND run/<pid>/.ready (tests/common/harness.rs).
  local run_dir="$dir/home/run/$pid" waited=0
  while [ "$waited" -lt 200 ]; do
    if [ -f "$run_dir/api.port" ] && [ -f "$run_dir/.ready" ]; then
      cp "$run_dir/api.port" "$dir/api.port"
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "--- daemon.log ---" >&2; tail -30 "$dir/daemon.log" >&2 || true
      die "daemon exited before becoming ready (pid $pid)"
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  echo "--- daemon.log ---" >&2; tail -30 "$dir/daemon.log" >&2 || true
  die "daemon startup timeout (20s) — $run_dir/{api.port,.ready} never appeared"
}

# ── seed ─────────────────────────────────────────────────────────────────────
# Seeding is not graded; seed.sh prints the ids it created as JSON on stdout.
cmd_seed() {
  local dir="$1" scenario="$2" pair="$3"
  local seed="$HERE/scenarios/$scenario/seed.sh"
  [ -x "$seed" ] || die "scenario $scenario has no executable seed.sh"
  env PATH="$dir/bin:$PATH" AGEND_HOME="$dir/home" \
      "$seed" "$dir/home" "$pair" > "$dir/seed.json" 2>"$dir/seed.err" \
    || { echo "--- seed.err ---" >&2; cat "$dir/seed.err" >&2; die "seed.sh failed"; }
}

# ── down ─────────────────────────────────────────────────────────────────────
# Collect final state FIRST, then stop the daemon.  Nothing is deleted here;
# sandbox dirs survive until `matrix.sh --clean`.
cmd_down() {
  local dir="$1"
  assert_under_tmp "$dir"
  collect_final_state "$dir"
  local pid=""
  [ -f "$dir/daemon.pid" ] && pid="$(cat "$dir/daemon.pid")"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    kill -TERM "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
    local waited=0
    while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 50 ]; do
      sleep 0.1; waited=$((waited + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
      kill -KILL "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
  fi
  # daemon.log keeps growing until the daemon is gone — refresh the copy.
  cp "$dir/daemon.log" "$dir/final_state/daemon.log" 2>/dev/null || true
}

collect_final_state() {
  local dir="$1" home="$dir/home" out="$dir/final_state"
  mkdir -p "$out"
  if [ -d "$home/inbox" ]; then
    mkdir -p "$out/inbox"
    find "$home/inbox" -maxdepth 1 -name '*.jsonl' -exec cp {} "$out/inbox/" \; 2>/dev/null || true
  fi
  # SPEC §3 list, plus the names this daemon build actually uses
  # (fleet_events.jsonl is written as event-log.jsonl; the daemon's own log is
  # home/daemon.<date>.log, so DIR/daemon.log only carries pre-logger output).
  for f in task_events.jsonl task_index.jsonl sent_ledger.jsonl fleet_events.jsonl \
           event-log.jsonl mcp-usage-stats.jsonl runtime-config.json fleet.yaml \
           snapshot.json; do
    [ -e "$home/$f" ] && cp "$home/$f" "$out/$f"
  done
  find "$home" -maxdepth 1 -name 'daemon.*.log' -exec cp {} "$out/" \; 2>/dev/null || true
  [ -e "$home/state/tool_call_observations.json" ] &&
    cp "$home/state/tool_call_observations.json" "$out/tool_call_observations.json"
  [ -d "$home/decisions" ] && { rm -rf "$out/decisions"; cp -R "$home/decisions" "$out/decisions"; }
  [ -e "$dir/seed.json" ] && cp "$dir/seed.json" "$out/seed.json"
  [ -e "$dir/fleet.template.yaml" ] && cp "$dir/fleet.template.yaml" "$out/fleet.template.yaml"
  cp "$dir/daemon.log" "$out/daemon.log" 2>/dev/null || true
  return 0
}

# ── dispatch ─────────────────────────────────────────────────────────────────
case "${1:-}" in
  up)   [ $# -eq 5 ] || die "usage: sandbox.sh up DIR SCENARIO ARM PAIR"; cmd_up   "$2" "$3" "$4" "$5" ;;
  seed) [ $# -eq 4 ] || die "usage: sandbox.sh seed DIR SCENARIO PAIR";   cmd_seed "$2" "$3" "$4" ;;
  down) [ $# -eq 2 ] || die "usage: sandbox.sh down DIR";                 cmd_down "$2" ;;
  *) die "usage: sandbox.sh {up DIR SCENARIO ARM PAIR|seed DIR SCENARIO PAIR|down DIR}" ;;
esac
