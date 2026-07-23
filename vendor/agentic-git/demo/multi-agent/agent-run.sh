#!/usr/bin/env sh
#
# agent-run.sh — what ONE agent executes inside its own guarded session (git =
# the shim). It runs a fixed set of commands, saves every command's raw
# stdout/stderr/exit code plus a machine-recomputable evidence snapshot into its
# artifact dir, and writes a self-verdict. Per the design (fugu review), the
# agent's verdict is MATERIAL to be judged — the supervisor's synthesize step
# re-derives the truth independently from git/home/audit state.
#
# Args: <agent> <my-branch> <other-branch> <artifact-dir> <canonical-checkout>
# Env : AGENTIC_GIT_HOME, AGENTIC_GIT_BIN are inherited from the guarded session.
set -u
agent="$1"; mine="$2"; other="$3"; art="$4"; canonical="$5"
mkdir -p "$art"

# ── machine-recomputable snapshot (the supervisor recomputes/compares these) ──
{
  echo "AGENT=$agent"
  echo "BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)"
  echo "HEAD=$(git rev-parse HEAD 2>/dev/null)"
  echo "GITBIN=$(command -v git)"
  echo "HOOKSPATH=$(git config --get core.hooksPath 2>/dev/null)"
  echo "WORKTREE=$(pwd -P)"
} > "$art/evidence.env"
git status --porcelain=v1 -z 2>/dev/null | base64 > "$art/status.b64"

# ── capture a numbered command: cap <n> <ok|deny> <label> -- <cmd...> ─────────
cap() {
  n="$1"; expect="$2"; label="$3"; shift 3; [ "$1" = "--" ] && shift
  "$@" > "$art/$n.out" 2> "$art/$n.err"; rc=$?
  echo "$rc" > "$art/$n.rc"; printf '%s\n' "$label" > "$art/$n.cmd"
  printf '%s\n' "$expect" > "$art/$n.expect"
}

# do real work first (prep, not a graded step)
printf 'def solve_%s():\n    return 42  # %s hard-won fix\n' "$agent" "$agent" > "solver_$agent.py"
git add "solver_$agent.py" 2>/dev/null

# ── the graded steps ─────────────────────────────────────────────────────────
cap 1 ok   "isolation: on my own branch"        -- test "$(git rev-parse --abbrev-ref HEAD)" = "$mine"
cap 2 ok   "isolation: git is the shim"          -- sh -c 'case "$(command -v git)" in */bin/git) exit 0;; *) exit 1;; esac'
cap 3 ok   "commit my own work"                  -- git commit -q -m "$agent: work"
cap 4 ok   "provenance trailer names me"         -- sh -c "git log -1 --format=%B | grep -q 'Agentic-Agent: $agent'"
cap 5 ok   "push my own branch"                  -- git push -u origin "$mine"
cap 6 deny "cross-branch checkout of $other"     -- git checkout "$other"
cap 7 deny "force-push over $other (clobber)"    -- git push origin "+HEAD:$other"
cap 8 deny "delete $other on the remote"         -- git push origin ":$other"
cap 9 ok   "the other agent's file is NOT here"  -- sh -c "! test -f solver_${other#feat/}.py"
cap 10 deny "touch the other agent's checkout"   -- sh -c "cd '$canonical' && git checkout HEAD~1"

# ── self-verdict (MATERIAL, not proof — synthesize re-derives) ───────────────
verdict=PASS
for n in 1 2 3 4 5 6 7 8 9 10; do
  exp=$(cat "$art/$n.expect"); rc=$(cat "$art/$n.rc")
  case "$exp" in
    ok)   [ "$rc" = 0 ]  || { verdict=FAIL; echo "step $n expected ok, rc=$rc"   >> "$art/why.txt"; } ;;
    deny) [ "$rc" != 0 ] || { verdict=FAIL; echo "step $n expected deny, rc=$rc" >> "$art/why.txt"; } ;;
  esac
done
echo "$verdict" > "$art/verdict.txt"
