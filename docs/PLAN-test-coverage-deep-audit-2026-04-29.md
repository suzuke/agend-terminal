# PLAN: Test-Coverage Deep Audit

**Date**: 2026-04-29
**Basis**: main HEAD `eea15f2` (post-Sprint-30 wave-close)
**Predecessor**: `docs/audit-tests-2026-04-29.md` (LOC-based estimation, base `102e6ad`)
**Operator brief**: deeper audit than predecessor — real coverage %, fake-test catalogue, dead-test forensics, executable backlog
**Team**: lead2 (orchestrator, minimal-delta) · dev2 / kiro-cli (structural measurement) · reviewer2 / codex (forensic archaeology)

---

## TL;DR

| Metric | Predecessor audit (`102e6ad`) | This audit (`eea15f2`, measured) |
|---|---|---|
| Risk ranking method | LOC × inline-test-count | Line coverage % via `cargo llvm-cov` + forensic patterns |
| Overall coverage | not measured | **72.8 %** (28 068 / 38 553 lines, 105 files) |
| `verify.rs` | claimed "0 tests, HIGH" | **34.8 %** measured + 13 inline tests, ~10 tautological |
| `mcp/handlers/instance.rs` | claimed "0 tests, HIGH" | **46.8 %** measured (integration coverage) |
| `agend-mcp-bridge.rs` | claimed "0 tests, HIGH retry" | **73.7 %** measured (integration coverage) |
| Ignored tests in tree | 8 cited | **6 actual** (2 already deleted in `df27e83`) |
| Tautology count | "good test quality" hand-wave | **6 named coupling/tautology sites** + 14 assertion-light + 4 unwrap-only + 3 let-only |
| Sprint-29-30 deletion residue | not checked | **clean** — no orphan tests |

**Headline correction**: the predecessor audit's three "HIGH risk, 0 tests" calls (`verify.rs`, `instance.rs`, `agend-mcp-bridge.rs`) all turn out to have meaningful integration coverage when measured. The real concern at `eea15f2` is not gaps but **test quality** — specifically `verify.rs`'s tautological inline tests and a small cluster of mock-pair / confirmation-bias coupling in `channel/mod.rs` + `framing.rs` + `backend_harness.rs`.

The single under-covered surface that **does** warrant attention: `src/mcp/handlers/channel.rs` at **10.3 %** (4 / 39 lines) — small file, MCP-handler scope, low coverage; either write meaningful tests or delete dead code per KISS.

---

## 1. Structural perspective — measured coverage (dev2)

### 1.1 Tooling & method

- `cargo llvm-cov 0.8.5` (llvm-tools-preview)
- Tarpaulin skipped: macOS/Darwin without ptrace
- Branch coverage unavailable (would need nightly `-Cinstrument-coverage=branch`)
- Wall time: ~12 min, no toolchain hiccups

### 1.2 Overall

**72.8 % line coverage** (28 068 covered / 38 553 total) across 105 files.

### 1.3 Bottom-30 (lowest coverage) — abridged

The full 30-row table is in dev2's report. Categorised here for triage:

**Category A — TUI / startup / CLI (KISS-skip per single-operator threat model)**:

| File | LOC | Cov % |
|---|---|---|
| `app/api_server.rs` | 84 | 0.0 |
| `app/commands.rs` | 244 | 0.0 |
| `app/dispatch.rs` | 242 | 0.0 |
| `app/session.rs` | 288 | 0.0 |
| `app/telegram_hooks.rs` | 54 | 0.0 |
| `bootstrap/daemon_spawn.rs` | 47 | 0.0 |
| `bootstrap/telegram_init.rs` | 19 | 0.0 |
| `bridge_client.rs` | 35 | 0.0 |
| `cli.rs` | 381 | 0.0 |
| `connect.rs` | 136 | 0.0 |
| `daemon/tui_bridge.rs` | 97 | 11.3 |
| `app/mod.rs` | 720 | 15.3 |
| `tui.rs` | 140 | 20.0 |
| `app/pane_factory.rs` | 395 | 23.8 |
| `quickstart.rs` | 324 | 25.6 |
| `bugreport.rs` | 188 | 28.7 |
| `app/tui_events.rs` | 426 | 29.1 |
| `main.rs` | 376 | 30.9 |
| `render.rs` | 1 780 | 39.3 |
| `app/overlay.rs` | 866 | 41.5 |
| `app/mouse.rs` | 439 | 47.6 |

