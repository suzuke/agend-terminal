#!/usr/bin/env bash
# Sprint 58 Wave 3 PR-1 (#12) — cross-platform clippy gate.
#
# Local pre-push verification helper: runs `cargo clippy` against each
# common target triple so cfg-gated branches (Windows .exe handling,
# Linux-only tray deps, macOS-specific paths) get linted before push,
# closing the fix-forward cycles that burned Sprint 57 Wave 3 PR-2 r1+r2
# and Wave 3 PR-3 r0.
#
# Usage:
#   scripts/clippy-all-platforms.sh             # all common targets
#   scripts/clippy-all-platforms.sh --quick     # only host triple
#   scripts/clippy-all-platforms.sh --strict    # error on missing targets
#                                                 (CI mode)
#
# Exit codes:
#   0  — every available target passed clippy clean
#   1  — at least one target failed clippy
#   2  — invocation error (rustup unavailable, invalid args, etc.)
#
# Limitations:
#   - cargo clippy lints during analysis but does NOT link, so crates
#     with C dependencies (gtk, openssl-sys) MAY fail at the build-script
#     stage when targeting a non-host triple. The script catches and
#     skips these with a clear "build-script failure" warning.
#   - Full cross-compile linking still needs `cross` (Docker required) or
#     CI matrix coverage. This script bridges the lint gap, not the
#     link gap.
#   - Rationale: see docs/LINT-DISCIPLINE.md for the patterns the cross-
#     platform gate is designed to catch.

set -euo pipefail

readonly SCRIPT_NAME="${0##*/}"

# Common targets covering the 3 CI matrix platforms. Linux uses gnu;
# Windows targets gnu (mingw) since msvc requires Visual Studio toolchain
# which dev machines rarely have. macOS covers both Intel and Apple Silicon.
readonly DEFAULT_TARGETS=(
    "x86_64-unknown-linux-gnu"
    "x86_64-pc-windows-gnu"
    "x86_64-apple-darwin"
    "aarch64-apple-darwin"
)

# Default cargo clippy invocation matches CI's:
#   cargo clippy --all-targets --features tray -- -D warnings
readonly DEFAULT_CLIPPY_ARGS=(
    "--all-targets"
    "--features" "tray"
    "--"
    "-D" "warnings"
)

mode="all"
strict_missing="false"

while (( $# > 0 )); do
    case "$1" in
        --quick) mode="quick"; shift ;;
        --strict) strict_missing="true"; shift ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "[$SCRIPT_NAME] unknown arg: $1" >&2
            exit 2
            ;;
    esac
done

if ! command -v rustup >/dev/null 2>&1; then
    echo "[$SCRIPT_NAME] rustup not found in PATH" >&2
    echo "  install: https://rustup.rs/" >&2
    exit 2
fi

host_triple="$(rustc -vV | awk -F': ' '/host:/{print $2}')"
if [[ -z "$host_triple" ]]; then
    echo "[$SCRIPT_NAME] could not determine host triple" >&2
    exit 2
fi

# Decide which targets to actually run.
if [[ "$mode" == "quick" ]]; then
    targets=("$host_triple")
else
    targets=("${DEFAULT_TARGETS[@]}")
fi

# Detect installed targets so we can skip + remediate missing ones.
# `mapfile` is bash 4+; macOS ships bash 3.2, so we use a `while read`
# loop for portability (script must run on both dev machines + CI Linux).
installed_targets=()
while IFS= read -r line; do
    [[ -n "$line" ]] && installed_targets+=("$line")
done < <(rustup target list --installed 2>/dev/null || true)

is_installed() {
    local t="$1"
    local i
    for i in "${installed_targets[@]}"; do
        [[ "$i" == "$t" ]] && return 0
    done
    return 1
}

# Track outcomes.
passed=()
failed=()
skipped_missing=()
skipped_buildscript=()

run_clippy_for_target() {
    local target="$1"
    local logfile
    logfile="$(mktemp -t "clippy-${target}.XXXXXX")"

    echo
    echo "──────────────────────────────────────────────────────────────"
    echo "[$SCRIPT_NAME] target=$target"
    echo "──────────────────────────────────────────────────────────────"

    if cargo clippy --target "$target" "${DEFAULT_CLIPPY_ARGS[@]}" 2>&1 | tee "$logfile"; then
        passed+=("$target")
        rm -f "$logfile"
        return 0
    fi

    # Distinguish build-script failures (C dep missing for non-host
    # target — a known limitation, not a lint failure) from real lint
    # failures. The former gets categorized as a skip + warn so dev
    # doesn't get blocked on unfixable cross-link issues; the latter
    # blocks.
    if grep -qE "(failed to run custom build command|linker .* not found|cc: .*: No such file|ld: framework not found)" "$logfile"; then
        skipped_buildscript+=("$target")
        rm -f "$logfile"
        return 0
    fi

    failed+=("$target")
    rm -f "$logfile"
    return 1
}

# Main loop.
for target in "${targets[@]}"; do
    if ! is_installed "$target"; then
        skipped_missing+=("$target")
        continue
    fi
    run_clippy_for_target "$target" || true
done

# ────────────────────────────────────────────────────────────
# Reporting.
# ────────────────────────────────────────────────────────────
echo
echo "══════════════════════════════════════════════════════════════"
echo "[$SCRIPT_NAME] Summary"
echo "══════════════════════════════════════════════════════════════"
echo "  passed        (${#passed[@]}): ${passed[*]:-<none>}"
echo "  failed        (${#failed[@]}): ${failed[*]:-<none>}"
echo "  skipped (missing target, ${#skipped_missing[@]}):"
for t in "${skipped_missing[@]:-}"; do
    [[ -n "$t" ]] && printf "    - %-30s rustup target add %s\n" "$t" "$t"
done
echo "  skipped (build-script C-dep, ${#skipped_buildscript[@]}):"
for t in "${skipped_buildscript[@]:-}"; do
    [[ -n "$t" ]] && printf "    - %s (rely on CI matrix for full link)\n" "$t"
done
echo

# Exit semantics.
if (( ${#failed[@]} > 0 )); then
    echo "[$SCRIPT_NAME] CLIPPY FAILED on: ${failed[*]}" >&2
    exit 1
fi

if [[ "$strict_missing" == "true" ]] && (( ${#skipped_missing[@]} > 0 )); then
    echo "[$SCRIPT_NAME] STRICT mode: missing targets count as failure" >&2
    exit 1
fi

if (( ${#passed[@]} == 0 && ${#skipped_buildscript[@]} == 0 )); then
    echo "[$SCRIPT_NAME] no targets actually ran clippy — install at least one" >&2
    exit 2
fi

echo "[$SCRIPT_NAME] OK"
exit 0
