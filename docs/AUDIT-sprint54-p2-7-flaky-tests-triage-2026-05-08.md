# Sprint 54 P2-7 — Flaky Tests Triage Audit

**Date**: 2026-05-08
**Tier**: 1 (single primary review — codex)
**Path**: B (test/doc only)
**Verdict-summary**: 6 candidates surfaced from ground-truth + archived references; **0 inline quarantine adds**; 2 follow-up Sprint recommendations (macOS RCA cluster + integration.rs sleep refactor); CI history shows zero recent flake fires.
**Scope**: triage + categorize, NOT fix-all. Per dispatch m-20260508012306580408-309 + prioritization verdict m-20260508012856140953-314.

---

## Sources of truth

1. **CI history**: `gh -R suzuke/agend-terminal run list --limit 30 --json conclusion,name,databaseId,headBranch,event,createdAt`
2. **Archived audit**: `docs/archived/audit-tests-2026-04-29.md` (8 ignored tests inventory + flagged candidates)
3. **Sprint 19 cleanup report**: `docs/archived/SPRINT-19-CLEANUP-REPORT.md` (Track 3 flaky test infra: 2 named tickets)
4. **Quarantine state scan**: `grep -rn '#\[ignore' tests/ src/`
5. **Platform-cfg gates**: `grep -rn '#\[cfg(target_os' tests/ src/`
6. **Test-infra anchors**: `tests/common/harness.rs` (`platform_timeout`), `tests/mcp_subprocess_is_zero_state.rs` (zero-state invariant)

---

## Methodology

Triage walked five discovery surfaces in parallel:

- **CI history** (last 30 runs) for re-run-green / mixed-conclusion patterns indicating flakiness.
- **Archived audit ingestion** to extract previously-flagged but unresolved flaky candidates.
- **Quarantine state** (`#[ignore]`) inventory check — every gated test inspected for reason annotation completeness (audit-2026-04-29 prior finding `state::replay_session` "未標明原因").
- **Platform-cfg gates** to distinguish intentional platform code from quarantined-due-to-platform-flake tests.
- **Mitigation precedent** scan (`git log --grep='flaky\|flake\|timeout\|race'`) to confirm tree's flaky-mitigation discipline (Sprint 42 platform_timeout, PR #420 Connection: close, etc.).

No live macOS-repro work performed (out of triage scope per Q2 verdict).

---

## CI history baseline (informational)

Last 30 runs: **1 failure** — `sprint54-p1b-bug1-name-residual-fix` `25527914611` (2026-05-07 23:34) re-ran green at 23:54 (`25528560482`).

