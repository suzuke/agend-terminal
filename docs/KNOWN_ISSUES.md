# Known Issues

A living list of issues that are **known and intentionally not being worked on
right now**, with the reason and the condition under which they'd be
reconsidered.

**Please check this list before opening an issue or PR.** Reports that
re-raise an item here without new evidence — or that propose work already
deferred for a stated reason — may be closed with a pointer back to this page.
If you have new evidence or a change that affects the "Revisit when" condition,
say so explicitly in your report.

Status legend: **Upstream-blocker** (fix lives in another project) ·
**Unsupported** (intentionally not maintained for now) · **Needs-operator-input**
(blocked on a capture or decision only the maintainer can provide) ·
**Stale** (no current owner).

---

## Upstream / external (not fixed in agend-terminal)

### Antigravity CLI (`agy`) backend — unsupported
- **Status:** Unsupported
- **Why:** Beyond the missing Fleet MCP bridge, known problems with this
  backend are not being addressed for now. Supporting it properly depends on
  project-scoped MCP being available, or on a first-party CLI tool.
- **Revisit when:** project-scoped MCP is supported (#1262), or a first-party
  CLI tool lands.
- **Refs:** #1547, #1262, #987

### `agy` ignores a hidden parent-directory workspace
- **Status:** Upstream-blocker
- **Why:** `agy` treats paths under a hidden directory (e.g.
  `~/.agend-terminal/*`) as hidden and skips them, so it won't operate in the
  daemon-managed workspace. The fix belongs in the upstream CLI
  (`@google/antigravity-cli`).
- **Revisit when:** upstream supports hidden parent-directory workspaces.
- **Refs:** #998

### `opencode --continue` occasionally fails to resume
- **Status:** Upstream bug (mitigated)
- **Why:** the OpenCode TUI can send a placeholder ("dummy") session id on
  resume, producing an "Unexpected server error". agend-terminal mitigates this
  by falling back to a fresh session (#1519) — functional, but the prior
  session is not resumed. This is an OpenCode-side bug, not an agend root fix.
- **Revisit when:** OpenCode fixes the dummy-session id upstream.
- **Refs:** #1526 (agend mitigation: #1519)

## Deferred — awaiting operator capture or decision

### Real PTY corpus (5 backends × 2 scenarios) incomplete
- **Status:** Needs-operator-input
- **Why:** robust state-detection work needs real terminal captures across the
  supported backends as a validation gate; the corpus isn't complete yet.
- **Revisit when:** the operator captures the remaining corpus.
- **Refs:** #1014

### Claude Code "Yes, proceed" modal — default cursor position unverified
- **Status:** Needs-operator-input
- **Why:** confirming the modal's default cursor position needs a real capture.
- **Revisit when:** the operator captures the modal.
- **Refs:** #1054

### Operator Mode (active / away / sleep / dnd + delegation)
- **Status:** Needs-operator-input
- **Why:** needs an operator-policy freeze and a phased breakdown before
  implementation can start.
- **Revisit when:** the operator freezes the policy and the work is phased.
- **Refs:** #1339

## Stale — no current owner

### Schedule fire-strategy
- **Status:** Stale
- **Why:** no current owner; the strategy is undecided.
- **Refs:** #1521
