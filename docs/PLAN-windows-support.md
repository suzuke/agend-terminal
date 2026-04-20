# Windows Support — Implementation Plan

> Date: 2026-04-17 (rev. 2026-04-20 — all phases folded into `main`; the 26200 "ConPTY regression" was re-diagnosed as agend-side and fixed in Session 4).
> Prereq: Read `docs/EVAL-cross-platform.md` for the state-of-play.
> Remaining effort: **none** — Phase A/B/C are all on `main`. The 26200 nested-ConPTY symptom turned out to be the CPR-reply + ignore-Ctrl+C bugs documented in `docs/HANDOVER-windows-conpty-nested.md` (see "Session 4 correction" section). Both fixes landed.

---

## Overview

The Unix-only blockers are all down. IPC moved to TCP loopback, CI runs on `windows-latest`, shell wrappers emit `.cmd` on Windows, and the Windows-only runtime gaps (CPR auto-reply, Ctrl+C delivery, manifest, `which` resolution) have all landed.

Phases:
- **Phase A — DONE**. Platform-agnostic fixes (paths, file locking, PID helpers, chmod guards).
- **Phase B — DONE** (2026-04-17). IPC migration (UDS → TCP loopback + port-file registry in `src/ipc.rs`).
- **Phase C.1 — DONE**. `windows-latest` in the CI matrix (`.github/workflows/ci.yml`).
- **Phase C.2 — DONE**. `.cmd` wrappers emitted alongside `.sh` in `src/instructions.rs` / `src/mcp_config.rs`.
- **Phase C.3 — DONE**. Windows-specific ConPTY fixes landed: DSR CPR auto-reply (`src/vterm.rs` `PtyWriteListener`), Ctrl+C delivery via `SetConsoleCtrlHandler(None, 0)` reset in `src/daemon.rs`, embed-resource manifest declaring Win10/11 + UTF-8, backend resolution via `which`. The 26200 "nested-ConPTY hang" was re-diagnosed in Session 4 as the same CPR bug, not an OS regression — see `docs/HANDOVER-windows-conpty-nested.md` (Status: resolved). GitHub Actions `windows-latest` (Server 2022) is green.
- **Phase C.4 — DONE (de facto)**. Backend PATH resolution fixed in commit `7fa064a`; `doctor` covers availability.
- **Phase C.5 — DONE (de facto)**. Real-PTY start/attach/app/stop have been exercised repeatedly (every HANDOVER-windows-* document was produced by running the E2E smoke flow).

---

## Phase A — DONE

Left here for context / audit trail. Re-verify with `git log` + `grep` rather than redoing any of this.

| Item | Resolution |
|------|------------|
| A1. Replace `/tmp` hardcoded fallbacks | `user_home_dir` / `home_dir` helpers in `src/main.rs:43-60` use `dirs::home_dir()` with `std::env::temp_dir()` fallback. Production code uses these (`src/ops.rs:735`, `src/mcp_config.rs:298`, `src/app.rs:192,232`, `src/fleet.rs:176`). Remaining `/tmp` literals are test-only. |
| A2. Replace `nix::fcntl::Flock` with `fs2` | `src/store.rs:37`, `src/daemon.rs:211`, `src/fleet.rs:255` all use `fs2::FileExt`. |
| A3. Cross-platform PID helper | `src/process.rs` — `is_pid_alive` / `terminate` with `#[cfg(unix)]` (`libc::kill`) and `#[cfg(windows)]` (`OpenProcess` + `TerminateProcess`) impls. |
| A4. Remove `nix`, add conditional deps | `Cargo.toml` has `libc = "0.2"` under `[target.'cfg(unix)'.dependencies]` and `windows-sys` under `[target.'cfg(windows)'.dependencies]`. No `nix` anywhere. |
| A5. `chmod` / `PermissionsExt` guards | Already `#[cfg(unix)]` in `src/mcp_config.rs`, `src/instructions.rs`. Windows builds compile past these. The `.bat` / `.ps1` variant for statusline/MCP wrappers is **not** done — tracked in Phase C2 below since it is a runtime concern, not a compile blocker. |

