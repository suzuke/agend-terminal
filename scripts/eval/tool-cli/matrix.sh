#!/usr/bin/env bash
# tool-cli Phase 0b eval harness — confirmation matrix driver (SPEC.txt §6).
#
#   matrix.sh [--dry-run] [--jobs N] [--model M] [--timeout SECS] OUT_DIR
#   matrix.sh --clean
#
# Plan (frozen by SPEC §6):
#   S01..S06  both arms, 10 pairs each          -> 120 runs, N = 60 pairs
#   S13       mcp arm only, 45 runs
#   S14       cli arm only, 45 runs
#                                               -> 210 runs total
# Pair ordering is predeclared and interleaved: odd pair -> mcp first,
# even pair -> cli first.  Scenarios flagged "smoke" in meta.json (S00-smoke)
# are harness self-checks and are never part of the matrix.
#
# Resumable: a run directory that already holds metadata.json is skipped, so
# re-running the same OUT_DIR continues where the last invocation stopped.
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

die() { echo "matrix.sh: $*" >&2; exit 1; }

DRY=0; JOBS=3; MODEL="claude-fable-5"; TIMEOUT_SECS=900; OUT=""; CLEAN=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1; shift ;;
    --clean) CLEAN=1; shift ;;
    --jobs) JOBS="$2"; shift 2 ;;
    --model) MODEL="$2"; shift 2 ;;
    --timeout) TIMEOUT_SECS="$2"; shift 2 ;;
    -*) die "unknown option: $1" ;;
    *) OUT="$1"; shift ;;
  esac
done

# ── --clean: drop leftover sandboxes (run.sh removes its own on success) ─────
if [ "$CLEAN" -eq 1 ]; then
  n=0
  for sb in "${TMPDIR:-/tmp}"/agend-eval-0b-*; do
    [ -d "$sb" ] || continue
    "$HERE/sandbox.sh" down "$sb" >/dev/null 2>&1 || true
    rm -rf "$sb"; n=$((n + 1))
  done
  echo "matrix.sh: removed $n leftover sandbox dir(s)"
  exit 0
fi

[ -n "$OUT" ] || die "usage: matrix.sh [--dry-run] [--jobs N] OUT_DIR"

# ── scenario resolution ─────────────────────────────────────────────────────
# SPEC §6 pins which scenarios run and how; the directory names are resolved by
# prefix so `S01` and `S01-inbox-drain` both work.  A scenario that is not on
# disk yet is still planned (and flagged), so the plan is reviewable before the
# scenario authors land their directories.
# The SPEC §6 plan, and nothing else. These were overridable from the
# environment so the harness could self-test its resume logic; an env var that
# can empty the frozen plan (and still exit 0 reporting a complete run of zero
# runs) is not a frozen plan.
CONFIRMATION="S01 S02 S03 S04 S05 S06"
MIXING="S13:mcp S14:cli"

resolve_dir() {
  local id="$1" d
  for d in "$HERE/scenarios/$id" "$HERE/scenarios/$id"-*; do
    [ -f "$d/meta.json" ] && { basename "$d"; return 0; }
  done
  return 1
}

meta_field() {
  python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2], sys.argv[3]))' \
    "$HERE/scenarios/$1/meta.json" "$2" "$3"
}

