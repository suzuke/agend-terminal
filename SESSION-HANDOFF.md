# SESSION-HANDOFF — fixup-dev

## Active task
**freeze fix IMPL** (correlation_id `t-20260620162900769323-34628-0`, dispatched by fixup-lead).
Candidate 1: render snapshot reads lock-free published AgentState. Branch `fix/freeze-render-lockfree-snapshot`.

## State: IMPL DONE → PR #2380 open, awaiting DUAL review
- Implemented + pushed @ `80a5c26c`. **PR #2380** https://github.com/suzuke/agend-terminal/pull/2380.
- Reported to lead (kind=report, correlation_id), DUAL review requested. Task OPEN until VERIFIED+merge.

## What shipped (candidate 1, as designed)
1. `AgentState`: `#[repr(u8)]` + `from_u8()` (18 variants).
2. `StateTracker`: `published: Arc<AtomicU8>` written in `record_set` (sole `current` funnel, state/mod.rs),
   seeded in `new()` from initial_state; `published_handle()` clones.
3. `AgentHandle`: `published_state` cloned via `published_state_of(&core)` helper (agent/mod.rs) at all
   10 construction sites (spawn + test builders).
4. `build_agent_state_snapshot` (core_render.rs) reads `from_u8(published_state.load(Relaxed))` — zero core.lock.

## Key correctness notes (for reviewers / if findings come back)
- **reclaim.rs:1045 `.current=UsageLimit` is `#[test]` code, NOT a prod bypass** (Explore mislabeled).
  record_set is already the SOLE prod `.current` writer → no funnel-close needed.
- 2 test direct-writes (poll_reminder, supervisor mk-helpers) explicitly `.store()` published to stay consistent.
- Relaxed ordering OK: standalone u8, no companion memory; render tolerates ≤1-frame staleness (snapshots per draw).
- Producers (pty_read_loop feed) UNTOUCHED → perf-R1 "feed lock-shrink UNSAFE" inapplicable.
- lock-order: spawn locks core (L2) inside registry (L1) = canonical ascending, no self-IPC → no tier violation.

## Verification done
- `cargo fmt --check` + `clippy --features tray` clean.
- 630 targeted tests (state/render/agent/daemon handle-builders) + 3 new deterministic tests GREEN.
- CoreMutex sole-wrapper #1535 invariant GREEN.
- New tests: `snapshot_reads_published_state_without_core_lock` (200ms-held-lock → <50ms snapshot),
  `agentstate_u8_roundtrip`, `published_mirror_tracks_current_through_record_set`.

## ⚠ Pre-existing unrelated failure (flagged to lead, NOT this PR)
Full nextest: 1 red `teardown_completeness_regression::teardown_leaves_zero_residual_after_full_exercise_1907`
(residual `fleet_events.jsonl (content)`). **Fails identically on clean origin/main** (verified via git stash).
Touches no teardown/fleet-events code here. Likely separate issue (#1907 teardown-completeness class).

## Next
Await DUAL review verdicts → address findings on this branch (additive commits, no force-push) → merge →
operator rebuild+restart to go live.
