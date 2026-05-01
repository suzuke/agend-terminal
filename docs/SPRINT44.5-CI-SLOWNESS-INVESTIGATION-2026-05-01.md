# Sprint 44.5-B — CI Slowness Investigation

**Spike date**: 2026-05-01
**Time spent**: ~45 minutes
**Conclusion**: (b) hypothesis with strong evidence — Windows runner test hang, likely GH Actions infra + test process spawn interaction

## Incidents reviewed

| PR | Run ID | OS | Hung Step | Duration | Normal baseline |
|---|---|---|---|---|---|
| #384 Phase A merge (aefe1c1) | 25175736006 | Windows | Step 10: Unit tests | 16:06 → cancelled 22:05 (~6h) | ~96s |
| #385 Phase B merge (aefe1c1) | 25181849981 | Windows | Step 12: Integration tests | 18:27 → cancelled 00:20 (~6h) | ~109s |
| #385 Phase B (14c0404) | 25176299208 | Windows | cancelled (all 3 OS) | 16:12 → 16:29 | manual cancel |
| #385 Phase B (b55ae21) | 25177489581 | Windows | cancelled | 16:38 → 16:51 | manual cancel |

## Common pattern findings

1. **Windows-only**: All 4 hung runs are Windows-latest. Ubuntu and macOS complete normally (5-8 min) on the same commits.
2. **Test step specifically**: The hang occurs during `cargo test` execution (Unit tests or Integration tests step), not during build/clippy/fmt.
3. **Indefinite hang**: The process doesn't timeout — it hangs until the 6-hour GH Actions job timeout or manual cancellation.
4. **Non-deterministic**: Same commit succeeds on retry. Run 25171242543 (same PR #384 b01f1d3) completed Windows in 11 min. Run 25177997209 (PR #385 ea9b67c) completed Windows normally.
5. **No code correlation**: Hangs occur on different commits with different code changes (Phase A claim verifier vs Phase B SHA gate). The common factor is the runner, not the code.

## Hypotheses tested

### H1: cargo build cache miss → REJECTED
Build step (step 9) completes normally in ~4 min on all hung runs. The hang is post-build, during test execution. rust-cache action works correctly.

### H2: Runner queue starvation → REJECTED
Jobs start promptly (< 1 min queue time). The hang is mid-execution, not in queue.

### H3: Integration test inherent slow (claim_verifier shell-out) → REJECTED
The hang occurs on Unit tests too (run 25175736006 step 10), not just integration tests. Unit tests don't shell out to git/rustfmt.

### H4: Windows test process spawn deadlock → STRONG CANDIDATE
`cargo test` on Windows spawns test binaries as child processes. The test suite includes PTY-related tests (`pty_smoke`, `vterm`, `alacritty_terminal` integration) that interact with Windows ConPTY APIs. A known class of Windows CI issues involves:
- ConPTY handle inheritance causing child process hangs
- `WaitForSingleObject` on process handles that never signal
- Antivirus (Windows Defender) scanning test binaries mid-execution

Evidence: the hang is always in the test execution phase, never in build. The same tests pass on retry (non-deterministic = timing-dependent race).

### H5: GH Actions Windows runner infra instability → CONTRIBUTING FACTOR
GitHub Actions Windows runners are known to have higher variance than Linux/macOS. The combination of H4 (process spawn sensitivity) + H5 (runner instability) explains the pattern.

## Root cause / recommendation

**Category (b)**: Strong hypothesis but no conclusive single root cause. The pattern is consistent with Windows ConPTY/process-spawn timing sensitivity on GH Actions runners.

### Recommended mitigations (follow-up tasks if approved)

1. **Add `timeout-minutes: 15` to Windows CI job** — prevents 6-hour hangs, fails fast for retry
2. **Add CI workflow `concurrency` group** — auto-cancel superseded runs on same branch (reduces wasted runner time)
3. **Consider `--test-threads=1` on Windows CI** — reduces process spawn concurrency, may avoid the race
4. **Long-term**: investigate specific test that hangs (add per-test timeout via `#[timeout]` or `cargo nextest` with per-test limits)

### Standard mitigation (already in use)

Cancel-rerun is the correct immediate response. The hang is non-deterministic and resolves on retry.

## Follow-up tasks

- [ ] Add `timeout-minutes: 15` to CI Windows job (if operator approves)
- [ ] Add `concurrency` group to ci.yml (if operator approves)
- [ ] Investigate `cargo nextest` for per-test timeout on Windows
