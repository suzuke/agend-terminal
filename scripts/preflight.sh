#!/usr/bin/env bash
# #1490 — one-shot CI-parity preflight.
#
# Mirrors the GitHub Actions `check` job (.github/workflows/ci.yml) locally so
# the local-green -> CI-red round-trip stops costing a push + a ~10min wait.
# Born from the retro HIGH point: a single session burned 5 CI-red cycles on
# problems a local check would have caught — Windows-only compile errors,
# fmt drift, tray-gated failures, and the 750-LOC file_size_invariant.
#
# Runs, in order — the fmt/clippy surface MATCHES CI's `check` job (task83/d-46):
#   1. scripts/fmt-owned.sh --check   (owned *.rs, vendor/ excluded — CI's exact surface)
#   2. cargo clippy <owned targets + agentic-git wrapper> --features tray -- -D warnings  (CI's exact targets)
#   3. cargo test --tests --features tray   (unit + integration + invariants)
#      NOTE: CI runs these via `nextest`, not `cargo test` — the test SELECTION
#      matches but the runner differs, and a floating stable toolchain is not
#      byte-exact over time. "Green here" is a strong pre-check, not a byte-exact
#      guarantee of CI.
#   4. Windows cross-check (x86_64-pc-windows-msvc)   <- the keystone
#
# Step 4 catches the class that hurts most: Windows-only code
# (libc::getppid, /bin/sh spawns, UnixStream) compiles fine on a unix dev
# box but breaks CI's windows-latest runner. There is a wrinkle — a plain
# `cargo check --target x86_64-pc-windows-msvc` cannot complete on a stock
# macOS/Linux box because a transitive C dependency (`ring`, via TLS) needs
# the Windows C toolchain (`assert.h` et al.) and its build script aborts
# before our crate is ever type-checked. So this step prefers `cargo xwin`
# (bundles the MSVC CRT/SDK) and degrades gracefully:
#   - `cargo-xwin` installed  -> `cargo xwin check` (real, complete check)
#   - else plain `cargo check` -> works only if the host has a Windows
#     toolchain; on a C-dep build-script failure it SKIPS with a hint
#     rather than reporting a false failure.
#   - target not installed     -> SKIP + `rustup target add` hint.
#
# Usage:
#   scripts/preflight.sh            # full matrix (default)
#   scripts/preflight.sh --quick    # skip the Windows cross-check (host only)
#   scripts/preflight.sh -h|--help
#
# Exit codes:
#   0  — every step that ran passed (a skipped Windows step is still 0)
#   1  — at least one step failed
#   2  — invocation / environment error (cargo missing, bad arg)
#
# NOT a git hook: the full matrix takes minutes; wiring it into pre-push
# would tax every push. Run it manually before pushing (see CLAUDE.md).
# CI stays the source of truth — this just front-runs it.

set -uo pipefail

readonly SCRIPT_NAME="${0##*/}"
readonly WINDOWS_TARGET="x86_64-pc-windows-msvc"

