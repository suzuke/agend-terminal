#!/usr/bin/env sh
#
# agent-launch.sh — what ONE real agent runs to execute its guarded role against
# a live.sh world (Layer 2).  Usage: sh agent-launch.sh <a|b>
#
# Self-contained on purpose: it runs inside whatever (possibly hostile) session
# the agent has, so it scrubs its OWN environment, resolves a real non-shim git,
# and trusts NOTHING from the ambient env — only the supervisor-written
# world.env beside it. It then hands off to the same agent-run.sh verify.sh uses.
#
# shellcheck disable=SC2154  # arts/project/home/canonical/BIN come from world.env
set -u
role="${1:?usage: agent-launch.sh <a|b>}"
case "$role" in a) other=b ;; b) other=a ;; *) echo "role must be 'a' or 'b'" >&2; exit 2 ;; esac
WORLD="$(cd "$(dirname "$0")" && pwd)"
[ -f "$WORLD/world.env" ] || { echo "LAUNCH-FAIL: no world.env in $WORLD (run 'live.sh setup' first)" >&2; exit 2; }
# shellcheck disable=SC1091
. "$WORLD/world.env"
[ "${STATE:-}" = ready ] || { echo "LAUNCH-FAIL: world not ready (STATE=${STATE:-})" >&2; exit 2; }

# ── scrub the hostile ambient environment (mirrors lib.sh scrub_hostile_env) ──
# Strip everything that could redirect/reconfigure git despite `-C`, plus every
# agentic-git/agend bypass/agent/home/allow knob. HOME/REAL_GIT/BIN are set by us
# at the run site below, so a blanket unset here is safe.
for v in $(env 2>/dev/null | sed -n 's/^\(AGENTIC_GIT_[A-Za-z0-9_]*\)=.*/\1/p; s/^\(AGEND_GIT_[A-Za-z0-9_]*\)=.*/\1/p'); do
  [ "$v" = AGENTIC_GIT_BIN ] && continue   # deliberate binary override (mirrors lib.sh scrub_hostile_env)
  unset "$v" 2>/dev/null || true
done
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES \
      GIT_COMMON_DIR GIT_NAMESPACE GIT_CEILING_DIRECTORIES GIT_PREFIX \
      GIT_CONFIG GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_NOSYSTEM \
      GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS 2>/dev/null || true

resolve_real_git() { d=; IFS=:; for d in $PATH; do case "$d" in *.agentic-git*|*.agend-terminal*|*/.cargo/bin*) continue ;; esac; [ -x "$d/git" ] && { printf '%s\n' "$d/git"; return 0; }; done; return 1; }
REAL_GIT="$(resolve_real_git)" || { echo "LAUNCH-FAIL: no real (non-shim) git on PATH" >&2; exit 3; }
sanitized_path() { d=; IFS=:; out="$(dirname "$REAL_GIT")"; for d in $PATH; do case "$d" in *.agentic-git*|*.agend-terminal*) continue ;; esac; out="$out:$d"; done; printf '%s\n' "$out"; }
[ -x "$BIN" ] || { echo "LAUNCH-FAIL: guarded binary missing: $BIN" >&2; exit 3; }

art="$arts/$role"; mkdir -p "$art"
( cd "$project" && \
  AGENTIC_GIT_HOME="$home" AGENTIC_GIT_REAL_GIT="$REAL_GIT" AGENTIC_GIT_BIN="$BIN" \
  PATH="$(sanitized_path)" \
  "$BIN" run --agent "agent-$role" --branch "feat/$role" -- \
    sh "$WORLD/agent-run.sh" "agent-$role" "feat/$role" "feat/$other" "$art" "$canonical" \
) > "$art/session.log" 2>&1
rc=$?

gitbin="$(sed -n 's/^GITBIN=//p' "$art/evidence.env" 2>/dev/null)"
verdict="$(cat "$art/verdict.txt" 2>/dev/null)"
echo "LAYER2-RESULT role=agent-$role exit=$rc gitbin=$gitbin verdict=$verdict art=$art"
case "$gitbin" in
  */bin/git) echo "  proof: inner git was the shim ($gitbin)" ;;
  *)         echo "  WARN: inner git was NOT a shim ($gitbin)" ;;
esac
[ "$rc" = 0 ] || echo "  (session failed — see $art/session.log)"
exit "$rc"
