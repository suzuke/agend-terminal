# Cross-Platform Compatibility Evaluation

> Date: 2026-04-16
> Status: Unix-only (macOS + Linux). Windows cannot compile.

---

## 1. Current State: Unix-Only

The project **will not compile on Windows** — `nix` crate is a hard unconditional dependency and has no Windows support.

On Linux it should work as-is (same Unix APIs), but has not been tested.

---

## 2. Platform Dependency Inventory

### 2.1 Blocking: `nix` Crate (Unix-only, compile failure on Windows)

`Cargo.toml` declares `nix = "0.29"` as unconditional dependency with features `["term", "signal", "process", "user", "fs"]`.

| File | Usage | Purpose |
|------|-------|---------|
| `daemon.rs:211-216` | `nix::fcntl::Flock` + `FlockArg` | Exclusive daemon lock (prevent double-start) |
| `daemon.rs:158,398` | `nix::libc::kill(pid, 0)` | PID liveness check (is process alive?) |
| `store.rs:37-42` | `nix::fcntl::Flock` + `FlockArg` | Atomic file store mutation lock |
| `fleet.rs:247,255` | `nix::fcntl::Flock` | fleet.yaml write lock |
| `connect.rs:148` | `nix::libc::kill(pid, SIGTERM)` | Kill child process on user Ctrl+C |

**Windows alternative**: `fs2` crate for file locking, `windows-sys` for process management, or `sysinfo` for PID checks.

### 2.2 Blocking: Unix Domain Sockets (compile failure on Windows)

Core IPC architecture is built on UDS (`std::os::unix::net`). This is the **largest structural blocker**.

| File | Usage | Purpose |
|------|-------|---------|
| `api.rs:10,28,69,453` | `UnixListener`, `UnixStream` | Daemon control API (`api.sock`) |
| `daemon.rs:12,32` | `UnixListener` | Per-agent TUI socket server |
| `tui.rs:9,21` | `UnixStream` | TUI attach client |
| `cli.rs:397` | `UnixStream::connect` | Health check socket probe |

Socket path structure:
```
~/.agend/run/{PID}/api.sock          # daemon control
~/.agend/run/{PID}/{agent_name}.sock # per-agent PTY stream
```

**Windows alternatives**:
- Named pipes (`\\.\pipe\agend-*`) — closest equivalent, but different API
- TCP localhost — simplest cross-platform option, but port management adds complexity
- Tokio's `tokio::net::windows::named_pipe` — async named pipe support

### 2.3 Blocking: `std::os::fd` (compile failure on Windows)

| File | Usage | Purpose |
|------|-------|---------|
| `daemon.rs:212` | `std::os::fd::AsFd` | Convert File to fd for flock |
| `store.rs:38` | `std::os::fd::AsFd` | Convert File to fd for flock |

Tightly coupled to `nix::fcntl::Flock` — both go away together when replacing file locking.

### 2.4 Non-blocking: `chmod` (guarded by `#[cfg(unix)]`)

Already properly gated — will silently skip on Windows (scripts won't be marked executable, but Windows doesn't use Unix permission bits anyway).

| File | Lines | Purpose |
|------|-------|---------|
| `instructions.rs` | 12-15 | Make statusline.sh executable |
| `mcp_config.rs` | 92-96 | Make Claude settings script executable |
| `mcp_config.rs` | 139-143 | Make Kiro MCP wrapper executable |

### 2.5 Non-blocking: Hardcoded `/tmp` fallback

| Files | Pattern |
|-------|---------|
| `main.rs:45`, `ops.rs:527`, `mcp_config.rs:283`, `instructions.rs:33`, `mcp/handlers.rs:573` | `unwrap_or_else(\|_\| "/tmp")` as HOME fallback |

**Fix**: Replace with `std::env::temp_dir()`. Trivial change.

### 2.6 Cross-platform OK (no changes needed)

| Dependency | Status | Notes |
|------------|--------|-------|
| `portable-pty` | ✅ | Supports Windows ConPTY natively |
| `crossterm` | ✅ | Full Windows terminal support |
| `ratatui` | ✅ | Platform-agnostic rendering |
| `alacritty_terminal` | ✅ | Pure terminal emulation, no platform deps |
| `tokio` | ✅ | Cross-platform async runtime |
| `teloxide` | ✅ | HTTP-based, no platform deps |
| `serde` / `serde_json` / `serde_yaml` | ✅ | Pure data |
| `clap` | ✅ | Platform-agnostic CLI |
| `regex` / `chrono` / `anyhow` | ✅ | Pure logic |
| `arboard` | ✅ | Cross-platform clipboard |
| `reqwest` | ✅ | Cross-platform HTTP |

---

## 3. Effort Estimate by Platform

### macOS → Linux

**Effort: Near zero.**

