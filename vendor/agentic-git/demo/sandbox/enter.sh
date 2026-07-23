#!/usr/bin/env bash
#
# enter.sh — a PERSISTENT hands-on sandbox for agentic-git. Drops you into a
# guarded session (a shell where `git` IS the shim) with a cheat-sheet; type
# each command and watch the guardrail react. The sandbox repos live next to
# this script and survive across runs — poke at them, break them, re-enter.
# Nothing leaves this folder; the only "remote" is a LOCAL bare repo.
#
# Complements ../recovery-demo.sh (one scripted story) and ../playground.sh
# (a throwaway build-from-source playground): this one is PERSISTENT and uses
# your installed `agentic-git`, so it's the closest thing to a real setup.
#
#   ./enter.sh            # interactive guarded shell
#   ./enter.sh --scripted # non-interactive: run every scene and assert
#   ./enter.sh --reset    # wipe and rebuild the sandbox repos
#
# The generated repos (project/ origin.git/ your-checkout/ home/) are gitignored,
# so running it in place never dirties the repo. Or copy this folder anywhere.
#
set -u
unset AGENTIC_GIT_BYPASS AGEND_GIT_BYPASS \
      AGENTIC_GIT_BYPASS_AGENT AGEND_GIT_BYPASS_AGENT \
      AGENTIC_GIT_BYPASS_UNTIL AGEND_GIT_BYPASS_UNTIL 2>/dev/null || true

root="$(cd "$(dirname "$0")" && pwd)"
project="$root/project"; canonical="$root/your-checkout"; bare="$root/origin.git"; home="$root/home"

c() { printf '\033[%sm' "$1"; }
say()  { printf '\n%s▸ %s%s\n' "$(c '1;36')" "$*" "$(c 0)"; }
fail() { printf '\n%s✗ %s%s\n' "$(c '1;31')" "$*" "$(c 0)" >&2; exit 1; }

MODE=interactive
[ "${1:-}" = "--scripted" ] && MODE=scripted
[ "${1:-}" = "--reset" ] && MODE=reset
[ -t 0 ] || { [ "$MODE" = interactive ] && MODE=scripted; }   # no TTY → never hang

# Resolve the agentic-git binary: an explicit override, then an installed one,
# then (if we're inside the agentic-git repo) the release build.
BIN="${AGENTIC_GIT_BIN:-}"
{ [ -n "$BIN" ] && [ -x "$BIN" ]; } || BIN="$(command -v agentic-git || true)"
repo_root="$(cd "$root/../.." 2>/dev/null && pwd || true)"
if { [ -z "$BIN" ] || [ ! -x "$BIN" ]; } && [ -n "$repo_root" ] && [ -f "$repo_root/Cargo.toml" ]; then
  BIN="$repo_root/target/release/agentic-git"
  if [ ! -x "$BIN" ]; then
    say "Building agentic-git from this repo (or run: cargo install agentic-git)…"
    ( cd "$repo_root" && cargo build --release -p agentic-git >/dev/null ) || fail "build failed"
  fi
fi
{ [ -n "$BIN" ] && [ -x "$BIN" ]; } || fail "agentic-git not found — install it (cargo install agentic-git) or run from inside the repo"

resolve_real_git() { local d IFS=:; for d in $PATH; do case "$d" in *.agentic-git*|*.agend-terminal*|*/.cargo/bin*) continue ;; esac; [ -x "$d/git" ] && { printf '%s\n' "$d/git"; return 0; }; done; return 1; }
REAL_GIT="$(resolve_real_git)" || fail "no real (non-shim) git on PATH"
sanitized_path() { local d IFS=: out; out="$(dirname "$REAL_GIT")"; for d in $PATH; do case "$d" in *.agentic-git*|*.agend-terminal*) continue ;; esac; out="$out:$d"; done; printf '%s\n' "$out"; }

if [ "$MODE" = reset ]; then
  say "Resetting sandbox repos…"; rm -rf "$project" "$canonical" "$bare" "$home"; MODE=interactive
fi

# ── build the throwaway world once (idempotent) ─────────────────────────────
if [ ! -d "$project/.git" ]; then
  say "Setting up sandbox repos in $root"
  rm -rf "$project" "$canonical" "$bare"; mkdir -p "$project" "$canonical"
  "$REAL_GIT" init -q --bare "$bare"
  "$REAL_GIT" -C "$project" init -q -b main
  "$REAL_GIT" -C "$project" config user.name  you
  "$REAL_GIT" -C "$project" config user.email you@example.com
  "$REAL_GIT" -C "$project" remote add origin "$bare"
  printf 'def solve():\n    pass  # TODO\n' > "$project/solver.py"
  "$REAL_GIT" -C "$project" add -A && "$REAL_GIT" -C "$project" commit -qm "project baseline"
  "$REAL_GIT" -C "$project" push -q origin main
  # a stand-in for YOUR real project checkout (has an origin → protected)
  "$REAL_GIT" -C "$canonical" init -q -b main
  "$REAL_GIT" -C "$canonical" config user.name you
  "$REAL_GIT" -C "$canonical" config user.email you@example.com
  "$REAL_GIT" -C "$canonical" remote add origin https://example.invalid/your-project.git
  printf 'print("your real work")\n' > "$canonical/app.py"
  "$REAL_GIT" -C "$canonical" add -A && "$REAL_GIT" -C "$canonical" commit -qm base
  "$REAL_GIT" -C "$canonical" commit -q --allow-empty -m "more of your work"
fi

