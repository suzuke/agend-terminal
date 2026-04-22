# Codebase Review Fix Plan — COMPLETE

> **Status: SHIPPED** — all 8 stages merged + pushed to `origin/main` (2026-04-21). Doc retained for historical/provenance.

**Date:** 2026-04-21
**Completed:** 2026-04-21
**Scope:** Full codebase review → 17 findings, 11 actionable items across 3 phases

**Status: COMPLETE** (C3 deferred by design — low priority, 17 call sites)
- Phase A (P0): DONE — merged 9b1863a
- Phase B1+B2 (cron): DONE — merged via fix/p1-cron-robustness
- Phase B3 (regex): DONE — merged via fix/p1-fleet-regex-safety
- Phase B4: no-op (already handled)
- Phase B5 (spawn refactor): DONE — merged via refactor/split-spawn-agent
- Phase C1 (pane_ids): DONE — collect_pane_ids() added
- Phase C2 (ACL cache): DONE — OnceLock
- Phase C3 (terminal size): DEFERRED — 17 call sites, low priority
- Bonus: schedules.rs timezone fallback fixed (found by /simplify)

---

## Phase A — P0 Correctness (1 branch: `fix/p0-correctness`)

### A1. MCP Content-Length error recovery EOF guard

- **File:** `src/mcp/mod.rs:110-121`
- **Problem:** When `Content-Length` header fails to parse, the error branch
  calls `read_line` to consume the separator line but ignores its return
  value. If that `read_line` hits EOF (returns 0), the outer loop continues
  with a corrupted reader — subsequent frames are silently lost or
  misinterpreted.
- **Fix:** Check `read_line` return; if 0 → `return Ok(None)` (stream ended),
  not `continue`.
- **Scope:** ~3 lines changed + 1 unit test
- **Verify:**
  1. Unit test: feed `BufReader` with `Content-Length: garbage\n` followed by
     immediate EOF — assert `read_message()` returns `Ok(None)`, not infinite
     loop.
  2. Unit test: feed malformed header + valid frame after it — assert the
     valid frame is still returned (resync works).
  3. `cargo clippy --all-targets -- -D warnings` passes.

### A2. State pattern compile failure must not be silent

- **File:** `src/state.rs:381-393`
- **Problem:** `filter_map` silently drops patterns whose regex fails to
  compile. Callers have no way to know that detection coverage is degraded.
- **Fix:** All patterns are hardcoded `&str` constants — a compile failure is a
  code bug, not a runtime condition. Replace `filter_map` with `map` +
  `expect("BUG: invalid state regex")` so the bug surfaces immediately
  during development/CI instead of hiding silently in production.
- **Scope:** ~5 lines changed
- **Verify:**
  1. `cargo test` passes — all existing state pattern tests still green
     (proves every hardcoded pattern compiles).
  2. Temporarily inject an invalid regex `r"(["` into one pattern, run
     `cargo test` → expect panic with "BUG: invalid state regex" message.
     Revert after confirming.
  3. `cargo clippy --all-targets -- -D warnings` passes.

### A3. ANSI stripper: remove cursor-move space insertion

- **File:** `src/agent.rs:148-150`
- **Problem:** CSI final bytes `C` (cursor forward) and `D` (cursor back)
  insert a space into the stripped output while all other CSI codes produce
  nothing. This inconsistency can cause false-positive pattern matches in
  dialog detection.
- **Context:** `strip_ansi()` is used for dialog/dismiss detection on raw PTY
  bytes. The VTerm screen dump (used by `state.rs`) already handles cursor
  positioning correctly, so injecting spaces here has no benefit and adds
  noise.
- **Fix:** Remove the `if ch == 'C' || ch == 'D' { out.push(' '); }` branch.
  Update related tests.
- **Scope:** ~2 lines deleted + test update
- **Verify:**
  1. `grep -rn 'strip_ansi' src/` — confirm all call sites are for dialog
     detection, not vterm feed. If any call site relies on the space, adjust.
  2. Unit test: `strip_ansi("\x1b[5C hello")` → `" hello"` (no leading
     space from cursor move), not `"  hello"`.
  3. Existing `strip_ansi` tests updated and passing.
  4. Manual smoke: launch a Claude agent, trigger a dismiss dialog, confirm
     dismiss still works (dismiss patterns match against vterm screen dump,
     not strip_ansi output — but verify this assumption).

