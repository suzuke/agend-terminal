#!/usr/bin/env bash
#
# recovery-demo.sh — the 60-second "an agent erased my work; one command put it
# back" story, as a REAL guarded session you can run or screen-record.
#
# It doubles as a cold-start ACCEPTANCE CHECK: every step is asserted, and it
# exits non-zero with a clear FAIL if the shipped binary doesn't actually
# recover the work. If it prints "DEMO PASSED", the tool does what the README
# promises on THIS machine — no git internals required from the viewer.
#
#   ./demo/recovery-demo.sh            # builds ./target/release/agentic-git
#   AGENTIC_GIT_BIN=$(command -v agentic-git) ./demo/recovery-demo.sh   # test an install
#
# The framing to keep in mind while watching:
#   a worktree isolates WHERE damage can happen; agentic-git records WHAT was
#   about to be lost — so a `reset --hard` / `clean -fd` is undoable, which
#   plain git cannot offer.

set -euo pipefail

# A demo of the guard must not itself run with the guard bypassed — clear any
# ambient bypass so the session genuinely routes, denies, and snapshots.
unset AGENTIC_GIT_BYPASS AGEND_GIT_BYPASS \
      AGENTIC_GIT_BYPASS_AGENT AGEND_GIT_BYPASS_AGENT \
      AGENTIC_GIT_BYPASS_UNTIL AGEND_GIT_BYPASS_UNTIL 2>/dev/null || true

say()  { printf '\n\033[1;36m▸ %s\033[0m\n' "$*"; }
run()  { printf '  \033[2m$ %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31m✗ FAIL: %s\033[0m\n' "$*" >&2; exit 1; }
ok()   { printf '  \033[1;32m✓ %s\033[0m\n' "$*"; }

# ── resolve the binary (default: build the release binary from this repo) ──
here="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${AGENTIC_GIT_BIN:-$here/target/release/agentic-git}"
if [ ! -x "$BIN" ]; then
  say "Building agentic-git (set AGENTIC_GIT_BIN to skip)"
  ( cd "$here" && cargo build --release -p agentic-git >/dev/null )
fi
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"   # absolutise

# Resolve a REAL git, skipping any git-shim wrapper already on PATH (this
# repo's own shim, or an outer fleet shim) — so the demo is robust even where
# `git` is itself wrapped. For a normal user with a plain PATH this is just the
# first `git` found.
resolve_real_git() {
  local d IFS=:
  for d in $PATH; do
    case "$d" in *.agentic-git*|*.agend-terminal*) continue ;; esac
    [ -x "$d/git" ] && { printf '%s\n' "$d/git"; return 0; }
  done
  return 1
}
# A PATH with shim dirs stripped, so the shim's own bare `git` spawns still
# reach the real binary. Real git's dir goes first.
sanitized_path() {
  local d IFS=: out; out="$(dirname "$REAL_GIT")"
  for d in $PATH; do
    case "$d" in *.agentic-git*|*.agend-terminal*) continue ;; esac
    out="$out:$d"
  done
  printf '%s\n' "$out"
}
REAL_GIT="$(resolve_real_git)" || fail "no real (non-shim) git on PATH"

say "agentic-git: $($BIN version)"
run "git: $REAL_GIT ($($REAL_GIT --version))"

# ── a throwaway project that stands in for YOUR repo ──────────────────────
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
repo="$work/project"; home="$work/agentic-home"; wtfile="$work/worktree_path"
mkdir -p "$repo"
export GIT_AUTHOR_NAME=you GIT_AUTHOR_EMAIL=you@example.com \
       GIT_COMMITTER_NAME=you GIT_COMMITTER_EMAIL=you@example.com
"$REAL_GIT" -C "$repo" init -q -b main
printf 'def solve():\n    pass  # TODO\n' > "$repo/solver.py"
"$REAL_GIT" -C "$repo" add -A && "$REAL_GIT" -C "$repo" commit -q -m "project baseline"
say "Your project has one file, committed:"
run "cat solver.py  →  def solve(): pass  # TODO"

# ── launch an AGENT inside a guarded session (ONE command) ────────────────
# `agentic-git run` provisions a worktree + signed binding + hooks and puts the
# shim on the agent's PATH — the agent just speaks the git it already knows.
say "Launch an agent in a guarded session — one command, no hand-wiring:"
run "agentic-git run --agent demo --branch demo/work -- <agent script>"

