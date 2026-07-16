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

<!--
#1014, #1054, and #1339 were removed after their acceptance work completed
and the issues closed on 2026-06-01. Do not re-add them as deferred.

#1521 Schedule fire-strategy — SHIPPED (removed from this list 2026-07).
`FireStrategy::{Always, UntilSuccess}` lives in `src/schedules.rs` and is
enforced by `src/daemon/cron_tick.rs` (linked-task gate + per-day suppress).
Do not re-add as "undecided".
-->
