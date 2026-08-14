#!/usr/bin/env bash
# scripts/windows_symlink_canary.sh — #3248 native-symlink readiness probe.
#
# WHY THIS EXISTS, AND WHY IT NEVER FAILS THE JOB
#
# scripts/test_coverage_run.sh skips 14 symlink contracts on the Windows runner
# because Git Bash's `ln -s` deep-copies by default, so `[ -L ]` is false and the
# premise is honestly reported unavailable. The obvious fix — export
# MSYS=winsymlinks:nativestrict and make those premises fail-closed — rests on
# three facts that NO documentation settles:
#
#   1. whether this hosted image can create a native symlink AT ALL.
#      SeCreateSymbolicLinkPrivilege is assigned only to Administrators and is
#      guarded by UAC; Administrators still need elevation unless Developer Mode
#      is on (git-for-windows.github.io, content/symbolic-links.md).
#   2. whether nativestrict accepts a DANGLING target. Windows symlinks are
#      TYPED — file or directory — and with no target there is nothing to type
#      from. msys2.org documents the target-must-exist rule for deepcopy only;
#      the Cygwin winsymlinks section does not address non-existent targets.
#      Four of the 14 contracts need dangling links.
#   3. whether it accepts a DIRECTORY target, a separate creation path. One
#      contract (test_relative_missing_profile_dir_under_symlinked_cwd) needs it.
#
# Guessing wrong on any of the three turns 14 honest skips into a permanently RED
# required job. So this step OBSERVES and reports; it always exits 0 and the
# workflow marks it continue-on-error. Nothing is made fail-closed until the log
# from a real run answers all three.
#
# The probes are NOT gated on the platform — they run wherever they are invoked.
# On a POSIX host all three necessarily succeed, which is what makes the
# instrument trustworthy (see scripts/test_windows_symlink_canary.sh); the
# Windows-only part is the workflow's `if: runner.os == 'Windows'`.
#
# Usage:
#   scripts/windows_symlink_canary.sh                # probe and report; exit 0
#   scripts/windows_symlink_canary.sh --print-msys   # print composed MSYS; exit 0
set -uo pipefail

TAG='canary(#3248)'
NOTE_MAX=160

# MSYS is a WHITESPACE-SEPARATED MULTI-OPTION variable. Overwriting it would
# discard whatever the runner already set, and appending blindly would leave two
# winsymlinks tokens whose precedence is undefined — so drop any existing
# winsymlinks mode, keep every other option in order, then append the strict one.
compose_msys() {
    local out='' tok
    # shellcheck disable=SC2086  # deliberate: MSYS is an IFS-separated option list
    for tok in ${MSYS-}; do
        case "$tok" in
        winsymlinks:*) continue ;;
        esac
        out="${out:+$out }$tok"
    done
    printf '%s\n' "${out:+$out }winsymlinks:nativestrict"
}

if [ "${1:-}" = '--print-msys' ]; then
    compose_msys
    exit 0
fi

# One line per probe, each bounded: a canary that floods the log with an
# unbounded error is a different failure mode from the one it is diagnosing.
bounded_note() {
    local text
    text="$(printf '%s' "$1" | tr '\n\r\t' '   ' | cut -c "1-$NOTE_MAX")"
    printf '%s' "${text:--}"
}

sandbox="$(mktemp -d "${TMPDIR:-/tmp}/agend-3248-canary-XXXXXX" 2>/dev/null)" || sandbox=''
if [ -z "$sandbox" ] || [ ! -d "$sandbox" ]; then
    echo "$TAG summary: file=unknown dangling=unknown directory=unknown"
    echo "$TAG note: sandbox could not be created; nothing was probed"
    exit 0
fi
trap 'rm -rf "$sandbox"' EXIT

echo "$TAG platform: $(uname -s) image=${ImageOS:-<unset>} bash=${BASH_VERSION:-<unset>}"
echo "$TAG MSYS before: ${MSYS-<unset>}"
MSYS="$(compose_msys)"
export MSYS
echo "$TAG MSYS after: $MSYS"

# $1 label, $2 link target, $3 link path. Records the ln exit status AND what
# actually landed on disk, because they disagree in the case this is chasing:
# deepcopy EXITS ZERO and leaves a regular file, which is kind=copy, not a link.
#
# The verdict comes back in PROBE_IS_LINK rather than on stdout: stdout is the
# CI log, and a probe that returned its answer through a command substitution
# would swallow the very report line the step exists to produce.
PROBE_IS_LINK=no
probe() {
    local label="$1" target="$2" link="$3" err rc kind
    err="$(ln -s "$target" "$link" 2>&1 >/dev/null)"
    rc=$?
    if [ -L "$link" ]; then
        kind='link'
        PROBE_IS_LINK=yes
    elif [ -e "$link" ]; then
        kind='copy'
        PROBE_IS_LINK=no
    else
        kind='absent'
        PROBE_IS_LINK=no
    fi
    printf '%s probe %s: ln_exit=%s is_link=%s kind=%s note=%s\n' \
        "$TAG" "$label" "$rc" "$PROBE_IS_LINK" "$kind" "$(bounded_note "$err")"
}

printf 'target contents' >"$sandbox/live-target.txt"
mkdir -p "$sandbox/live-dir"

probe file-target "$sandbox/live-target.txt" "$sandbox/link-to-file"
file_ok="$PROBE_IS_LINK"
probe dangling-target "$sandbox/never-created.bin" "$sandbox/link-to-missing"
dangling_ok="$PROBE_IS_LINK"
probe directory-target "$sandbox/live-dir" "$sandbox/link-to-dir"
directory_ok="$PROBE_IS_LINK"

echo "$TAG summary: file=$file_ok dangling=$dangling_ok directory=$directory_ok"
echo "$TAG verdict: observation only — this step never gates the job (#3248)"
exit 0
