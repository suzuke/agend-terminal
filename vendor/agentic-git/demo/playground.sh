#!/usr/bin/env bash
#
# playground.sh — a hands-on GUARDED SESSION you drive yourself, to see each
# agentic-git capability fire with your own commands. Unlike recovery-demo.sh
# (a scripted pass/fail check), this drops you into a real guarded shell with a
# cheat-sheet: you type, you watch the guardrail react, you build trust.
#
#   ./demo/playground.sh          # interactive (needs a TTY)
#   ./demo/playground.sh --scripted   # non-interactive: run every scene + assert
#   AGENTIC_GIT_BIN=$(command -v agentic-git) ./demo/playground.sh   # test an install
#
# Nothing here touches your real repos: it builds throwaway repos in a temp dir
# (removed on exit) and never pushes to a real remote.

set -u

# A playground of the guard must not run with the guard bypassed.
unset AGENTIC_GIT_BYPASS AGEND_GIT_BYPASS \
      AGENTIC_GIT_BYPASS_AGENT AGEND_GIT_BYPASS_AGENT \
      AGENTIC_GIT_BYPASS_UNTIL AGEND_GIT_BYPASS_UNTIL 2>/dev/null || true

SCRIPTED=0
[ "${1:-}" = "--scripted" ] && SCRIPTED=1
[ -t 0 ] || SCRIPTED=1   # no TTY (CI, pipe) → scripted, never hang on an interactive shell

c() { printf '\033[%sm' "$1"; }  # color; c 0 resets
say()  { printf '\n%s▸ %s%s\n' "$(c '1;36')" "$*" "$(c 0)"; }
fail() { printf '\n%s✗ FAIL: %s%s\n' "$(c '1;31')" "$*" "$(c 0)" >&2; exit 1; }
ok()   { printf '  %s✓ %s%s\n' "$(c '1;32')" "$*" "$(c 0)"; }

here="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${AGENTIC_GIT_BIN:-$here/target/release/agentic-git}"
if [ ! -x "$BIN" ]; then
  say "Building agentic-git (set AGENTIC_GIT_BIN to skip)"
  ( cd "$here" && cargo build --release -p agentic-git >/dev/null ) || fail "build failed"
fi
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

# A real (non-shim) git, and a PATH with shim dirs stripped (robust even where
# git is already wrapped by another shim).
resolve_real_git() { local d IFS=:; for d in $PATH; do case "$d" in *.agentic-git*|*.agend-terminal*) continue ;; esac; [ -x "$d/git" ] && { printf '%s\n' "$d/git"; return 0; }; done; return 1; }
REAL_GIT="$(resolve_real_git)" || fail "no real (non-shim) git on PATH"
sanitized_path() { local d IFS=: out; out="$(dirname "$REAL_GIT")"; for d in $PATH; do case "$d" in *.agentic-git*|*.agend-terminal*) continue ;; esac; out="$out:$d"; done; printf '%s\n' "$out"; }

# ── throwaway world: a project (with a LOCAL bare 'origin' so push scenes are
#    real, never network) and a stand-in "your real checkout" ────────────────
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
project="$work/project"; canonical="$work/canonical"; bare="$work/origin.git"; home="$work/agentic-home"
export GIT_AUTHOR_NAME=you GIT_AUTHOR_EMAIL=you@example.com \
       GIT_COMMITTER_NAME=you GIT_COMMITTER_EMAIL=you@example.com
mkdir -p "$project" "$canonical"
"$REAL_GIT" init -q --bare "$bare"
"$REAL_GIT" -C "$project" init -q -b main
"$REAL_GIT" -C "$project" remote add origin "$bare"
printf 'def solve():\n    pass  # TODO\n' > "$project/solver.py"
"$REAL_GIT" -C "$project" add -A && "$REAL_GIT" -C "$project" commit -qm "project baseline"
"$REAL_GIT" -C "$project" push -q origin main
# a separate repo that stands in for YOUR real project checkout (has an origin
# remote → the shim treats it as a protected canonical checkout)
"$REAL_GIT" -C "$canonical" init -q -b main
"$REAL_GIT" -C "$canonical" remote add origin https://example.invalid/your-project.git
printf 'print("your real work")\n' > "$canonical/app.py"
"$REAL_GIT" -C "$canonical" add -A && "$REAL_GIT" -C "$canonical" commit -qm base
"$REAL_GIT" -C "$canonical" commit -q --allow-empty -m "more of your work"

run_session() {  # run_session <shell-command>
  ( cd "$project" && \
    AGENTIC_GIT_HOME="$home" AGENTIC_GIT_REAL_GIT="$REAL_GIT" AGENTIC_GIT_BIN="$BIN" \
    CANONICAL_CHECKOUT="$canonical" PLAYGROUND_HOME="$home" \
    PATH="$(sanitized_path)" \
    "$BIN" run --agent playground --branch demo/playground -- sh -c "$1" )
}

# ════════════════════════════════════════════════════════════════════════════
if [ "$SCRIPTED" = 0 ]; then
  # ── Interactive: print the cheat-sheet, then hand over a guarded shell ─────
  export CHEATSHEET
  CHEATSHEET="$(cat <<CHEAT
