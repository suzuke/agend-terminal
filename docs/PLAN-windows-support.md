# Windows Support — Implementation Plan

> Date: 2026-04-16
> Status: Planning (not started)
> Prereq: Read `docs/EVAL-cross-platform.md` for full analysis
> Estimated total effort: 2-3 weeks

---

## Overview

The project is currently Unix-only. This plan describes all changes needed to compile and run on Windows, ordered by dependency and risk.

The plan is split into 3 phases:
- **Phase A**: Safe fixes (no architecture change, no risk to existing platforms)
- **Phase B**: IPC migration (core architectural change)
- **Phase C**: Validation (CI, testing, backend compatibility)

---

## Phase A: Platform-Agnostic Fixes

These can be done independently, in any order, and merged immediately. Each is a standalone PR that improves the codebase regardless of whether Windows support ships.

### A1. Replace `/tmp` hardcoded fallbacks

**Files to change (5 production sites)**:

| File | Line | Current | Replace with |
|------|------|---------|-------------|
| `src/main.rs` | 45 | `std::env::var("HOME").unwrap_or_else(\|_\| "/tmp".to_string())` | `dirs::home_dir().unwrap_or_else(\|\| std::env::temp_dir())` |
| `src/ops.rs` | 527 | same pattern | same fix |
| `src/mcp_config.rs` | 283 | same pattern | same fix |
| `src/mcp/handlers.rs` | 573 | same pattern | same fix |
| `src/instructions.rs` | 33 | same pattern | same fix |

**Test fixtures** (`/tmp/test-*` in tests): replace with `tempfile::tempdir()`. Affected test files: `backend.rs`, `fleet.rs`, `store.rs`, `snapshot.rs`, `telegram.rs`, `mcp_config.rs`, `mcp/handlers.rs`.

**New dependency**: `dirs = "6"` (cross-platform home/config dir resolution).

**Acceptance criteria**: No `/tmp` literal in production code. All tests use `tempfile`.

---

### A2. Replace `nix::fcntl::Flock` with `fs2`

**Files to change (3 sites)**:

**`src/daemon.rs:207-217`** — daemon exclusive lock:
```rust
// Before
use nix::fcntl::{Flock, FlockArg};
use std::os::fd::AsFd;
let _daemon_lock = Flock::lock(
    lock_file.as_fd().try_clone_to_owned()?,
    FlockArg::LockExclusiveNonblock,
).map_err(|(_, e)| ...)?;

// After
use fs2::FileExt;
lock_file.try_lock_exclusive()
    .map_err(|e| anyhow::anyhow!("Another daemon is already running (lock held): {e}"))?;
// Lock auto-released when lock_file is dropped
```

**`src/store.rs:36-43`** — store mutation lock:
```rust
// Before
use nix::fcntl::{Flock, FlockArg};
use std::os::fd::AsFd;
let _lock = Flock::lock(lock_file.as_fd().try_clone_to_owned()?, FlockArg::LockExclusive)
    .map_err(|(_, e)| ...)?;

// After
use fs2::FileExt;
lock_file.lock_exclusive()
    .map_err(|e| anyhow::anyhow!("store lock failed: {e}"))?;
```

**`src/fleet.rs:246-258`** — fleet.yaml lock:
```rust
// Before
fn acquire_lock(home: &Path) -> Result<nix::fcntl::Flock<std::fs::File>> { ... }

// After
fn acquire_lock(home: &Path) -> Result<std::fs::File> {
    let lock_path = home.join(".fleet.yaml.lock");
    let f = std::fs::OpenOptions::new()
        .write(true).create(true).truncate(true)
        .open(&lock_path)
        .context("failed to open lock file")?;
    fs2::FileExt::lock_exclusive(&f)
        .map_err(|e| anyhow::anyhow!("flock failed: {e}"))?;
    Ok(f)  // Lock released when File is dropped
}
```

**Note**: `fleet.rs` returns the lock object so callers hold it. With `fs2`, return the `File` itself — lock is released on drop, same semantics.

**New dependency**: `fs2 = "0.4"`.

**Acceptance criteria**: Zero `nix::fcntl` and `std::os::fd` references remaining.

---

### A3. Replace `nix::libc::kill` PID checks

**Files to change (3 sites)**:

Create a helper in `src/process.rs` (new file):
```rust
/// Check if a process with the given PID is alive.
pub fn is_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::FromRawHandle;
        let handle = unsafe {
            windows_sys::Win32::System::Threading::OpenProcess(
                windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
                0, pid,
            )
        };
        if handle == 0 { return false; }
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle); }
        true
    }
}
```

Replace call sites:
| File | Line | Change |
|------|------|--------|
| `daemon.rs` | 158 | `unsafe { nix::libc::kill(...) }` → `crate::process::is_pid_alive(pid)` |
| `daemon.rs` | 398 | same |
| `connect.rs` | 148 | `nix::libc::kill(child_id, SIGTERM)` → see below |

