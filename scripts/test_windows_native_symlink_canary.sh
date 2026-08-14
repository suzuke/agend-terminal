#!/usr/bin/env bash
# #3248 — diagnostic-only Windows native-symlink readiness canary.
#
# The caller deliberately runs this with continue-on-error. A failing shape is
# evidence that the runner premise is unavailable; it must not silently turn
# the existing coverage contracts fail-closed before a live canary proves the
# premise is ready.

set -u
set -o pipefail

# Keep any runner-provided MSYS options and append the native-symlink mode only
# for this process. In particular, do not change checkout or later CI steps.
existing_msys="${MSYS:-}"
case " $existing_msys " in
    *" winsymlinks:nativestrict "*) ;;
    *)
        if [ -n "$existing_msys" ]; then
            MSYS="$existing_msys winsymlinks:nativestrict"
        else
            MSYS="winsymlinks:nativestrict"
        fi
        export MSYS
        ;;
esac

root="$(mktemp -d "${TMPDIR:-/tmp}/agend-native-symlink-canary.XXXXXX" 2>/dev/null)" || {
    printf 'CANARY FAIL setup (temporary directory unavailable)\n'
    exit 1
}

passed=0
failures=0
cleanup() {
    rm -rf "$root"
}
trap cleanup EXIT

report_pass() {
    printf 'CANARY PASS %s\n' "$1"
    passed=$((passed + 1))
}

report_fail() {
    printf 'CANARY FAIL %s (%s)\n' "$1" "$2"
    failures=$((failures + 1))
}

# Existing-target file link: prove the directory entry is a native symlink,
# not an MSYS copy of the target.
file_target="$root/file-target"
file_link="$root/file-link"
if printf 'file-target\n' >"$file_target" 2>/dev/null &&
    ln -s "$file_target" "$file_link" 2>/dev/null &&
    [ -L "$file_link" ]; then
    report_pass "file target"
else
    report_fail "file target" "native symlink unavailable"
fi

# Dangling file link: a live-link probe alone is insufficient because a copied
# target can make the existing-file case look healthy.
dangling_link="$root/dangling-link"
if ln -s "$root/target-does-not-exist" "$dangling_link" 2>/dev/null &&
    [ -L "$dangling_link" ] && [ ! -e "$dangling_link" ]; then
    report_pass "dangling target"
else
    report_fail "dangling target" "native dangling symlink unavailable"
fi

# Directory link: verify the native link can resolve to a directory as well as
# preserving the file-link and dangling-link premises above.
directory_target="$root/directory-target"
directory_link="$root/directory-link"
if mkdir "$directory_target" 2>/dev/null &&
    ln -s "$directory_target" "$directory_link" 2>/dev/null &&
    [ -L "$directory_link" ] && [ -d "$directory_link" ]; then
    report_pass "directory target"
else
    report_fail "directory target" "native directory symlink unavailable"
fi

printf 'CANARY SUMMARY %s passed, %s failed\n' "$passed" "$failures"
exit "$failures"
