# PLAN: Sprint 42 — TUI test harness (review-and-extend on claude-a1f200 v2)

**Date:** 2026-04-30
**Status:** plan-review-extend; awaiting operator GO before any Phase 1 IMPL dispatch
**Branch:** `docs/sprint42-tui-test-harness-review`
**Origin:** general m-20260430054947092086-323 dispatch on operator-approved claude-a1f200 audit-v2 PLAN at `docs/PLAN-tui-test-harness-2026-04-30.md` (branch `plan/tui-test-harness` commit `0def4d0`)
**Process:** 4-perspective challenge round (Sprint 32~40 model, REVIEW-AND-EXTEND mode)
**Scope decision:** project decision `d-20260430055142729903-6`

---

## 0. KISS gate (§0) + closure of Sprint 39 promise

- **What real problem does this solve?** Sprint 39 closed (3/3 PRs). 17 PRs shipped today since onboarding. Testing infrastructure remains in shell scripts (3 scripts in `scripts/`); 5351 LOC of unit tests cover Layer 1 but Layer 2 CLI + Layer 3 TUI scenarios are bash-only. claude-a1f200 audit-v2 proposes 3-layer test pyramid bringing E2E coverage into Rust.
- **Would deletion break anyone?** Not deleting; this is review-and-extend on existing operator-approved PLAN. Deletion = continued bash-script test fragility + cross-platform CI gaps.
- **Operator philosophy alignment**: 一勞永逸 favors moving E2E into Rust testbed (deterministic, cross-platform, in-CI) vs bash scripts (flaky, Unix-only, manual).

---

## 1. Verified current state (lead minimal-delta)

### claude-a1f200 PLAN doc inventory at `0def4d0`
- 443 LOC plan doc at `docs/PLAN-tui-test-harness-2026-04-30.md`
- 3-layer test pyramid:
  - Layer 1: Rust unit (existing 5351 LOC, untouched)
  - Layer 2: CLI subcommand e2e via `assert_cmd` (5 commands MVP)
  - Layer 3: TUI scenario harness via `AgendHarness` + `TuiClient` (zero new prod deps; reuses `portable-pty` 0.9 + `alacritty_terminal` 0.26 already in tree)
- 5-phase rollout: Phase 1 immediate; Phase 2 awaits Sprint 41 closure (TUI/App scope conflict); Phases 3-5 sequenced
- Total ~3.5-4.5 working days

### Existing prod infrastructure relevant to harness
- `src/vterm.rs` — VTerm wrapper around `alacritty_terminal::Term` with `extract_text` / `read_scrollback` / `safe_cell` / `process` (synchronous)
- `src/auth_cookie.rs` — cross-platform cookie file (already handles `#[cfg(unix)]` 0600 + `#[cfg(windows)]` ACL)
- `src/framing.rs` — wire protocol (NDJSON over TCP, captured Sprint 32 PR-A teloxide cascade)
- `tests/` — 18 integration test files using existing patterns

---

## 2. Three perspectives (challenge round summary)

### 2.1 lead — minimal-delta synthesis
claude-a1f200's design choice (real binary + wire protocol + in-process vterm via alacritty_terminal) is rigorous and aligns with production `VTerm` 1:1 — eliminates parser-divergence risk that would plague a separate test-only parser. Sprint 32 production-path-coupled fixture rule applies cleanly. Two cross-perspective overlap items strongly indicate Phase 2 implementation hardening (process-group cleanup + early-exit detection); these are the actual-blocking items operator should resolve before IMPL dispatch.

### 2.2 dev (kiro) STRUCTURAL (m-20260430055459689131-334)

**S1 API feasibility against portable-pty 0.9 + alacritty_terminal 0.26**: COMPILES with caveats:
- `Term<()>` violates EventListener bound → use `Term<NoopListener>` (~5 LOC; existing `PtyWriteListener` precedent in `src/vterm.rs`)
- `Line`/`Column` newtype indexing in `cell_at(usize, usize)` API
- Send-not-Sync acceptable for single-threaded tests
- Drop semantics sound (Child::kill + Child::wait)