run_quick="false"
while (( $# > 0 )); do
    case "$1" in
        --quick) run_quick="true"; shift ;;
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

if ! command -v cargo >/dev/null 2>&1; then
    echo "[$SCRIPT_NAME] cargo not found in PATH" >&2
    echo "  install: https://rustup.rs/" >&2
    exit 2
fi

# Run from repo root so cargo finds the manifest regardless of CWD.
cd "$(dirname "$0")/.." || exit 2

passed=()
failed=()
skipped=()

banner() {
    echo
    echo "──────────────────────────────────────────────────────────────"
    echo "[$SCRIPT_NAME] $1"
    echo "──────────────────────────────────────────────────────────────"
}

# step "<label>" cmd args...   — runs cmd, records pass/fail, never aborts the
# script (run-all so the dev sees every problem in one pass, not one at a time).
step() {
    local label="$1"; shift
    banner "$label"
    echo "  \$ $*"
    if "$@"; then
        passed+=("$label")
    else
        failed+=("$label")
    fi
}

step "fmt --check (owned surface)" \
    scripts/fmt-owned.sh --check
step "clippy (owned targets --features tray -D warnings)" \
    cargo clippy --lib --bin agend-terminal --bin agend-git --bin agend-mcp-bridge --bin agentic-git --tests --examples --features tray -- -D warnings
step "test (--tests --features tray: unit + integration + invariants)" \
    cargo test --tests --features tray

# ── Step 4: Windows cross-check ──────────────────────────────────────────
windows_check() {
    if [[ "$run_quick" == "true" ]]; then
        skipped+=("windows check ($WINDOWS_TARGET) — --quick")
        return
    fi

    if command -v rustup >/dev/null 2>&1 \
        && ! rustup target list --installed 2>/dev/null | grep -qx "$WINDOWS_TARGET"; then
        banner "windows check ($WINDOWS_TARGET) — target not installed, SKIP"
        echo "  enable (catches windows-only compile errors locally):"
        echo "      rustup target add $WINDOWS_TARGET"
        skipped+=("windows check ($WINDOWS_TARGET) — target not installed")
        return
    fi

    # Preferred path: cargo-xwin bundles the MSVC CRT/SDK so the `ring` C
    # build script (and every other Windows compile) succeeds on a unix host.
    if command -v cargo-xwin >/dev/null 2>&1; then
        step "windows check (cargo xwin check --target $WINDOWS_TARGET --all-targets --features tray)" \
            cargo xwin check --target "$WINDOWS_TARGET" --all-targets --features tray
        return
    fi

    # Fallback: plain cargo check. Works only when the host already has a
    # Windows C toolchain. On a C-dep build-script failure (the `ring`
    # assert.h wall) we SKIP with a hint instead of false-failing.
    banner "windows check (cargo check --target $WINDOWS_TARGET --all-targets --features tray)"
    local log
    log="$(mktemp -t "preflight-win.XXXXXX")"
    if cargo check --target "$WINDOWS_TARGET" --all-targets --features tray 2>&1 | tee "$log"; then
        passed+=("windows check ($WINDOWS_TARGET)")
        rm -f "$log"
        return
    fi

    if grep -qE "(error occurred in cc-rs|failed to run custom build command|fatal error: '.*\.h' file not found|linker .* not found)" "$log"; then
        echo
        echo "[$SCRIPT_NAME] windows check blocked by a C-dependency build script"
        echo "  (e.g. 'ring' needs the Windows C toolchain) — this is NOT your code."
        echo "  for a real local Windows check on unix, install cargo-xwin:"
        echo "      cargo install cargo-xwin && rustup target add $WINDOWS_TARGET"
        echo "  (otherwise CI's windows-latest runner is the backstop.)"
        skipped+=("windows check ($WINDOWS_TARGET) — C-dep toolchain missing; install cargo-xwin")
        rm -f "$log"
        return
    fi

    failed+=("windows check ($WINDOWS_TARGET)")
    rm -f "$log"
}
windows_check

# ── Summary ──────────────────────────────────────────────────────────────
echo
echo "══════════════════════════════════════════════════════════════"
echo "[$SCRIPT_NAME] Summary"
echo "══════════════════════════════════════════════════════════════"
echo "  passed  (${#passed[@]}): ${passed[*]:-<none>}"
echo "  failed  (${#failed[@]}): ${failed[*]:-<none>}"
if (( ${#skipped[@]} > 0 )); then
    echo "  skipped (${#skipped[@]}):"
    for s in "${skipped[@]}"; do
        echo "    - $s"
    done
fi
echo

if (( ${#failed[@]} > 0 )); then
    echo "[$SCRIPT_NAME] PREFLIGHT FAILED — fix the above before pushing." >&2
    exit 1
fi

echo "[$SCRIPT_NAME] OK — local CI matrix clean. Safe to push."
exit 0