$(c '1;35')╔═══ agentic-git playground — you are INSIDE a guarded session ═══╗$(c 0)
  agent = playground   branch = demo/playground   (here, \`git\` IS the shim)
  Type each command below, watch the guardrail react. Your real repos are
  untouched. Type $(c '1;37')exit$(c 0) when you're done.

$(c '1;36')① ISOLATION$(c 0) — you're on the agent's own branch, in its own worktree
   try:    git rev-parse --abbrev-ref HEAD
   expect: demo/playground   (not your project's main)

$(c '1;36')② CROSS-BRANCH DENY$(c 0) — the agent can't trample another branch
   try:    git checkout main
   expect: agentic-git: ERROR ... cross-branch ... cannot switch to 'main'
   verify: git rev-parse --abbrev-ref HEAD   (still demo/playground)

$(c '1;36')③ WORKTREE DENY$(c 0) — worktree lifecycle belongs to the orchestrator
   try:    git worktree add /tmp/pg -b whatever
   expect: agentic-git: ERROR ... worktree lifecycle is session-managed

$(c '1;36')④ PROVENANCE$(c 0) — every commit is stamped with who made it
   try:    echo hi >> solver.py && git commit -am wip && git log -1 --format=%B
   expect: ... Agentic-Agent: playground / Agentic-Branch: demo/playground

$(c '1;36')⑤ PUSH GUARD$(c 0) — all-refs pushes and secret leaks are blocked
   try:    git push --mirror origin
   expect: ERROR ... \`--mirror\` pushes ALL local refs ...
   try:    echo KEY > fleet.yaml && git add fleet.yaml && git commit -m oops && git push origin HEAD
   expect: ERROR ... push range contains a trust-root file: \`fleet.yaml\` ...

$(c '1;36')⑥ RECOVERY$(c 0) — an agent erased uncommitted work? one command brings it back
   try:    echo KEEPME >> solver.py && git reset --hard      # looks gone:
   verify: tail -1 solver.py     (KEEPME is gone)
   try:    \$AGENTIC_GIT_BIN snapshots restore --repo "\$PWD"
   verify: tail -1 solver.py     (KEEPME is back — recovered, unstaged)

$(c '1;36')⑦ CONTAINMENT (optional)$(c 0) — even in YOUR real checkout the agent can't move HEAD
   try:    (cd "\$CANONICAL_CHECKOUT" && git checkout HEAD~1)
   expect: ERROR ... cross-branch ...   (your checkout's HEAD stays put)

$(c '1;36')⑧ AUDITED BYPASS (optional, advanced)$(c 0) — you can override a guard, on the record
   try:    AGENTIC_GIT_BYPASS=1 git push --mirror origin     (the op ⑤ just blocked)
   expect: it SUCCEEDS — a deliberate override (pushes to the throwaway origin)
   verify: grep bypass "\$PLAYGROUND_HOME/fleet_events.jsonl"   (your exact argv is recorded)

$(c '1;35')Type exit to leave the guarded shell (the throwaway repos are then removed).$(c 0)
CHEAT
)"
  say "Entering the guarded shell — git is the shim from here."
  run_session 'printf "%s\n" "$CHEATSHEET"; exec "${SHELL:-/bin/sh}" -i'
  say "You have left the guarded session."
  printf '  audit log (bypass/snapshot events): %s\n' "$home/fleet_events.jsonl"
  printf '  throwaway repos removed. Re-run any time.\n'
  exit 0
fi

# ── Scripted: drive every scene and assert (CI-safe, no TTY needed) ──────────
say "agentic-git playground — scripted mode ($($BIN version))"
probe='
set -u
fail() { printf "SCENE-FAIL: %s\n" "$*"; exit 1; }
git rev-parse --abbrev-ref HEAD | grep -qx demo/playground || fail "① not on agent branch"
git checkout main 2>&1 | grep -q "cross-branch" || fail "② checkout not denied"
git worktree add /tmp/pg-s -b y 2>&1 | grep -q "worktree lifecycle" || fail "③ worktree not denied"
echo hi >> solver.py; git commit -qam wip; git log -1 --format=%B | grep -q "Agentic-Agent: playground" || fail "④ no trailer"
git push --mirror origin 2>&1 | grep -q -- "--mirror" || fail "⑤a mirror not denied"
echo KEY > fleet.yaml; git add fleet.yaml; git commit -qam oops
git push origin HEAD 2>&1 | grep -q "trust-root file" || fail "⑤b trust-root leak not denied"
git rm -q --cached fleet.yaml; git commit -qam un
printf "KEEPME\n" >> solver.py; git reset --hard >/dev/null 2>&1
tail -1 solver.py | grep -qx KEEPME && fail "⑥ reset did not discard"
"$AGENTIC_GIT_BIN" snapshots restore --repo "$PWD" >/dev/null 2>&1
tail -1 solver.py | grep -qx KEEPME || fail "⑥ restore did not recover"
( cd "$CANONICAL_CHECKOUT" && git checkout HEAD~1 2>&1 | grep -q "cross-branch" ) || fail "⑦ containment"
AGENTIC_GIT_BYPASS=1 git push --mirror origin >/dev/null 2>&1 || fail "⑧ bypass push did not succeed"
grep bypass "$AGENTIC_GIT_HOME/fleet_events.jsonl" 2>/dev/null | grep -q "\"push\"" || fail "⑧ bypass not audited"
echo ALL-SCENES-OK
'
out="$(run_session "$probe" 2>&1)"
echo "$out" | grep -q "ALL-SCENES-OK" || { echo "$out" | grep -E "SCENE-FAIL|ERROR" | head; fail "a scene did not behave as documented"; }
ok "① isolation  ② cross-branch deny  ③ worktree deny  ④ provenance trailer"
ok "⑤ push guard (mirror + trust-root leak)  ⑥ one-command recovery"
ok "⑦ containment (your checkout is safe)  ⑧ audited bypass (logged)"
say "PLAYGROUND VERIFIED — every capability behaved as the cheat-sheet promises."