agent_script='
set -e
# PROOF the agent is actually going through the shim (cold-start check #1):
case "$(command -v git)" in
  *"/bin/git") echo "  [agent] git resolves to the guarded shim" ;;
  *) echo "  [agent] WARNING: git is NOT the shim: $(command -v git)" ;;
esac
pwd -P > "'"$wtfile"'"                       # tell the demo where the worktree is
# The agent does real, valuable work — but leaves it UNCOMMITTED:
printf "def solve():\n    return 42  # the hard-won fix\n" > solver.py
printf "the tricky part was the off-by-one at the boundary\n" > NOTES.md
# ...then makes a classic machine-speed mistake: wipes the slate to "start clean"
git reset --hard >/dev/null 2>&1             # erases the solver.py fix
rm -f NOTES.md                               # and the research note is gone too
echo "  [agent] reset --hard done — my uncommitted fix is gone, and so is the note"
'

( cd "$repo" && \
  AGENTIC_GIT_HOME="$home" \
  AGENTIC_GIT_REAL_GIT="$REAL_GIT" \
  PATH="$(sanitized_path)" \
    "$BIN" run --agent demo --branch demo/work -- sh -c "$agent_script" )

WT="$(cat "$wtfile")"
[ -d "$WT" ] || fail "could not locate the session worktree"

# ── the damage, in plain git ──────────────────────────────────────────────
say "In plain git, your work is simply gone — there is nothing to restore from:"
solver_now="$(cat "$WT/solver.py")"
case "$solver_now" in
  *"return 42"*) fail "reset --hard should have erased the fix, but it's still here" ;;
  *) ok "solver.py is back to the committed stub — the fix is gone" ;;
esac
if [ ! -f "$WT/NOTES.md" ]; then ok "NOTES.md (untracked) is gone too"; else fail "NOTES.md unexpectedly survived"; fi
# Be honest about what `git status` really shows in a session worktree: the
# ONLY untracked entry is the session's own `.agend-managed` marker — no trace
# of the erased work. No diff, no stash, no reflog for uncommitted changes.
real_changes="$("$REAL_GIT" -C "$WT" status --porcelain | grep -v '^?? \.agend-managed$' || true)"
run "git status --porcelain   →  ?? .agend-managed   (only the session marker — none of your work)"
if [ -z "$real_changes" ]; then
  ok "plain git has no record of the erased work to recover from"
else
  fail "unexpected residual changes after reset: $real_changes"
fi

# ── recovery: ONE command, no ref, no git internals ───────────────────────
say "Recover with one command — no snapshot ref, no git knowledge:"
run "agentic-git snapshots restore --repo <worktree>"
AGENTIC_GIT_REAL_GIT="$REAL_GIT" "$BIN" snapshots restore --repo "$WT"

# ── verify the promise held ───────────────────────────────────────────────
grep -q "return 42" "$WT/solver.py" || fail "solver.py fix was NOT recovered"
ok "solver.py fix recovered:  $(grep return "$WT/solver.py" | sed 's/^ *//')"
if [ -f "$WT/NOTES.md" ] && grep -q "off-by-one" "$WT/NOTES.md"; then
  ok "NOTES.md (untracked) recovered too"
else
  fail "NOTES.md was NOT recovered"
fi
# ...and the recovery landed UNSTAGED — it won't sneak into the next commit.
staged="$("$REAL_GIT" -C "$WT" diff --cached --name-only)"
if [ -z "$staged" ]; then
  ok "recovery landed unstaged — nothing forced into your index"
else
  fail "recovery unexpectedly staged: $staged"
fi

say "The other guards were on the whole time (they just never fired here):"
run "git checkout main   → denied (cross-branch)"
run "git push --mirror   → denied (would leak the snapshot/trust-root refs)"

printf '\n\033[1;32m════════════════════════════════════════════════════\033[0m\n'
printf '\033[1;32m  DEMO PASSED\033[0m\n'
printf '  What was protected: uncommitted + untracked work a `reset --hard` erased.\n'
printf '  How to opt out:     AGENTIC_GIT_SNAPSHOTS=0, or just do not use the shim.\n'
printf '\033[1;32m════════════════════════════════════════════════════\033[0m\n'
