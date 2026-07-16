# PTY fd hygiene audit (t-20260704054906745324-67777-4)

> **Historical audit snapshot:** Findings below were verified at audit time and
> are retained as evidence. They are not a continuously maintained runtime
> contract; re-check current dependencies and source before relying on them.

Scope note on provenance: this task was framed as "herdr-inspired" — herdr is
AGPL-3.0, so this is a clean-room audit of **our own dependency**
(`portable-pty` 0.9.0, MIT) and **our own code**, not a reading of herdr's
implementation. Nothing here cites or is derived from herdr source.

## 1. PTY master fd CLOEXEC (Unix) — premise did not hold

**Finding: already correctly hygienic, at two independent layers. No
production code change needed.**

`portable-pty` 0.9.0's `src/unix.rs::openpty()` calls `cloexec()` on both the
master and slave fd immediately after `libc::openpty()`:

```rust
// portable-pty-0.9.0/src/unix.rs:60-65
// Ensure that these descriptors will get closed when we execute
// the child process.  This is done after constructing the Pty
// instances so that we ensure that the Ptys get drop()'d if
// the cloexec() functions fail (unlikely!).
cloexec(master.fd.as_raw_fd())?;
cloexec(slave.fd.as_raw_fd())?;
```

Every fd-duplication path also preserves this: `try_clone_reader()` /
`take_writer()` go through the `filedescriptor` crate's `try_clone()`
(`filedescriptor-0.8.3/src/unix.rs:144-149`, using `F_DUPFD_CLOEXEC`), and the
child's own stdio wiring (`as_stdio()`, used by `spawn_command`'s
`.stdin()/.stdout()/.stderr()`) goes through the same `F_DUPFD_CLOEXEC` dup
(`filedescriptor-0.8.3/src/unix.rs:261-266`) — the resulting `Stdio` handle is
itself cloexec'd, and Rust's own `std::process::Command` spawn machinery
clears `FD_CLOEXEC` on the *new* fd it creates via `dup2` onto 0/1/2 in the
child (standard POSIX `dup2` semantics — the destination descriptor never
inherits `FD_CLOEXEC` from the source), so the child's actual stdin/stdout/
stderr correctly survive `exec` while every other duplicate does not.

**A second, independent layer**: `UnixSlavePty::spawn_command`'s `pre_exec`
closure (run post-fork, pre-exec, in the child) unconditionally calls
`close_random_fds()` (`unix.rs:152-172`), which enumerates `/dev/fd` and
force-closes every fd numbered ≥ 3. Per its own doc comment, this exists
because "Cocoa leaks various file descriptors to child processes" on macOS
and "gnome/mutter leak shell extension fds" on Linux — i.e. it's a blanket
backstop independent of and stronger than per-fd `CLOEXEC`, added to catch
leaks from *any* source, not just our own PTY fds.

Net effect: there is no fd-hygiene gap to close for item ①. Confirmed by
direct experiment (see §2) — a deliberately non-cloexec'd fd (`libc::dup()`,
no `F_DUPFD_CLOEXEC`) still does **not** survive into a child spawned via
`UnixSlavePty::spawn_command`, because `close_random_fds()` sweeps it anyway.

Our own code was also checked for anywhere it might bypass these safe paths
(e.g. extracting the raw master fd and duplicating it manually): the only
`as_raw_fd()` call on a `MasterPty` in this codebase
(`src/backend_harness.rs`, pre-existing) reads the fd number for a
`tcgetpgrp()` ioctl — it does not duplicate or leak it.

## 2. fd-leak regression test (added)

`src/backend_harness.rs`:
- `pty_master_fd_does_not_leak_into_spawned_child` — pins the finding above:
  opens a real PTY pair, spawns a child via the actual `spawn_command` path
  this project uses, and asserts the master fd is **not** visible in the
  child's own fd table (checked via `/dev/fd/N`, which — unlike Linux-only
  `/proc/self/fd` — works identically on macOS and Linux, so the test runs on
  both CI Unix runners).
- `fd_leak_probe_detects_fds_that_are_genuinely_open_control_group` — a
  control group proving the probe mechanism itself has real detection power
  (checks fd 1, which is always open in any spawned child) rather than being
  a probe that vacuously reports "not leaked" for anything. An earlier
  version of this control tried to manufacture a leak via a raw
  non-cloexec'd `dup()`; it didn't leak either, which is what led to
  discovering `close_random_fds()` in §1 — the control was revised once the
  root cause was understood, rather than force-fitted to the original design.

Both tests are Unix-only (`#[cfg(unix)]`), matching the task's own Unix scope
for item ①.

