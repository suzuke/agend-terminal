#!/usr/bin/env bash
# Sprint 61 W1 PR-3 (#P0-3) — LOC overrun check.
#
# Closes the Sprint 60 #582 Component 2 deferral. Parses a
# `<!-- LOC-EST: lower-upper -->` marker from the PR description body,
# compares the actual diff LOC against the upper bound, and applies the
# methodology's soft-warn / hard-fail thresholds (per #582 §5).
#
# Reviewer cohesion-accept override: presence of the
# `loc-overrun-accepted` label on the PR makes the script exit 0 even
# when the hard-fail threshold is crossed.
#
# Usage (CI): invoked by `.github/workflows/loc-overrun-check.yml` with
# the PR body in stdin and labels + numbers in env vars.
#
#   PR_BODY=...      # PR description body (input via stdin OR env)
#   PR_LABELS=...    # comma-separated label list (e.g. "bug,loc-overrun-accepted")
#   PR_DIFF_LOC=...  # actual added LOC (override; otherwise computed via gh)
#   PR_NUMBER=...    # PR number for `gh pr diff` lookup if PR_DIFF_LOC unset
#
#   LOC_SOFT_WARN_PCT=130   # soft-warn threshold (default 130%)
#   LOC_HARD_FAIL_PCT=150   # hard-fail threshold (default 150%)
#
# Exit codes: 0 = pass / soft-warn / override; 1 = hard-fail.

set -euo pipefail

soft_pct="${LOC_SOFT_WARN_PCT:-130}"
hard_pct="${LOC_HARD_FAIL_PCT:-150}"

body="${PR_BODY:-}"
labels="${PR_LABELS:-}"

# Read body from stdin if env unset (allows piping).
if [[ -z "$body" && ! -t 0 ]]; then
    body="$(cat)"
fi

# Parse `<!-- LOC-EST: lower-upper -->` (whitespace-tolerant inside).
# If absent: skip with note (exit 0); pre-existing PRs predate the
# convention.
marker=$(echo "$body" | grep -oE '<!--[[:space:]]*LOC-EST:[[:space:]]*[0-9]+-[0-9]+[[:space:]]*-->' | head -1 || true)
if [[ -z "$marker" ]]; then
    echo "[loc-overrun] no <!-- LOC-EST: X-Y --> marker in PR body — skipping check." >&2
    echo "[loc-overrun] (To enforce: add the marker per docs/PROCESS-LOC-ESTIMATION-METHODOLOGY.md §5.1)" >&2
    exit 0
fi
range=$(echo "$marker" | grep -oE '[0-9]+-[0-9]+')
lower="${range%-*}"
upper="${range#*-}"

# Compute actual diff LOC (additions). Caller may pre-compute via
# PR_DIFF_LOC for tests; otherwise fall back to `gh pr diff --name-only`
# parsed counts when PR_NUMBER is set. Unset both → 0 (no PR context).
actual="${PR_DIFF_LOC:-}"
if [[ -z "$actual" ]]; then
    if [[ -n "${PR_NUMBER:-}" ]] && command -v gh >/dev/null 2>&1; then
        actual=$(gh pr diff "$PR_NUMBER" 2>/dev/null \
            | grep -cE '^\+[^+]' || true)
    else
        echo "[loc-overrun] no PR_DIFF_LOC + no PR_NUMBER — cannot compute actual LOC." >&2
        exit 0
    fi
fi
actual=$((actual + 0))  # coerce numeric

# Override: cohesion-accept label bypasses hard-fail.
override=0
case ",$labels," in
    *,loc-overrun-accepted,*) override=1 ;;
esac

# Compute thresholds.
soft_threshold=$(( upper * soft_pct / 100 ))
hard_threshold=$(( upper * hard_pct / 100 ))

echo "[loc-overrun] estimate range: $lower-$upper LOC"
echo "[loc-overrun] actual diff LOC: $actual"
echo "[loc-overrun] soft-warn threshold ($soft_pct%): $soft_threshold"
echo "[loc-overrun] hard-fail threshold ($hard_pct%): $hard_threshold"

if (( actual <= upper )); then
    echo "[loc-overrun] PASS — within estimate range."
    exit 0
elif (( actual <= soft_threshold )); then
    echo "[loc-overrun] PASS — over upper bound but within soft-warn range." >&2
    exit 0
elif (( actual <= hard_threshold )); then
    echo "[loc-overrun] SOFT-WARN — actual ($actual) exceeds soft-warn threshold ($soft_threshold)." >&2
    echo "[loc-overrun] PR description should include a Scope-overage transparency section per #582 §5.2." >&2
    exit 0
elif (( override == 1 )); then
    echo "[loc-overrun] HARD-FAIL OVERRIDE — actual ($actual) > hard-fail threshold ($hard_threshold), but loc-overrun-accepted label present (cohesion-accept option (a))." >&2
    exit 0
else
    echo "[loc-overrun] HARD-FAIL — actual ($actual) > hard-fail threshold ($hard_threshold)." >&2
    echo "[loc-overrun] Per #582 §5.3: lead/general escalation required before merge." >&2
    echo "[loc-overrun] Apply 'loc-overrun-accepted' label after reviewer cohesion-accept verdict to override." >&2
    exit 1
fi
