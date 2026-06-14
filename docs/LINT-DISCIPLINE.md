[繁體中文](LINT-DISCIPLINE.zh-TW.md)

# Lint Discipline (cross-platform pre-push checklist)

**Purpose**: catch the recurring cross-platform lint issues that burned
fix-forward cycles in Sprint 56–57 *before* push, so CI matrix runs
verify rather than discover.

**Companion**: `scripts/clippy-all-platforms.sh`. This document captures
the *patterns* the script is designed to surface. Running the script
without internalizing the patterns leaves you blind to the failure
modes the script can't detect (because they're at link/runtime, not
lint).

---

## Quick checklist (pre-push)

Run before every push that touches platform-specific code:

```bash
scripts/clippy-all-platforms.sh           # full matrix
scripts/clippy-all-platforms.sh --quick   # host only (fast iteration)
```

If the script reports a `failed` target, **fix locally first** rather
than relying on CI to discover. CI cycle = ~10–15 min wall time per
fix-forward; local cycle = ~30 sec per `cargo clippy` invocation.

---

## Patterns to watch for

### 1. cfg-gated `dead_code`

**Symptom**: `error: function/struct/method is never used` appearing
on a different platform than where the code was written. Triggered
because the function is only called inside a `#[cfg(target_os = "...")]`
or `#[cfg(feature = "...")]` block; on platforms that don't enable that
cfg, the symbol becomes unreferenced and `-D warnings` blocks.

**Fix shape**: scope `#[allow(dead_code)]` to the symbol with a comment
explaining the platform conditionality. Do *not* slap `#[allow(dead_code)]`
on the parent module — that hides real dead code.

```rust
// Used only on Windows (see cfg block at line 142). Other platforms
// see this as dead, hence the explicit allow.
#[allow(dead_code)]
fn windows_only_helper(...) -> ... { ... }
```

**Sprint 57 incident**: Wave 3 PR-2 r1 + r2 (commit 438878b) — service
template test helpers gated on `cfg(unix)` failed clippy on the Windows
runner. Two fix-forward cycles before the proper `#[allow(dead_code)]`
scope landed.

---

### 2. fire-and-forget spawn rationale

**Symptom**: clippy doesn't directly enforce this — but the project's
**Phase 5b invariant test** (`tests/spawn_rationale_invariant.rs`) does.
Every `tokio::spawn` and `thread::spawn` site MUST carry either:

- a `// fire-and-forget: <reason>` comment on the call site, OR
- explicit `JoinHandle` storage for graceful join.