**S2 5-CLIs assertion gaps**:
- `bugreport` error path with nonexistent AGEND_HOME untested
- `completions` only bash; zsh/fish smoke missing
- `--help` snapshot (insta) recommendation

**S3 ConPTY**: Production-ready in portable-pty 0.9; CI windows-2022 has ConPTY; resize async handled by drain_for poll pattern; cookie file ACL already cross-platform; **no blocking**.

**S4 5 vte gotchas mechanically addressable**: + 1 missed (Unicode wide chars CJK/emoji via `Flags::WIDE_CHAR_SPACER`); ~50 LOC Phase 3.

**S5 LOC est accurate**: ~910 total LOC, 3.5-4.5 days; Phase 1 dispatchable immediately; Phase 2 Sprint 41 dependency real.

### 2.3 reviewer-kiro (kiro backend) PRIOR-ART / CROSS-VANTAGE (m-20260430060010397970-337)

**P1 Sprint 32 4-PR pattern applicability**:
- Sprint 32 PRs were feature slices (independently shippable)
- Sprint 42 phases are **layered dependencies** (Phase 2 depends on Phase 1's dev-deps; Phase 3 depends on Phase 2's harness)
- Strict serial mandatory; can't parallelize Phase 2+3 across devs (merge conflicts in `tests/common/`)
- **Tier-2 dual-reviewer recommended for Phase 2** (testing infra novelty; design mistake compounds across Phase 3-5); Tier-1 others

**P2 Rust testing infra prior-art**:
- `assert_cmd` industry standard for CLI binary testing — good fit
- `wiremock` correctly deferred to §9 (Sprint 32+ patterns established own TCP mock)
- `insta` correctly deferred (grid output stabilization needed first)
- **`portable-pty` + `alacritty_terminal::Term` is strongest design choice** — `TuiClient` essentially mirrors production `VTerm` API + TCP socket; zero parser divergence
- vs alternatives (`vt100` simpler but parser divergence; `vte` low-level reimpl; `rexpect`/`expectrl` correctly rejected — agend-terminal has wire protocol)

**P3 §5.2 tmux removal adversarial challenge**:
- Multi-pane / splits: `alacritty Term` is single-emulator-instance; `TuiClient` per-agent CANNOT verify layout composition (split positions, tab grouping). Existing `repro-team-tab-bug.sh` used `tmux capture-pane` for composed TUI output.
- **NIT (not BLOCKING)** — PLAN's §4.3 scope boundary excludes layout, but §5.2 "zero capability loss" claim overstated; should be softened
- Resize / keybinding / heavy output: NO gaps

