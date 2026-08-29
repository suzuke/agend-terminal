#!/usr/bin/env bash
# tool-cli Phase 0b eval harness — one run (SPEC.txt §3).
#
#   run.sh --arm mcp|cli --scenario Sxx --pair N --out DIR [--model M]
#          [--sandbox DIR] [--keep-sandbox] [--timeout SECS]
#
# Boots a fresh isolated sandbox, runs one real `claude -p` against it, and
# writes stream.jsonl / stderr.txt / metadata.json / final_state/ into --out.
# Exit 0 even when the agent failed the task; non-zero only for harness faults.
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

die() { echo "run.sh: $*" >&2; exit 2; }

ARM=""; SCENARIO=""; PAIR=""; OUT=""; SANDBOX=""; KEEP=0
# MAX_TURNS is the budget SPEC.txt:52 pins for the frozen runner command; the
# grader refuses a run recorded with any other value (max_turns_not_frozen).
MODEL="claude-fable-5"; MAX_TURNS=15; TIMEOUT_SECS=900
while [ $# -gt 0 ]; do
  case "$1" in
    --arm) ARM="$2"; shift 2 ;;
    --scenario) SCENARIO="$2"; shift 2 ;;
    --pair) PAIR="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --model) MODEL="$2"; shift 2 ;;
    --sandbox) SANDBOX="$2"; shift 2 ;;
    --max-turns) MAX_TURNS="$2"; shift 2 ;;
    --timeout) TIMEOUT_SECS="$2"; shift 2 ;;
    --keep-sandbox) KEEP=1; shift ;;
    *) die "unknown option: $1" ;;
  esac
done
[ -n "$ARM" ] && [ -n "$SCENARIO" ] && [ -n "$PAIR" ] && [ -n "$OUT" ] ||
  die "usage: run.sh --arm mcp|cli --scenario Sxx --pair N --out DIR"
case "$ARM" in mcp|cli) : ;; *) die "arm must be mcp or cli" ;; esac
[ -d "$HERE/scenarios/$SCENARIO" ] || die "unknown scenario: $SCENARIO"
command -v claude >/dev/null 2>&1 || die "claude CLI not found on PATH"

GIT_HEAD="$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)"
[ -n "$GIT_HEAD" ] || GIT_HEAD=unknown

now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }
now_iso() { python3 -c 'import datetime; print(datetime.datetime.now(datetime.timezone.utc).isoformat())'; }

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

if [ -z "$SANDBOX" ]; then
  SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/agend-eval-0b-${SCENARIO}-${ARM}-${PAIR}-XXXXXX")"
fi
CLEANED=0
cleanup() {
  if [ "$CLEANED" -eq 0 ]; then
    CLEANED=1
    "$HERE/sandbox.sh" down "$SANDBOX" >/dev/null 2>&1 || true
    rm -rf "$OUT/final_state"
    cp -R "$SANDBOX/final_state" "$OUT/final_state" 2>/dev/null || true
    # The grader reads the seed ids straight from OUT/seed.json.
    cp "$SANDBOX/seed.json" "$OUT/seed.json" 2>/dev/null || true
  fi
  return 0
}
trap cleanup EXIT

"$HERE/sandbox.sh" up "$SANDBOX" "$SCENARIO" "$ARM" "$PAIR"

# ── arm-specific invocation surface ─────────────────────────────────────────
# NOTE: `--tools` only filters Claude Code's BUILT-IN tools; an `mcp__*` glob is
# accepted but inert.  What actually decides the surface is --mcp-config plus
# --strict-mcp-config: the cli arm gets an empty server map, so no mcp__ tool
# can exist.  Bash stays available in BOTH arms on purpose (S13).
if [ "$ARM" = "mcp" ]; then
  python3 - "$SANDBOX" > "$SANDBOX/mcp.json" <<'PY'
import json, sys
d = sys.argv[1]
print(json.dumps({"mcpServers": {"agend-terminal": {
    "command": f"{d}/bin/agend-mcp-bridge",
    "args": [],
    "env": {"AGEND_HOME": f"{d}/home", "AGEND_INSTANCE_NAME": "probe"},
}}}))
PY
  TOOLS="Bash,Read,mcp__agend-terminal__*"
else
  echo '{"mcpServers":{}}' > "$SANDBOX/mcp.json"
  TOOLS="Bash,Read"
fi

cat "$HERE/prompts/base.txt" > "$SANDBOX/system.txt"
printf '\n\n' >> "$SANDBOX/system.txt"
cat "$HERE/prompts/$ARM.txt" >> "$SANDBOX/system.txt"

PROMPT_FILE="$HERE/scenarios/$SCENARIO/prompt.txt"

# order_in_pair: odd pair -> mcp first, even pair -> cli first (SPEC §6).
if [ $((PAIR % 2)) -eq 1 ]; then
  [ "$ARM" = "mcp" ] && ORDER=first || ORDER=second
else
  [ "$ARM" = "cli" ] && ORDER=first || ORDER=second
fi