These are visible-immediately to the single operator (TUI, CLI, bootstrap). KISS principle: do not promote to backlog. Bugs surface via direct interaction; auto-test ROI is low. (See `audit-over-engineering-2026-04-28.md` and `ARCHITECTURE-QUICK-START.md` threat-model section.)

**Category B — non-UI surfaces with genuine low coverage**:

| File | LOC | Cov % | Notes |
|---|---|---|---|
| `bootstrap/signals.rs` | 44 | 9.1 | 1 ignored test (SIGTERM mutation), narrow scope |
| **`mcp/handlers/channel.rs`** | **39** | **10.3** | **MCP handler. 35 lines unreached. Either dead code or under-tested.** |
| `mcp/handlers/schedule.rs` | 21 | 28.6 | small file, 15 uncovered lines |
| `mcp/handlers/ci.rs` | 105 | 32.4 | non-trivial gap |
| `verify.rs` | 549 | 34.8 | inline tests are 10 / 13 tautological — see §3.1 |
| `mcp/handlers/instance.rs` | 570 | 46.8 | integration coverage; predecessor audit overstated risk |
| `api/handlers/mcp_proxy.rs` | 51 | 47.1 | small file |
| `backend_harness.rs` | 311 | 43.4 | mostly ignored (require real CLIs) |
| `daemon/legacy_backfill.rs` | 510 | 53.5 | migration helper; cov reflects migration paths exercised |

### 1.4 Top-30 (highest coverage)

Healthy modules ≥94 % — `decisions.rs`, `auth_cookie.rs`, `mcp_config.rs`, `inbox.rs`, `task_events.rs`, `tasks.rs`, `fleet_broadcast.rs`, `daemon/ticker.rs`, `fleet.rs`, `store.rs`, `instance_monitor.rs`, `daemon/poll_reminder.rs`, `dispatch_tracking.rs`, `framing.rs`, `channel/ux_event.rs`, `instructions.rs`, `mcp/tools.rs`, `channel/contract.rs`, `snapshot.rs`, `worktree_cleanup.rs`, `daemon/heartbeat_pair.rs`, `mcp/handlers/mod.rs`. Six files at 100 % (`channel/auth.rs`, `channel/caps.rs`, `channel/event.rs`, `channel/sink_registry.rs`, `daemon/watchdog.rs`, `identity.rs`, `protocol.rs`, `sync.rs`).

### 1.5 Cross-reference with predecessor audit gap claims

| Predecessor claim | Measured | Verdict |
|---|---|---|
| `verify.rs` "0 tests, HIGH" | 34.8 % (191 / 549), **13 inline tests exist** | Claim factually wrong; conclusion partly stands (low cov + tautology — see §3.1) |
| `mcp/handlers/instance.rs` "0 tests, HIGH" | 46.8 % (267 / 570) via integration | Claim collapses two states; coverage moderate, not crisis |
| `agend-mcp-bridge.rs` "0 unit, HIGH retry" | **73.7 % (168 / 228)** via `mcp_bridge_idle_reconnect` etc. | Claim refuted; recommendation already implemented |
| `api/handlers/instance.rs` (not flagged) | 69.7 % (264 / 379) | n/a — confirms HTTP layer well-tested |

### 1.6 Weak-assertion grep (dev2)

- **0** literal `assert!(true)` / `assert_eq!(x, x)` tautologies (good — none of that pattern slipped in)
- **60** raw `#[test]`-without-`assert` matches; after manual classification:
  - 24 e2e behavioural tests using `e2e_fixture_behavioral()` helper that contains the asserts (valid)
  - 12 intentional no-panic smoke tests (valid by design)
  - ~10 grep false positives (have `assert_*` via macros)
  - **~14 genuinely assertion-light** — strengthen-or-delete candidates