**`connect.rs:148` is different** — it sends SIGTERM to gracefully stop a child. Options:
- Unix: keep `libc::kill(pid, SIGTERM)` behind `#[cfg(unix)]`
- Windows: use `TerminateProcess` (no graceful equivalent without console tricks)
- Or: refactor to hold `std::process::Child` and use `.kill()` (cross-platform but forceful)

**New dependencies**:
- `libc = "0.2"` (Unix, replaces `nix::libc` usage — lighter than full `nix`)
- `windows-sys = { version = "0.59", features = ["Win32_System_Threading", "Win32_Foundation"] }` under `[target.'cfg(windows)'.dependencies]`

**Acceptance criteria**: Zero `nix::libc::kill` references. `cargo check` passes on both platforms.

---

### A4. Make `nix` dependency conditional

After A2 and A3 are done, `nix` has zero remaining call sites.

**`Cargo.toml` change**:
```toml
# Remove from [dependencies]:
# nix = { version = "0.29", features = ["term", "signal", "process", "user", "fs"] }

# Add platform-specific deps:
[target.'cfg(unix)'.dependencies]
libc = "0.2"

[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.59", features = ["Win32_System_Threading", "Win32_Foundation"] }

# Add cross-platform deps:
[dependencies]
fs2 = "0.4"
dirs = "6"
```

**Acceptance criteria**: `cargo check --target x86_64-pc-windows-msvc` passes (for non-IPC code, see Phase B).

---

### A5. Cross-platform shell script generation

**Files to change**:

| File | Script | Current |
|------|--------|---------|
| `instructions.rs:7` | `statusline.sh` | Generates bash script |
| `mcp_config.rs:88` | `statusline.sh` | Generates bash script |
| `mcp_config.rs:132` | `agend-mcp-wrapper.sh` | Generates bash wrapper |

**Approach**: Generate `.cmd` on Windows, `.sh` on Unix.

```rust
fn script_ext() -> &'static str {
    if cfg!(windows) { "cmd" } else { "sh" }
}

fn script_header() -> &'static str {
    if cfg!(windows) { "@echo off\r\n" } else { "#!/usr/bin/env bash\n" }
}
```

Each script's body needs a Windows equivalent:
- `statusline.sh`: typically runs `curl` or writes to a file — translate to `cmd` equivalent
- `agend-mcp-wrapper.sh`: sets env vars and exec's a command — translate to `set` + call

The existing `#[cfg(unix)]` guards for `chmod 0o755` are already correct.

**Acceptance criteria**: Scripts generated match platform. Both `.sh` and `.cmd` variants tested.

---

## Phase B: IPC Migration (UDS → TCP)

This is the only architectural change. It should be done as a single, well-tested PR.

### B1. Introduce TCP-based IPC alongside UDS

**Strategy**: TCP localhost on all platforms. Keep UDS as optional fast-path on Unix (future optimization, not required for v1).

**Port management design**:
```
~/.agend/run/{PID}/
    ports.json     # {"api": 51234, "agents": {"agent1": 51235, "agent2": 51236}}
    .daemon        # existing: "pid:start_time"
```

Daemon startup:
1. Bind `TcpListener::bind("127.0.0.1:0")` for API (OS picks port)
2. Per agent: bind another `TcpListener::bind("127.0.0.1:0")`
3. Write all ports to `ports.json`
4. Clients read `ports.json` to discover ports

**Files to modify**:

| File | Current | New |
|------|---------|-----|
| `daemon.rs:32` | `UnixListener::bind(socket_path)` | `TcpListener::bind("127.0.0.1:0")` |
| `daemon.rs:138-144` | `run_dir()`, `agent_socket_path()` | Port registry read/write helpers |
| `api.rs:28` | `UnixListener::bind(&sock)` | `TcpListener::bind("127.0.0.1:0")` |
| `api.rs:69` | `stream: UnixStream` | `stream: TcpStream` |
| `api.rs:453` | `UnixStream::connect(&sock)` | `TcpStream::connect(("127.0.0.1", port))` |
| `tui.rs:21` | `UnixStream::connect(socket_path)` | `TcpStream::connect(("127.0.0.1", port))` |
| `cli.rs:397` | `UnixStream::connect(entry.path())` | `TcpStream::connect(("127.0.0.1", port))` |
| `framing.rs` | No change needed — reads/writes on `impl Read + Write` | Verify trait bounds |
| `tests/integration.rs` | `UnixStream` | `TcpStream` |

**Key implementation detail**: `framing.rs` already works with generic `Read + Write` streams (verify this). If so, the migration is purely at connection setup — the framing protocol is transport-agnostic.

