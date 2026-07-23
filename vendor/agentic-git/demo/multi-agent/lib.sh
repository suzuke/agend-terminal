#!/usr/bin/env bash
#
# lib.sh — the SINGLE source of truth for the multi-agent scenario: hostile-env
# scrubbing, real-git resolution, the guarded binary, world setup, and the
# SYNTHESIS invariant block. Sourced by verify.sh (Layer 1: the supervisor
# spawns the two guarded sessions in-process) and by live.sh (Layer 2: two real
# agents each drive one guarded session). Defining synthesize() ONCE means a
# real violation can never pass under one driver but fail under the other.
#
# Not executable on its own — `source` it from a bash driver.

# ── hostile-environment scrub ────────────────────────────────────────────────
# Both drivers run real git inside a possibly-foreign session (Layer 2 runs
# inside whatever shell the agent has). Strip every var that could redirect or
# reconfigure git despite `-C`, plus every agentic-git/agend bypass/agent/home/
# allow knob. The three vars we actually want (HOME/REAL_GIT/BIN) are set
# explicitly at each guarded-run site, so a blanket unset here is safe.
scrub_hostile_env() {
  local v
  # AGENTIC_GIT_BIN is a deliberate binary override (resolve_bin honours it), not
  # a hostile knob — keep it; scrub every other agentic-git/agend var.
  for v in $(env 2>/dev/null | sed -n 's/^\(AGENTIC_GIT_[A-Za-z0-9_]*\)=.*/\1/p; s/^\(AGEND_GIT_[A-Za-z0-9_]*\)=.*/\1/p'); do
    [ "$v" = AGENTIC_GIT_BIN ] && continue
    unset "$v" 2>/dev/null || true
  done
  unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES \
        GIT_COMMON_DIR GIT_NAMESPACE GIT_CEILING_DIRECTORIES GIT_PREFIX \
        GIT_CONFIG GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_NOSYSTEM \
        GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS 2>/dev/null || true
}

# ── colours / ui ─────────────────────────────────────────────────────────────
c()    { printf '\033[%sm' "$1"; }
say()  { printf '\n%s▸ %s%s\n' "$(c '1;36')" "$*" "$(c 0)"; }
pass() { printf '  %s✓ %s%s\n' "$(c '1;32')" "$*" "$(c 0)"; }
bad()  { printf '  %s✗ %s%s\n' "$(c '1;31')" "$*" "$(c 0)"; FAILS=$((FAILS+1)); }
die()  { printf '\n%s✗ %s%s\n' "$(c '1;31')" "$*" "$(c 0)" >&2; exit 1; }

# ── a real (non-shim) git, and a PATH with shim dirs stripped ────────────────
resolve_real_git() { local d IFS=:; for d in $PATH; do case "$d" in *.agentic-git*|*.agend-terminal*|*/.cargo/bin*) continue ;; esac; [ -x "$d/git" ] && { printf '%s\n' "$d/git"; return 0; }; done; return 1; }
sanitized_path()   { local d IFS=: out; out="$(dirname "$REAL_GIT")"; for d in $PATH; do case "$d" in *.agentic-git*|*.agend-terminal*) continue ;; esac; out="$out:$d"; done; printf '%s\n' "$out"; }

LIBDIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ── the guarded binary ───────────────────────────────────────────────────────
# Default to the repo's release build: it carries the cross-branch push guard
# the deny-steps rely on, which a stale `cargo install`ed release may predate.
# AGENTIC_GIT_BIN is an ADVANCED override (e.g. to test a specific install).
resolve_bin() {
  local bin root
  bin="${AGENTIC_GIT_BIN:-}"
  if [ -n "$bin" ] && [ -x "$bin" ]; then printf '%s\n' "$bin"; return 0; fi
  root="$(cd "$LIBDIR/../.." 2>/dev/null && pwd || true)"
  if [ -n "$root" ] && [ -f "$root/Cargo.toml" ]; then
    bin="$root/target/release/agentic-git"
    [ -x "$bin" ] || ( cd "$root" && cargo build --release -p agentic-git >/dev/null ) || return 1
    printf '%s\n' "$bin"; return 0
  fi
  bin="$(command -v agentic-git || true)"
  if [ -n "$bin" ] && [ -x "$bin" ]; then printf '%s\n' "$bin"; return 0; fi
  return 1
}

