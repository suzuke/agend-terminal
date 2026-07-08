#!/usr/bin/env bash
# Build-together (agentic-git migration, design §3.A / #2524 P2).
#
# Builds the vendored `agentic-git` shim binary from the PINNED submodule and
# installs it as a sibling of the daemon exe, so the flag-gated git-shim swap
# (fleet.yaml `use_agentic_git_shim: true`) can resolve it at daemon startup via
# `current_exe().with_file_name("agentic-git")` (see src/binding/shim_install.rs).
#
# Why the submodule's OWN manifest (not a workspace `-p agentic-git`): the
# vendored crate uses `edition.workspace = true` and lives under vendor/agentic-git,
# which declares its own `[workspace]`. It therefore CANNOT be a member of
# agend-terminal's package/workspace without editing the pinned submodule (a
# revert-clean spike confirms cargo rejects it). Building via the submodule
# manifest keeps agend-terminal's Cargo.lock with EXACTLY ONE agentic-git-core
# (tests/agentic_core_single_source.rs) while the binary links the SAME vendored
# core SOURCE dir the daemon links — structural same-version, no registry drift.
#
# Usage: scripts/build_agentic_git_shim.sh [debug|release]   (default: debug)
set -euo pipefail

PROFILE="${1:-debug}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SUB="$REPO/vendor/agentic-git"
DEST_DIR="${CARGO_TARGET_DIR:-$REPO/target}/$PROFILE"

case "$PROFILE" in
  debug|release) ;;
  *) echo "usage: $0 [debug|release]" >&2; exit 2 ;;
esac

if [ ! -f "$SUB/Cargo.toml" ]; then
  echo "error: submodule not initialized at $SUB — run: git submodule update --init" >&2
  exit 1
fi

# Build in the submodule's OWN target dir (its own Cargo.lock over the same
# vendored source) to avoid sharing agend-terminal's target-dir — no
# cross-workspace rebuild churn or target-lock contention. (No bash array for
# the profile flag — keeps `set -u` happy on macOS's bash 3.2.)
if [ "$PROFILE" = release ]; then
  cargo build -p agentic-git --manifest-path "$SUB/Cargo.toml" --release
else
  cargo build -p agentic-git --manifest-path "$SUB/Cargo.toml"
fi

mkdir -p "$DEST_DIR"
install -m 0755 "$SUB/target/$PROFILE/agentic-git" "$DEST_DIR/agentic-git"
echo "build-together: installed agentic-git shim → $DEST_DIR/agentic-git"