- **4 unwrap-only tests** (assertion is "doesn't panic"): `backend_harness.rs:392`, `backend_harness.rs:397`, `auth_cookie.rs:340`, `telegram.rs:2732`
- **3 let-only tests** (`let _ = ...` with no assert): `instance_monitor.rs:172`, `behavioral.rs:306`, `legacy_backfill.rs:632`

### 1.7 `cargo bloat --release` summary

`agend_terminal` crate 1.5 MiB / 18 % of 8.6 MiB `.text` binary. Hot fns (size proxy for code paths worth covering): `daemon::run_core` (73.5 K), `app::run_app` (58.8 K), `api::handle_session` (54.6 K), `mcp::handlers::handle_tool` (36.5 K), `main` (35.4 K), `mcp::tools::tool_definitions` (38.7 K), `mcp::tools::instance_tools` (28.6 K). The largest entries are run-loops that integration tests already drive — no surprise gaps.

---

## 2. Forensic perspective — patterns + evidence (reviewer2)

### 2.1 Churn × no-test leaderboard (last 60 days)

| Rank | File | Commits 60d | Inline unit tests | Top recent SHA |
|---|---|---|---|---|
| 1 | `src/cli.rs` | 48 | 0 | `4345289` |
| 2 | `src/verify.rs` | 43 | 0 inline (13 *trivial* — see §3.1) | `8473547` |
| 3 | `src/app/dispatch.rs` | 13 | 0 | `3883a34` |
| 4 | `src/tray/mod.rs` | 10 | 0 | `1b3f4a9` |
| 5 | `src/app/session.rs` | 10 | 0 | `d97df24` |
| 6 | `src/app/commands.rs` | 9 | 0 | `e797ebd` |
| 7 | `src/connect.rs` | 8 | 0 | `e797ebd` |
| 8 | `src/api/handlers/team.rs` | 7 | 0 | `21d78be` |
| 9 | `src/bin/agend-mcp-bridge.rs` | 5 | 0 | `8310b25` |
| 10 | `src/mcp/handlers/instance.rs` | 1 | 0 | `37f2dd1` |

*reviewer2 reports "0 inline" for verify.rs — the 13 inline tests at `:620-731` are noisy enough to count as 0 functionally; see §3.1.*

Cross-correlated with §1: the high-churn TUI/CLI files (1, 3, 4, 5, 6, 7) match the §1.3 Category-A KISS-skip list. **Churn × no-test alone is not a backlog signal under the single-operator threat model**; combine with §1's measured coverage to filter.

### 2.2 Pass-but-don't-test patterns (file : line + SHA)

reviewer2 found six concrete sites:

1. **Mock-pair tautology** — `src/backend_harness.rs:456-460` (`904f67e`): writes `level` into matrix, asserts matrix equals same `level`.
2. **Confirmation-bias coupling #1** — impl `src/channel/mod.rs:196-211` and tests `:390-411` from same commit `21d78be` (same author, same SHA → §3.5.11 anti-pattern residue).
3. **Confirmation-bias coupling #2** — impl `src/channel/mod.rs:225-227` and test `:546-553` from same commit `289373e`.
4. **Tautological struct-literal assert** — `src/channel/mod.rs:430-435` (`21d78be`) asserts struct fields against the same values just constructed two lines above.
5. **Tautological constants** — `src/framing.rs:206-208` (`1f584d7`) asserts constants equal their literal assignments.
6. **Test mocks SUT boundary** — `src/channel/mod.rs:342-387` and `:390-411` (`21d78be`) validates trait-default plumbing on `MockChannel` rather than real `Channel` impl.

Three of six sites concentrate in `channel/mod.rs` from commit `21d78be` — that PR is the primary cluster.

### 2.3 Ignored-test forensics — eight cited, six actual

