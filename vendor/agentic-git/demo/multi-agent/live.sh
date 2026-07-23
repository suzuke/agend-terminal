#!/usr/bin/env bash
#
# live.sh — Layer 2 driver. Same scenario as verify.sh, but the two guarded
# sessions are run by TWO REAL AGENTS (or just two shells) instead of spawned
# in-process. It uses the SAME lib.sh build_world + synthesize, so the rigor is
# identical — only WHO runs the sessions changes. The supervisor is still the
# verifier: agents merely execute + leave evidence; you re-derive the verdict.
#
#   ./live.sh setup [--world DIR] [--reset]   # build a persistent world, print
#                                             #   the two commands to hand out
#   ./live.sh synth <world>                   # re-derive the verdict yourself
#
# Typical flow:
#   ./live.sh setup --world /tmp/l2           # prints: sh /tmp/l2/agent-launch.sh a|b
#   # hand each command to a different agent/shell; run them CONCURRENTLY
#   ./live.sh synth /tmp/l2                    # VERIFIED / FAILED, from state you own
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source-path=SCRIPTDIR
# shellcheck source=lib.sh
. "$HERE/lib.sh"

usage() { printf 'usage: %s setup [--world DIR] [--reset] | synth <world>\n' "$0" >&2; exit 2; }

cmd="${1:-}"; shift 2>/dev/null || true
case "$cmd" in
  setup)
    world=""; reset=0
    while [ $# -gt 0 ]; do
      case "$1" in
        --world) world="${2:?--world needs a DIR}"; shift 2 ;;
        --reset) reset=1; shift ;;
        *) usage ;;
      esac
    done
    [ -n "$world" ] || world="$(mktemp -d)"
    scrub_hostile_env
    REAL_GIT="$(resolve_real_git)" || die "no real (non-shim) git on PATH"
    BIN="$(resolve_bin)" || die "no guarded agentic-git — build from the repo, or set AGENTIC_GIT_BIN"

    # Freshness (fugu design review, holes 1+3): refuse a non-empty world unless
    # --reset — otherwise a PRIOR run's tips/worktrees/artifacts could let synth
    # report VERIFIED for a run that never happened.
    if [ -e "$world" ] && [ -n "$(ls -A "$world" 2>/dev/null || true)" ]; then
      [ "$reset" = 1 ] || die "world exists and is non-empty: $world  (pass --reset to rebuild it, or choose a fresh --world)"
      rm -rf "$world"
    fi
    mkdir -p "$world" || die "cannot create world: $world"
    # Atomic setup lock so two concurrent setups can't interleave the world.
    mkdir "$world/.lock" 2>/dev/null || die "another setup holds $world/.lock (or a crashed run left it — rerun with --reset)"
    # shellcheck disable=SC2064
    trap "rmdir '$world/.lock' 2>/dev/null || true" EXIT

    build_world "$world"   # sets globals incl. RUN_ID
    cp "$HERE/agent-run.sh"    "$world/agent-run.sh"
    cp "$HERE/agent-launch.sh" "$world/agent-launch.sh"
    chmod +x "$world/agent-launch.sh"

    say "World ready: $world"
    printf '  guarded binary : %s\n' "$BIN"
    printf '  version        : %s\n' "$("$BIN" version 2>/dev/null || echo '?')"
    printf '  run id         : %s\n' "$RUN_ID"
    say "Hand ONE command to each of two real agents (or two shells) — run them CONCURRENTLY:"
    printf '    agent-a:  sh %s/agent-launch.sh a\n' "$world"
    printf '    agent-b:  sh %s/agent-launch.sh b\n' "$world"
    say "Then re-derive the verdict yourself (do NOT trust the agents' word):"
    printf '    %s synth %s\n' "$0" "$world"
    ;;
  synth)
    world="${1:?usage: synth <world>}"
    [ -d "$world" ] || die "no such world: $world"
    scrub_hostile_env
    synthesize "$world"; exit $?
    ;;
  *) usage ;;
esac
