# Handover — Windows ConPTY nested spawn silent hang

**Status**: unresolved, blocks Windows usability on Insider Dev builds.
**Environment reproducing**: Windows 11 Insider Dev, build **10.0.26200**.
**Environment known-good**: GitHub Actions `windows-latest` (Windows Server 2022 ≈ Windows 11 22H2). CI is green.

## Symptom

1. `agend-terminal start` (or `daemon shell:powershell.exe`) spawns a child successfully — `Get-Process cmd` / `Get-Process powershell` confirms the child PID is alive.
2. The child process produces **zero bytes of output** through the PTY. `state=starting silent=129.98s` health warning fires after 120s.
3. Both `agend-terminal attach shell` and the app-mode pane show a completely black screen — the vterm grid is empty because nothing has been read from the PTY.
4. Typing into the pane/attach produces no echo. Input framing is fine (verified by framing tests); cmd.exe/powershell.exe is either discarding input or never reaching its read loop.

Hang detection log pattern:
```
INFO spawned agent="shell" backend="cmd.exe" args=
INFO TUI socket ready agent="shell" port=9948
WARN hang detected agent=shell state="starting" silent=129.9844526s
```

## What has been ruled out

- **Not a framing / TCP bug**. `TUI client connected` / `disconnected` events fire correctly. dump frame is sent (it's just empty because vterm has nothing).
- **Not a `KeyEventKind::Release` regression**. That was fixed in PR #4 (merge `ef22b58`).
- **Not a conpty.dll version issue**. User sideloaded `conpty.dll` + `OpenConsole.exe` next to `agend-terminal.exe` (`portable-pty` auto-prefers sideload, see `portable-pty-0.9.0/src/win/psuedocon.rs:55`). **No change in behavior.**
- **Not a cmd.exe quirk**. PowerShell also silent. Any child via `portable-pty` is silent on 26200.
- **Not a PATH / spawn failure**. `Get-Process cmd` shows the child is alive with PID 18796 in a live repro.

## Confirmed working control: WezTerm

User installed WezTerm (also uses `portable-pty` internally) on the same 26200 box. WezTerm successfully spawns cmd.exe and shows its banner + prompt. → **`portable-pty` itself works on this build. Our use of it differs from WezTerm's.**

## Leading hypothesis — nested ConPTY under inherited parent console

- **WezTerm**: compiled as **GUI subsystem** (`windows`). Has no parent console — it creates its own window and calls `CreatePseudoConsole` from a clean state.
- **agend-terminal**: compiled as **console subsystem**. When launched from PowerShell, it inherits PowerShell's console (conhost). It then calls `CreatePseudoConsole` on top of that inherited console → nested pseudo-console chain.

On Windows 10.0.26200 (Insider Dev), this nested-ConPTY scenario appears to silently fail: child is spawned and attached to the new PseudoConsole, but output never flows to our read pipe (`stdout.read` in `portable-pty/src/win/conpty.rs:29`). Windows Server 2022 (CI) does not exhibit this regression.

## Proposed investigation — direction A (cheapest first)

`FreeConsole()` the parent console before the first `openpty` on Windows and redirect daemon logging to file. This removes the nested-console condition that WezTerm implicitly avoids by being GUI subsystem.

**Concrete steps:**

1. **On Windows `daemon::run` entry**, before any agent spawn:
   - Detect if stderr is a console (`GetConsoleMode` / `std::io::IsTerminal`).
   - If yes, redirect `tracing_subscriber` to `{home}/daemon.log` (similar to how app mode already does at `src/app/mod.rs:39`).
   - Call `FreeConsole()` via `windows-sys` crate's `Win32::System::Console::FreeConsole`.

2. **Replace `ctrlc` on Windows**. `ctrlc` requires a console — it wires `SetConsoleCtrlHandler`. After `FreeConsole`, that handler is dead. Use `windows-sys`'s `SetConsoleCtrlHandler` directly, or call it **before** `FreeConsole` and keep the handler registered (Windows may keep the handler alive across console detach, needs verification).

3. **Verify** on the 26200 box that `agend-terminal start` now produces cmd.exe output within a second.

4. If direction A works, productize:
   - Only run `FreeConsole` on daemon subcommand (`start`, `daemon`, fleet variants) — not on `attach`, `list`, etc., which need to print to the user's console.
   - Add a startup warning if `GetVersion` reports build >= 26000 and sideloaded conpty is absent (so we guide users even if future Insider builds regress differently).

## Fallback — direction B

If A doesn't help, read WezTerm's `pty/src/win/` (they ship a patched `portable-pty`) and diff against upstream 0.9. Look for:
- `SetConsoleMode` calls before `CreatePseudoConsole`
- `AllocConsole` / `AttachConsole`
- Different `PSEUDOCONSOLE_*` flag combos (upstream hardcodes `PSUEDOCONSOLE_INHERIT_CURSOR | PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE` at `portable-pty-0.9.0/src/win/psuedocon.rs:87-89`)
- Whether WezTerm explicitly clears `STARTF_USESTDHANDLES`

WezTerm source: `https://github.com/wez/wezterm/tree/main/pty/src/win`.

## Known-good fixes already landed (context, do not redo)

- `fix/windows-misc` (PR #4, merged `ef22b58`):
  - `src/tui.rs` filter `KeyEventKind::Release` so attach input & `Ctrl+B d` work on Windows.
  - `src/daemon.rs` `shutdown_rx` channel wakes the main loop immediately on `Ctrl+C` (previously up to 10s tick delay).
- `fix/windows-path-separator` (merged `2066ce8`): `std::env::split_paths` / `join_paths` in `src/agent.rs:208-219` instead of `:`.
- `health::check_hang` signature: takes `Duration`, not `Instant` (merged `2066ce8`) — avoids `Instant` boot-anchor overflow on Windows.

## Test harness for Windows PTY work

- **CI**: `windows-latest` runner passes all 6 integration tests + unit tests. Use PR-triggered CI as the baseline signal.
- **Local repro (user has)**: Windows 11 Insider Dev 26200. Sideloaded `conpty.dll` + `OpenConsole.exe` next to the binary — keep these in place for any direction-A test, they're not the problem but don't remove them either.
- **Diagnostic commands (PowerShell)**:
  - `Get-Process agend-terminal,cmd,powershell | Format-Table Id,ProcessName,Path` — confirms child spawn.
  - `Get-ChildItem "$env:USERPROFILE\.agend\run" -Recurse -Force` — inspect run dir state.
  - `[System.Environment]::OSVersion.Version` — report build.
- **Log**: `agend-terminal start` runs in foreground and prints tracing to stderr — capture by redirecting: `.\agend-terminal.exe start 2>&1 | Tee-Object daemon-log.txt`.

## Session 2 update (2026-04-19) — directions A and B refuted, manifest fix landed, real cause narrowed to daemon glue

### What we ruled out

| Hypothesis | Test | Result |
|---|---|---|
| **A. Console inheritance** (nested ConPTY from PowerShell's conhost) | Branch `fix/windows-freeconsole`: `FreeConsole()` after ctrlc registration | Silent hang unchanged, `ctrlc` handler dead after `FreeConsole`. Branch deleted. |
| **Subsystem** (console vs GUI) | Launched daemon with true `DETACHED_PROCESS` (P/Invoke, 0 console attached, same state as GUI subsystem) | Silent hang unchanged at +91s → `health_state=hung`. |
| **Missing Windows app manifest** (compat shims on unmanifested apps) | Added `build.rs` + `assets/windows/agend-terminal.manifest` declaring Win10/11 supportedOS + UTF-8 codepage (matches WezTerm's `console.manifest`). Rebuilt + retested with DETACHED_PROCESS. | Silent hang unchanged at +125s. Manifest is still a correctness improvement — it's landed (this PR). |
| **Sideload version** (v1.14.2281 too old for 26200) | Removed sideload → kernel32 in-box ConPTY / conhost.exe. Retested. | Silent hang unchanged at +125s. |
| **portable-pty itself broken** | Built `examples/pty_smoke.rs` (minimal, 70 LOC, spawns cmd.exe + reads). Ran from mintty parent. | Got **20 bytes** of cmd.exe banner at +0.04s, then `reader.read` blocked. portable-pty is NOT broken — it can receive some output. |
| **Launch context matters** | Launched `agend-terminal start` from bash (mintty parent, not PowerShell/DETACHED). | Same 0-byte silent hang. Parent env is not the variable. |

### What we confirmed

**WezTerm still works on this 26200 box today.** Downloaded `WezTerm-windows-20240203-110809-5046fc22.zip`, extracted, launched `wezterm-gui.exe` — cmd.exe child stays alive, OpenConsole.exe host stays alive. The bundled `conpty.dll` + `OpenConsole.exe` inside that zip are **SHA256-identical** to the sideload next to `agend-terminal.exe`:

- `conpty.dll` SHA256 `2F09EAA55C60E11241CA21FFF19336529470D9B76A77BCB45DE78CFABDB50308`
- `OpenConsole.exe` SHA256 `6B0E73145462116B2ED3D422AC71E25C8554B1A52295D8CF55CF6025775276EE`

Same `portable-pty` 0.9.0 code (verified `pty/src/win/*.rs` byte-identical between WezTerm main and crates.io 0.9.0 — portable-pty is published from WezTerm's mono-repo so they're the same crate), same sideload, yet different outcome. **The bug is in agend-terminal glue**, not Microsoft / Windows / portable-pty / sideload version.

### Partial breakthrough: OpenConsole swap fixes the read path

Replacing `OpenConsole.exe` with Terminal stable 1.24.10921 or preview 1.25.923 (keeping the v1.14 `conpty.dll`) → daemon transitions out of `starting` in **under a second**, `agent_state=restarting` + `health_state=recovering` immediately. Output flows.

But cmd.exe exits within ~110ms of spawn, triggering auto-respawn loop. `conpty.dll` v1.14 + newer OpenConsole.exe is a protocol mismatch (Microsoft stopped shipping a sideloadable `conpty.dll` after ~2021). The matched newer pair is not publicly distributed.

**Not a shippable fix.** Does prove the reader-hang side is OpenConsole-side. But also proves WezTerm's v1.14 OpenConsole IS capable of delivering output on 26200 — since WezTerm itself works with that exact file — so something in our spawn path is poking OpenConsole wrong.

### Direction for next session — bisect `pty_smoke` → `agent::spawn_agent`

`examples/pty_smoke.rs` gets bytes out of cmd.exe on 26200. `src/agent.rs::spawn_agent` doesn't. The structural differences:

1. `spawn_agent` calls `take_writer()` on the master after `spawn_command` returns. `pty_smoke` doesn't.
2. `spawn_agent` moves `pair.master` into `Arc<Mutex<Box<dyn MasterPty + Send>>>` after cloning reader.
3. `spawn_agent`'s `CommandBuilder` may inherit/add env vars from the daemon context.
4. The reader runs in a spawned thread (`pty_read_loop`), not on the thread that called openpty.
5. The daemon holds the `.daemon.lock` file and other handles when spawn happens.
6. Other threads (API server, TUI server) are spawned — though AFTER the agent spawn, so they shouldn't race.

**Concrete next step**: extend `pty_smoke` incrementally — add `take_writer()`, move to Arc<Mutex<>>, move read into a spawned thread, inherit env, etc. — one change at a time until it breaks. The change that flips 20 bytes → 0 bytes is the bug.

### Manifest fix landed (this PR `fix/windows-manifest`)

`build.rs` embeds `assets/windows/agend-terminal.manifest` via `embed-resource`. Manifest declares Win10/11 `supportedOS` GUID and UTF-8 `activeCodePage`. It's correct cross-platform hygiene and matches what Windows Terminal / WezTerm do. It is **necessary but not sufficient** for fixing 26200 — keeps it in for when the next session nails the real cause.

### Diagnostic artifacts preserved in repo

- `examples/pty_smoke.rs` — run via `cargo build --release --example pty_smoke` then launch the `.exe` via PowerShell P/Invoke `DETACHED_PROCESS` to reproduce the 26200 environment.
- Launcher helper referenced in previous session: `%TEMP%\launch-detached.ps1` — standalone P/Invoke wrapper for `CreateProcessW` with `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`. Recreate from session log if lost.

## Session 3 update (2026-04-19) — partial results are environment-dependent, not glue-dependent

Attempted to bisect `examples/pty_smoke.rs` variants v1→v8 (each adds one structural piece of `spawn_agent`). Got inconsistent results that inverted the earlier session-2 conclusion: the problem isn't only in daemon glue.

Binaries tested (all under the same Win11 Insider Dev 26200 build):

| Binary | Launch context | Bytes read (cmd.exe banner is ~100 B) |
|---|---|---|
| `examples/pty_smoke_minimal.rs` | Bash (mintty parent) | 20 bytes then reader blocks |
| `examples/pty_smoke_minimal.rs` | PowerShell interactive | 20 bytes then reader blocks |
| `examples/pty_smoke_minimal.rs` | `DETACHED_PROCESS` | **0 bytes, process exits before it can even write a log file** |
| `pty_smoke v1_isolated` (inline-coded equivalent of minimal) | Bash | 20 bytes then blocks |
| `examples/pty_smoke.rs` `AGEND_SMOKE_MODE=v1` (variant-gated but v1 takes exact same code path) | Bash | **0 bytes, hard-timeout** |
| `examples/pty_smoke.rs` v2–v8 | Bash | 0 bytes for all |
| `agend-terminal start` (full daemon) | Bash / PowerShell / DETACHED | 0 bytes in every context |

### What this means

1. **26200's ConPTY is broken for every consumer**. Not just our daemon. Even a 50-line minimal `openpty → spawn cmd → read` reliably gets only ~20 bytes of banner then blocks forever. The first write from OpenConsole gets through; subsequent writes don't. WezTerm masks this because its async I/O tolerates slow/stuck reads and its GUI rendering doesn't panic at "only partial banner".
2. **`DETACHED_PROCESS` launch context makes it strictly worse.** The same minimal binary that gets 20 bytes from Bash/PowerShell gets zero bytes under `DETACHED_PROCESS`. So launch context IS a factor — just not the only one.
3. **Structurally-equivalent Rust binaries behave differently.** `pty_smoke_minimal.rs` (works, 20 bytes) and `pty_smoke.rs` with `AGEND_SMOKE_MODE=v1` (fails, 0 bytes) are supposed to execute the exact same code at runtime for v1. They don't. Likely explanations: `embed-resource`-pulled compile artifacts, Windows Defender real-time scanning with different code paths, or memory layout dependence in conpty.dll/OpenConsole's initial-handshake timing.
4. **The daemon is not uniquely broken.** Everything that touches ConPTY on 26200 is broken. That invalidates the session-2 "find the bad line in `agent::spawn_agent`" plan — there isn't a single bad line to find.

### New verdict: it's a Microsoft bug in 26200

Ship the manifest fix (this PR), stop trying to work around it in our code. Users on 26200 should either (a) switch to Windows stable channel or (b) install and use WezTerm as their terminal until Microsoft fixes it. Users on 22H2/23H2/24H2 GA builds are unaffected (CI `windows-latest` ≈ Server 2022 + Win11 22H2 stays green).

## Open questions for next session

1. Is the 26200 regression filed on Microsoft Feedback Hub / `microsoft/terminal` issues? File one if not — link attaches this repro log.
2. If next Insider build (26300+) fixes it, retest with the same `pty_smoke_minimal.exe` — if 20 bytes becomes 100 bytes, the bug is fixed upstream and no work needed here.
3. **Not worth doing unless a user asks**: build `conpty.dll` from `microsoft/terminal` source and sideload the matched pair; it's days of work for an Insider-only workaround whose value evaporates when Microsoft ships their fix.