## 3. ConPTY dll side-load probe (Windows) — confirmed, proposal below

**Finding: the probing behavior is real, and is a deliberate feature, not an
accident.** `portable-pty` 0.9.0's `src/win/psuedocon.rs::load_conpty()`:

```rust
// portable-pty-0.9.0/src/win/psuedocon.rs:44-56
fn load_conpty() -> ConPtyFuncs {
    // If the kernel doesn't export these functions then their system is
    // too old and we cannot run.
    let kernel = ConPtyFuncs::open(Path::new("kernel32.dll")).expect(
        "this system does not support conpty.  Windows 10 October 2018 or newer is required",
    );

    // We prefer to use a sideloaded conpty.dll and openconsole.exe host deployed
    // alongside the application.  We check for this after checking for kernel
    // support so that we don't try to proceed and do something crazy.
    if let Ok(sideloaded) = ConPtyFuncs::open(Path::new("conpty.dll")) {
        sideloaded
    } else {
        kernel
    }
}
```

`ConPtyFuncs::open` (the `shared_library!` macro, from the `shared_library`
0.1.9 crate, Apache-2.0/MIT) resolves to a **bare, unqualified**
`LoadLibraryW(filename)` call (`shared_library-0.1.9/src/dynamic_library.rs`
Windows arm, ~line 340) — no `LoadLibraryExW` flags restricting the search
path. This is the textbook shape of CWE-427 (Uncontrolled Search Path
Element / "DLL side-loading"): if a `conpty.dll` exists anywhere earlier in
the default Windows DLL search order than the legitimate one (the
application's own directory is searched first by default, before `System32`
— an attacker who can write into that directory, or into a directory earlier
in `PATH`, can plant a malicious `conpty.dll` whose exported
`CreatePseudoConsole`/`ResizePseudoConsole`/`ClosePseudoConsole` get called
in-process).

This is not a bug in the sense of "unintended behavior" — the comment says
the crate *deliberately* wants to support a sideloaded, updated
`conpty.dll` dropped next to the app binary (a real, useful feature: it lets
an app use a newer ConPTY without requiring a Windows update). The
vulnerability is that the *search* for that sideload uses the unrestricted
default order instead of specifically checking the app's own directory.

**Proposal (three options, my recommendation marked):**

1. **Runtime protection (recommended)** — call
   `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_APPLICATION_DIR |
   LOAD_LIBRARY_SEARCH_SYSTEM32)` once, early in `main()` on Windows, before
   any PTY is opened. This is a **process-wide** setting — per Microsoft's
   docs, once called, `LoadLibrary`/`LoadLibraryEx` calls that don't specify
   their own search flags (which is exactly what `shared_library`'s bare
   `LoadLibraryW` does) use the directories established here instead of the
   full default order. This closes the `PATH`/current-directory hijack
   vector while **preserving** the intended sideload-next-to-the-exe feature
   (`LOAD_LIBRARY_SEARCH_APPLICATION_DIR` covers exactly that case), requires
   no changes to `portable-pty` or `shared_library`, and is defense-in-depth
   for every other unqualified `LoadLibrary` call anywhere else in the
   process (ours or any other dependency's). Effort: one FFI call
   (`windows-sys` or raw `kernel32` binding) gated `#[cfg(windows)]`, at
   process startup. Caveat: I could not build/test this on this machine
   (developing on macOS); it needs verification on a real `windows-latest`
   CI run or a Windows machine before merging with confidence, and someone
   with Windows access should confirm the ConPTY sideload feature still
   works as intended afterward (an app-directory-placed `conpty.dll` should
   still load; a `PATH`- or cwd-placed one should not).
2. **Vendor/patch `portable-pty`** — fork and patch `load_conpty()` to build
   an absolute path via `std::env::current_exe()`'s parent directory instead
   of a bare `"conpty.dll"`. More surgical and doesn't depend on a
   process-wide flag some other code path might not expect, but commits us
   to maintaining a patched fork across upstream version bumps.
3. **Upstream PR to wezterm/portable-pty** — report this as a CWE-427-class
   finding and propose the same absolute-path fix upstream. Best long-term
   outcome for the whole ecosystem, but timeline and acceptance are outside
   our control, so it doesn't help us in the near term on its own (could be
   combined with option 1 as an interim mitigation).

I lean toward **option 1** as the immediate fix (low blast radius, doesn't
touch a dependency, verifiable in CI) with **option 3** filed as a follow-up
regardless of what's decided here, since it fixes the root cause for every
downstream consumer of the crate. Deferring the actual implementation
decision to the lead per this task's own instruction; happy to implement
whichever option is chosen in a follow-up PR.