| # | Test | Status at `eea15f2` | Recommendation |
|---|---|---|---|
| 1 | `test_backend_semantics_kiro` | live, ignored, `:453-460`, added `904f67e` | keep (real-CLI gate) |
| 2 | `test_backend_semantics_codex` | live, ignored, `:463-471`, added `904f67e` | keep |
| 3 | `test_backend_semantics_gemini` | live, ignored, `:474-482`, added `904f67e` | keep |
| 4 | `test_backend_semantics_claude` | live, ignored, `:485-493`, added `904f67e` | keep |
| 5 | `test_claimed_task_not_touched_by_dep_eval` | **DELETED** in `df27e83` (PR #300, 2026-04-29) | drop from tracking |
| 6 | `test_list_default_hides_done_older_than_14d` | **DELETED** in `df27e83` (PR #300, 2026-04-29) | drop from tracking |
| 7 | `install_term_only_catches_sigterm` | live, ignored `bootstrap/signals.rs:139-140`, added `a9df3a8` | keep (process-global SIGTERM mutation) |
| 8 | `replay_session` | live, ignored `state.rs:2143-2145`, added `a2f4350`, evolved `7faac1a` | keep but classify as manual harness; annotate `#[ignore = "manual harness — replay session fixture"]` |

Confirmation: predecessor audit's "delete 2 ignored" recommendation is **already done** by PR #300.

### 2.4 Sprint-29-30 deletion residue scan

reviewer2 grepped for stale tests referencing removed defenses (RBAC / outbound-capability, slow-loris, heartbeat-spam, self-healing supervisor, symlink-escape, const-time-cookie, frame-env override). **No stale behavioural assertions found.** Only residual references are historical comments (e.g., `tests/mcp_bridge_idle_reconnect.rs` mentions slow-loris in context, not as enforcement). Sprint-29-30 cleanup was complete.

---

## 3. Minimal-delta perspective — KISS filter (lead2)

This perspective answers: given operator's KISS philosophy and §0 "what real problem does this solve", which findings merit a fix-PR vs. which are noise?

### 3.1 The predecessor audit got `verify.rs` factually wrong, conclusion partly right

**Predecessor**: "0 tests, 616 LOC, HIGH risk."

**Reality**: 731 LOC, 13 inline tests at `src/verify.rs:620-731`, 34.8 % measured coverage. **Tests are mostly tautological**:

| Test | Pattern | file:line |
|---|---|---|
| `test_result_ok_sets_passed_true` | constructor `ok()` asserts `.passed == true` — pure tautology | `src/verify.rs:623` |
| `test_result_fail_sets_passed_false` | inverse | `src/verify.rs:631` |
| `test_result_from_bool_true` | `from_bool(true)` asserts `.passed` | `src/verify.rs:638` |
| `test_result_from_bool_false` | inverse | `src/verify.rs:645` |
| `test_spawn_config_defaults` | hardcoded constants asserted against hardcoded constants — both change in same commit | `src/verify.rs:652` |
| `test_mcp_framing_returns_result` | mock-pair tautology — asserts the production verify-helper returns `passed=true`; if helper is broken in prod, test still passes (helper IS the SUT) | `src/verify.rs:691` |
| `test_backend_config_returns_result` | same shape | `src/verify.rs:700` |
| `test_instructions_returns_result` | same + disjunction `passed \|\| detail.contains("false")` accepts either outcome | `src/verify.rs:709` |
| `test_inbox_returns_result` | same shape | `src/verify.rs:723` |

**Genuinely useful** (3 of 13): `poll_until_returns_true_immediately`, `poll_until_returns_false_on_timeout`, `poll_until_succeeds_after_retries`, plus `test_spawn_config_with_home`. The remaining nine deserve deletion, not strengthening. Coverage of the deleted lines drops, but the dropped lines aren't being exercised meaningfully — the coverage number is misleadingly green.

### 3.2 Predecessor's "delete 2 ignored tests" already merged in PR #300

reviewer2's archaeology (§2.3) confirms `df27e83` (PR #300 wave-2 refactor) deleted both. Drop the recommendation from forward tracking.

### 3.3 `instance.rs` and `agend-mcp-bridge.rs` claims refuted

`instance.rs` 46.8 %, bridge 73.7 % (§1.2, §1.5). Predecessor's "needs unit tests" framing for both modules misreads integration coverage as absence. Bridge's "zero crate dependencies" architecture (`audit-over-engineering-2026-04-28.md` finding #5) makes integration the right test layer; do not promote to backlog.

### 3.4 `mcp/handlers/channel.rs` 10.3 % is the real gap

Small file (39 lines), 4 covered, 35 not. Either dead code or genuine handler that lacks coverage. Worth investigating — KISS triage:

- if dead → delete (counter-example: nothing breaks → §3.5.12 (d) gate satisfied)
- if live → either real test or argument for why this handler doesn't need one
- size estimate: ≤ 50 LOC delta either way

### 3.5 Fake-test cluster in `channel/mod.rs`

reviewer2's six sites (§2.2) concentrate at `channel/mod.rs` — three from `21d78be`, one from `289373e`, plus `framing.rs:206-208` and `backend_harness.rs:456-460`. Pattern: trait-default plumbing tested against the mock the test wires up. **Net-negative LOC** opportunity: delete the six tests + leave behavioural tests that drive real channels.

### 3.6 §3.5.10 wire-format invariant test gap (new, not in predecessor audit)

`agend-mcp-bridge.rs` is canonical wire-format scope per §3.5.10. The Sprint-30 amendment requires an invariant test pinning the post-change shape. `tests/mcp_tools_count.rs` exists for the daemon's tool count — verify whether a parallel invariant exists for the bridge's protocol surface (handshake bytes, supported methods, framing contract). If absent, add.

### 3.7 KISS-skip categorical (§1.3 Category A confirms)

The predecessor audit's "low priority — UI 層 / clap 保護 / 平台相關" tier is correct. dev2's measurements confirm those modules are at 0–47 %. **Do not promote to backlog**; the threat model (single operator, immediate-visible failures) makes auto-test ROI low. Specifically not in scope: `app/*.rs`, `cli.rs`, `connect.rs`, `tray/mod.rs`, `bridge_client.rs`, `bootstrap/{daemon_spawn, telegram_init}.rs`, `bugreport.rs`, `quickstart.rs`, `tui.rs`, `render.rs` (despite high LOC, render is observed visually).

### 3.8 Borderline case: `daemon/tui_bridge.rs` 11.3 %

97 LOC, 11 covered. Daemon-side, not pure TUI — straddles Category A and B. Suggest deferring: if a daemon bug surfaces here, fix-it-then. KISS: don't pre-emptively test.

---

## 4. Executable backlog (synthesis)

Each item: priority · LOC estimate (negative = net deletion) · rationale (the "what real problem does deletion break?" answer per §0) · references.

### P1 — real action

| ID | Title | Δ LOC | Rationale | References |
|---|---|---|---|---|
| **B1** | `verify.rs`: delete 9 tautological tests; add 1–2 behavioural tests that mutate a verify-helper and observe the corresponding test fail | **−80** | Tautologies pass while the verify subsystem (operator's debug entry point) is silently broken. Deletion breaks no real signal — the tests cannot catch any bug they aren't already wired to. | §3.1 · `src/verify.rs:623-731` |
| **B2** | `mcp/handlers/channel.rs`: investigate 10.3 % coverage — either delete dead handler code or write 1 real test | ±50 | Smallest non-UI low-coverage file; either dead code (KISS-delete per §3.5.12 (d)) or live MCP handler that warrants ≥1 behavioural test | §3.4 · `src/mcp/handlers/channel.rs` |
| **B3** | Delete `channel/mod.rs` mock-pair / coupling cluster + `framing.rs` constant tautology + `backend_harness.rs:456-460` | **−60** | Six tests that assert what they wire up — pass-but-don't-test by reviewer2 forensics. Deleting breaks no real coverage; each was authored in the same commit as its impl (§3.5.11 anti-pattern residue) | §2.2, §3.5 · 6 sites in §2.2 |
| **B4** | §3.5.10 wire-format invariant test for `agend-mcp-bridge.rs` — verify presence; add if absent (parallel to `tests/mcp_tools_count.rs`) | 0 or +30 | Sprint-30 amendment compliance; bridge is canonical wire-format surface. Catches silent regression of supported methods / handshake bytes | §3.6 · `docs/FLEET-DEV-PROTOCOL-v1.md` §3.5.10, PR #299 |

### P2 — hygiene / docs

| ID | Title | Δ LOC | Rationale | References |
|---|---|---|---|---|
| H1 | Annotate `src/state.rs:2143` `#[ignore]` with reason: `#[ignore = "manual harness — replay session fixture"]` | +1 | Predecessor audit flagged unmarked ignore; reviewer2 confirms manual-harness intent | §2.3 #8, predecessor audit "8 個 Ignored Tests" #8 |
| H2 | Drop two stale entries from `docs/audit-tests-2026-04-29.md` (already deleted in PR #300) | −2 rows | Otherwise future audit cycles re-confirm deleted artifacts | §2.3 #5 #6, §3.2 |
| H3 | Strengthen-or-delete the 14 assertion-light tests + 4 unwrap-only + 3 let-only | net negative | Default-delete unless a behavioural assertion is identifiable | §1.6, file:line list there |
| H4 | Document KISS-skip rationale for §3.7 categorical list in repo (`CLAUDE.md` or per-file comment), so future audits don't re-litigate | +20 doc LOC | Preempts `app/*.rs` / `cli.rs` re-flag | §3.7 |

### Explicitly NOT in backlog (operator-visible KISS-skip)

- `app/api_server.rs`, `app/commands.rs`, `app/dispatch.rs`, `app/session.rs`, `app/telegram_hooks.rs`, `app/mod.rs`, `app/pane_factory.rs`, `app/tui_events.rs`, `app/overlay.rs`, `app/mouse.rs`
- `cli.rs`, `connect.rs`, `tui.rs`, `render.rs`
- `tray/mod.rs`, `bridge_client.rs`, `bugreport.rs`, `quickstart.rs`, `main.rs`
- `bootstrap/{daemon_spawn, telegram_init, signals}.rs`
- `daemon/tui_bridge.rs` (per §3.8 borderline — fix-on-failure)

---

## 5. Process notes

- **Worktree**: `/Users/suzuke/.agend-terminal/workspace/lead2/repo` on branch `plan/test-coverage-deep-audit-2026-04-29` off `eea15f2`
- **Decision**: `d-20260429061614945353-1` (scope frozen)
- **Fleet task**: `t-20260429061615252875-2` (lead2 owner)
- **Dispatches** (4-perspective challenge round, parallel):
  - dev2 (kiro-cli) — structural — dispatched 06:18Z, reported 06:28Z (10 min wall, 12 min llvm-cov)
  - reviewer2 (codex) — cross-vantage — dispatched 06:18Z, reported 06:22Z (4 min)
  - lead2 (this doc) — minimal-delta synthesis
- **PR path**: §3.5.5 LOW docs-only single-reviewer self-merge. lead2 owns `watch_ci`. Verdict mirrors per §3.5.13.
- **Out-of-scope for this PR**: implementing any backlog item. Implementation dispatches happen in subsequent rounds, one per priority lane.

### Bug surfaced during orchestration

`mcp/handlers/comms.rs::handle_unified_send` for `request_kind: task` reads `args["task"]` (line 174) but the unified `send` schema documents `message` as the field name. The mapping `message → task` is not performed (compare `kind: report` at lines 38-42 which maps `message → summary`). Workaround used: dispatched both task messages via plain `send` with the task semantics inlined in the `message` body and a leading `## TASK …` header. Recommend follow-up fix to add the `message → task` mapping (small parallel to existing `message → summary` mapping). Not in this audit's scope; flag for a separate one-line PR.
