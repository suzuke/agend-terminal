# Sprint 47 PLAN — CI Infrastructure Upgrade

**Date**: 2026-05-04
**Author**: lead
**Status**: PLAN (awaiting §8 GO)
**Source-of-truth**: `origin/main` HEAD `b484d5b` (Sprint 46 P2 just merged)
**Synthesis inputs**:
- dev STRUCTURAL — m-20260504025044245104-366
- reviewer PRIOR-ART — m-20260504025239606180-368
- reviewer COST-BENEFIT — m-20260504025337647732-370
- lead MINIMAL-DELTA — this document §5

---

## §0 Context

GitHub Actions Windows runner has hung on 5+ Sprint 45/46 PRs (#392, #395, #397, #398, #402, #407, #408, #409). Pattern: integration test step (`cargo test --tests`) on Windows queues but does not actually run, eating 25-90 min before manual cancel-rerun. Already 5 admin-merges authorized by operator due to pattern. CI is sprint cadence's primary bottleneck.

Operator dispatch m-20260504024910666293-361: bumped Sprint 47 priority. Goal = solve Windows runner hang root cause + eliminate wasted runner-hours during r-fix cycles.

## §1 Goal

Sprint 47 = **floor fix** for Windows runner hang via timeout + concurrency hardening.
Sprint 48 = **performance optimization** via cargo-nextest adoption (separate scope).

**Non-goals (Sprint 47)**:
- nextest migration (Sprint 48)
- sccache adoption (deferred — marginal benefit, complex setup)
- Self-hosted runner (deferred — last resort, not needed if floor fix works)

## §2 Verified state (origin/main b484d5b)

`.github/workflows/ci.yml`:
- 1 job (`check`) with 9 steps
- 3-OS matrix (ubuntu-latest, macos-latest, windows-latest)
- `fail-fast: false`
- **Zero `timeout-minutes`** anywhere — hung jobs run to GitHub's default 6-hour cap
- **Zero `concurrency` config** — every push spawns full 3-OS run, stale runs not auto-cancelled
- 9 steps: checkout, rust-toolchain, rust-cache, Linux tray deps (Linux only), fmt, clippy, clippy tray, build release, unit tests, unit tests tray, integration tests + anti-bypass, CLI smoke (Phase C.5 with bounded 30s+20s manual loops), upload artifact

## §3 Design — A timeout-minutes + C concurrency cancel

### §3.1 timeout-minutes per step + job-level safety net

```yaml
jobs:
  check:
    timeout-minutes: 60  # job-level safety net
    steps:
      - uses: actions/checkout@v5
      - uses: dtolnay/rust-toolchain@stable
        timeout-minutes: 5
      - uses: Swatinem/rust-cache@v2
        timeout-minutes: 5
      - name: Install Linux tray deps
        timeout-minutes: 5
      - name: Format check
        timeout-minutes: 5
      - name: Clippy
        timeout-minutes: 10
      - name: Clippy (tray feature)
        timeout-minutes: 10
      - name: Build
        timeout-minutes: 20
      - name: Unit tests
        timeout-minutes: 20
      - name: Unit tests (tray feature)
        timeout-minutes: 20
      - name: Integration tests + anti-bypass invariants
        timeout-minutes: 30
      - name: CLI smoke (Phase C.5)
        timeout-minutes: 10
      - name: Upload binary
        timeout-minutes: 5
```

**Rationale per reviewer COST-BENEFIT m-370 §2**: GitHub kills the process group on timeout. Tight limits expose true hang sources. Job-level 60 first; if observed CI norm > 45m, bump to 75/90.

### §3.2 Concurrency cancel-in-progress

```yaml
concurrency:
  group: ${{ github.workflow }}-${{ github.event.pull_request.number || github.ref }}
  cancel-in-progress: true
```

**Rationale per reviewer COST-BENEFIT m-370 §3**: PR number key handles force-push iteration cleanly. Each new commit cancels stale runs. `github.ref` fallback for direct push to main. Avoids cross-workflow false-cancel.

**Risk acknowledged**: Force-push during r-fix mid-verify cancels in-flight CI. Acceptable since new push supersedes anyway.

### §3.3 Sprint 48 preview — nextest adoption (NOT in Sprint 47)

Per reviewer COST-BENEFIT m-370 §4 + §7:
- nextest replaces `cargo test --bin` and `cargo test --tests`
- Keep `cargo test --doc` for doctest coverage (nextest doesn't run doctests)
- `nextest.toml` profile config:
  - Test groups for `fleet_test_guard` mutex pattern → `max-threads = 1` for fleet-touching tests
  - Slow timeout per-test
  - Retries on flake
- Pre-Sprint 48 audit: enumerate `serial_test` usage (none expected, codebase uses `fleet_test_guard`), `tests/file_size_invariant.rs` compatibility, doctest count

**Defer to Sprint 48** with Tier-2 dual review. Tracked as follow-up.

## §4 Phase split

### Phase 1 — Sprint 47 (this PLAN) — Tier-1 single

**Scope**: ~14 LOC config change to `.github/workflows/ci.yml`
- Workflow-level: 3 lines `concurrency:` block
- Job-level: 1 line `timeout-minutes: 60`
- Per-step: 11 lines `timeout-minutes: N`

**Tier**: Tier-1 single primary (codex review only) — workflow config is low blast radius; reversible by removing lines.

**Tests**: None directly (CI workflow), but operator validates by:
- Force-push to a test branch and observe stale run cancellation
- Synthetic hang test (sleep 999) to verify timeout kills it

**Done definition**:
- ci.yml has timeout-minutes on every step
- Concurrency cancel verified on test branch
- 1 week monitoring window: no Windows hang exceeding job timeout
- 5 successive PRs since Sprint 47 ship: zero admin-merge for "CI hang" reason (vs 5 in last sprint)

### Phase 2 — Sprint 48 (separate PR, deferred dispatch) — Tier-2 dual

**Scope**: nextest adoption + test audit + doc-test dual track
- `cargo install --locked cargo-nextest` step in CI
- `nextest.toml` config with test-groups
- Replace `cargo test --bin` and `cargo test --tests` with `cargo nextest run`
- Keep `cargo test --doc` separately
- Test audit PR (or first commit): enumerate fleet_test_guard usage, ensure compatibility

**Tier**: Tier-2 dual review — touches test execution semantics + observability.

**Estimated**: ~20 LOC CI + ~15 LOC `nextest.toml` + audit work. ETA ~3-5h IMPL + 1-2 review cycles.

**Sprint 48 closure**: nextest replaces cargo test in CI, parity proven on 2-3 successive PRs, no test count regression.

### Phase 3 — Deferred (no sprint allocated)

- D sccache: marginal benefit + Windows MSVC linker issues (PRIOR-ART m-368)
- E self-hosted runner: cost + security + maintenance overhead unjustified if A+C solves hang

Re-evaluate if A+C insufficient after 1-week monitoring.

## §5 MINIMAL-DELTA verification (lead vantage)

Read current `ci.yml` (149 lines). Confirms:
- 12 steps total, 11 of which can hang in worst case
- Manual `for i in $(seq 1 60); do ... sleep 0.5` loops in CLI smoke = self-bounded (30s daemon-ready, 20s shutdown), NOT the hang source
- Most likely Windows hang surface: `Integration tests + anti-bypass invariants` (`cargo test --tests`) — process-per-test crate compilation on Windows can stall in cargo's job server

**Floor fix**: A timeout + C concurrency = ~14 LOC. No test infra change. Reversible. Does not add new dependencies.

**Smaller alternative considered + rejected**: Only add `timeout-minutes` to integration tests step (1 LOC). Rejected — other steps (Build, Unit tests) have observed slow Windows runs and are also at risk.

**Larger alternative considered + rejected**: Bundle nextest in same PR. Rejected per reviewer COST-BENEFIT — different risk profiles, separate PR.

## §6 Backward compat

- All existing workflow triggers unchanged (push to main + PR to main)
- All existing test commands unchanged
- Existing rust-cache, dtolnay/rust-toolchain unchanged
- Force-push semantics: cancel-in-progress means r-fix cycle won't accumulate runs. Lead/dev should be aware mid-verify CI may be cancelled by new push.

## §7 Risks

**MED**:
- Cancellation during r-fix: dev pushes r1-fix, lead is reviewing CI, dev pushes r2-fix → r1's CI cancelled. **Mitigation**: dev should wait for verify before next push, OR accept that final push's CI is what matters.
- Tight per-step timeouts may false-positive on slow runners. **Mitigation**: 1-week observation window; if false-positive rate > 2%, bump per-step limits up.

**LOW**:
- ci.yml syntax error blocks CI. **Mitigation**: lead validates yaml locally with `actionlint` before push.

## §8 §13 candidate questions for operator

1. **Job-level timeout final value**: 60 min (per reviewer §2) vs 90 min (more headroom)?
2. **Concurrency key**: `${{ github.workflow }}-${{ github.event.pull_request.number || github.ref }}` (recommended) vs simpler `${{ github.ref }}` (fewer special cases)?
3. **Cancel-in-progress on main pushes**: enabled by default (current proposal) or restrict to PR-only paths to avoid cancelling main commits?
4. **Per-step timeout values**: build=20m, unit=20m, integration=30m, smoke=10m, fmt=5m — accept or adjust?
5. **Sprint 47 ship as standalone PR** OR bundle with Sprint 46 P3 (audit trail)?
6. **Tier classification confirm**: Phase 1 Tier-1 single — agree, or want lead cross-vantage despite low LOC?
7. **Sprint 48 nextest dispatch trigger**: ship Sprint 47 first then dispatch + observe 1 week, OR queue Sprint 48 immediately?
8. **Done definition metric**: "5 PRs since ship with zero admin-merge for CI hang" — agree or different threshold?
9. **Monitoring window**: 1 week post-ship before declaring root-cause solved, agree?
10. **D sccache / E self-hosted**: confirm permanently deferred (not just to Sprint 48)?

## §9 Estimates

- Phase 1 IMPL: ~15min code + 30min review + 1-2 CI cycles to verify = ~2h elapsed
- Phase 2 IMPL: ~3-5h code + 2-4h review = ~6-9h elapsed across separate sprint
- Total Sprint 47: ~2h (cheapest sprint of cycle)

## §10 Reuse from prior synthesis

- ci.yml structural inspection: 100% applicable
- dev STRUCTURAL m-366 timeout values: adopted with one adjust (build 20m vs originally 30m)
- reviewer PRIOR-ART m-368 maturity ratings: integrated into Phase 1/2 split rationale
- reviewer COST-BENEFIT m-370 phase split + tier classification: adopted as-is

---

**End of PLAN — awaiting operator §13 answers + §8 GO**