# ── build the shared world ───────────────────────────────────────────────────
# Uses the REAL_GIT and BIN globals set by the caller. Writes a run-unique
# baseline (so a persistent world's freshness is provable) and an atomically
# published world.env carrying STATE=ready. Also EXPORTS the world's paths and
# RUN_ID/PROJECT_BASE/CANON_BASE as globals so the caller can use them directly
# (verify.sh) without re-sourcing world.env.  build_world <world-dir>
build_world() {
  local world="$1"
  project="$world/project"; canonical="$world/your-checkout"; bare="$world/origin.git"; home="$world/home"; arts="$world/artifacts"
  mkdir -p "$project" "$canonical" "$arts/a" "$arts/b"
  RUN_ID="run-$(date +%Y%m%d%H%M%S)-$$"
  export GIT_AUTHOR_NAME=you GIT_AUTHOR_EMAIL=you@example.com GIT_COMMITTER_NAME=you GIT_COMMITTER_EMAIL=you@example.com
  "$REAL_GIT" init -q --bare "$bare"
  "$REAL_GIT" -C "$project" init -q -b main
  "$REAL_GIT" -C "$project" config user.name you; "$REAL_GIT" -C "$project" config user.email you@example.com
  "$REAL_GIT" -C "$project" remote add origin "$bare"
  printf 'shared project\nrun: %s\n' "$RUN_ID" > "$project/README.md"
  "$REAL_GIT" -C "$project" add -A; "$REAL_GIT" -C "$project" commit -qm "project baseline ($RUN_ID)"
  "$REAL_GIT" -C "$project" push -q origin main
  PROJECT_BASE="$("$REAL_GIT" -C "$project" rev-parse HEAD)"
  "$REAL_GIT" -C "$canonical" init -q -b main
  "$REAL_GIT" -C "$canonical" config user.name you; "$REAL_GIT" -C "$canonical" config user.email you@example.com
  "$REAL_GIT" -C "$canonical" remote add origin https://example.invalid/your-project.git
  printf 'your real work\n' > "$canonical/app.py"; "$REAL_GIT" -C "$canonical" add -A; "$REAL_GIT" -C "$canonical" commit -qm base
  "$REAL_GIT" -C "$canonical" commit -q --allow-empty -m more
  CANON_BASE="$("$REAL_GIT" -C "$canonical" rev-parse HEAD)"
  { printf 'RUN_ID=%s\n' "$RUN_ID"
    printf 'project=%s\n' "$project"; printf 'canonical=%s\n' "$canonical"
    printf 'bare=%s\n' "$bare"; printf 'home=%s\n' "$home"; printf 'arts=%s\n' "$arts"
    printf 'BIN=%s\n' "$BIN"
    printf 'PROJECT_BASE=%s\n' "$PROJECT_BASE"; printf 'CANON_BASE=%s\n' "$CANON_BASE"
    printf 'STATE=ready\n'
  } > "$world/world.env.tmp"
  mv "$world/world.env.tmp" "$world/world.env"
}

