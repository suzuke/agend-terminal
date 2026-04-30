# PLAN: Sprint 45 — Architecture-group fix wave (9 Groups × 1 PR with bundled test-coverage uplift)

**Date:** 2026-05-01
**Status:** Drafted by general (operator-proxy) per operator instruction "Sprint 44 收尾就直接寫 Sprint 45 PLAN" (TUI direct 2026-04-30). Awaiting operator §13 GO before any IMPL dispatch.
**Branch:** `plan/sprint45-architecture-fix-wave`
**Origin:** kiro-cli-ea377a code review m-20260430143550920865-672 (50 issues: 17 HIGH + 33 MEDIUM, not counting the 6 already closed in Sprint 44 prelude). Operator decision: 1 Group = 1 PR, each PR bundles fixes + test-coverage uplift for the same files.
**Process:** General (claude) drafted from ea377a's audit cross-referenced with `docs/ARCHITECTURE-GROUPS.md` (Sprint 40 baseline coverage data). 4-perspective synthesis NOT run — operator chose direct draft over delegated PLAN-first to keep cadence after Sprint 44 closeout.

---

## 0. KISS gate (§0) + operator constraint

- **What real problem does this solve?** Two coupled debts surfaced by ea377a's deep architecture audit:
  1. **17 HIGH-severity bugs** spread across 7 of 9 architecture groups — concurrency unsoundness (`std::env::set_var` Rust ≥1.66 UB), unbounded resources (CI watch threads, telegram threads, forwarder threads, poll_reminder leaks), non-atomic state writes (event_log rotation, task_events scan-on-append), unsafe primitives (constant-time comparison missing in auth_cookie, async-signal-unsafe `process::exit` in signal handlers), bypass-able invariants (team isolation by name "general", path traversal in handle_checkout_repo), and silent failure modes (`spawn_or_block_on` discarded results).
  2. **Coverage debt** in 4 groups (G2 76.8% / G4 73.2% / G7 25.8% / G9 52.8%) — Group 7 TUI/App is the worst at 25.8% with 5 files at 0%. Bug fixes without coverage uplift means future regressions ship silently.
- **Would deletion break anyone?** No deletion — all fixes are net-add-or-tighten. Doing nothing = continued recurring incidents (every cron tick spawns new threads, every CI watch blocks on poll, every panic leaves terminal broken, every concurrent deployment can drop fields).
- **Operator rule applied:** 1 Group = 1 PR, fixes + tests bundled. Rationale (operator m-): touch-the-file pattern — when you're already changing `src/agent.rs` for H1 (broadcast lock release), adding tests for the same file is the cheapest moment. Avoid 9 fix PRs + 9 separate coverage PRs (18 cycles of overhead).
- **Sprint 44 dogfooding:** every Sprint 45 PR pushes through M1 push-time claim verifier + M2 pre-push hook + M3 reviewer SHA-staleness gate + M6 ci-watch supersede. Sprint 45 stress-tests Sprint 44's gates against 9 real PRs of varying complexity.

**Daemon-rebuild precondition:** Sprint 44's 6 fixes (M1-M6) shipped to main but the running daemon still executes the pre-Sprint-44 binary. Until operator rebuilds + restarts the daemon, the gates are LATENT — Sprint 45 dispatch will use Sprint 44 gates only after operator triggers rebuild. PLAN does not block on this; if rebuild is delayed, Sprint 45 ships under pre-Sprint-44 review discipline (lead manual cross-vantage proven adequate during Sprint 44).

---

## 1. Verified current state

### 1.1 Sprint 44 closure

