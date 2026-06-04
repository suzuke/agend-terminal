#!/usr/bin/env bash
# Install the tracked git hooks. Idempotent — safe to re-run any time.
#
# Two install targets, because there are two distinct push contexts:
#
#   1. Standalone clone (operator / CI): point git at the tracked hooks in
#      `scripts/hooks/` via `core.hooksPath`.
#
#   2. Fleet agents: the daemon sets each worktree's `core.hooksPath` to
#      `$AGEND_HOME/hooks` (src/binding.rs::install_hooks) and writes ONLY
#      `prepare-commit-msg` there — it does NOT clear the directory. So we
#      COPY the tracked `pre-push` into `$AGEND_HOME/hooks/pre-push`, where it
#      coexists with the daemon's hook and fires for agent pushes (the pushes
#      the CI-parity gate in pre-push exists to guard). One copy covers every
#      current and future worktree because they all share that dir.
#
# Coexistence assumption: the daemon never deletes `$AGEND_HOME/hooks/pre-push`.
# That matches the current binding.rs behaviour (write prepare-commit-msg, no
# dir clear). If that ever changes, re-run this script.

set -euo pipefail

cd "$(dirname "$0")/.."

chmod +x scripts/hooks/*

# 1. Standalone clone.
git config core.hooksPath scripts/hooks
echo "Installed: core.hooksPath -> scripts/hooks"

# 2. Fleet agents ($AGEND_HOME/hooks, coexisting with the daemon's hook).
AGEND_HOME="${AGEND_HOME:-$HOME/.agend-terminal}"
if [ -d "$AGEND_HOME" ]; then
    mkdir -p "$AGEND_HOME/hooks"
    cp scripts/hooks/pre-push "$AGEND_HOME/hooks/pre-push"
    chmod +x "$AGEND_HOME/hooks/pre-push"
    echo "Installed: $AGEND_HOME/hooks/pre-push (coexists with daemon prepare-commit-msg)"
else
    echo "Note: \$AGEND_HOME ($AGEND_HOME) not found — skipped the fleet-agent copy." >&2
    echo "      Re-run with AGEND_HOME set if you want the agent-push CI-parity gate." >&2
fi

echo "Hooks active:"
ls -1 scripts/hooks