# ── the run ─────────────────────────────────────────────────────────────────
STARTED_AT="$(now_iso)"; T0="$(now_ms)"
TIMED_OUT=false
set +e
(
  cd "$SANDBOX/cwd" &&
  exec env PATH="$SANDBOX/bin:$PATH" \
      AGEND_HOME="$SANDBOX/home" \
      AGEND_INSTANCE_NAME=probe \
      claude -p "$(cat "$PROMPT_FILE")" \
        --output-format stream-json --verbose \
        --no-session-persistence \
        --max-turns "$MAX_TURNS" \
        --model "$MODEL" \
        --permission-mode bypassPermissions \
        --setting-sources project \
        --strict-mcp-config \
        --mcp-config "$SANDBOX/mcp.json" \
        --append-system-prompt-file "$SANDBOX/system.txt" \
        --tools "$TOOLS" \
    < /dev/null > "$OUT/stream.jsonl" 2> "$OUT/stderr.txt"
) &
CLAUDE_PID=$!
( exec >/dev/null 2>&1 </dev/null; sleep "$TIMEOUT_SECS"; kill -TERM "$CLAUDE_PID" 2>/dev/null ) & WATCHDOG=$!
wait "$CLAUDE_PID"; EXIT_CODE=$?
# kill the watchdog subshell AND its sleep child (an orphaned sleep would hold fds for TIMEOUT_SECS)
pkill -P "$WATCHDOG" 2>/dev/null || true; kill "$WATCHDOG" 2>/dev/null || true; wait "$WATCHDOG" 2>/dev/null || true
set -e
[ "$EXIT_CODE" -ge 128 ] && TIMED_OUT=true
T1="$(now_ms)"; ENDED_AT="$(now_iso)"

cleanup

# ── metadata ────────────────────────────────────────────────────────────────
EV_OUT="$OUT" EV_SANDBOX="$SANDBOX" EV_SCENARIO="$SCENARIO" EV_ARM="$ARM" \
EV_PAIR="$PAIR" EV_ORDER="$ORDER" EV_MODEL="$MODEL" EV_EXIT="$EXIT_CODE" EV_MAX_TURNS="$MAX_TURNS" EV_TIMEOUT_SECS="$TIMEOUT_SECS" \
EV_TIMED_OUT="$TIMED_OUT" EV_STARTED="$STARTED_AT" EV_ENDED="$ENDED_AT" \
EV_T0="$T0" EV_T1="$T1" EV_HERE="$HERE" EV_REPO="$REPO" \
EV_GIT_HEAD="$GIT_HEAD" \
python3 - > "$OUT/metadata.json" <<'PY'
import hashlib, json, os, subprocess, sys

E = os.environ
out, sandbox, scenario, arm, pair, order, model = (
    E["EV_OUT"], E["EV_SANDBOX"], E["EV_SCENARIO"], E["EV_ARM"],
    E["EV_PAIR"], E["EV_ORDER"], E["EV_MODEL"])
exit_code, timed_out = E["EV_EXIT"], E["EV_TIMED_OUT"]
started, ended, t0, t1 = E["EV_STARTED"], E["EV_ENDED"], E["EV_T0"], E["EV_T1"]
here, repo = E["EV_HERE"], E["EV_REPO"]

def sha(p):
    try:
        with open(p, "rb") as f:
            return hashlib.sha256(f.read()).hexdigest()
    except OSError:
        return None

events, model_resolved, turns, invalid = [], None, 0, None
result_subtype, num_turns, stop_reason, is_error = None, None, None, None
try:
    with open(os.path.join(out, "stream.jsonl"), encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            events.append(ev)
            if ev.get("type") == "system" and ev.get("subtype") == "init":
                model_resolved = ev.get("model")
            if ev.get("type") == "assistant":
                turns += 1
            if ev.get("type") == "result":
                result_subtype = ev.get("subtype")
                num_turns = ev.get("num_turns")
                stop_reason = ev.get("stop_reason")
                is_error = ev.get("is_error")
except OSError:
    pass

claude_version = None
for ev in events:
    if ev.get("type") == "system" and ev.get("subtype") == "init":
        claude_version = ev.get("claude_code_version")
        break
if claude_version is None:
    try:
        claude_version = subprocess.run(
            ["claude", "--version"], capture_output=True, text=True, timeout=30
        ).stdout.strip() or None
    except Exception:
        claude_version = None

git_head = E["EV_GIT_HEAD"]

if not events:
    invalid = "no stream events (harness fault)"
elif model_resolved is None:
    invalid = "no system/init event — resolved model unknown"
elif model_resolved != model:
    invalid = f"model mismatch: requested {model}, resolved {model_resolved}"
elif timed_out == "true":
    invalid = "run timed out"

json.dump({
    "schema": 1,
    "scenario": scenario,
    "arm": arm,
    "pair": int(pair),
    "order_in_pair": order,
    "model_requested": model,
    "model_resolved": model_resolved,
    "claude_version": claude_version,
    "git_head": git_head,
    "binary_sha256": {
        "agend-terminal": sha(os.path.join(repo, "target/release/agend-terminal")),
        "agend-mcp-bridge": sha(os.path.join(repo, "target/release/agend-mcp-bridge")),
    },
    "system_prompt_sha256": sha(os.path.join(sandbox, "system.txt")),
    "prompt_sha256": sha(os.path.join(here, "scenarios", scenario, "prompt.txt")),
    "fence": True,
    "fleet_sha256": sha(os.path.join(out, "final_state", "fleet.template.yaml")),
    "seed_sha256": sha(os.path.join(here, "scenarios", scenario, "seed.sh")),
    "started_at": started,
    "ended_at": ended,
    "duration_ms": int(t1) - int(t0),
    "exit_code": int(exit_code),
    "turns": turns,
    "max_turns": int(E.get("EV_MAX_TURNS", "0")) or None,
    "timeout_secs": int(E.get("EV_TIMEOUT_SECS", "0")) or None,
    "result_subtype": result_subtype,
    "num_turns": num_turns,
    "stop_reason": stop_reason,
    "is_error": is_error,
    "hit_max_turns": (result_subtype == "error_max_turns"),
    "timed_out": timed_out == "true",
    "invalid_reason": invalid,
}, sys.stdout, indent=2, sort_keys=True)
print()
PY

if [ "$KEEP" -eq 0 ]; then rm -rf "$SANDBOX"; else echo "sandbox kept: $SANDBOX" >&2; fi
exit 0
