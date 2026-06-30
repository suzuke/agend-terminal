#!/usr/bin/env bash
#
# AgEnD L1 guardrail (Claude Code PreToolUse) — ADVISORY, Claude-only.
#
# Blocks Write/Edit/NotebookEdit whose target path is inside a managed canonical
# repo working tree. Agents must write to their git worktree, NEVER the canonical
# main tree (§10.4 worktree-mandatory). This is the fast, local UX guard for Claude
# Code ONLY — it does nothing for Codex/opencode/raw shell, so it is NOT
# authoritative: the daemon's `canonical_drift` dirty detector (L2) is the
# universal, backend-agnostic guard that actually closes the blind spot.
#
# The set of canonical roots is published by the daemon at
# `$AGEND_HOME/canonical-roots.json` (see `bootstrap::canonical_hygiene::
# write_canonical_roots`); this hook only READS it.
#
# Fail-OPEN by design: on any ambiguity (no roots file, no python3, malformed
# input) the hook exits 0 and lets the write through — an advisory guard must never
# spuriously block legitimate work. Exit 2 (with a stderr reason) is the only
# blocking path, and only for a write that lands inside a known canonical root.
set -uo pipefail

roots_file="${AGEND_HOME:-$HOME/.agend-terminal}/canonical-roots.json"
[ -f "$roots_file" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

# The hook's stdin is the PreToolUse JSON; it flows to python3 unchanged. The
# canonical roots file is passed as argv[1]. `exec` so python3's exit code is the
# hook's exit code (2 = block).
exec python3 -c '
import json, os, sys
try:
    data = json.load(sys.stdin)
    roots = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(0)
# Self-defending: only the file-WRITE tools are guarded. Read/Grep/Bash/etc. — and
# any unknown tool — pass through untouched even if their payload carries a
# canonical file_path. Do not rely on the settings.json matcher alone.
if (data.get("tool_name") or "") not in ("Write", "Edit", "NotebookEdit"):
    sys.exit(0)
ti = data.get("tool_input") or {}
fp = ti.get("file_path") or ti.get("notebook_path")
if not fp:
    sys.exit(0)
fp = os.path.abspath(os.path.expanduser(fp))
for root in roots:
    try:
        root = os.path.abspath(root)
    except Exception:
        continue
    if fp == root or fp.startswith(root + os.sep):
        sys.stderr.write(
            "AgEnD L1 guardrail: refusing to write into the canonical repo working "
            "tree (%s). Canonical repos must stay clean under AgEnD - write to your "
            "git worktree, NOT canonical main.\n" % root)
        sys.exit(2)
sys.exit(0)
' "$roots_file"