`d-20260430181600917488-11` records 6/6 fixes shipped:
- M1 push-time claim verifier (PR #384 a5fe…aefe1c1)
- M2 git pre-push hook (`core.hooksPath = scripts/hooks/`, PR #384)
- M3 reviewer SHA-staleness gate (PR #385 e73a228)
- M4 hallucinated-fn check (syn-lite + rg fallback, PR #386 c53c6f8)
- M5 self-route bug fix (PR #383 b795d42)
- M6 ci-watch supersede (`InboxMessage::superseded_by`, PR #385)

Source on main; daemon-rebuild gap noted above.

### 1.2 ea377a audit intake

Inbox source: `m-20260430143550920865-672` (not counting the 6 already-closed Sprint 44 prelude tasks C1-C3 + H1-H3).

Per-group enumeration (counts derived directly from the itemised list; **the source's summary line stated 17 HIGH + 33 MEDIUM = 50 total but the itemised list actually contains 21 HIGH + 37 MEDIUM = 58 — the summary line was an arithmetic mismatch and the itemised list is authoritative**):

| Group | HIGH | MEDIUM | Total | §2 anchor |
|-------|------|--------|-------|-----------|
| G1 Agent State Classifier | 0 | 2 | 2 | §2 G1 |
| G2 Agent Lifecycle & Process | 2 | 3 | 5 | §2 G2 |
| G3 Daemon Core | 3 | 5 | 8 | §2 G3 |
| G4 MCP Layer | 2 | 6 | 8 | §2 G4 |
| G5 Fleet Config & Management | 0 | 4 | 4 | §2 G5 |
| G6 Persistence & Audit | 4 | 4 | 8 | §2 G6 |
| G7 TUI / App Layer | 5 | 5 | 10 | §2 G7 |
| G8 Channel Layer | 3 | 4 | 7 | §2 G8 |
| G9 CLI / Entry / Bootstrap | 2 | 4 | 6 | §2 G9 |
| **Total** | **21** | **37** | **58** | — |

Severity classes:
- HIGH (21): concurrency / soundness / unbounded / data-loss / security
- MEDIUM (37): consistency / cleanup / refactoring / clippy

§2 below enumerates each issue per group as a one-line bullet — anyone reviewing this PLAN can grep the source inbox message + cross-check against the per-group count column above for completeness audit.

### 1.3 Coverage baseline

Source: `docs/ARCHITECTURE-GROUPS.md` (commit `9061a1b`, `cargo llvm-cov 0.8.5`):

| # | Group | LOC | Coverage now | Coverage target | Δ |
|---|-------|-----|-------------|-----------------|---|
| 1 | Agent State Classifier | 3,940 | 92.5% | 93%+ | maintain |
| 2 | Agent Lifecycle & Process | 4,175 | 76.8% | 85% | +8 |
| 3 | Daemon Core | 8,814 | 81.5% | 87% | +5.5 |
| 4 | MCP Layer | 7,682 | 73.2% | 85% | +12 |
| 5 | Fleet Config & Management | 3,359 | 97.0% | 97%+ | maintain |
| 6 | Persistence & Audit | 6,896 | 92.2% | 94% | +2 |
| 7 | TUI / App Layer | 9,482 | 25.8% | 50%+ | +24 |
| 8 | Channel Layer | 7,344 | 80.1% | 85% | +5 |
| 9 | CLI / Entry Points | 4,524 | 52.8% | 65% | +12 |

Targets are conservative — uplift dimensioned by what touch-the-file affords during fix work, not aspirational coverage drives. Groups 1/5/6 already strong; maintain not lift.

---

## 2. 9 Group breakdown

Per ea377a `m-20260430143550920865-672`. Each Group entry: HIGH × MEDIUM count → main files → fix scope → coverage uplift scope → LOC est → Tier.

### Group 1 — Agent State Classifier (0H + 2M)

- **Issues**: regex re-compile in `classify_pty_output` (cache); `strip_ansi` OSC terminator handling
- **Main files**: `src/state.rs` (2674 LOC, 92.8%)
- **Fix scope**: `LazyLock<Regex>` cache; OSC `\x1b]...\x07` / `\x1b\\` terminator coverage
- **Coverage uplift**: maintain 92.5%+ (already strong; add 1-2 OSC variant tests)
- **LOC est**: ~40 + 30 test
- **Tier**: Tier-1 (single file, low risk)

### Group 2 — Agent Lifecycle & Process (2H + 3M)

- **Issues**: H1 `inject_to_agent` lock-across-sleep (typed mode 10KB → 20s lock); H2 `try_dismiss_dialog` unbounded threads; M1 `save_metadata` non-atomic; M2 `kill_process_tree` PID==PGID assumption; M3 `spawn_instructions_bootstrap` census class
- **Main files**: `src/agent.rs` (1244 LOC, 71.6%), `src/agent_ops.rs` (417 LOC, 93.5%), `src/process.rs` (53 LOC, 83%)
- **Fix scope**: collect-then-release-then-inject (mirrors Sprint 44 H1 broadcast pattern); thread budget + reuse for dialog dismissal; `mutate_versioned` for save_metadata; `getpgid()` real query for PGID; correct fire-and-forget census
- **Coverage uplift**: agent.rs 71.6% → 80% (+8) — add tests for the patched lock paths + PGID resolution
- **LOC est**: ~150 + 120 test
- **Tier**: Tier-2 (concurrency invariants + spawn-site protocol)

### Group 3 — Daemon Core (3H + 5M)

- **Issues**: H1 `std::env::set_var` multi-thread UB (Rust ≥1.66); H2 CI watch unbounded threads + tokio runtimes per poll; H3 `poll_reminder` global HashMap leak for deleted agents; M1 CI watch state file non-atomic; M2 `legacy_backfill`/`task_sweep` duplicate `strip_html_comments`/`sha256_hex`; M3 team isolation bypassable by agent named "general"; M4 `tui_bridge` output thread leak post-disconnect; M5 `register_external` lock ordering undocumented
- **Main files**: `src/daemon/mod.rs` (1145, 77.1%), `src/daemon/ci_watch.rs` (1744, 92.6%), `src/daemon/poll_reminder.rs` (178, 97.2%), `src/daemon/tui_bridge.rs` (97, 14.4%), `src/daemon/legacy_backfill.rs` (747, 53.5%), `src/daemon/task_sweep.rs` (372, 65.6%)
- **Fix scope**: replace `set_var` with config injection at startup (or process restart for env change); single shared tokio runtime + bounded worker pool for CI watch; agent-deletion hook into poll_reminder cleanup; `mutate_versioned` for CI state; extract shared helpers to `daemon/utils.rs`; team-isolation reserved-name check (general/lead-as-instance); tui_bridge cleanup on disconnect; `DAEMON-LOCK-ORDERING.md` cross-reference
- **Coverage uplift**: daemon/mod.rs 77.1% → 85% (+8); tui_bridge.rs 14.4% → 60% (+46, biggest gap in group); task_sweep.rs 65.6% → 80% (+15); legacy_backfill.rs 53.5% — opportunistic, primary question is whether to remove (deferred to §6)
- **LOC est**: ~280 + 220 test
- **Tier**: Tier-2 (cross-module concurrency contracts + lock ordering)

### Group 4 — MCP Layer (2H + 6M)

- **Issues**: H1 Inbox enqueue O(n) (read full file → rewrite); H2 `handle_checkout_repo` source path unvalidated; M1 `pane_snapshot` u64→usize 32-bit wrap; M2 `Sender::new()` no name format validation; M3 `OnceLock` ACL cache (env change requires restart); M4 dedup counter `AtomicU16` 65536 wrap; M5 team spawn 3s hardcoded sleep + missing fire-and-forget annotation; M6 `pending_pickup_ids` cleanup may desync with inbox
- **Main files**: `src/inbox.rs` (2408, 95%), `src/mcp/handlers/comms.rs` (700, 86.9%), `src/mcp/handlers/instance.rs` (564, 47.5%), `src/mcp/handlers/ci.rs` (105, 32.4%), `src/mcp/handlers/channel.rs` (39, 10.3%), `src/mcp/handlers/schedule.rs` (21, 28.6%), `src/identity.rs` (46, 100%)
- **Fix scope**: append-only inbox with index file (eliminate O(n) rewrite); checkout_repo path canonicalize + system-prefix reject (mirrors Sprint 44 H3 pattern); explicit u64→usize bounds check; `Sender::new()` with regex format check; ACL cache invalidation hook on env-set MCP signal; `AtomicU64` for dedup counter; `// fire-and-forget: ...` annotation per §10.5; pickup_ids cleanup correlated to inbox-drain transaction
- **Coverage uplift**: instance handler 47.5% → 75% (+27, largest gap); channel handler 10.3% → 70% (+60); schedule handler 28.6% → 80% (+51); ci handler 32.4% → 70% (+38) — these 4 handlers are the main MCP debt
- **LOC est**: ~250 + 350 test (large because 4 low-coverage handlers need uplift)
- **Tier**: **Tier-2** (MCP wire surface + inbox storage contract)

### Group 5 — Fleet Config & Management (0H + 4M)

- **Issues**: M1 `ready_pattern` regex no catastrophic-backtracking guard; M2 `working_directory` no path-traversal validation; M3 `teams.rs` update TOCTOU (re-load may read stale store); M4 `teams.rs` create success-path warnings dead code
- **Main files**: `src/fleet.rs` (1494, 96.8%), `src/teams.rs` (504, 92.9%)
- **Fix scope**: `regex::RegexBuilder::size_limit()` for ready_pattern; `Path::canonicalize()` + reject `..` traversal; `mutate_versioned` for team-update atomicity; remove dead-code paths
- **Coverage uplift**: maintain 97%+ (already strong; add 4-5 invariant tests)
- **LOC est**: ~80 + 80 test
- **Tier**: Tier-1 (config-layer cleanup, low blast-radius)

### Group 6 — Persistence & Audit (4H + 4M)

- **Issues**: H1 `task_events::max_seq_for_instance()` scans full file per append; H2 `task_events::replay()` unbounded in-memory load; H3 Schedule ID seconds-granularity collision; H4 `event_log` rotation non-atomic (readers see empty); M1 `store::load()` parse-fail silent default + next-write overwrite; M2 `snapshot.rs` re-implements `atomic_write()`; M3 `dispatch_tracking.rs` entries never cleaned; M4 `auto_close_merged_tasks()` uses "system" sender to bypass `can_mutate_record`
- **Main files**: `src/task_events.rs` (1722, 95.7%), `src/schedules.rs` (925, 92.8%), `src/event_log.rs` (139, 84.2%), `src/store.rs` (257, 96.9%), `src/snapshot.rs` (143, 98.6%), `src/dispatch_tracking.rs` (178, 97.2%), `src/tasks.rs` (2170, 95.7%)
- **Fix scope**: index file for max_seq tracking (H1+H2 shared design); ID = seconds-µs or seconds + counter (H3); event_log rename-then-truncate (atomic from reader's perspective); parse-fail loud + manual rescue (M1); snapshot use `store::atomic_write` (M2); dispatch_tracking 30-day TTL cleanup (M3); proper `system` sender ACL (M4 — this one is contentious; may need protocol decision)
- **Coverage uplift**: event_log 84.2% → 92% (+8); maintain others 92%+ (add 6-8 invariant tests for ID-collision / rotation-atomicity / TTL-cleanup)
- **LOC est**: ~280 + 200 test
- **Tier**: **Tier-2** (data-integrity contracts; H1+H2+H3+H4 are real correctness bugs)

### Group 7 — TUI / App Layer (5H + 5M)

- **Issues**: H1 Forwarder threads not cleaned post-pane-close (zombie accumulation); H2 `sync_fleet_yaml` post-crash deletion of unspawned panes; H3 `flush_idle_notifications` 50ms disk I/O without throttle; H4 `split_chunks` underflow on tiny terminals; H5 `overlay.rs MovePaneTarget` cleared overlay before move logic (fragile); M1 `render.rs` 2385 LOC + `layout.rs` 2106 LOC need split; M2 `key_to_bytes` Ctrl+non-letter wrong bytes; M3 Task board Assign mode uses col/row instead of task ID; M4 tab-bar hit-test re-renders layout; M5 `run_app` 300+ lines / 15+ locals
- **Main files**: `src/app/mod.rs` (1039, 15.3% — worst), `src/render.rs` (2385, 40%), `src/layout.rs` (2106, 75.5%), `src/app/overlay.rs` (1196, 41.5%), `src/app/tui_events.rs` (697, 29.1%), `src/app/mouse.rs` (439, 47.6%)
- **Fix scope**: forwarder lifecycle hooks tied to pane drop; sync_fleet_yaml dry-run + commit pattern; flush_idle_notifications throttle (≥1s + dirty-flag); split_chunks `saturating_sub` + min-size guard; overlay state machine for MovePaneTarget; render.rs/layout.rs sub-module extraction (deferred to Phase 2 if scope explodes); key_to_bytes correct CSI mapping; Assign mode by task ID; cached layout for tab-bar hit-test; run_app extract to `app/event_loop.rs`
- **Coverage uplift**: app/mod.rs 15.3% → 50% (+35 — biggest single-file gap); render.rs 40% → 60% (+20); layout.rs 75.5% → 85% (+10); overlay.rs 41.5% → 65% (+24); tui_events.rs 29.1% → 60% (+31)
- **LOC est**: ~400 + 600 test (largest by far due to 25.8%→50% uplift target)
- **Tier**: **Tier-2** (high LOC + cross-module + behavioral changes)

### Group 8 — Channel Layer (3H + 4M)

- **Issues**: H1 `notify_telegram_inner` per-notification thread + tokio runtime; H2 Topic registry no atomicity/lock; H3 `spawn_or_block_on` silently discards results (react/edit reports success but may fail); M1 telegram.rs 4205 LOC needs split; M2 `pending_pickup_ids` unbounded; M3 Discord caps declares react but returns NotSupported; M4 Reply BindingRef confuses msg_id and topic_id
- **Main files**: `src/channel/telegram.rs` (4205, 62.1%), `src/channel/mod.rs` (214, 70.1%), `src/channel/binding.rs` (77, 92.2%), `src/channel/caps.rs` (41, 100%)
- **Fix scope**: shared tokio runtime + bounded sender pool for telegram; `Mutex` around topic registry mutation; `spawn_or_block_on` propagates `Result` (caller responsible); telegram.rs split into `telegram/{handler,topic,sender,state}.rs` (Phase 2 if scope blows); pending_pickup_ids cap + LRU eviction; Discord caps actually-implemented or remove from declared caps; BindingRef msg_id/topic_id type-distinguish
- **Coverage uplift**: telegram.rs 62.1% → 75% (+13 — best ROI in group); mod.rs 70.1% → 85% (+15)
- **LOC est**: ~300 + 250 test
- **Tier**: **Tier-2** (concurrency + state-mutation invariants)

### Group 9 — CLI / Entry Points / Bootstrap (2H + 4M)

- **Issues**: H1 `connect.rs` signal handler calls `process::exit()` (async-signal-unsafe); H2 `auth_cookie::verify` non-constant-time comparison; M1 Telegram token validation only checks `:` presence; M2 `admin.rs` uses `git branch -D` (force) instead of `-d`; M3 `bugreport.rs` redaction only handles single-line YAML; M4 `worktree_cleanup.rs` test uses non-thread-safe `set_var`
- **Main files**: `src/connect.rs` (136, 0%), `src/auth_cookie.rs` (252, 94.8%), `src/admin.rs` (200, 83%), `src/bugreport.rs` (188, 28.7%), `src/worktree_cleanup.rs` (250, 98.8%)
- **Fix scope**: signal handler sets atomic flag, main loop exits cleanly; `subtle::ConstantTimeEq` for cookie compare; telegram token regex (12+ digits : 35-char alphanum); `git branch -d` then fallback for unmerged confirmation; multi-line YAML aware redaction; explicit-param closures replace test set_var (mirrors Sprint 44 lesson)
- **Coverage uplift**: connect.rs 0% → 50% (+50 — extract pure-logic if possible); bugreport.rs 28.7% → 65% (+36)
- **LOC est**: ~150 + 200 test
- **Tier**: Tier-2 (security-sensitive constant-time + signal-safety)

---

## 3. Phase rollout (proposed)

### 3.1 Ordering rationale

Two competing rankings:
- **By severity-incidence (ea377a HIGH count)**: G7(5) > G6(4) > G3(3) = G8(3) > G2(2) = G4(2) = G9(2) > G1(0) = G5(0)
- **By production-risk × user-facing surface (ARCHITECTURE-GROUPS.md priority + coverage gap)**: G4(H) > G7(H) > G8(M) > G2(M) > G3(M) > G1(L) = G5(L) = G6(L) = G9(L)

Hybrid scoring (data-integrity & soundness first, UX last):

| Rank | Group | Why first | Why not later |
|---|-------|-----------|---------------|
| 1 | **G6 Persistence** | 4 HIGH all data-integrity (task_events scan/replay, schedule ID collision, event_log rotation atomicity); fix here unblocks confident downstream changes | Delay risks losing audit data on every collision-prone schedule write |
| 2 | **G3 Daemon Core** | 3 HIGH includes `set_var` UB (multi-thread Rust UB), unbounded CI watch threads, poll_reminder leak — runtime stability | These compound — every cron tick worsens until fix |
| 3 | **G7 TUI/App** | 5 HIGH (zombie threads, post-crash desync, idle-notification I/O storm, terminal underflow, overlay race); biggest coverage gap (25.8% → 50%) means ROI on tests is highest | Visible to operator daily; fixes pay back fast |
| 4 | **G8 Channel** | 3 HIGH (telegram thread storm, topic registry race, silent failure dropping); telegram.rs is 4205 LOC at 62.1% — worth investing | Defer-able if G3 fix already addresses notify_telegram_inner thread pattern |
| 5 | **G4 MCP Layer** | 2 HIGH (inbox O(n), checkout_repo path); biggest coverage uplift surface (4 handlers <50%) | After G3 because daemon-side fixes change MCP handler context |
| 6 | **G2 Agent Lifecycle** | 2 HIGH (inject lock-across-sleep, dialog thread bomb); agent.rs 71.6% needs uplift | Sequential after G3 (daemon hooks change spawn semantics) |
| 7 | **G9 CLI/Entry/Bootstrap** | 2 HIGH security-grade (signal-safety, constant-time auth) | Rare execution paths but soundness matters |
| 8 | **G1 State Classifier** | 0 HIGH, 2 MEDIUM (regex cache, OSC); already 92.5% covered | Truly low priority — opportunistic |
| 9 | **G5 Fleet Config** | 0 HIGH, 4 MEDIUM (ready_pattern, path validation, TOCTOU, dead code); already 97% | Last — least urgent |

### 3.2 Phase table

| Phase | Group | LOC est | Tier | Coverage Δ | Trigger |
|-------|-------|---------|------|------------|---------|
| **PR-1** | G6 Persistence | ~480 (280+200) | Tier-2 | +2 (maintain 92%+) | After PLAN merge + daemon rebuild |
| **PR-2** | G3 Daemon Core | ~500 (280+220) | Tier-2 | +5.5 | After PR-1 merge |
| **PR-3** | G7 TUI/App | ~1000 (400+600) | Tier-2 | +24 (largest) | After PR-2 merge |
| **PR-4** | G8 Channel | ~550 (300+250) | Tier-2 | +5 | After PR-3 merge |
| **PR-5** | G4 MCP Layer | ~600 (250+350) | Tier-2 | +12 | After PR-4 merge |
| **PR-6** | G2 Agent Lifecycle | ~270 (150+120) | Tier-2 | +8 | After PR-5 merge |
| **PR-7** | G9 CLI / Entry / Bootstrap | ~350 (150+200) | Tier-2 | +12 | After PR-6 merge |
| **PR-8** | G1 State Classifier | ~70 (40+30) | Tier-1 | maintain | After PR-7 merge |
| **PR-9** | G5 Fleet Config | ~160 (80+80) | Tier-1 | maintain | After PR-8 merge |

**Total ~3,980 LOC** (~2,030 fix + ~1,950 test) across **9 PRs**, est **5-7 working days** for dev (kiro) sequential.

PR-1 through PR-7 are Tier-2 dual (`codex` PRIMARY + `lead` cross-vantage). PR-8 / PR-9 Tier-1 single.

### 3.3 Reviewer config (Sprint 44 baseline)

The fresh fleet is single-team (`dev` team only). Tier-2 dual is achieved by:
- **Primary**: `reviewer` (codex) full review → VERIFIED/REJECTED
- **Cross-vantage**: `lead` (claude) reads PR independently, posts attestation in PR comment + inbox `kind=report` per §3.5.4

NO cross-team borrow (no `reviewer2` exists in the new fleet). This pattern proven in Sprint 44 across 5 PRs.

---

## 4. §3.5 / §3.6 / Sprint 44 enforcement application

### 4.1 §3.5.13 — push form (+ Sprint 44 M1 push-time gate)

- Each PR's push must declare via M1 grammar v1.0:
  - `"no other changes"` (validated by `git diff --stat` filtered against PR scope)
  - `"scope follows dispatch spec X"` (cross-ref against `dispatch_tracking.json`)
  - `"only formatting"` (rustfmt-applied head==base check)
- M1 v1.1 Phase C `Claim::FunctionExists` validates any function-name claims in push form

### 4.2 §3.5.10 — production-path-coupled fixture

Every PR's tests use real production paths:
- G6 tests use real `task_events.jsonl` rotation
- G3 tests run real daemon with bounded runtime
- G7 tests use real ratatui buffer (snapshot tests in `insta`)
- G4 inbox tests use real JSONL append + index
- No mock-rewire patterns

### 4.3 §3.5.11 — test-first

For each fix in a PR:
- RED commit: test exhibits the bug
- GREEN commit: fix lands
- Order recorded in PR description per phase

### 4.4 §3.6.9 — atomic cleanup

Each PR closeout:
- Worktree removed
- Branch deleted (remote + local)
- ci-watch unwatch (Sprint 44 M6 supersede should auto-handle)
- Master task marked done

### 4.5 Sprint 44 M3 reviewer SHA-staleness gate

Every reviewer verdict on Sprint 45 PRs is rejected by daemon if `reviewed_head` doesn't match PR HEAD at verdict-emit time. This eliminates the false-claim cascade observed in Sprint 39/40.

### 4.6 Sprint 44 M6 ci-watch supersede

Force-pushed PRs (every retry round) auto-supersede stale ci-pass/ci-fail messages. Lead retro from Sprint 44 confirmed M6's value (r3 misdiagnosis was caused by stale notification).

---

## 5. Cumulative risks

| Risk | Mitigation |
|---|---|
| PR scope explodes mid-implementation (e.g., `render.rs` split balloons G7 to 1500+ LOC) | Hard cap at 1.2× est; if exceeded, split into Phase 2 follow-up. Stop and escalate to operator. |
| G6 H4 (event_log rotation atomicity) fix introduces transient empty-file race during transition | Transition strategy: rename-old-then-create-new, readers retry on empty file once. Documented in Phase 1 commit message. |
| G3 H1 (`set_var` UB) requires startup-time config injection — touches every callsite | Catalog all callsites first; if >20, split into "preparation PR" + "set_var removal PR" (subject to operator decision since this would expand to 10 PRs). |
| Sprint 44 M1 claim verifier rejects legitimate Sprint 45 pushes due to grammar v1.0 strictness | Operator §13 #2 answer: KISS 5 sentences only; if false-positive seen, dev uses `git push --no-verify` + report rejection log → grammar v1.1 amendment in follow-up sprint. |
| Daemon-rebuild gap means Sprint 44 gates are LATENT during early Sprint 45 PRs | Pre-rebuild PRs (PR-1 / PR-2 if rebuild delayed): lead enforces manually, same as proven in Sprint 44 itself. |
| Coverage uplift target 50% for G7 (current 25.8%) is aggressive; may slip to 40% | Acceptable. Each PR must stay positive (no regression); aspiration not blocker. |
| 9 sequential PRs over 5-7 days with single dev (kiro) is fatigue-prone | Operator may pause mid-sprint; partial completion (PR-1..PR-5) still ships net positive value. |

---

## 6. Open candidate items (operator decision points — see §7)

- **G3 H3** `legacy_backfill.rs` 53.5% coverage — is this still needed? Removable?
- **G6 M4** `auto_close_merged_tasks()` "system" sender bypass — proper ACL or accept as documented exception?
- **G7 M1** render.rs (2385 LOC) / layout.rs (2106 LOC) split — in this Sprint 45 PR-3 or defer to dedicated refactor sprint?
- **G8 M1** telegram.rs (4205 LOC) split — same question, in PR-4 or defer?
- **G3 H1** `set_var` removal scope — single PR or split prep + removal?

---

## 7. §13 candidate questions for operator

1. **PR ordering**: Accept G6 → G3 → G7 → G8 → G4 → G2 → G9 → G1 → G5 (severity-first hybrid)? Or prefer architecture-priority order (G4 → G7 → G8 → G2 → G3 → ...)? Or mix?
2. **Sprint 44 daemon rebuild**: Required before Sprint 45 PR-1 dispatch (recommended for live M1-M6 enforcement), OR proceed with manual lead cross-vantage discipline as fallback?
3. **G3 `legacy_backfill.rs` removal**: Delete entirely or treat as fix-coverage-only?
4. **G6 M4 `auto_close_merged_tasks()` "system" bypass**: Tighten ACL (proper system identity with `can_mutate_record` allow-list) or accept + document as audited exception?
5. **G7 render.rs/layout.rs split**: Inline in PR-3 (G7 LOC ~1000) or defer to dedicated refactor sprint (Sprint 46+)?
6. **G8 telegram.rs split**: Inline in PR-4 (G8 LOC ~550) or defer?
7. **G3 H1 `set_var` removal**: Single PR or "preparation PR" (catalog callsites + non-`set_var` infra) + "removal PR"?
8. **Coverage targets per Group**: Accept the +Δ targets in §1.3 table, or set different aspirations?
9. **Tier classification**: Accept PR-1..PR-7 = Tier-2, PR-8/PR-9 = Tier-1, OR raise PR-8/PR-9 to Tier-2 for consistency?
10. **Phase trigger discipline**: Strict serial (PR-N+1 triggers on PR-N merge) or allow operator-judged parallelism for non-conflicting groups (e.g., G1 + G5 could parallel)?

---

## 8. Out of scope (this Sprint)

- 16-pattern reviewer prompt catalog reshape (Sprint 44 already validated M1+M4 should atrophy this naturally)
- Daemon auto-rebuild post-merge ritual (Sprint 44.5 candidate per lead m-733)
- GH Actions Windows runner Integration tests slowness investigation (Sprint 44.5 candidate)
- New skill system / fleet skill registry (separate operator-discussed item, requires its own PLAN)
- fleet.yaml `schema_version` field (operator earlier discussed — separate sprint candidate)
- Architecture-group document refresh (will rebase from this Sprint's actual coverage deltas at closeout, not as a separate doc PR)
- Codex / Kiro / OpenCode / Gemini per-backend skill on-demand mechanism (separate research)

---

## 9. Status & execution sequence

**PLAN authored 2026-05-01 by general (operator-proxy). Awaiting operator §13 GO answers (10 questions §7).**

Process source ledger:
- ea377a code review intake (m-20260430143550920865-672) ✓
- ARCHITECTURE-GROUPS.md baseline coverage data ✓
- Sprint 44 closure (d-20260430181600917488-11) ✓
- Operator instruction "1 Group = 1 PR + coverage uplift" + "Sprint 44 收尾就直接寫 Sprint 45 PLAN" ✓

Execution sequence (assumes operator §13 GO + daemon rebuild):

1. **General self-merges this PLAN PR** (Tier-1 docs-only per §3.5.5 LOW exception; reviewer codex single PRIMARY; CI green).
2. **Operator rebuilds + restarts daemon** to activate Sprint 44 M1-M6 gates (recommended preflight).
3. **Lead dispatches PR-1 (G6 Persistence)** to dev (kiro) — Tier-2 dual (codex PRIMARY + lead cross-vantage).
4. **PR-1 merges → PR-2 (G3 Daemon Core)** dispatched.
5. ... continue strict serial through PR-9.
6. **Sprint 45 closeout report**: aggregate coverage delta + issues-fixed inventory + Sprint 44 gate stress-test results (how many M1 rejects, M3 stale-SHA catches, M6 supersedes happened across 9 PRs).