Acceptance of Phase A: `grep nix::` in `src/` returns nothing; `grep '"/tmp"'` returns only test files.

---

## Phase B — IPC Migration (DONE, landed 2026-04-17)

Kept verbatim below for audit / design-rationale reference. **Implementation is in `src/ipc.rs`** and every call-site listed in B.2 has been migrated. `grep 'std::os::unix::net' src/` returns no matches.

Unix domain sockets are the only architectural blocker left. Choose one strategy and land it as a single well-reviewed PR.

### B.0 Strategy choice

Default recommendation: **TCP localhost on all platforms**. Rationale:

- Single code path; no `#[cfg]` explosion across `daemon.rs` / `api.rs` / `tui.rs` / `cli.rs`.
- `framing.rs` already works over any `Read + Write`, so the protocol layer is transport-agnostic.
- Reusable if a future frontend needs WebSockets (GUI, web dashboard).

Alternatives — pick only if the defaults hurt:
- **UDS on Unix + named pipes on Windows**: optimal per-platform but doubles code paths.
- **IPC trait with Unix / NamedPipe / TCP impls**: clean but over-engineered for two platforms.

### B.1 Port registry

```
~/.agend/run/{PID}/
    ports.json        # {"api": 51234, "agents": {"dev": 51235, "reviewer": 51236}}
    .daemon           # existing: "pid:start_time"
```

Daemon startup:
1. `TcpListener::bind("127.0.0.1:0")` for the API socket — OS picks port.
2. Same per agent.
3. Atomic-write `ports.json`.
4. Clients read `ports.json` to discover the port they need.

Helpers in a new `src/ipc.rs`:
```rust
pub fn bind_random() -> io::Result<TcpListener> { TcpListener::bind("127.0.0.1:0") }
pub fn local_port(l: &TcpListener) -> u16 { l.local_addr().map(|a| a.port()).unwrap_or(0) }
pub fn write_ports(run_dir: &Path, api: u16, agents: &HashMap<String, u16>) -> Result<()>;
pub fn read_ports(run_dir: &Path) -> Result<PortRegistry>;
pub fn connect_api(home: &Path) -> Result<TcpStream>;
pub fn connect_agent(home: &Path, name: &str) -> Result<TcpStream>;
```

### B.2 Call-site migration

| File | Current | New |
|------|---------|-----|
| `src/daemon.rs:32` | `UnixListener::bind(socket_path)` | `TcpListener::bind("127.0.0.1:0")` + register port |
| `src/api.rs:48` | `UnixListener::bind(&sock)` | `TcpListener::bind("127.0.0.1:0")` + register port |
| `src/api.rs:670` | `UnixStream::connect(&sock)` | `ipc::connect_api(home)` |
| `src/tui.rs:21` | `UnixStream::connect(socket_path)` | `ipc::connect_agent(home, name)` |
| `src/cli.rs:402` | socket probe via `UnixStream::connect` | TCP probe using `ports.json` |
| `tests/integration.rs` | `UnixStream` | `TcpStream` |
| `src/framing.rs` | No change | Verify generics over `Read + Write` |

### B.3 Hygiene

- `TcpStream::set_nodelay(true)` on both ends — avoids Nagle latency on small frame writes.
- Bind only to `127.0.0.1`, never `0.0.0.0` — no network exposure.
- Consider `SO_REUSEADDR` for fast daemon restart (avoid `EADDRINUSE` after crash).
- Clean up `ports.json` on daemon shutdown.
- Remove every `use std::os::unix::net::*` outside `#[cfg(unix)]` blocks.

### B.4 Acceptance criteria

- All integration tests pass on macOS + Linux with TCP transport.
- `cargo check --target x86_64-pc-windows-msvc` passes.
- `grep "std::os::unix::net"` returns no matches outside `#[cfg(unix)]` blocks.

