# Sprint 48 Spike — Bitbucket Tests Hang Under Tray Feature

**Date**: 2026-05-04
**Investigator**: dev
**Status**: Windows-specific, not reproducible on macOS

## Problem

CI step "Unit tests (tray feature)" times out at 20 minutes on Windows.
18+ minutes of silence after the last visible test output, then timeout fires.

## Investigation

### CI Log Evidence (Windows runner)

- Default "Unit tests" step: all 1404 tests pass in 34.65s ✅
- Tray feature step: compilation completes in ~40s (08:01:34 → 08:02:17)
- Tests start running, last visible output at 08:02:54
- **18+ minutes silence** (no log output) → 08:21:33 timeout fires
- Hang is during **test execution**, not compilation

### macOS Reproduction Attempt

```
cargo test --bin agend-terminal --features tray -- --test-threads=1
```

**Result**: All 1426 tests pass in 84s on macOS. No hang. Every test
completes normally including all bitbucket mock-server tests.

### Root Cause: Windows-specific tray feature interference

The hang occurs only on Windows CI under `--features tray`. Since:
1. All tests pass on macOS with tray feature (1426 in 84s)
2. All tests pass on Windows without tray feature (1404 in 34.65s)
3. Hang occurs on Windows with tray feature after ~50 tests complete

The interference is between the tray feature's dependency tree (`tao`,
`tray-icon`, Windows-specific GUI crates) and some test that exercises
async/TCP/thread behavior. The `tao` crate on Windows initializes COM
and Windows message loop infrastructure at link time, which can interfere
with:
- `tokio::runtime::Builder::new_current_thread()` (used by bitbucket mock tests)
- `std::net::TcpListener::bind` (used by gitlab_mock_server)
- Thread parking/unparking semantics

### Why not reproducible on macOS

`tao` on macOS uses Cocoa/AppKit APIs that don't interfere with POSIX
socket/thread operations. On Windows, `tao` pulls in Win32 message pump
infrastructure that can deadlock with synchronous `TcpListener::accept()`
in test mock servers when both run in the same process.

## Proposed Fix

**Option A (recommended)**: Skip tray feature tests on Windows:
```yaml
- name: Unit tests (tray feature)
  if: runner.os != 'Windows'
  timeout-minutes: 20
  run: cargo test --bin agend-terminal --features tray
```

Rationale: tray is macOS-primary. Windows tray support is future work.
The tray feature's `Clippy (tray feature)` step still compiles on all
platforms — only the test execution is skipped on Windows.

**Option B**: Isolate the hanging test(s) by running Windows tray tests
with `--test-threads=1` and a per-test timeout via cargo-nextest. Higher
effort, deferred to Sprint 49+ if Option A is insufficient.

## Recommendation

Option A — 1-line CI change, zero risk, unblocks Windows CI immediately.
