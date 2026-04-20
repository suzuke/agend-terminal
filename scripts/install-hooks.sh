#!/usr/bin/env bash
# Point git at the tracked hooks in scripts/hooks/.
#
# Per-clone setup: run this once after `git clone`. Hooks are not tracked in
# .git/ so the canonical copy lives in scripts/hooks/ and this script tells
# git to look there.

set -euo pipefail

cd "$(dirname "$0")/.."

git config core.hooksPath scripts/hooks
chmod +x scripts/hooks/*

echo "Installed: core.hooksPath -> scripts/hooks"
echo "Hooks active:"
ls -1 scripts/hooks
