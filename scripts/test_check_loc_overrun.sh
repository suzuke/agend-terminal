#!/usr/bin/env bash
# Sprint 61 W1 PR-3 (#P0-3) — smoke test for check_loc_overrun.sh.
# Each scenario sets PR_BODY + PR_DIFF_LOC + PR_LABELS env vars,
# invokes the helper, and asserts the exit code matches expectation.
#
# Usage: ./scripts/test_check_loc_overrun.sh
# Exits 0 on all-pass, 1 on first failing scenario.

set -uo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
helper="$script_dir/check_loc_overrun.sh"

pass=0
fail=0

assert_exit() {
    local expected="$1"
    local label="$2"
    shift 2
    local actual=0
    "$@" >/dev/null 2>&1 || actual=$?
    if [[ "$actual" -eq "$expected" ]]; then
        echo "PASS  $label (exit=$actual)"
        pass=$((pass + 1))
    else
        echo "FAIL  $label (expected exit=$expected, got=$actual)"
        fail=$((fail + 1))
    fi
}

assert_exit 0 "under range" \
    env PR_BODY='<!-- LOC-EST: 100-200 -->' PR_DIFF_LOC=150 PR_LABELS= "$helper"

assert_exit 0 "exactly at upper bound" \
    env PR_BODY='<!-- LOC-EST: 100-200 -->' PR_DIFF_LOC=200 PR_LABELS= "$helper"

assert_exit 0 "soft-warn (above upper, within 130%)" \
    env PR_BODY='<!-- LOC-EST: 100-200 -->' PR_DIFF_LOC=240 PR_LABELS= "$helper"

assert_exit 0 "soft-warn (at exact 130% boundary)" \
    env PR_BODY='<!-- LOC-EST: 100-200 -->' PR_DIFF_LOC=260 PR_LABELS= "$helper"

assert_exit 0 "soft-warn (above 130%, within 150%)" \
    env PR_BODY='<!-- LOC-EST: 100-200 -->' PR_DIFF_LOC=290 PR_LABELS= "$helper"

assert_exit 1 "hard-fail without override" \
    env PR_BODY='<!-- LOC-EST: 100-200 -->' PR_DIFF_LOC=350 PR_LABELS=bug "$helper"

assert_exit 0 "hard-fail with cohesion-accept override label" \
    env PR_BODY='<!-- LOC-EST: 100-200 -->' PR_DIFF_LOC=350 PR_LABELS='bug,loc-overrun-accepted' "$helper"

assert_exit 0 "no marker present (skip)" \
    env PR_BODY='no marker here' PR_DIFF_LOC=999 PR_LABELS= "$helper"

assert_exit 0 "marker with whitespace tolerance" \
    env PR_BODY='<!--   LOC-EST:  50-100   -->' PR_DIFF_LOC=80 PR_LABELS= "$helper"

echo
echo "summary: $pass passed, $fail failed"
if [[ "$fail" -gt 0 ]]; then
    exit 1
fi
exit 0