---

## Phase B — P1 Robustness (2-3 branches)

### Branch `fix/p1-cron-robustness` (B1 + B2)

#### B1. Cron last_check atomic write

- **File:** `src/daemon/cron_tick.rs:137`
- **Problem:** `std::fs::write` is not atomic. If two daemon instances briefly
  coexist during startup/shutdown, both read the same `last_check_utc` and
  fire the same schedule twice. `Once` triggers are especially affected.
- **Fix:** Replace `std::fs::write` with `crate::store::atomic_write()` (the
  project already has an atomic write-to-tmp-then-rename helper used by
  fleet.yaml and other stores).
- **Scope:** 1 line changed
- **Verify:**
  1. `cargo test` passes (no behaviour change for normal path).
  2. Inspect `crate::store::atomic_write` — confirm it writes to `.tmp` +
     renames, matching expected semantics.
  3. Manual: run daemon, check `.schedule_last_check` file is written without
     truncation window (ls -la before/after).

#### B2. Cron timezone invalid → skip schedule, not fallback UTC

- **File:** `src/daemon/cron_tick.rs:49-56`
- **Problem:** Invalid timezone in `fleet.yaml` silently falls back to UTC.
  Schedules fire at the wrong local time with only a `warn!` log that
  operators may never see.
- **Fix:** On parse failure, `tracing::error!` + `continue` (skip schedule for
  this tick). The error surfaces every tick until corrected, making it
  impossible to miss.
- **Scope:** ~5 lines changed
- **Verify:**
  1. Unit test: add a schedule with `timezone: "Not/A/Zone"` in test
     fixture → assert `check_schedules` does NOT fire it (agent registry
     unchanged).
  2. Grep logs for `tracing::error!` output — confirm message includes
     schedule ID and bad timezone name.
  3. Existing cron tests still pass (valid timezones unaffected).

### Branch `fix/p1-fleet-regex-safety` (B3 + B4)

#### B3. Fleet ready_pattern ReDoS protection

- **File:** `src/verify.rs:485-486`
- **Problem:** `ready_pattern` from fleet.yaml is user-controllable. Malicious
  or badly crafted regex can hang agent verification via catastrophic
  backtracking.
- **Fix:** Use `regex::RegexBuilder::new(pat).size_limit(1 << 20).build()`
  to cap compiled regex size. On failure, return an error instead of falling
  back to the universal match `.`.
- **Scope:** ~5 lines changed
- **Verify:**
  1. Unit test: `ready_pattern: "((((a])*)*)*)*"` (pathological backtrack)
     → assert `RegexBuilder` rejects it with size limit error.
  2. Unit test: valid `ready_pattern: "\\$"` → still compiles and matches.
  3. Integration: `agend verify` with bad `ready_pattern` in fleet.yaml →
     returns a clear error message, not a hang.

#### B4. Fleet YAML instances type check

- **File:** `src/fleet.rs:318-324`
- **Status:** Already handled — line 323-324 has
  `.context("instances is not a mapping")?` which propagates the error.
- **Action:** Verify no other code path bypasses this check. If none found,
  mark as no-op.
- **Verify:**
  1. `grep -rn 'get_mut("instances")' src/fleet.rs` — confirm all mutation
     paths go through `mutate_fleet_yaml` which includes the type check.
  2. Manual: set `instances: []` in fleet.yaml → run `agend spawn` → expect
     clear error "instances is not a mapping", not silent failure.

### Branch `refactor/split-spawn-agent` (B5, independent)

#### B5. Refactor spawn_agent() (263 lines → 3 focused functions)

- **File:** `src/agent.rs:205-467`
- **Problem:** Single function handles PTY setup, registry insertion, reaper
  thread spawn, and instructions bootstrap. If registry insertion fails, the
  PTY reader thread is already running with no cleanup.