**Port discovery helper** (new in `src/ipc.rs`):
```rust
use std::net::{TcpListener, TcpStream};

pub fn bind_random() -> std::io::Result<TcpListener> {
    TcpListener::bind("127.0.0.1:0")
}

pub fn local_port(listener: &TcpListener) -> u16 {
    listener.local_addr().unwrap().port()
}

/// Write port registry to run dir.
pub fn write_ports(run_dir: &Path, api_port: u16, agent_ports: &HashMap<String, u16>) -> Result<()> { ... }

/// Read port registry from run dir.
pub fn read_ports(run_dir: &Path) -> Result<PortRegistry> { ... }

/// Connect to agent by name (reads ports.json, connects via TCP).
pub fn connect_agent(home: &Path, name: &str) -> Result<TcpStream> { ... }

/// Connect to daemon API (reads ports.json, connects via TCP).
pub fn connect_api(home: &Path) -> Result<TcpStream> { ... }
```

**Cleanup**: Remove all `use std::os::unix::net::*` imports, delete `.sock` path construction.

**Risk mitigation**:
- Add `TcpStream::set_nodelay(true)` to avoid Nagle's algorithm latency on small writes
- Bind to `127.0.0.1` only (not `0.0.0.0`) — no network exposure
- Consider `SO_REUSEADDR` for quick daemon restart

**Acceptance criteria**:
- All existing integration tests pass with TCP transport
- `cargo check --target x86_64-pc-windows-msvc` passes
- No `std::os::unix` imports remaining (except inside `#[cfg(unix)]` blocks)

---

## Phase C: Validation

### C1. Windows CI setup

Add to `.github/workflows/`:
```yaml
jobs:
  build:
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo build --release
      - run: cargo test
```

### C2. ConPTY / PTY behavior testing

`portable-pty` uses ConPTY on Windows. Known quirks:
- ANSI escape sequence handling differs from Unix PTY
- Terminal size reporting may have timing issues
- No SIGWINCH equivalent — resize is handled differently

**Test plan**:
- Spawn a simple shell (`cmd.exe` / `powershell.exe`) via `portable-pty`
- Verify `VTerm` processes ConPTY output correctly
- Verify resize propagation works
- Test `state.rs` regex patterns against ConPTY output (agent state detection)

### C3. Agent backend availability on Windows

| Backend | Windows support | Notes |
|---------|----------------|-------|
| Claude Code | Yes (npm global) | `claude` CLI works on Windows |
| Codex | Unknown | Check OpenAI docs |
| Kiro CLI | Unknown | AWS-backed, likely Windows support |
| OpenCode | Unknown | Go binary, likely cross-compiled |
| Gemini | Unknown | Google CLI, likely Windows support |

Each unsupported backend should be gracefully handled — clear error message, not a crash.

### C4. End-to-end smoke test on Windows

- `agend-terminal start` with a simple fleet (1 agent, shell backend)
- `agend-terminal list` shows running agent
- `agend-terminal attach {name}` connects and shows terminal
- `agend-terminal inject {name} "echo hello"` works
- `agend-terminal app` TUI launches and renders
- `agend-terminal stop` shuts down cleanly

---

## Task Checklist

```
Phase A — Platform-Agnostic Fixes (can start immediately)
  [ ] A1. Replace /tmp hardcoded fallbacks with dirs::home_dir / temp_dir
  [ ] A2. Replace nix::fcntl::Flock with fs2 (daemon.rs, store.rs, fleet.rs)
  [ ] A3. Replace nix::libc::kill with cross-platform PID helper (+ src/process.rs)
  [ ] A4. Make nix conditional in Cargo.toml, verify cargo check --target windows
  [ ] A5. Cross-platform shell script generation (.sh / .cmd)

Phase B — IPC Migration (single PR, after Phase A)
  [ ] B1. Create src/ipc.rs with TCP port registry helpers
  [ ] B2. Migrate daemon.rs: UnixListener → TcpListener + port registry
  [ ] B3. Migrate api.rs: UnixListener/Stream → TcpListener/Stream
  [ ] B4. Migrate tui.rs: UnixStream → TcpStream
  [ ] B5. Migrate cli.rs: socket probe → TCP probe
  [ ] B6. Update framing.rs if needed (verify Read+Write generics)
  [ ] B7. Update integration tests
  [ ] B8. Remove all std::os::unix imports outside #[cfg(unix)]

Phase C — Validation (after Phase B)
  [ ] C1. Add Windows to GitHub Actions CI matrix
  [ ] C2. Test ConPTY behavior (VTerm, state detection, resize)
  [ ] C3. Verify agent backend availability on Windows
  [ ] C4. End-to-end smoke test on Windows
```

---

## Dependencies Between Tasks

```
A1 ──┐
A2 ──┤
A3 ──┼── A4 ── B1~B8 ── C1~C4
A5 ──┘
```

A1-A3 and A5 are independent of each other.
A4 requires A2+A3 (removes nix entirely).
Phase B requires A4 (clean nix removal).
Phase C requires Phase B (need TCP IPC to test on Windows).
