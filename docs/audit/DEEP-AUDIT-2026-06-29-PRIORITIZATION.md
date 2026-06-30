# Deep Audit 2026-06-29 — Phase 5: Prioritization & Fix Order

Categorisation of every confirmed finding in `DEEP-AUDIT-2026-06-29-ISSUES.md`. No fixes applied —
this is the recommended order only.

## Severity buckets

### 🔴 Critical
None reach unconditional Critical (no unauthenticated-remote, no guaranteed data loss). **AUDIT2-001
is Critical *if* untrusted/3rd-party agents or operator-supplied self-hosted CI URLs are in scope** —
treat it as Critical in that deployment.

### 🟠 High
| ID | Title | Why high |
|----|-------|----------|
| AUDIT2-001 | SSRF + GitHub-token exfil via `ci_provider_url` | Forge token sent to an arbitrary host via **one documented tool call**, reachable by the least-privileged role. Fix is small. |
| AUDIT2-011 | Colliding task IDs at `send(kind=task)` auto-create | **Silent task loss** under concurrency; the regression guard is falsely green. Fix is one line. |
| AUDIT2-009 | CI rerun-to-green swallowed (≥2 workflows) | **Silent** broken reviewer handoff, ~50% of multi-workflow reruns; no error surfaced. |

### 🟡 Medium
AUDIT2-002 (worktree/lifecycle ACL), AUDIT2-003 (config-gate exposure), AUDIT2-004 (env deny-list
gap), AUDIT2-006 (event-bus tick blocking), AUDIT2-007 (crash-arm panic → daemon death),
AUDIT2-008 (recovery notify storm), AUDIT2-010 (cron DST mis/double-fire), AUDIT2-012
(runtime-config non-atomic), AUDIT2-013 (skills-stage boot race), AUDIT2-016 (`close_tab` focus
mis-route), AUDIT2-017 (scroll blank pane), AUDIT2-018 (`agend-supervisor` binary doc — the
material part).

### 🟢 Low
AUDIT2-005 (metadata DoS, owner-bounded), AUDIT2-014 (cross-board dep race, multi-board only),
AUDIT2-015 (parent-dir fsync), AUDIT2-018 (rest of the dead-command docs), AUDIT2-019 (stale keybind
table), AUDIT2-020 (dead env var + dead MCP param).

## Recommended fix order (impact ÷ effort)

**Wave 1 — cheap, high-impact (do first):**
1. **AUDIT2-001** — add https+host-allowlist validation to `ci_provider_url` and `api_base_url`; gate token attach to the known host. *(security, small diff)*
2. **AUDIT2-011** — append `{pid}`/uuid at `messaging.rs:190`; widen the regression guard to all mint sites. *(one-line correctness fix + test)*
3. **AUDIT2-007** — wrap the crash-event match in the existing `catch_unwind` guard. *(prevents daemon death; trivial, symmetry with #1002)*

**Wave 2 — silent failures & state integrity:**
4. **AUDIT2-009** — per-workflow rerun baseline; stop `run.id < threshold` hard-dropping attempt-advanced runs. *(restores reviewer handoff)*
5. **AUDIT2-012** — `store::atomic_write` + flock for runtime-config. *(prevents safety-gate reset on corrupt config)*
6. **AUDIT2-013** — pid/nonce staging dir + atomic rename (or per-digest flock). *(prevents fleet-boot skill loss)*
7. **AUDIT2-010** — add the `next > last_check_local` lower-bound guard in `is_due_in_tz`. *(DST correctness)*

**Wave 3 — authorization hardening (batch, shared root):**
8. **AUDIT2-002** — per-caller ownership/orchestrator ACL on `force_release_worktree`/`delete_instance`/`repo merge`.
9. **AUDIT2-003** — mark `progress_mode`/watchdog/recovery gates operator-only regardless of mode.
10. **AUDIT2-004** — extend `SENSITIVE_ENV_KEYS` (NODE_OPTIONS, GIT_SSH_COMMAND, BASH_ENV, …).

**Wave 4 — tick-thread reliability (larger refactor):**
11. **AUDIT2-006 + AUDIT2-008** — offload subscriber/notify delivery to a bounded worker queue with per-op timeouts; fire Stage-2 notify once on entry with a dedup-stable body.

**Wave 5 — TUI:**
12. **AUDIT2-016** — `active > idx` decrement in `close_tab`.
13. **AUDIT2-017** — clamp render `scroll_offset` to `scroll_max()` / on geometry change.

**Wave 6 — docs (quick wins, batchable any time):**
14. **AUDIT2-018 / 019 / 020** — rewrite `USAGE.md` (dead commands + nonexistent `agend-supervisor` binary + keybind table), drop the dead `AGEND_TURN_SENTINEL_SHADOW` env var and `task.duration` param from the reference docs (or re-implement the env var).

**Then:** AUDIT2-005, 014, 015 as opportunistic cleanups.

## Notes for the maintainer
- Several findings (AUDIT2-002/003 and the cross-agent-impersonation caveat) share one structural
  root: **the operator gate's default `Active` mode is fully permissive, so per-tool ACLs are the
  only authorization left.** A single design decision — minimal per-tool ACLs on destructive/
  sensitive tools independent of operator mode — closes the whole class. Worth a design issue above
  the individual fixes.
- AUDIT2-006/007/008 share the root **"blocking notification I/O on the single tick thread."** One
  worker-queue refactor addresses all three.
- The two most dangerous traits in this codebase are **silent failure** (AUDIT2-009, 011 succeed-but-
  lose) and **untested edge bands** (DST transition hours, multi-workflow CI, concurrent same-profile
  boot). Recommend regression tests land *with* each fix, in those exact bands.