**P4 claude-a1f200 blind spots**:
- §4.3 — `drain_for(Duration::ZERO)` ambiguous semantics; need explicit `set_nonblocking(true)` + read-until-`WouldBlock` pattern in Phase 2
- §6.4 — over-flags PtyWriteListener (test-side Term doesn't need); cookie ACL is non-issue (auth_cookie.rs handles)
- §7 — 2 missed gotchas: **Wide chars (CJK)** (cross-confirmed with dev S4) + **safe_cell bounds checking**

**P5 5 adversarial scenarios**:
- Scenario 1 Windows CI flakiness — NIT (platform timeout multiplier ~5 LOC)
- **Scenario 2 BLOCKING — AgendHarness::drop() must kill process group (not just daemon PID)**; orphans accumulate on CI; ~10 LOC fix in spawn() (use `setsid` or pgid kill in drop)
- Scenario 3 portable-pty drift — LOW risk (already prod dep)
- Scenario 4 alacritty 0.x churn — MEDIUM-LOW (mitigation: pinned `0.26`; vterm.rs migration guide)
- **Scenario 5 BLOCKING — spawn() must detect daemon early exit (not just timeout)**; child crash hangs N seconds with unhelpful error; ~8 LOC `child.try_wait()` in startup loop

---

## 3. BLOCKING items for plan revision (per reviewer P5)

These are the only two items that genuinely require PLAN updates before Phase 2 IMPL:

### 3.1 BLOCKING — AgendHarness::drop() process-group kill (Scenario 2)
**Risk**: orphan child agents (spawned by daemon via portable-pty) outlive daemon SIGTERM; CI degrades over time (port conflicts, file locks, OOM).

**Required PLAN amendment**:
- §4.3 `AgendHarness::drop()` spec: kill process group (`kill -SIGTERM -pgid` then SIGKILL after 3s)
- Implementation: `spawn()` uses `setsid()` (Unix) / job object (Windows) so entire process tree can be killed
- Cost: ~10 LOC in spawn + drop

**Phase**: MUST land in Phase 2 (not deferred to Phase 3+) — this is foundational.

### 3.2 BLOCKING — spawn() early-exit detection (Scenario 5)
**Risk**: daemon crash during startup (bad fleet.yaml, port conflict) hangs harness `spawn()` until timeout; unhelpful "DaemonStartTimeout" error masks real cause.

**Required PLAN amendment**:
- §4.3 `AgendHarness::spawn()` startup loop: poll BOTH `api.port` file existence AND `daemon.try_wait()`
- If child exited: capture stderr, fail with informative error (not opaque timeout)
- Cost: ~8 LOC in spawn

**Phase**: MUST land in Phase 2 alongside item 3.1.

---

## 4. NIT / RECOMMEND items for §13 operator awareness

### 4.1 §5.2 claim softening (per reviewer P3)
"Zero capability loss vs tmux" overstated. Layout composition (split positions, tab grouping) NOT testable per-agent via TuiClient. PLAN's §4.3 scope boundary correctly excludes layout, but §5.2 claim should reflect this.

**Recommended amendment**: Add to §9 "不解決的範圍": "Layout composition verification (split positions, tab grouping) — requires daemon-side layout state API or full-screen compositor test, out of scope for per-agent TuiClient."

### 4.2 §4.3 drain_for(Duration::ZERO) semantics (per reviewer P4)
Phase 2 IMPL spec needs explicit clarification: `set_nonblocking(true)` + read-until-`WouldBlock` + restore blocking, OR `set_read_timeout(Some(1ms))`. Currently ambiguous.

### 4.3 §7 wide chars + safe_cell bounds (per dev S4 + reviewer P4 cross-confirmed)
Add to Phase 3 vte gotchas list:
- 6th: Unicode wide chars (CJK/emoji) via `Flags::WIDE_CHAR_SPACER` skip
- 7th: `cell_at()` bounds checking via `safe_cell()` pattern (resize race defense)

Cost: ~10 LOC additional in Phase 3 (~60 LOC total).

### 4.4 §6.1 5-CLIs gaps (per dev S2)
Phase 1 expansion: bugreport-with-no-AGEND_HOME error path; completions zsh/fish smoke; --help snapshot via insta.

### 4.5 Tier classification per phase (per reviewer P1)
- **Phase 2 Tier-2 dual-reviewer** (testing infra novelty; foundation for Phase 3-5)
- Phase 1 / 3 / 4 / 5 Tier-1 single-reviewer

---

## 5. Phase sequencing (refined per dev S5 + reviewer P1)

| Phase | Scope | LOC | Tier | Sprint 41 dep | Trigger |
|---|---|---|---|---|---|
| **1** | 5 CLI smoke tests (`assert_cmd`) + dev-deps add | ~120 | Tier-1 | None | Dispatchable immediately |
| **2** | AgendHarness + TuiClient MVP + 3.1+3.2 BLOCKING fixes | ~310 (~300 base + ~20 BLOCKING) | **Tier-2 dual** | Yes (Group 2 TUI/App scope) | Awaits Sprint 41 closure |
| **3** | 7 vte gotchas (5 + wide chars + safe_cell) | ~60 | Tier-1 | After Phase 2 | After Phase 2 merge |
| **4** | Migrate 3 bash scripts to harness | ~400 | Tier-1 | After Phase 3 | After Phase 3 merge |
| **5** | CI matrix expansion + Windows timeout multiplier | ~30-50 | Tier-1 | After Phase 4 | After Phase 4 merge |

**Total ~920-940 LOC, 5 PRs, 4-4.5 working days.**

Strict serial per reviewer P1 — phases are layered dependencies, not feature slices.

---

## 6. §3.5.10 / §3.5.11 application per phase

### Phase 1
- §3.5.10 wire-format: assert_cmd captures real binary stdout/stderr; spec-quoted from Cargo.toml `[package]` for --version; clap auto-generated for --help
- §3.5.11 test-first: trivial (RED commit can fail compile if test refers to absent CLI subcommand)

### Phase 2
- §3.5.10 production-path-coupled: AgendHarness spawns real binary; TuiClient uses real TCP socket + alacritty_terminal::Term (mirroring production VTerm API); zero divergence
- §3.5.11 test-first: framework MVP test exercises spawn → connect → drain → assert; r3 empirical-revert exemption likely (impl-provided harness types)

### Phase 3-5
Standard fixture/test-first per phase scope.

---

## 7. Cumulative risks (Sprint 42-specific)

| Risk | Mitigation |
|---|---|
| Phase 2 process-group leak (BLOCKING 3.1) | setsid/pgid kill in spawn+drop (~10 LOC) |
| Phase 2 spawn early-exit hang (BLOCKING 3.2) | child.try_wait() poll in startup loop (~8 LOC) |
| Phase 2 Tier-2 cross-vantage reviewer availability | reviewer-kiro fill-in (codex returns ~3:14 PM); operator m-2549/m-2554 cross-team auth pattern available |
| Sprint 41 closure delays Phase 2 dispatch | Sprint 41 dev2 PLAN-first in flight; phase 2 holds until merge |
| Windows CI flakiness | platform timeout multiplier in TuiClient::wait_for (~5 LOC) |

---

## 8. Out of scope (preserved from claude-a1f200 §9)

- Layout composition verification (multi-pane, tab grouping) — added per reviewer P3
- Stress / heavy-output scenarios (deferred §11 #4)
- Snapshot testing via `insta` (deferred until Phase 3 grid output stabilization)
- New prod deps (alacritty_terminal + portable-pty already in tree)
- Channel adapter e2e (separate sprint when wiremock pattern adopted)

---

## 9. Open questions for operator (§13)

1. **Phase 1 immediate dispatch** OR Sprint 41 done first? (Phase 1 has zero Sprint 41 file overlap; can run in parallel with Sprint 41 PLAN-first / IMPL waves on dev2)
2. **5 CLI selection** — `--version/--help/list/status/bugreport/completions` adequate, or include `inbox` / `task` / `decision` MCP-flavored CLIs (none currently exist as standalone subcommands)?
3. **AgendHarness implementation** — implement `Drop` directly OR reuse existing helpers (e.g., from `tests/integration_*` bootstrap)?
4. **Phase 2 Sprint 41 dependency** — confirm acceptable wait OR escalate (Sprint 41 ETA?)
5. **Sprint number** — 42 confirmed (lead2 at 41)
6. **Tier classification** — Phase 2 Tier-2 dual (per reviewer P1 recommendation) OR keep all Tier-1?
7. **PLAN-doc updates** — amend `docs/PLAN-tui-test-harness-2026-04-30.md` directly (operator-approved v2 churn) OR keep this review-and-extend doc as the v3 deliverable?
8. **§13 BLOCKING items 3.1+3.2** — accept as Phase 2 MUST-HAVE OR escalate to Phase 1 (if process-group concerns block initial dispatch)?
9. **IMPL dispatch ownership** — dev (kiro) all phases, OR rotate dev2 in for Phase 4 (bash-script migration)?

---

## 11. Status & operator decisions (recorded 2026-04-30 per general m-20260430060731311284-342)

**Status**: PLAN approved 2026-04-30 by operator-proxy general; **IMPL DEFERRED pending Sprint 41 (Group 2 TUI/App PLAN+IMPL) + Sprint 43 (supervisor member-state-change notify) closure**. Re-trigger on Sprint 43 closeout.

**Sprint order**: 41 → 43 → 42 (TUI conflict avoidance; operator decision via general m-?). Sprint 42 phases hold.

### §13 final answers (all 9 RESOLVED per general m-20260430060731311284-342)

| §13 # | Decision | Rationale |
|---|---|---|
| 1 (Phase 1 immediate dispatch) | **DEFERRED** until Sprint 43 close | Sprint order 41 → 43 → 42 |
| 2 (Phase 2 wait Sprint 41) | YES, natural hold | claude-a1f200 §6.3 spec'd |
| 3 (Phase 3-5 sequential) | YES | per claude-a1f200 |
| 4 (vte gotchas +2: wide chars + safe_cell) | YES | reviewer-kiro P4 cross-confirmed dev S4 |
| 5 (§6.1 5-CLIs gaps: bugreport error / completions zsh,fish / --help snapshot) | YES | dev S2 negative-case coverage |
| 6 (Phase 2 Tier-2 dual reviewer) | YES | testing infra novelty per reviewer P1 |
| 7 (PLAN-doc update path) | claude-a1f200 v3 amends own v2 (single source of truth) | this review-extend doc serves as input to v3 authoring (separate work) |
| 8 (2 BLOCKING items 3.1 + 3.2 accepted) | YES | adversarial scenarios point to real CI degradation risk |
| 9 (Sprint number 42) | confirmed | |

### Re-trigger trigger
On Sprint 43 closeout:
- claude-a1f200 (or other authoring agent) authors PLAN doc v3 amending v2 with: §13 #5 (5-CLIs gaps closing), §13 #4 (vte gotchas +2), §13 #6 (Tier-2 Phase 2), §13 #8 (2 BLOCKING items folded into Phase 2 spec), §4.1 §5.2 layout-composition gap softening
- Phase 1 immediate dispatch (no Sprint 41 file overlap; can run parallel)
- Phase 2 holds for Sprint 41 closure
- Phase 3-5 strict serial

### CiHttpClient extraction follow-up
Sprint 39 retrospective task `t-20260430024226176283-9` — bundle with Sprint 43 IMPL wave OR independent nit-PR, decide at Sprint 43 IMPL dispatch time.

---

## 10. Cross-references

- general m-20260430054947092086-323 (operator scope)
- general m-20260430055106253561-326 (cross-team auth Option 2; superseded by m-20260430055323598776-331)
- general m-20260430055323598776-331 (operator override Option 2.5: spawn reviewer-kiro into dev team)
- decision `d-20260430055142729903-6` (Sprint 42 plan-first scope)
- master task `t-20260430055147257001-16`
- dev S1-S5 perspective: m-20260430055459689131-334
- reviewer-kiro P1-P5 perspective: m-20260430060010397970-337
- claude-a1f200 PLAN-v2 source: `docs/PLAN-tui-test-harness-2026-04-30.md` on branch `plan/tui-test-harness` `0def4d0`
- Sprint 32 multi-PR + production-path-coupled fixture precedent: `docs/PLAN-discord-channel-2026-04.md`
- Sprint 39 wave (PR #350/#358/#359 closure)
- `docs/FLEET-DEV-PROTOCOL-v1.md` §0 / §3.5.10 / §3.5.11 / §3.5.13 / §10.1
- `src/vterm.rs` (production VTerm reference for TuiClient mirroring)
- `src/auth_cookie.rs` (cross-platform cookie ACL precedent)
