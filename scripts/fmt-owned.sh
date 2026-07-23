#!/usr/bin/env bash
# scripts/fmt-owned.sh — THE single repository-owned rustfmt surface (task83, decision
# d-20260713150435301072-46).
#
# "Owned" = tracked *.rs EXCLUDING vendor/** — the in-tree agentic-git workspace
# keeps its own source-format boundary. This pathspec remains the explicit
# AgEnD-Terminal boundary. Every fmt caller —
# GitHub CI, GitLab CI, scripts/preflight.sh (and pre-push via preflight) — invokes
# THIS one script, so the owned-source boundary is defined in exactly one place.
#
# Usage:
#   scripts/fmt-owned.sh            # format owned *.rs in place
#   scripts/fmt-owned.sh --check    # verify formatting; non-zero exit on drift
#   scripts/fmt-owned.sh -h|--help
#
# Exit: 0 clean; 1 rustfmt drift or failure; 2 bad invocation / no rustfmt.
#
# Portable to macOS system bash 3.2 and Git Bash on the Windows CI runner:
# NUL-safe enumeration via a read loop (no `mapfile -d`, absent on bash 3.2).
set -euo pipefail

check=""
case "${1:-}" in
    --check)      check="--check" ;;
    ""|--write)   : ;;
    -h|--help)
        sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "fmt-owned: unknown arg '$1' (use --check, --write, or no arg)" >&2
        exit 2
        ;;
esac

if ! command -v rustfmt >/dev/null 2>&1; then
    echo "fmt-owned: rustfmt not found in PATH (install via rustup)" >&2
    exit 2
fi

# Version-scope evidence (task83/d-46): record the EXACT rustfmt this run used, to
# stderr, so a "matches CI" claim is scoped to a concrete version in the logs
# rather than asserted as byte-exact — and without pinning a toolchain here.
echo "fmt-owned: $(rustfmt --version 2>/dev/null) [edition 2021, owned *.rs, vendor/** excluded]" >&2

# Resolve the OUTERMOST superproject working tree so enumeration + rustfmt always
# run against the top-level owned tree, regardless of CWD. `--show-superproject-
# working-tree` climbs only ONE level, so from a deeply nested submodule a single
# call names the IMMEDIATE superproject (and would then format vendored sources
# under it); loop until it reports no superproject to reach the top.
root="$(git rev-parse --show-toplevel)"
while true; do
    super="$(git -C "$root" rev-parse --show-superproject-working-tree 2>/dev/null || true)"
    [ -n "$super" ] || break
    root="$super"
done
cd "$root"

# NUL-safe: enumerate tracked owned *.rs (vendor/** excluded). A -z stream keeps
# paths with spaces or newlines intact; the read loop preserves them exactly.
files=()
while IFS= read -r -d '' f; do
    files+=("$f")
done < <(git ls-files -z -- '*.rs' ':!:vendor/**')

if [ "${#files[@]}" -eq 0 ]; then
    echo "fmt-owned: no owned *.rs tracked under $root — nothing to do" >&2
    exit 0
fi

# One rustfmt pass over the owned set (reads no rustfmt.toml → edition-2021
# defaults, matching CI). rustfmt's exit status propagates: non-zero on a parse
# error (write mode) or on any drift (--check mode).
fmt_args=(--edition 2021)
[ -n "$check" ] && fmt_args+=("$check")
rustfmt "${fmt_args[@]}" "${files[@]}"