Root cause: `tests/file_size_invariant::mcp_handler_files_under_500_loc` rejected `instance.rs` at 947 LOC (700 LOC ceiling) — **NOT flakiness**, hard rule violation fixed via Option-3 split into `instance_lifecycle.rs` (PR #517 fixup `377e4a5`).

**Net: zero recent flake fires.** This is the baseline against which the per-candidate analysis below should be read.

---

## Per-candidate triage

### #1 — `worktree_cleanup` Windows path normalization
- **Ticket**: `t-20260424173948421544-1`
- **File:line**: `src/worktree_cleanup.rs:429` `test_v2_active_runtime_worktree_not_removed_under_bootstrap_redirect`
- **Category**: platform (Windows path-format)
- **Observed**: pre-Sprint-19 audit flagged "Windows path normalization for worktree_cleanup tests" without specifying which.
- **Current state**: `#[cfg(unix)] // Windows path format — t-20260424173948421544-1` — **already mitigated** with explicit ticket reference comment.
- **Recommendation**: **document closure**. The gate-with-rationale is the canonical pattern; ticket `t-20260424173948421544-1` can be marked resolved (or kept open as "Windows path-format normalization could re-enable test on Windows" tech-debt) at lead's discretion. No code action this Sprint.

### #2 — `mcp_bridge_idle_reconnect` (2 tests, Windows hangs)
- **Ticket**: implicit (audit-2026-04-29 flagged "容易 flaky")
- **File:line**: `tests/mcp_bridge_idle_reconnect.rs:1-26`
- **Category**: platform (Windows TCP loopback close timing) + env (mock daemon + child-process bridge spawn)
- **Observed**: bridge subprocess + mock daemon double-process shape hung in Windows CI, same surface that forced PR #263 PowerShell-only fixes.
- **Current state**: `#![cfg(unix)]` at file level + 6-line block comment justifying scope (Windows ships, retry logic covered by `is_retriable_io` classifier + macOS/Linux runners) — **already mitigated** at file granularity.
- **Recommendation**: **document closure**. Pattern is correct (file-level `cfg` + classifier-level coverage). No code action.

### #3-5 — macOS-flaky cluster: `agent_picked_up_*` + `test_describe_message_shows_delivery_mode`
- **Ticket**: `t-20260425035142945841-5`
- **File:line**:
  - `src/mcp/handlers/tests.rs:927` `agent_picked_up_emitted_on_inbox_drain`
  - `src/mcp/handlers/tests.rs:986` `agent_picked_up_fires_for_all_pending_messages`
  - `src/mcp/handlers/tests.rs:1173` `test_describe_message_shows_delivery_mode`
- **Category**: parallel-race (shared global state) — NOT timing/sleep (no `thread::sleep` in any of three)
- **Observed**: macOS-only flake reports per ticket. Three tests share a common pattern:
  - `fleet_test_guard()` returns `MutexGuard<'static, ()>` from `parking_lot::Mutex<()>` — serializes within process
  - `setup_recorder()` calls `std::env::set_var("AGEND_HOME", &home)` + `ux_sink_registry().clear_for_test()` — both process-global
  - Post-test: `std::env::remove_var("AGEND_HOME")` + `std::fs::remove_dir_all(&home).ok()` — best-effort cleanup
- **Suspected surface** (without macOS repro):
  - **Cross-process race**: `cargo test` runs multiple test binaries in parallel processes; `AGEND_HOME` env race within a single process is guarded, but if any other test crate's harness reads `home_dir()` outside the guard, race possible.
  - **`ux_sink_registry()` interleaving**: `clear_for_test()` + `register()` between two guarded test acquires; if a `handle_tool` call in test N+1 races a still-pending UxEvent emission from test N's drop, recorder snapshot polluted.
  - **macOS specificity hypothesis**: APFS extended-attribute cleanup in `remove_dir_all` may leave residue affecting next test's `set_var("AGEND_HOME", &home)` OR mach scheduler's preemption frequency intersects narrow guard release window differently than Linux CFS. Hypothesis only — not validated.
- **Recommendation**: **document + flag follow-up RCA Sprint**. Direct fix needs macOS reproduction harness (`cargo test agent_picked_up -- --test-threads=8 --repeat 1000` or stress loop with timing instrumentation), which is out of P2-7 triage scope per dispatch verdict Q2. Recommend Sprint 55 (or earliest reliability-themed Sprint) to:
  1. Reproduce on macOS CI runner with high test-thread count + repeat-runs
  2. Decide between **(a)** harden `fleet_test_guard()` to also serialize across `ux_sink_registry()` access via single combined guard, **(b)** migrate these 3 tests to per-test `tempfile::TempDir` + dedicated `AgentHome` config-injection avoiding env-var globals, or **(c)** mark `#[serial]` + add `serial_test` crate dependency
  3. Land RCA + chosen mitigation in dedicated PR

### #6 — `tasks::test_claimed_task_not_touched_by_dep_eval` + `tasks::test_list_default_hides_done_older_than_14d` (delete candidates)
- **Ticket**: implicit (audit-2026-04-29 explicitly recommended deletion: "場景已不可達 / 邏輯已被其他 Done tests 間接覆蓋")
- **Current state**: **NOT FOUND in current codebase** (`grep -rn 'test_claimed_task_not_touched_by_dep_eval\|test_list_default_hides_done_older_than_14d' src/ tests/` returns 0 hits)
- **Recommendation**: **document closure**. Already cleaned up in interim Sprint. No action.

---

## Bulk timing risk surface (informational, in-scope per Q3)

`tests/integration.rs` (497 LOC, 9 tests):

- **9 hard-sleeps** totaling ~23 seconds across spawn-wait-assert sequences:
  - `200ms × 2`, `300ms`, `500ms`, `1s`, `2s`, `3s`, `8s × 2`
- Pattern: `Command::new(binary).spawn()` + readiness `std::thread::sleep(Duration::from_secs(N))` — fixed-delay rather than poll-based readiness
- Audit-2026-04-29 finding: "Integration tests 太慢: integration.rs 9 tests 14s 佔總測試時間一半"
- **Currently passing CI** (no recent failures) — but conceptual flake risk if Windows CI runner ever degrades or test infrastructure adds load
- `tests/common/harness.rs:457` `platform_timeout(Duration)` already provides Windows 3x multiplier — but `tests/integration.rs` raw `thread::sleep` callers don't use it
- **Recommendation**: **flag follow-up Sprint** to migrate `tests/integration.rs` raw sleeps to `harness::wait_until(predicate, platform_timeout(Duration::from_secs(N)))` polling pattern. Estimated ~150 LOC change across 9 sites — broader test-infra refactor task, NOT P2-7 scope (per Q3 verdict). Flag with rationale: "preventive flake hardening; today's pass rate doesn't capture future degradation tail risk."

---

## Existing #[ignore] inventory health-check

**36 quarantined tests across the tree** (34 bare `#[ignore]` matching `rg -n "#\[ignore\]" src tests | wc -l` + 2 `#[ignore = "reason"]` reasoned-form annotations), all carrying reason annotations either as the attribute literal, source-comment header, or panic-message contract:

| Cluster | Files | Tests | Pattern |
|---|---|---|---|
| Stress (CI-fast quarantine) | `tests/sprint52_stress.rs` (7), `tests/agend_git_shim_phase1_stress.rs` (4), `tests/agend_git_shim_phase3_stress.rs` (5), `tests/agend_git_shim_phase4_stress.rs` (5), `tests/agend_git_shim_phase5_stress.rs` (4), `tests/agend_git_shim_phase2.rs` (4 stress at L142,174,195,240) | 29 | "Gated via `#[ignore]` for fast CI. Run manually" file-header — intentional |
| Env-gated (real binary) | `src/backend_harness.rs:453,463,474,485` (4), `src/agent.rs:1753` (1, `#[ignore = "spawns real kiro-cli process; run locally only"]`) | 5 | "Requires X installed" / real-process spawn |
| Env-driven A/B | `src/state.rs:2133` (`replay_session`) | 1 | env-driven (`REPLAY_FILE` panic-on-missing) — implicit reason |
| Process-global side effect | `src/bootstrap/signals.rs:139` (`#[ignore = "mutates process-global SIGTERM disposition; run explicitly"]`) | 1 | local-only env effect, reasoned-form annotation |

Per-file breakdown verification (`rg -n "#\[ignore" src tests | awk -F: '{print $1}' | sort | uniq -c | sort -rn`):
```
   7 tests/sprint52_stress.rs
   5 tests/agend_git_shim_phase4_stress.rs
   5 tests/agend_git_shim_phase3_stress.rs
   4 tests/agend_git_shim_phase5_stress.rs
   4 tests/agend_git_shim_phase2.rs
   4 tests/agend_git_shim_phase1_stress.rs
   4 src/backend_harness.rs
   1 src/state.rs
   1 src/bootstrap/signals.rs
   1 src/agent.rs
```
Sum = 36 (29 stress + 5 env-gated + 1 env-driven A/B + 1 process-global)

Audit-2026-04-29's "未標明原因" finding for `state::replay_session` is **resolved**: while the `#[ignore]` attribute itself has no `= "reason"` literal, the test body's first line is `std::env::var("REPLAY_FILE").expect("REPLAY_FILE env var required")` — the panic message functions as the reason annotation. Acceptable per current convention.

**No quarantine cleanup needed.**

---

## Test infrastructure observations

### Mitigation precedent in tree
- `cdec2fb` (PR #420): `Connection: close` to test mock servers for Windows CI hang
- `6c1ac32` (PR #377, Sprint 42 Phase 5): Windows timeout multiplier (`platform_timeout`)
- `tests/mcp_subprocess_is_zero_state.rs`: invariant test forbidding `OnceLock`/`lazy_static`/`once_cell`/`static Mutex` in MCP subprocess code — structural anti-flaky guardrail

The codebase has well-established flaky-mitigation discipline. Triage's recommendations align with existing patterns rather than introducing new ones.

### `platform_timeout` usage gap
`tests/common/harness.rs:457` `platform_timeout(Duration)` returns `timeout * 3` on Windows. Used by harness-callers (e.g. `wait_until_predicate`). NOT used by `tests/integration.rs` raw `thread::sleep` callers — see "Bulk timing risk surface" follow-up recommendation above.

### `fleet_test_guard()` pattern (concentrated in `src/mcp/handlers/tests.rs`)
20 tests acquire the same `MutexGuard<'static, ()>` for serialization. Pattern is sound for intra-process guarantees. macOS-flaky cluster (#3-5) suggests guard scope may be too narrow — recommend follow-up to combine guard with `ux_sink_registry()` access OR migrate to config-injection per #3-5 recommendation.

---

## Aggregate recommendations

| Rank | Item | Recommendation | Out-of-scope reason (if any) |
|---|---|---|---|
| HIGH | macOS-flaky cluster (#3-5, 3 tests) | Sprint 55 RCA + mitigation | Direct fix needs macOS-repro harness work |
| MEDIUM | `tests/integration.rs` 9 raw sleeps | Sprint 55+ migration to harness `platform_timeout` polling | ~150 LOC refactor, broader test-infra task |
| LOW | `worktree_cleanup` Windows path (#1) | Document closure (already mitigated `#[cfg(unix)]`) | None — closeable now |
| LOW | `mcp_bridge_idle_reconnect` (#2, 2 tests) | Document closure (already mitigated `#![cfg(unix)]`) | None — closeable now |
| LOW | 36 `#[ignore]` inventory (34 bare + 2 reasoned-form) | Document health-check pass | None — informational |
| INFO | CI history baseline | Document zero-flake-fire status | None — informational |

---

## Scope boundary

**In-scope (P2-7 triage)**: surface candidates, categorize, document, recommend. Read-only across CI history + archived audits + source tree.
**Out-of-scope (P2-7)**: flake reproduction harness, test-infra refactor (integration.rs sleep migration), `serial_test` crate adoption, RCA depth on macOS-flaky surface.
**This PR delivers**: this audit doc only (Phase 5 inline quarantines skipped per lead m-20260508012856140953-314 — 0 quarantine adds needed).

## Sprint 54 closeout context

P2-7 is the final P2 item per lead m-20260508012306580408-309 ("LAST P2 item — close out Sprint 54 P-tier work post-merge"). This audit doc plus its merge close the P-tier scope. Follow-up Sprint dispatches recommended above (macOS cluster RCA + integration.rs migration) are HIGH/MEDIUM-rank backlog items, not Sprint 54 carry-overs.