# ── SYNTHESIS: re-derive the truth from STATE, never from an agent's word ─────
# Reads only the world the supervisor owns. Returns 0 (VERIFIED) or 1 (FAILED),
# 2 on a malformed/not-ready world.  synthesize <world-dir>
synthesize() {
  # RUN_ID/PROJECT_BASE/CANON_BASE/project/bare/home/arts/canonical all come
  # from the sourced world.env below — shellcheck can't see that dynamic source.
  # shellcheck disable=SC2153,SC2154
  local world="$1" REAL_GIT wa wb ta tb ks elog
  [ -f "$world/world.env" ] || { printf 'no world.env in %s\n' "$world" >&2; return 2; }
  # shellcheck disable=SC1091
  . "$world/world.env"
  [ "${STATE:-}" = ready ] || { printf 'world not ready (STATE=%s)\n' "${STATE:-}" >&2; return 2; }
  REAL_GIT="$(resolve_real_git)" || { printf 'no real (non-shim) git on PATH\n' >&2; return 2; }

  say "Synthesis — re-deriving the truth from git/home/audit state:"
  FAILS=0
  wt_for_branch() { "$REAL_GIT" -C "$project" worktree list --porcelain 2>/dev/null | awk -v b="refs/heads/$1" '/^worktree /{w=$2} $0==("branch " b){print w; exit}'; }
  rbr() { "$REAL_GIT" -C "$1" symbolic-ref --short HEAD 2>/dev/null; }
  wa="$(wt_for_branch feat/a)"; wb="$(wt_for_branch feat/b)"

  # I0 FRESHNESS — this run's baseline is present, and BOTH origin branches
  #    descend from it. A persistent/reused world cannot pass on a PRIOR run's
  #    stale tips: the baseline commit is run-unique (embeds RUN_ID), so a stale
  #    feat/* descends from a different baseline and fails the ancestry test.
  if "$REAL_GIT" -C "$project" log -1 --format=%B "$PROJECT_BASE" 2>/dev/null | grep -qF "$RUN_ID" \
     && "$REAL_GIT" -C "$bare" merge-base --is-ancestor "$PROJECT_BASE" feat/a 2>/dev/null \
     && "$REAL_GIT" -C "$bare" merge-base --is-ancestor "$PROJECT_BASE" feat/b 2>/dev/null; then
    pass "both agent branches descend from THIS run's baseline ($RUN_ID) — not stale state"
  else bad "freshness failed: an agent branch is missing or does not descend from this run's baseline ($RUN_ID)"; fi

  # I1 two distinct worktrees LINKED TO THE SHARED PROJECT (from its own worktree
  #    list), each STILL on its own bound branch (real git — catches a cross-branch
  #    checkout that drifted HEAD)
  if [ -n "$wa" ] && [ -n "$wb" ] && [ "$wa" != "$wb" ]; then
    pass "the shared project has two distinct agent worktrees (from its own worktree list)"
  else bad "the shared project lacks two distinct agent worktrees (a=$wa b=$wb)"; fi
  if [ "$(rbr "$wa")" = feat/a ] && [ "$(rbr "$wb")" = feat/b ]; then
    pass "each worktree HEAD is on its own bound branch (no cross-branch drift)"
  else bad "a worktree drifted off its branch (a=$(rbr "$wa") b=$(rbr "$wb"))"; fi

  # I2 origin branches distinct + each trailered to its OWN agent — this alone
  #    catches a cross-agent force-push clobber or delete (no agent word trusted)
  # --verify -q: a missing ref yields empty + nonzero (never echoes the token),
  # so a not-yet-pushed branch can't spuriously satisfy the distinct-tips check.
  ta="$("$REAL_GIT" -C "$bare" rev-parse --verify -q feat/a 2>/dev/null || true)"; tb="$("$REAL_GIT" -C "$bare" rev-parse --verify -q feat/b 2>/dev/null || true)"
  if [ -n "$ta" ] && [ -n "$tb" ] && [ "$ta" != "$tb" ]; then
    pass "both branches on the shared origin, distinct tips (neither deleted/collapsed)"
  else bad "an origin branch was clobbered or deleted (a=$ta b=$tb)"; fi
  if "$REAL_GIT" -C "$bare" log -1 --format=%B feat/a 2>/dev/null | grep -q "Agentic-Agent: agent-a" \
     && "$REAL_GIT" -C "$bare" log -1 --format=%B feat/b 2>/dev/null | grep -q "Agentic-Agent: agent-b"; then
    pass "each origin branch's tip is trailered to its OWN agent (not clobbered)"
  else bad "origin provenance is wrong or one branch was clobbered by the other agent"; fi

  # I4 the shared source repo AND your stand-in real checkout were NOT moved. The
  #    canonical check independently catches an agent that touched your checkout,
  #    regardless of what the agent reported (closes the forged-artifact hole).
  if [ "$("$REAL_GIT" -C "$project" rev-parse HEAD)" = "$PROJECT_BASE" ] \
     && [ "$("$REAL_GIT" -C "$project" rev-parse --abbrev-ref HEAD)" = main ]; then
    pass "the shared source repo's HEAD is untouched"
  else bad "the shared source repo drifted"; fi
  if [ "$("$REAL_GIT" -C "$canonical" rev-parse HEAD)" = "$CANON_BASE" ]; then
    pass "your stand-in real checkout's HEAD is untouched (containment held)"
  else bad "the canonical checkout HEAD moved ($CANON_BASE -> $("$REAL_GIT" -C "$canonical" rev-parse HEAD))"; fi

  # I5 one 32-byte key, both signed bindings (no split-brain)
  ks=$(wc -c < "$home/.config-integrity-key" 2>/dev/null | tr -d ' ')
  if [ "$ks" = 32 ] && [ -f "$home/runtime/agent-a/binding.json.sig" ] && [ -f "$home/runtime/agent-b/binding.json.sig" ]; then
    pass "one 32-byte integrity key; both agents' signed bindings present"
  else bad "key/binding split-brain (keysize=$ks)"; fi

  # I6 both agents appear, attributed, in the shared audit log — independent proof
  #    each went through the shim and its activity was recorded to it.
  elog="$home/fleet_events.jsonl"
  if grep -q "agent-a" "$elog" 2>/dev/null && grep -q "agent-b" "$elog" 2>/dev/null; then
    pass "both agents' activity is recorded, per-agent, in the shared audit log"
  else bad "audit log missing per-agent attribution"; fi

  # I7 CONSISTENCY cross-check: each agent's OWN report must AGREE with the truth
  #    above. Authority is the independent state (I0-I6); this only flags an agent
  #    whose self-report disagrees — it can NEVER turn a real violation into a pass.
  consistent() { local role="$1" ok=0 n exp rc
    [ "$(cat "$arts/$role/verdict.txt" 2>/dev/null)" = PASS ] || ok=1
    for n in $(seq 1 10); do
      exp=$(cat "$arts/$role/$n.expect" 2>/dev/null); rc=$(cat "$arts/$role/$n.rc" 2>/dev/null)
      case "$exp" in ok) [ "$rc" = 0 ] || ok=1 ;; deny) [ "$rc" != 0 ] || ok=1 ;; *) ok=1 ;; esac
    done
    return "$ok"
  }
  if consistent a; then pass "agent-a's self-report agrees with the re-derived state"; else bad "agent-a self-report inconsistent"; fi
  if consistent b; then pass "agent-b's self-report agrees with the re-derived state"; else bad "agent-b self-report inconsistent"; fi

  if [ "$FAILS" = 0 ]; then
    say "$(c '1;32')MULTI-AGENT SCENARIO VERIFIED$(c 0) — every invariant held (re-derived from state): two agents shared one repo, stayed isolated, kept per-agent provenance, and could not clobber each other or touch your checkout."
    return 0
  fi
  say "$(c '1;31')FAILED$(c 0) — $FAILS invariant(s) did not hold."
  return 1
}
