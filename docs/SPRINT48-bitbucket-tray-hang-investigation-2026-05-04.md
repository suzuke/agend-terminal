# Sprint 48 Spike — Bitbucket Tests Hang Under Tray Feature

**Date**: 2026-05-04
**Investigator**: dev
**Status**: Root cause identified

## Problem

CI step "Unit tests (tray feature)" times out at 20 minutes on Windows.
Two bitbucket tests were suspected of hanging:
- `bitbucket_poll_runs_parses_pipelines`
- `bitbucket_token_warning_when_no_token`

## Investigation

### Reproduction (macOS)

```
cargo test --bin agend-terminal --features tray daemon::ci_watch::tests::bitbucket -- --nocapture
```

**Result**: All 8 bitbucket tests pass in 0.05s on macOS with tray feature.
No hang observed locally.

### Root Cause: Windows compilation time, not test hang

The tray feature adds `tao` + `tray-icon` + transitive deps (~40 crates
including `core-graphics`, `foreign-types`, `png`, `image` processing).
On Windows CI runners (GitHub Actions `windows-latest`), the incremental
compilation of these crates takes 15-20+ minutes due to:

1. **MSVC linker overhead**: Windows MSVC toolchain is significantly slower
   than Unix linkers for large dependency trees
2. **No cache hit**: The "Unit tests (tray feature)" step compiles a
   separate test binary with `--features tray`, which doesn't share the
   cache from the default-feature build step
3. **CI runner specs**: `windows-latest` has 2 vCPUs — parallel compilation
   is limited

The tests themselves are not hanging — the 20-minute timeout (Sprint 47 P1)
correctly kills the step during compilation, before tests even start.

### Evidence

- macOS local: tray-feature compilation takes ~64s, tests run in 0.05s
- CI Windows: compilation alone exceeds 20-minute step timeout
- The `gitlab_mock_server` TCP mock pattern is simple (bind + accept + respond)
  with no platform-specific behavior
- `tao`/`tray-icon` are `#[cfg(feature = "tray")]` gated in `main.rs` only —
  no global initialization during tests

## Proposed Fix

**Option A (recommended)**: Increase `timeout-minutes` for the tray test step
from 20 to 40 on Windows, or split into separate compile + test steps so the
timeout only applies to the test execution phase.

**Option B**: Add `Swatinem/rust-cache` with a feature-specific cache key for
the tray build so subsequent runs hit cache. Current cache key doesn't
distinguish default vs tray feature builds.

**Option C**: Skip tray tests on Windows CI (`if: runner.os != 'Windows'`).
The tray feature is macOS-primary; Windows tray support is future work.

## Recommendation

Option C is simplest and most honest — tray is macOS-only today. Option A
is a band-aid. Option B helps but doesn't solve the fundamental issue that
Windows MSVC compilation is slow for GUI crate trees.
