#!/usr/bin/env bash
# scripts/test_windows_symlink_canary.sh — contract tests for the #3248 Windows
# native-symlink readiness canary (scripts/windows_symlink_canary.sh).
#
# The canary exists to OBSERVE, on the real hosted Windows image, three facts no
# documentation settles: whether native symlinks can be created at all
# (SeCreateSymbolicLinkPrivilege is Administrators-only and UAC-guarded), whether
# `winsymlinks:nativestrict` accepts a DANGLING target, and whether it accepts a
# DIRECTORY target. Until those are observed, nothing may be made fail-closed.
#
# What is testable OFF Windows is the part that can silently be wrong there:
#   - the MSYS composition (MSYS is a whitespace-separated MULTI-option variable,
#     so a naive `export MSYS=winsymlinks:nativestrict` DISCARDS whatever the
#     runner already set, and a naive append leaves two conflicting winsymlinks
#     tokens whose precedence is undefined);
#   - that the probe can actually SEE a link when one exists — a probe that
#     always answers "no" would look identical to a Windows failure and would
#     send the whole issue down a false path;
#   - that the canary is observation-only: exit 0, no residue.
# Those are asserted on a POSIX host, where real symlinks are guaranteed, so the
# Windows run has a trustworthy instrument rather than an untested one.
#
# Usage: scripts/test_windows_symlink_canary.sh   # 0 all-pass, 1 any failure.
set -uo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
canary="$script_dir/windows_symlink_canary.sh"

pass=0
fail=0
# Both report helpers RETURN 0 by contract. The assertions below use the
# `cond && ok ... || bad ...` shape, which would double-count if `ok` could ever
# fail; the explicit return makes that impossible rather than incidental.
ok() {
    echo "PASS  $1"
    pass=$((pass + 1))
    return 0
}
bad() {
    echo "FAIL  $1"
    [ -n "${2:-}" ] && echo "      $2"
    fail=$((fail + 1))
    return 0
}

[ -x "$canary" ] || {
    echo "test_windows_symlink_canary: $canary missing or not executable" >&2
    exit 1
}

# ── MSYS composition ──────────────────────────────────────────────────────
# `--print-msys` prints the value the canary WOULD export, so the composition
# is checked directly instead of being inferred from probe outcomes.

compose() {
    # $1 = MSYS value to compose from; unset the variable entirely when absent.
    if [ "$#" -eq 0 ]; then
        env -u MSYS "$canary" --print-msys
    else
        MSYS="$1" "$canary" --print-msys
    fi
}

got="$(compose)"
[ "$got" = "winsymlinks:nativestrict" ] &&
    ok "an unset MSYS composes to exactly the strict option" ||
    bad "an unset MSYS composes to exactly the strict option" "got: $got"

got="$(compose "enable_pcon")"
case " $got " in
*" enable_pcon "*)
    case " $got " in
    *" winsymlinks:nativestrict "*)
        ok "an unrelated pre-existing option is preserved, not clobbered"
        ;;
    *) bad "an unrelated pre-existing option is preserved, not clobbered" "got: $got" ;;
    esac
    ;;
*) bad "an unrelated pre-existing option is preserved, not clobbered" "got: $got" ;;
esac

got="$(compose "winsymlinks:deepcopy")"
case "$got" in
*deepcopy*) bad "a pre-existing winsymlinks mode is REPLACED, not left to race" "got: $got" ;;
*)
    [ "$got" = "winsymlinks:nativestrict" ] &&
        ok "a pre-existing winsymlinks mode is REPLACED, not left to race" ||
        bad "a pre-existing winsymlinks mode is REPLACED, not left to race" "got: $got"
    ;;
esac

got="$(compose "alpha winsymlinks:lnk beta")"
[ "$got" = "alpha beta winsymlinks:nativestrict" ] &&
    ok "replacing a mid-list mode preserves the other options in order" ||
    bad "replacing a mid-list mode preserves the other options in order" "got: $got"

got="$(compose "winsymlinks:nativestrict")"
# shellcheck disable=SC2086  # deliberate: split the option list into one token per line
occurrences="$(printf '%s\n' $got | grep -c '^winsymlinks:nativestrict$')"
[ "$got" = "winsymlinks:nativestrict" ] && [ "$occurrences" -eq 1 ] &&
    ok "composition is idempotent — the option never doubles" ||
    bad "composition is idempotent — the option never doubles" "got: $got ($occurrences occurrences)"

got="$(compose "$(printf 'alpha\twinsymlinks:deepcopy')")"
[ "$got" = "alpha winsymlinks:nativestrict" ] &&
    ok "a TAB-separated option list is handled like a space-separated one" ||
    bad "a TAB-separated option list is handled like a space-separated one" "got: $got"

# ── observation-only contract ─────────────────────────────────────────────

out="$("$canary" 2>&1)"
rc=$?
[ "$rc" -eq 0 ] &&
    ok "the canary never fails the job — it reports, it does not gate" ||
    bad "the canary never fails the job — it reports, it does not gate" "exit $rc"

missing=""
for probe in file-target dangling-target directory-target; do
    echo "$out" | grep -q "probe $probe:" || missing="$missing $probe"
done
[ -z "$missing" ] &&
    ok "all three link shapes are probed and reported" ||
    bad "all three link shapes are probed and reported" "no line for:$missing"

echo "$out" | grep -q "MSYS before:" &&
    ok "the prior MSYS value is disclosed" ||
    bad "the prior MSYS value is disclosed" "no 'MSYS before:' line"

# The instrument itself: on a POSIX host every shape MUST come back yes. A probe
# that cannot see a link it just created would report the Windows image as
# incapable no matter what the image actually does.
summary="$(echo "$out" | grep 'summary:' | head -1)"
[ "${summary#*summary: }" = "file=yes dangling=yes directory=yes" ] &&
    ok "on a POSIX host every shape is observed as a real link" ||
    bad "on a POSIX host every shape is observed as a real link" "summary line: $summary"

# Bounded output: a canary that dumps an unbounded error into the CI log is a
# different failure mode from the one it is diagnosing.
longest="$(echo "$out" | awk '{ if (length($0) > m) m = length($0) } END { print m + 0 }')"
[ "$longest" -le 240 ] &&
    ok "every reported line stays bounded" ||
    bad "every reported line stays bounded" "longest line: $longest chars"

leftover="$(find "${TMPDIR:-/tmp}" -maxdepth 1 -name 'agend-3248-canary-*' 2>/dev/null | head -1)"
[ -z "$leftover" ] &&
    ok "the probe sandbox is removed" ||
    bad "the probe sandbox is removed" "left behind: $leftover"

echo "windows-symlink-canary contract: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