---

## Phase C — Validation (DONE)

### C.1 Windows CI

Extend `.github/workflows/ci.yml` with a matrix entry. Keep PR CI time bounded — Windows runners are slow.

```yaml
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

### C.2 Windows shell wrappers

The MCP wrapper and statusline scripts are generated as `.sh` today. On Windows, generate `.cmd` equivalents alongside. Call sites: `src/instructions.rs`, `src/mcp_config.rs`.

Sketch:
```rust
fn script_ext() -> &'static str { if cfg!(windows) { "cmd" } else { "sh" } }
fn script_header() -> &'static str {
    if cfg!(windows) { "@echo off\r\n" } else { "#!/usr/bin/env bash\n" }
}
```
Each script body needs a Windows translation — typically short (`set VAR=...` + `call ...`).

### C.3 ConPTY / PTY behavior

`portable-pty` uses ConPTY on Windows. Known quirks to validate:
- ANSI handling differs from Unix PTYs (some sequences interpreted/filtered by conhost).
- No SIGWINCH; resize uses a different API.
- Line buffering and timing differ — state-detection regexes (`src/state.rs`) may need tuning.

Test plan:
- Spawn `cmd.exe` / `powershell.exe` via `portable-pty` and drive a known script.
- Verify `VTerm` output matches on macOS / Linux / Windows for a small corpus.
- Test resize propagation through the daemon.
- Validate agent-state regex patterns against ConPTY output.

### C.4 Agent backends

| Backend | Windows? | Check |
|---------|----------|-------|
| Claude Code (`claude`) | Yes (npm global) | Verify PATH resolution. |
| Codex (`codex`) | Unknown | Check OpenAI docs. |
| Kiro CLI (`kiro-cli`) | Unknown | AWS — likely yes. |
| OpenCode (`opencode`) | Unknown | Go binary, usually cross-compiled. |
| Gemini (`gemini`) | Unknown | Google CLI, likely yes. |

Each missing backend should surface a clean error in `doctor`, not a crash.

### C.5 End-to-end smoke

- `agend-terminal start` with a 1-agent fleet using a shell backend.
- `agend-terminal list` → running.
- `agend-terminal attach` → renders.
- `agend-terminal inject <name> "echo hi"` → works.
- `agend-terminal app` → TUI renders.
- `agend-terminal stop` → clean.

---

## Task Checklist

```
Phase B — IPC migration (DONE 2026-04-17)
  [x] B.0 Strategy chosen: TCP localhost everywhere
  [x] B.1 src/ipc.rs — port-file registry + connect helpers
  [x] B.2 daemon.rs / api.rs / tui.rs / cli.rs / mcp/* / agent.rs / verify.rs / ops.rs migrated
  [x] B.3 framing.rs already generic; set_nodelay + loopback-only bind applied
  [x] B.4 integration tests on TCP
  [x] B.5 No std::os::unix::net outside #[cfg(unix)] blocks (grep clean)
  [x] B.6 cargo check --target x86_64-pc-windows-msvc passes

Phase C — Validation
  [x] C.1 Windows in CI matrix (.github/workflows/ci.yml)
  [x] C.2 .cmd wrappers on Windows (instructions.rs, mcp_config.rs)
  [x] C.3 ConPTY behavior — CPR auto-reply, Ctrl+C reset, manifest, which
          resolution. The 26200 "nested-ConPTY hang" was re-diagnosed as
          the same CPR bug (HANDOVER-windows-conpty-nested.md, Session 4).
  [x] C.4 Backend availability — which-based PATH resolution (commit 7fa064a)
  [x] C.5 End-to-end smoke — run repeatedly during C.3 debugging
```

---

## Remaining work

None. If a new Windows-only symptom appears, start by setting `AGEND_DEBUG_PTY_READ=1` and reading the actual PTY byte stream before assuming any OS-side regression — that's the Session 4 lesson from `docs/HANDOVER-windows-conpty-nested.md`.
