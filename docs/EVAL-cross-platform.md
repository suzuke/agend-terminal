# Cross-Platform Compatibility Evaluation

> Date: 2026-04-17 (rev. — reflects current Cargo.toml and src/ tree)
> Status: Unix-only (macOS + Linux) at runtime. Compiles on Windows for everything except IPC (UDS) — that remaining blocker is tracked in `PLAN-windows-support.md`.

---

## 1. Current State

- macOS: primary target, tested.
- Linux: same Unix paths, expected to work; not on CI yet.
- Windows: will not run — all daemon/TUI IPC uses Unix domain sockets. Several other items that used to block compilation have already been fixed (see §3).

---

## 2. Remaining Blockers

### 2.1 Blocking: Unix Domain Sockets

Core IPC is built on UDS (`std::os::unix::net`). This is the **only architectural blocker left**.

| File | Usage | Purpose |
|------|-------|---------|
| `api.rs:10` | `UnixListener`, `UnixStream` | Daemon control API (`api.sock`) |
| `daemon.rs:12,32` | `UnixListener` | Per-agent TUI socket server |
| `tui.rs:9,21` | `UnixStream` | TUI attach client |
| `cli.rs:402` | `UnixStream::connect` | Health check socket probe |

Socket path layout:
```
~/.agend/run/{PID}/api.sock          # daemon control
~/.agend/run/{PID}/{agent_name}.sock # per-agent PTY stream
```

Options for replacement: TCP localhost, Windows named pipes, or an IPC trait. See `PLAN-windows-support.md` §B for the decision and rollout.

---

## 3. Already Fixed (previously listed as blockers)

These items were called out in earlier revisions of this doc and have since been resolved. They are documented here so reviewers don't re-flag them.

| Previous blocker | Resolution |
|------------------|------------|
| `nix` crate as hard dependency | Removed entirely. File locking now uses `fs2` (`src/store.rs:37`, `src/daemon.rs:211`, `src/fleet.rs:255`). |
| `nix::libc::kill` for PID liveness / signals | Abstracted into `src/process.rs` — `is_pid_alive` / `terminate` with `#[cfg(unix)]` + `#[cfg(windows)]` impls. `windows-sys` pulled in under `[target.'cfg(windows)'.dependencies]`. |
| `std::os::fd::AsFd` tied to `nix::Flock` | Gone with the `nix` removal. |
| `/tmp` hardcoded fallbacks | Replaced with `std::env::temp_dir()` / `dirs::home_dir()` via the `user_home_dir` and `home_dir` helpers in `src/main.rs:43-60`. A few `/tmp` literals remain only inside tests (`src/backend.rs`, `src/telegram.rs`) — harmless, test-only paths. |
| `chmod` / `PermissionsExt` usage | Already guarded by `#[cfg(unix)]` (`src/mcp_config.rs:98,154`, `src/instructions.rs:18`) — Windows builds silently skip. |

---

## 4. Non-blocking Cross-Platform Notes

| Dependency | Status | Notes |
|------------|--------|-------|
| `portable-pty` | ✅ | Supports Windows ConPTY natively — but behavior differences (ANSI, SIGWINCH equivalents) will need validation. |
| `crossterm` / `ratatui` / `alacritty_terminal` | ✅ | Platform-agnostic. |
| `tokio` / `reqwest` / `teloxide` | ✅ | No platform-specific deps. |
| `serde` / `serde_json` / `serde_yaml` / `clap` / `chrono` / `regex` / `anyhow` | ✅ | Pure logic. |
| `arboard` | ✅ | Cross-platform clipboard. |
| `fs2` | ✅ | Works on both Unix and Windows. |

---

## 5. Effort Estimate

### macOS → Linux

**Near zero.** Same Unix APIs. Needs CI and PTY-behavior validation, not code changes.

### macOS → Windows

**Medium (1–2 weeks).** With `nix`/`libc` already abstracted and paths already portable, what remains is:

| Work Item | Scope | Difficulty |
|-----------|-------|------------|
| Replace UDS with TCP or named pipes | `api.rs`, `daemon.rs`, `tui.rs`, `cli.rs`, `framing.rs` | High — still architectural |
| Generate Windows shell helpers (`.bat`/`.ps1`) for MCP/Claude wrappers | `instructions.rs`, `mcp_config.rs` | Medium — parallel path alongside current `.sh` |
| CI + smoke test on Windows | `.github/workflows/ci.yml` | Medium |
| PTY behavior tuning (ConPTY ANSI quirks, state regex) | `state.rs`, `vterm.rs` | Medium — empirical |

---

## 6. IPC Strategy Options

### Option A — TCP Localhost

```
Daemon listens on 127.0.0.1:{port}
Port written to ~/.agend/run/{PID}/port
```

| Pro | Con |
|-----|-----|
| One code path everywhere | Port conflicts possible |
| Trivial to debug (`curl`, `nc`) | Firewall prompts on first run |
| Reusable if a GUI ever needs WebSocket | Slightly higher overhead than UDS |

### Option B — Platform-Conditional IPC

```rust
#[cfg(unix)]    mod ipc { /* UnixListener / UnixStream */ }
#[cfg(windows)] mod ipc { /* tokio::net::windows::named_pipe */ }
```

| Pro | Con |
|-----|-----|
| Optimal per platform | Two code paths to maintain |
| No port/firewall concerns | Named-pipe and socket APIs diverge |

### Option C — IPC Trait

```rust
trait IpcListener { fn accept(&self) -> Result<Box<dyn IpcStream>>; }
trait IpcStream: Read + Write {}
// Impls: UnixIpc, NamedPipeIpc, TcpIpc
```

Clean, testable, over-engineered for just two platforms.

**Current lean**: Option A if a GUI/WebSocket frontend happens anyway; otherwise Option B. `PLAN-windows-support.md` carries the final decision.

---

## 7. Risks

### Low
- File locking and `/tmp` fixes (already done).
- Linux parity (likely works; needs CI to confirm).

### Medium
- IPC replacement — large surface, many call sites.
- MCP/Claude wrapper script generation on Windows.
- Agent-backend availability on Windows (`claude`, `codex`, `gemini`, `opencode`, `kiro-cli` — each has its own Windows story).

### High
- **ConPTY behavior**: ANSI handling, resize signaling, and line-buffering differ from Unix. State-detection regexes in `state.rs` may need tuning.
- **Test coverage**: without Windows CI, regressions land silently.

---

## 8. Recommendation

1. Keep the Unix happy path clean — don't contort it for Windows speculation.
2. Land Linux CI first (cheap win).
3. For Windows, only move once IPC strategy is decided. Named pipes vs TCP localhost is the fork in the road.
4. Everything else on the Windows checklist is already done.