Same Unix APIs. Likely works already — needs testing for:
- PTY behavior differences (minor)
- Terminal capability detection
- Home directory resolution (`$HOME` vs `/home/user`)
- Package availability of agent backends (claude, codex, etc.)

### macOS → Windows

**Effort: Significant (2-4 weeks).**

| Work Item | Scope | Difficulty |
|-----------|-------|------------|
| Replace UDS with named pipes or TCP | `api.rs`, `daemon.rs`, `tui.rs`, `cli.rs`, `framing.rs` | High — architectural change |
| Replace `nix::fcntl::Flock` with cross-platform locking | `daemon.rs`, `store.rs`, `fleet.rs` | Medium — use `fs2` crate |
| Replace `nix::libc::kill` PID checks | `daemon.rs`, `connect.rs` | Medium — use `sysinfo` or Windows API |
| Make `nix` dependency conditional | `Cargo.toml` | Easy |
| Replace `/tmp` fallbacks | 5 files | Easy — `std::env::temp_dir()` |
| Shell script handling (.sh → .bat/.ps1) | `instructions.rs`, `mcp_config.rs` | Medium |
| Test on Windows CI | CI setup | Medium |

---

## 4. IPC Strategy Options (the core decision)

The biggest architectural decision for Windows support is replacing Unix domain sockets. Three options:

### Option A: TCP Localhost

```
Daemon listens on 127.0.0.1:{port}
Port written to ~/.agend/run/{PID}/port file
```

| Pro | Con |
|-----|-----|
| Works everywhere, trivial to implement | Port conflicts possible |
| Same API for all platforms | Firewall may block |
| Easy to debug (curl, netcat) | Slightly higher overhead than UDS |
| Can reuse with Tauri GUI later | Need port discovery mechanism |

### Option B: Platform-Conditional IPC

```rust
#[cfg(unix)]
mod ipc { /* UnixListener / UnixStream */ }

#[cfg(windows)]
mod ipc { /* Named pipes via tokio */ }
```

| Pro | Con |
|-----|-----|
| Optimal for each platform | Two code paths to maintain |
| No port conflicts | Named pipe API is different from socket API |
| No firewall issues | More complex abstraction layer |

### Option C: Abstract IPC Trait

```rust
trait IpcListener { fn accept(&self) -> Result<Box<dyn IpcStream>>; }
trait IpcStream: Read + Write { }

// Implementations: UnixIpc, NamedPipeIpc, TcpIpc
```

| Pro | Con |
|-----|-----|
| Clean abstraction | Over-engineering if only 2 platforms |
| Easy to add new transports | Trait object overhead (minor) |
| Testable | More code upfront |

**Recommendation**: Option A (TCP localhost) if you plan to do Tauri GUI anyway — WebSocket for xterm.js will need TCP regardless. UDS can be kept as preferred transport on Unix with TCP as fallback/Windows default.

---

## 5. Risk Assessment

### Low Risk
- File locking replacement (`fs2` is well-tested)
- `/tmp` path fixes (trivial)
- `chmod` guards (already done)
- Linux support (likely works already)

### Medium Risk
- IPC replacement — large surface area, many call sites, potential for subtle behavioral differences
- Shell script generation — Windows has different script conventions (.bat, .ps1, no shebang)
- Agent backend availability — claude CLI, codex CLI may not all support Windows

### High Risk
- **PTY behavior on Windows**: `portable-pty` uses ConPTY which has known quirks with ANSI escape sequences, terminal size reporting, and signal propagation. Agent state detection (`state.rs` regex patterns) may need tuning
- **Two-platform testing burden**: every change needs validation on both platforms. Without Windows CI, regressions will slip in

---

## 6. Recommendation

### If building Tauri GUI: Windows support comes almost for free

Tauri runs on Windows natively. The GUI frontend (xterm.js + WebSocket) is platform-agnostic. The only remaining work is:
1. Make `agend-core` compile on Windows (conditional deps, TCP IPC)
2. PTY behavior testing

**The Tauri path and the Windows path overlap significantly** — both require extracting core logic from Unix-specific transport.

### If staying TUI-only: Windows support is low ROI

- Target audience (developers using AI coding agents) overwhelmingly uses macOS/Linux
- WSL exists as an escape hatch for Windows developers
- Engineering effort better spent on core features

### Suggested Priority

1. **Now**: Fix `/tmp` fallbacks → `std::env::temp_dir()` (5 min, no downside)
2. **Now**: Move `nix` to `[target.'cfg(unix)'.dependencies]` and gate imports (prevents accidental Windows compilation attempts from confusing error messages)
3. **Phase 0 of GUI plan**: Extract `agend-core` crate, introduce IPC abstraction
4. **With Tauri**: Add TCP IPC backend, test on Windows
