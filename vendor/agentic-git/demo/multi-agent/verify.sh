#!/usr/bin/env bash
#
# verify.sh — Layer 1 driver for the multi-agent scenario. The SUPERVISOR owns
# setup and synthesis (both in lib.sh); here it spawns the two guarded agent
# sessions ITSELF, in-process and concurrently, against one shared repo. Then it
# re-derives the truth from git/home/audit STATE (never from an agent's own
# verdict) via the shared synthesize(). An agent that reports PASS but whose
# state disagrees still FAILS.
#
#   ./verify.sh            # run it, print the synthesis
#   ./verify.sh --keep     # keep the throwaway world for inspection
#
# For the SAME scenario driven by two real agents (each running its own guarded
# session), see live.sh — it reuses lib.sh's build_world + synthesize verbatim.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source-path=SCRIPTDIR
# shellcheck source=lib.sh
. "$HERE/lib.sh"

KEEP=0; [ "${1:-}" = "--keep" ] && KEEP=1
scrub_hostile_env
REAL_GIT="$(resolve_real_git)" || die "no real (non-shim) git on PATH"
BIN="$(resolve_bin)" || die "agentic-git not found — build from the repo, or set AGENTIC_GIT_BIN"

# ── the shared world (lib.sh owns setup) ─────────────────────────────────────
work="$(mktemp -d)"; [ "$KEEP" = 1 ] || trap 'rm -rf "$work"' EXIT
build_world "$work"   # sets globals: project canonical bare home arts PROJECT_BASE CANON_BASE RUN_ID

spawn_agent() {  # spawn_agent <role> <artifact-dir>
  local role="$1" art="$2" other
  [ "$role" = a ] && other=b || other=a
  ( cd "$project" && \
    AGENTIC_GIT_HOME="$home" AGENTIC_GIT_REAL_GIT="$REAL_GIT" AGENTIC_GIT_BIN="$BIN" \
    PATH="$(sanitized_path)" \
    "$BIN" run --agent "agent-$role" --branch "feat/$role" -- \
      sh "$HERE/agent-run.sh" "agent-$role" "feat/$role" "feat/$other" "$art" "$canonical" \
  ) >"$art/session.log" 2>&1
}

say "Launching agent-a and agent-b CONCURRENTLY against one shared repo…"
spawn_agent a "$arts/a" &  pa=$!
spawn_agent b "$arts/b" &  pb=$!
wait "$pa"; ra=$?
wait "$pb"; rb=$?
[ "$ra" = 0 ] || printf '  (agent-a session exit %s — see %s)\n' "$ra" "$arts/a/session.log"
[ "$rb" = 0 ] || printf '  (agent-b session exit %s — see %s)\n' "$rb" "$arts/b/session.log"

# ── synthesis (lib.sh owns it — same function live.sh calls) ─────────────────
synthesize "$work"; rc=$?
[ "$KEEP" = 1 ] && printf '\n  world kept at: %s\n' "$work"
exit "$rc"