run_session() {  # run_session <shell-command>
  ( cd "$project" && \
    AGENTIC_GIT_HOME="$home" AGENTIC_GIT_REAL_GIT="$REAL_GIT" AGENTIC_GIT_BIN="$BIN" \
    CANONICAL_CHECKOUT="$canonical" PLAYGROUND_HOME="$home" \
    PATH="$(sanitized_path)" \
    "$BIN" run --agent me --branch sandbox/work -- sh -c "$1" )
}

if [ "$MODE" = scripted ]; then
  say "agentic-git sandbox — scripted self-check ($($BIN version))"
  probe='
set -u; fail() { printf "SCENE-FAIL: %s\n" "$*"; exit 1; }
git rev-parse --abbrev-ref HEAD | grep -qx sandbox/work || fail "① branch"
git checkout main 2>&1 | grep -q "cross-branch" || fail "② checkout deny"
git worktree add /tmp/agx -b y 2>&1 | grep -q "worktree lifecycle" || fail "③ worktree deny"
echo hi >> solver.py; git commit -qam wip; git log -1 --format=%B | grep -q "Agentic-Agent: me" || fail "④ trailer"
git push --mirror origin 2>&1 | grep -q -- "--mirror" || fail "⑤a mirror deny"
echo K > fleet.yaml; git add fleet.yaml; git commit -qam oops
git push origin HEAD 2>&1 | grep -q "trust-root file" || fail "⑤b trust-root deny"
git rm -q --cached fleet.yaml; git commit -qam un
printf "KEEPME\n" >> solver.py; git reset --hard >/dev/null 2>&1
tail -1 solver.py | grep -qx KEEPME && fail "⑥ reset"
"$AGENTIC_GIT_BIN" snapshots restore --repo "$PWD" >/dev/null 2>&1
tail -1 solver.py | grep -qx KEEPME || fail "⑥ restore"
( cd "$CANONICAL_CHECKOUT" && git checkout HEAD~1 2>&1 | grep -q "cross-branch" ) || fail "⑦ containment"
AGENTIC_GIT_BYPASS=1 git push --mirror origin >/dev/null 2>&1 || fail "⑧ bypass push"
grep bypass "$AGENTIC_GIT_HOME/fleet_events.jsonl" 2>/dev/null | grep -q "\"push\"" || fail "⑧ audit"
echo ALL-SCENES-OK'
  out="$(run_session "$probe" 2>&1)"
  echo "$out" | grep -q ALL-SCENES-OK || { echo "$out" | grep -E "SCENE-FAIL|ERROR" | head; fail "a scene misbehaved"; }
  printf '  %s✓ all 8 capabilities behaved as documented%s\n' "$(c '1;32')" "$(c 0)"
  say "SANDBOX OK — run ./enter.sh (no args) to explore it by hand."
  exit 0
fi

# ── interactive: cheat-sheet + a guarded shell ──────────────────────────────
export CHEATSHEET
CHEATSHEET="$(cat <<CHEAT
$(c '1;35')╔═══ agentic-git sandbox — you are INSIDE a guarded session ═══╗$(c 0)
  agent = me   branch = sandbox/work   (here, \`git\` IS the shim)
  Type each command, watch the guardrail react. Repos persist between runs.
  Type $(c '1;37')exit$(c 0) to leave (nothing is deleted).

$(c '1;36')① ISOLATION$(c 0)          try: git rev-parse --abbrev-ref HEAD
                       expect: sandbox/work  (the agent's own branch/worktree)

$(c '1;36')② CROSS-BRANCH DENY$(c 0)  try: git checkout main
                       expect: ERROR … cross-branch … cannot switch to 'main'

$(c '1;36')③ WORKTREE DENY$(c 0)      try: git worktree add /tmp/x -b y
                       expect: ERROR … worktree lifecycle is session-managed

$(c '1;36')④ PROVENANCE$(c 0)         try: echo hi >> solver.py && git commit -am wip
                            git log -1 --format=%B
                       expect: … Agentic-Agent: me / Agentic-Branch: sandbox/work

$(c '1;36')⑤ PUSH GUARD$(c 0)         try: git push --mirror origin
                       expect: ERROR … \`--mirror\` pushes ALL local refs …
                       try: echo K > fleet.yaml && git add fleet.yaml \\
                              && git commit -m oops && git push origin HEAD
                       expect: ERROR … push range contains a trust-root file …

$(c '1;36')⑥ RECOVERY$(c 0)          try: echo KEEP >> solver.py && git reset --hard
                       verify: tail -1 solver.py   (KEEP is gone)
                       try: \$AGENTIC_GIT_BIN snapshots restore --repo "\$PWD"
                       verify: tail -1 solver.py   (KEEP is back — unstaged)

$(c '1;36')⑦ CONTAINMENT$(c 0)       try: (cd "\$CANONICAL_CHECKOUT" && git checkout HEAD~1)
                       expect: ERROR … cross-branch …  (your checkout's HEAD is safe)

$(c '1;36')⑧ AUDITED BYPASS$(c 0)    try: AGENTIC_GIT_BYPASS=1 git push --mirror origin
                       expect: it SUCCEEDS — a deliberate, logged override
                       verify: grep bypass "\$PLAYGROUND_HOME/fleet_events.jsonl"

$(c '1;35')exit to leave. ./enter.sh --reset rebuilds the repos. --scripted asserts all 8.$(c 0)
CHEAT
)"
say "Entering the guarded shell — git is the shim from here. (exit to leave)"
run_session 'printf "%s\n" "$CHEATSHEET"; exec "${SHELL:-/bin/sh}" -i'
say "You left the guarded session. Repos kept at $root — re-run ./enter.sh any time."
printf '  audit log: %s\n' "$home/fleet_events.jsonl"