PLAN=""     # one "scenario<TAB>pair<TAB>arm<TAB>order" line per run
MISSING=""
add_run() { PLAN="${PLAN}${1}	${2}	${3}	${4}
"; }

for id in $CONFIRMATION; do
  if name="$(resolve_dir "$id")"; then
    [ "$(meta_field "$name" smoke False)" = "True" ] && continue
    pairs="$(meta_field "$name" pairs 10)"
  else
    name="$id"; pairs=10; MISSING="$MISSING $id"
  fi
  p=1
  while [ "$p" -le "$pairs" ]; do
    if [ $((p % 2)) -eq 1 ]; then
      add_run "$name" "$p" mcp first; add_run "$name" "$p" cli second
    else
      add_run "$name" "$p" cli first; add_run "$name" "$p" mcp second
    fi
    p=$((p + 1))
  done
done

for spec in $MIXING; do
  id="${spec%%:*}"; arm="${spec##*:}"
  if name="$(resolve_dir "$id")"; then
    [ "$(meta_field "$name" smoke False)" = "True" ] && continue
    pairs="$(meta_field "$name" pairs 45)"
  else
    name="$id"; pairs=45; MISSING="$MISSING $id"
  fi
  p=1
  while [ "$p" -le "$pairs" ]; do
    # run.sh derives order from parity x arm for every scenario, and SPEC §3
    # types metadata order_in_pair ("first"|"second"), so a single-arm scenario
    # records it too. Planning "only" here made the plan and the run disagree.
    if [ $((p % 2)) -eq 1 ]; then
      [ "$arm" = mcp ] && o=first || o=second
    else
      [ "$arm" = cli ] && o=first || o=second
    fi
    add_run "$name" "$p" "$arm" "$o"
    p=$((p + 1))
  done
done

TOTAL="$(printf '%s' "$PLAN" | grep -c . || true)"

# ── manifest ────────────────────────────────────────────────────────────────
STAMP="$(basename "$OUT")"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
GIT_HEAD="$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)"

# The manifest is this tree's account of itself. One already here must describe
# THIS matrix, or the tree belongs to another run and is not ours to overwrite.
if [ -f "$OUT/manifest.json" ]; then
  python3 -c '
import json, os, sys
here, repo, path, stamp, model, head, jobs = sys.argv[1:]
sys.path.insert(0, here)
import grade
try:
    with open(path, "r", encoding="utf-8") as fh:
        manifest = json.load(fh)
except (OSError, ValueError) as exc:
    sys.exit("unreadable manifest.json: %s" % exc)
bad = ["%s=%r (want %r)" % (key, manifest.get(key), want)
       for key, want in (("stamp", stamp), ("model", model), ("git_head", head),
                         ("jobs", int(jobs)))
       if manifest.get(key) != want]
# the same field/value table the grader applies to a finished tree
bad += ["%s is not what this matrix records" % field
        for field, ok in grade.MANIFEST_CONTRACT
        if field not in manifest or not ok(manifest[field])]
# the seed map lives outside that table because it needs the scenarios dir; the
# grader checks it, so the guard must too
if manifest.get("seed_sha256") != grade.frozen_seed_digests(os.path.join(here, "scenarios")):
    bad.append("seed_sha256 is not the frozen seed map")
# the digests must be of the binaries THIS matrix would run, not merely 64 hex
current = {name: grade.file_sha256(os.path.join(repo, "target/release", name))
           for name in ("agend-terminal", "agend-mcp-bridge")}
if manifest.get("binary_sha256") != current:
    bad.append("binary_sha256 is not the build this matrix would run")
if bad:
    sys.exit("; ".join(bad))
' "$HERE" "$REPO" "$OUT/manifest.json" "$STAMP" "$MODEL" "$GIT_HEAD" "$JOBS" \
    || die "refusing to overwrite $OUT/manifest.json: it does not describe this matrix"
fi

MF_OUT="$OUT" MF_STAMP="$STAMP" MF_MODEL="$MODEL" MF_GIT="$GIT_HEAD" \
MF_REPO="$REPO" MF_HERE="$HERE" MF_PLAN="$PLAN" MF_MISSING="$MISSING" \
MF_JOBS="$JOBS" MF_DRY="$DRY" \
python3 - > "$OUT/manifest.json" <<'PY'
import hashlib, json, os, sys, datetime
E = os.environ
sys.path.insert(0, E["MF_HERE"])
import grade

def sha(p):
    try:
        with open(p, "rb") as f:
            return hashlib.sha256(f.read()).hexdigest()
    except OSError:
        return None

plan = []
for line in E["MF_PLAN"].splitlines():
    if not line.strip():
        continue
    scenario, pair, arm, order = line.split("\t")
    plan.append({
        "scenario": scenario, "pair": int(pair), "arm": arm,
        "order_in_pair": order,
        "dir": f"{scenario}/pair-{int(pair):02d}/{arm}",
    })

repo, here = E["MF_REPO"], E["MF_HERE"]
json.dump({
    "schema": 1,
    "stamp": E["MF_STAMP"],
    "created_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "dry_run": E["MF_DRY"] == "1",
    "git_head": E["MF_GIT"],
    "model": E["MF_MODEL"],
    "jobs": int(E["MF_JOBS"]),
    "binary_sha256": {
        "agend-terminal": sha(os.path.join(repo, "target/release/agend-terminal")),
        "agend-mcp-bridge": sha(os.path.join(repo, "target/release/agend-mcp-bridge")),
    },
    "prompt_sha256": {
        n: sha(os.path.join(here, "prompts", f"{n}.txt")) for n in ("base", "mcp", "cli")
    },
    # the identity the runs will record, stated by the tree up front: the fleet
    # template sandbox.sh writes, and each planned scenario's seed script
    "fleet_sha256": grade.frozen_fleet_digest(),
    "seed_sha256": {
        row["scenario"]: sha(os.path.join(here, "scenarios", row["scenario"], "seed.sh"))
        for row in plan
    },
    "missing_scenarios": E["MF_MISSING"].split(),
    "total_runs": len(plan),
    "plan": plan,
}, os.sys.stdout, indent=2)
print()
PY

# ── print the plan ──────────────────────────────────────────────────────────
echo "matrix.sh plan  (stamp=$STAMP  git=$GIT_HEAD  model=$MODEL  jobs=$JOBS)"
echo "out: $OUT"
printf '%s' "$PLAN" | awk -F'\t' '{printf "  %-24s pair %-3s %-3s (%s)\n", $1, $2, $3, $4}'
echo "total runs: $TOTAL"
[ -n "$MISSING" ] && echo "WARNING: scenario dirs not on disk yet (planned from SPEC §6):$MISSING" >&2
echo "manifest: $OUT/manifest.json"

# ── resume bookkeeping (computed for --dry-run too, so the plan a dry run
# prints is exactly the work a real run would do) ───────────────────────────
resume_matches() {  # rundir scenario pair arm order
  python3 -c '
import json, os, sys
here, run_dir, scenario, pair, arm, order, model, head, manifest_path = sys.argv[1:]
sys.path.insert(0, here)
import grade
try:
    with open(os.path.join(run_dir, "metadata.json"), "r", encoding="utf-8") as fh:
        meta = json.load(fh)
except (OSError, ValueError) as exc:
    sys.exit("unreadable metadata.json: %s" % exc)
want = {"scenario": scenario, "arm": arm, "pair": int(pair),
        "order_in_pair": order, "model_requested": model,
        "model_resolved": model, "git_head": head}
bad = ["%s=%r (want %r)" % (k, meta.get(k), v) for k, v in want.items() if meta.get(k) != v]
try:
    with open(manifest_path, "r", encoding="utf-8") as fh:
        manifest = json.load(fh)
except (OSError, ValueError):
    manifest = {}
if manifest.get("binary_sha256") is not None and meta.get("binary_sha256") != manifest["binary_sha256"]:
    bad.append("binary_sha256 does not match the manifest")
events, stream_path = [], os.path.join(run_dir, "stream.jsonl")
if not os.path.exists(stream_path):
    bad.append("no stream.jsonl")
else:
    with open(stream_path, "r", encoding="utf-8") as fh:
        for line in fh:
            try:
                events.append(json.loads(line))
            except ValueError:
                pass
# A skipped run is a run claimed to have happened, so it must be one the grader
# would accept: the whole metadata contract, and a stream that names the model.
reason = grade.detect_invalid(meta, run_dir, None, False,
                              os.path.join(here, "scenarios"), events)
if reason:
    bad.append("would not grade: %s" % reason)
if bad:
    sys.exit("; ".join(bad))
' "$HERE" "$1" "$2" "$3" "$4" "$5" "$MODEL" "$GIT_HEAD" "$OUT/manifest.json"
}

QUEUE="$OUT/.queue"
: > "$QUEUE"
skipped=0; queued=0
while IFS=$'\t' read -r scenario pair arm order; do
  [ -n "$scenario" ] || continue
  rundir="$OUT/$scenario/pair-$(printf '%02d' "$pair")/$arm"
  if [ -f "$rundir/metadata.json" ]; then
    # Skipping a directory means claiming its run already happened, for THIS
    # matrix. Read it before believing that: a stale tree from another head, a
    # copied directory or a hand-written file used to count as complete.
    if ! resume_matches "$rundir" "$scenario" "$pair" "$arm" "$order"; then
      echo "resume refused: $scenario/pair-$(printf '%02d' "$pair")/$arm holds a metadata.json that is not this matrix's run" >&2
      exit 1
    fi
    skipped=$((skipped + 1)); continue
  fi
  if [ -d "$rundir" ] && [ -n "$(ls -A "$rundir" 2>/dev/null)" ]; then
    echo "refusing to overwrite: $scenario/pair-$(printf '%02d' "$pair")/$arm exists without a metadata.json" >&2
    exit 1
  fi
  printf '%s\t%s\t%s\n' "$scenario" "$pair" "$rundir" >> "$QUEUE"
  queued=$((queued + 1))
done <<< "$PLAN"
echo "resume: $skipped already complete, $queued to run"

if [ "$DRY" -eq 1 ]; then
  rm -f "$QUEUE"
  echo "--dry-run: nothing executed."
  exit 0
fi

[ -z "$MISSING" ] || die "refusing to execute: missing scenario dirs:$MISSING"

# ── execute ─────────────────────────────────────────────────────────────────

export MATRIX_HERE="$HERE" MATRIX_MODEL="$MODEL" MATRIX_TIMEOUT="$TIMEOUT_SECS"
# One NUL-terminated "scenario<TAB>pair<TAB>rundir" record per run; the child
# splits on TAB so paths containing spaces survive.
# shellcheck disable=SC2016
tr '\n' '\0' < "$QUEUE" | xargs -0 -P "$JOBS" -I REC bash -c '
  set -euo pipefail
  IFS="$(printf "\t")" read -r scenario pair rundir <<< "$1"
  arm="$(basename "$rundir")"
  mkdir -p "$rundir"
  "$MATRIX_HERE/run.sh" --arm "$arm" --scenario "$scenario" --pair "$pair" \
      --out "$rundir" --model "$MATRIX_MODEL" --timeout "$MATRIX_TIMEOUT" \
    >> "$rundir/run.log" 2>&1 \
    || echo "matrix.sh: HARNESS FAULT in $rundir (see run.log)" >&2
' _ REC

rm -f "$QUEUE"
echo "matrix.sh: done. grade with: python3 $HERE/grade.py --aggregate $OUT"