**Fix shape**: add the rationale comment. The reason should explain
*why* nothing waits for the task to complete (e.g. "logging is best-
effort", "background cache warmer, daemon shutdown waits via global
cancellation token").

```rust
tokio::spawn(async move {
    // fire-and-forget: telemetry is best-effort; daemon shutdown
    // happens via the global cancellation token observed inside the
    // future, no join needed.
    emit_telemetry(...).await;
});
```

**Reference**: `FLEET-DEV-PROTOCOL.md` §10.5. Tests are exempt
(test helpers may spawn ad-hoc); trait methods inherit the caller's
rationale.

---

### 3. Format-aware shell-template escaping

**Symptom**: a service template (e.g. systemd unit file, launchd plist,
PowerShell script) that escapes characters using one shell's rules
silently corrupts on another. Tests pass on the platform that wrote
the template; fail in CI on the OS that *consumes* the template.

**Fix shape**: every template renderer must be *format-aware* — pick
the escape rules from the target shell, not the host shell. POSIX sh
escaping (`'$value'`) ≠ PowerShell escaping (`"$value"` with backtick
escape) ≠ JSON escaping (backslash escape).

**Sprint 57 incident**: Wave 3 PR-3 r2 (commit 71cb3b6) — cross-platform
service template installer's escape table assumed POSIX shell on all
platforms; PowerShell consumers got broken paths. One fix-forward
cycle.

---

### 4. Windows `.exe` extension handling

**Symptom**: helper-binary path resolution (`agend-git`, `agend-mcp-bridge`)
or test-harness binary lookup omits the `.exe` suffix on Windows. The
binary exists; the lookup fails because `Path::exists()` returns false.

**Fix shape**: use `std::env::consts::EXE_SUFFIX` to compose the
expected filename. Never hardcode `.exe` (breaks on non-Windows
test paths) and never omit it (breaks on Windows).

```rust
let bin_name = format!("agend-git{}", std::env::consts::EXE_SUFFIX);
```

**Sprint 58 incident**: caught proactively in Wave 2 PR-1 (#11 helper-
staleness warn, commit 9a2fc32) — `classify_helper_staleness` uses
`EXE_SUFFIX` so the doctor diagnostic is correct on Windows.

---

### 5. mtime cross-platform branches

**Symptom**: `std::fs::metadata().modified()` returns `Err` on some
file systems (older NFS, older Windows ReFS), or returns a timestamp
with platform-specific resolution (FAT32 = 2-second granularity).

**Fix shape**: degrade gracefully on `Err`. Surface a fourth-arm
classifier (e.g. `UndeterminableDaemonPath` in
`classify_helper_staleness`) rather than panic or silently return a
default that misleads the operator.

```rust
match std::fs::metadata(&path).and_then(|m| m.modified()) {
    Ok(mtime) => HelperStaleness::classify_from_mtime(mtime, ...),
    Err(_) => HelperStaleness::UndeterminableDaemonPath,
}
```

**Reference**: Wave 2 PR-1 (#11) PR description — full enum reasoning.

---

### 6. Path separator + canonical-path round-trip

**Symptom**: tests assert `path.to_str() == "a/b/c"` but Windows produces
`"a\\b\\c"`. Or: a stored canonical path round-trips fine on the host
but normalizes to a different shape (case sensitivity, UNC prefix `\\?\`)
on a different OS.

**Fix shape**: assert via `Path::components()` or compare
`Path::canonicalize()` results to other `Path::canonicalize()` results
(NEVER raw strings). Avoid embedding literal path separators in test
expectations.

---

### 7. Timing-sensitive cross-platform tests

**Symptom**: a test sleeps 100ms and asserts a heartbeat fired. Passes
on Linux runners (fast scheduler), flakes on macOS runners (10–50ms
slop) or on Windows (kernel timer resolution coarser).

**Fix shape**: use larger sleep budgets in tests, OR (preferred) drive
the test off a deterministic event (channel, condvar, observed counter)
rather than wall-clock time.

---

## When the script can't help

The cross-platform clippy gate catches **lint-level** cross-platform
issues (cfg branches, dead_code, type errors in cfg-gated blocks). It
does **not** catch:

- **Link-level failures**: missing C library on the target (gtk on
  non-Linux, openssl-sys on bare Windows). The script categorizes these
  as `skipped (build-script C-dep)` and defers to CI matrix.
- **Runtime behavior differences**: shell-template escape semantics
  (Pattern 3), path-separator handling (Pattern 6), timing flakes
  (Pattern 7). These need targeted tests, reviewed against this doc.
- **Linker-only warnings**: e.g. unused imports that only fire when the
  full build/link runs. Those still need CI matrix.

The expected workflow is:

1. Edit code.
2. `cargo clippy --features tray --bin agend-terminal --tests -- -D warnings`
   (host-only, fast).
3. **`scripts/clippy-all-platforms.sh`** — catches Patterns 1, 4, 5, 6.
4. `cargo test --features tray` (host-only, fast).
5. `git push` → CI matrix verifies link + runtime on all 3 platforms.
6. CI green → merge.

If you skip step 3, you may get fix-forward cycles for Patterns 1, 4,
5, 6 — each cycle costs ~10–15 min of wall time per platform. The local
script costs ~30 sec.

---

## History (when this gate was added)

- **Sprint 56**: 4 cycles of platform-specific fix-forward observed
  during Track I-Phase2c hard-removal cleanup.
- **Sprint 57 Wave 3 PR-2 r1 + r2**: dead_code on platform-gated test
  helpers (Pattern 1) — 2 fix-forward cycles.
- **Sprint 57 Wave 3 PR-3 r0**: format-aware service template escaping
  (Pattern 3) — 1 fix-forward cycle.
- **Sprint 58 Wave 3 PR-1** (this gate): added the local helper script
  and this doc. Shape (c) per general FINAL LOCK — passive operator-
  resolved, no auto-installation, no git-hook injection. Aligns with
  Wave 2 PR-1 (#11 helper-staleness warn) Q3 design pattern.