- **Fix:** Extract into:
  - `build_command(cfg) -> CommandBuilder` — env, args, cwd
  - `spawn_pty(cmd, size) -> (Master, Child, Reader)` — PTY creation
  - `register_and_start(registry, handle) -> Result<()>` — insert + spawn
    reader/reaper threads with rollback on failure
- **Risk:** Highest of all items — behaviour-preserving refactor that touches
  the critical spawn path. Needs careful testing.
- **Scope:** ~100 lines moved/reorganized, no behaviour change
- **Verify:**
  1. `cargo test` — all existing agent tests pass.
  2. Integration test: `agend daemon` + `agend spawn` → agent starts, PTY
     I/O works, `agend list` shows correct state.
  3. Error injection: make `register_and_start` fail after PTY spawn →
     assert no orphan reader threads (check thread count before/after).
  4. Manual smoke: launch 3 agents, kill one, respawn — confirm lifecycle
     unchanged from before refactor.
  5. `cargo clippy --all-targets -- -D warnings` passes.

---

## Phase C — P2 Performance (1 branch: `perf/allocation-hotspots`)

### C1. layout.rs pane_ids() in-place collection

- **File:** `src/layout.rs:169-177`
- **Problem:** `pane_ids()` allocates a new `Vec` on every call (recursive).
  Called 4+ times per keyboard navigation event.
- **Fix:** Add `collect_pane_ids_into(&self, buf: &mut Vec<usize>)`. Callers
  reuse a pre-allocated buffer.
- **Verify:**
  1. Unit test: call `collect_pane_ids_into` on a 3-level split tree →
     assert same result as old `pane_ids()`.
  2. Ensure old `pane_ids()` is removed or delegates to new method (no
     leftover duplicate).
  3. `cargo test` passes.

### C2. MCP tool ACL cache

- **File:** `src/mcp/mod.rs:20-49`
- **Problem:** `tool_acl()` re-parses `AGEND_MCP_TOOLS_ALLOW` /
  `AGEND_MCP_TOOLS_DENY` env vars into `HashSet` on every tool invocation.
- **Fix:** Use `std::sync::OnceLock<(HashSet<String>, HashSet<String>)>` to
  compute once at first call.
- **Verify:**
  1. Unit test: set `AGEND_MCP_TOOLS_DENY=foo`, call `tool_is_allowed("foo")`
     twice → both return false, env var parsed only once (assert via
     `OnceLock::get().is_some()` after first call).
  2. `cargo test` — existing MCP tests pass.

### C3. Terminal size cache

- **File:** `src/app/mod.rs` (11 `crossterm::terminal::size()` calls across
  `app/*.rs`)
- **Problem:** Each call is a syscall; queried multiple times per event loop
  iteration.
- **Fix:** Store `(cols, rows)` in event loop state. Update only on
  `Event::Resize`.
- **Verify:**
  1. Manual: resize terminal window → UI reflows correctly (no stale size).
  2. `grep -rn 'crossterm::terminal::size' src/` — confirm all direct calls
     removed except the initial query and the Resize handler.
  3. `cargo clippy --all-targets -- -D warnings` passes.

---

## Execution Order

```
Phase A  →  1 branch, 3 commits, ship first
Phase B  →  2-3 branches, can run in parallel after A merges
Phase C  →  1 branch, low risk, can ship anytime
```

```
Week 1:  Phase A (fix/p0-correctness)
Week 1-2: Phase B (fix/p1-cron-robustness, fix/p1-fleet-regex-safety)
Week 2+: Phase B5 (refactor/split-spawn-agent) + Phase C
```

## Global Gate (every branch, before merge)

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All three must pass. CI (`ci.yml`) enforces the same checks on push.

---

## Non-Actionable Notes (from review, no fix needed)

- **lib.rs public API** — minimal and intentional, no change needed
- **No circular dependencies** — confirmed across 40+ modules
- **Auth cookie implementation** — production-grade, no issues
- **fleet.yaml `instances` type check** (B4) — already present at line 323
- **Error handling contract inconsistency** (Result vs JSON) — documented,
  defer to future API layer refactor
- **Structured logging inconsistency** — defer to future standardization pass
- **mcp/handlers.rs split** — optional readability improvement, no urgency
