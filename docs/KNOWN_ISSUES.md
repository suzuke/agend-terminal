[繁體中文](KNOWN_ISSUES.zh-TW.md)

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
(blocked on a capture or decision only the maintainer can provide).

---

## Upstream / external (not fixed in agend-terminal)

### `opencode --continue` occasionally fails to resume
- **Status:** Upstream bug (mitigated)
- **Why:** the OpenCode TUI can send a placeholder ("dummy") session id on
  resume, producing an "Unexpected server error". agend-terminal mitigates this
  by falling back to a fresh session (#1519) — functional, but the prior
  session is not resumed. This is an OpenCode-side bug, not an agend root fix.
- **Revisit when:** OpenCode fixes the dummy-session id upstream.
- **Refs:** #1526 (agend mitigation: #1519)
- **Wedge variant (2026-07-02, needs-repro):** the same dummy-session bug can
  also manifest as the OpenCode *process not exiting* — the TUI chrome keeps
  rendering while the uncaught exception stack bleeds into the pane, and
  agend's three existing detection layers (state-pattern classifier,
  respawn-stuck watchdog, backend-exit detection) all miss it because the
  process never crashes and no known error signature matches the stack. One
  incident is captured (confirmed a real capture exists this time, contra an
  earlier session's belief it was unrecoverable); a second sample is needed
  before a detection pattern is added, to avoid false-positiving on
  legitimate output (see t-20260702144219394508-56872-6). Structural
  hardening for the watchdog side is tracked under the round-3 #2549 scope
  rather than as a standalone fix. **If you hit this again: capture it BEFORE
  restarting or otherwise intervening** — `pane_snapshot(to_file=true)` on the
  wedged instance writes the full pane to `$AGEND_HOME/captures/`; a
  restart/replace destroys the only evidence.

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

<!--
#1521 Schedule fire-strategy — SHIPPED (removed from this list 2026-07).
`FireStrategy::{Always, UntilSuccess}` lives in `src/schedules.rs` and is
enforced by `src/daemon/cron_tick.rs` (linked-task gate + per-day suppress).
Do not re-add as "undecided".
-->