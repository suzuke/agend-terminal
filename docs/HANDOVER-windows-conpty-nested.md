# Handover — Windows ConPTY nested spawn silent hang

> **Status: SHIPPED** — implementation landed on `main` (Session 4, 2026-04-19). Doc retained for historical/provenance.

**Status**: **resolved (Session 4, 2026-04-19)**. The symptom was real, but the root causes were agend-side, not an OS/ConPTY regression. Two fixes landed on `main`:
- `src/vterm.rs` — replace `NoopListener` with `PtyWriteListener` so the DSR-CPR `\x1b[6n` reply that ConPTY blocks on actually reaches the child.
- `src/daemon.rs` — call `SetConsoleCtrlHandler(None, 0)` before `ctrlc::set_handler` so the daemon doesn't inherit an ignore-CTRL+C flag.

**If a future session opens this file, skip straight to "Session 4 correction" (line ~180).** Sessions 1–3 above it document the diagnostic journey (including a wrong "it's Microsoft's bug" verdict at the end of Session 3) and are preserved only so past mistakes aren't repeated — **do not act on their `unresolved` framing**.

**Environment where it reproduced before the fixes**: Windows 11 Insider Dev, build **10.0.26200** (and, per Session 4, also 23H2 after rollback — CPR was the dominant cause on both).
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

### External bug reports on 26200 — not the same bug (updated 2026-04-20)

> **Earlier drafts of this section claimed these three projects "corroborated" a common 26200 ConPTY OS regression. That was wrong.** After the Session 4 CPR fix landed and `agend-terminal app` worked cleanly on 23H2, a careful reread of the three upstream issues shows each one is a *different* problem. They are kept here only so future diagnosis doesn't mistake them for additional evidence against agend.

| Project | Actual symptom | Relation to agend |
|---|---|---|
| [pinokiocomputer/pinokio#1017](https://github.com/pinokiocomputer/pinokio/issues/1017) | `C:\Windows\System32\conpty.dll` **is missing** on certain 26200 boxes; node-pty falls back to the deprecated winpty path and fails at `connect ENOENT \\.\pipe\winpty-conout-...`. | Independent 26200 OS-side packaging regression. agend uses `portable-pty` which talks to `kernelbase.dll!CreatePseudoConsole` (conhost.exe backend), not a sideloaded `conpty.dll`, so this never affected us. |
| [openai/codex#13973](https://github.com/openai/codex/issues/13973) | MSVC runtime assertion `remove_pty_baton(baton->id)` at `node-pty/src/win/conpty.cc:106`. node-pty-specific refcount/lifecycle bug. | Unrelated to portable-pty. Could have been triggered more often on 26200 because of timing differences, but the root cause is in node-pty's C++ baton tracking — agend's Rust path doesn't have equivalent bookkeeping. |
| [google-gemini/gemini-cli#12019](https://github.com/google-gemini/gemini-cli/issues/12019) / [#12060](https://github.com/google-gemini/gemini-cli/issues/12060) | "Cannot resize a pty that has already exited" — PTY exits mid-command on 26200.6901. | Plausibly the same *class* of bug as agend's CPR gap (missing reply → child dies at a weird time), but gemini-cli runs node-pty + its own terminal emulator and would need its own fix upstream. Not evidence of any remaining agend-side issue. |

Common factor was just "all three reporters happened to be on 26200", not "26200 breaks ConPTY the same way for everyone". Microsoft's [official 25H2 known-issues page](https://learn.microsoft.com/en-us/windows/release-health/status-windows-11-25h2) as of 2026-04-17 still lists only a Microsoft-account sign-in bug and a WUSA path bug — no acknowledged ConPTY regression. We have **no evidence of any residual agend-side issue on 26200** after the Session 4 fixes.

Rollback path (tested 2026-04-19): user's box had `C:\Windows.old` within the 10-day rollback window → Settings → System → Recovery → **Go back** rolls 25H2 → 24H2 (build 26100) while preserving files and apps. This was done before the CPR fix was identified; with the fix, rollback is no longer required for agend to work on 26200.

## Session 4 correction (2026-04-19) — the *real* root causes were ours

After the user rolled back from 25H2 → 23H2 (build 22631.4602), `agend-terminal app` still showed an empty pane and Ctrl+C still hung the daemon. That forced the diagnosis to continue past the Session-3 "it's Microsoft's bug" verdict, and two separate agend-side bugs surfaced:

### Bug 1: `\x1b[6n` DSR-CPR query silently dropped — pane stays black

Windows ConPTY's `conhost.exe --headless` emits a cursor-position query (`ESC [ 6 n`) to the master at startup and **blocks the child process until it gets a reply**. `alacritty_terminal` (our vterm) detects the query and emits `Event::PtyWrite("\x1b[1;1R")`, but the previous `NoopListener` dropped every event. No reply ever reached the PTY writer → cmd.exe / PowerShell / any shell never got past the pre-banner handshake → the pane stayed empty forever.

Fix: replace `NoopListener` with `PtyWriteListener` (`src/vterm.rs`) that holds an `Arc<Mutex<Box<dyn Write + Send>>>` clone of the agent's PTY writer and forwards every `Event::PtyWrite` back to the pty. macOS/Linux kernel PTY never sends CPR on startup, so the bug was Windows-only.

Diagnostic left in the tree: `AGEND_DEBUG_PTY_READ=1` env var in `pty_read_loop` dumps every read (byte count + first 64 bytes, printable+hex). That's how the 4-byte `\x1b[6n` was isolated — without it the reader just looked silent.

### Bug 2: Daemon inherits Windows "ignore CTRL+C" flag — Ctrl+C doesn't fire handler

`SetConsoleCtrlHandler(NULL, TRUE)` is a **per-process, inheritable** flag that skips the entire handler chain for CTRL_C_EVENT while leaving CTRL_BREAK_EVENT unaffected. Something in the daemon's init (either inherited from the parent shell or set by a dependency) had that flag on, so `ctrlc::set_handler` installed its routine but Windows never called it when CTRL_C arrived. Users saw "no response" from Ctrl+C; `agend-terminal stop` (API-based, not signal-based) still worked — which is why this wasn't caught earlier.

How the bug was isolated: `scripts/test_ctrlc.py` sends CTRL_BREAK_EVENT via `CREATE_NEW_PROCESS_GROUP` + `os.kill(pid, CTRL_BREAK_EVENT)` → clean 1.14s shutdown. `scripts/test_ctrlc_v2.py` sends real CTRL_C_EVENT via `CREATE_NEW_CONSOLE` + `AttachConsole` + `GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)` → handler never fires, daemon hangs until force-killed. Both signals go through the same `ctrlc` handler routine, so only the flag explains the asymmetry.

Fix: `src/daemon.rs` explicitly calls `SetConsoleCtrlHandler(None, 0)` (Add=FALSE with null handler re-enables Ctrl+C) before `ctrlc::set_handler`. Requires the `Win32_System_Console` feature on `windows-sys`.

Diagnostic left in the tree: `AGEND_CTRLC_SENTINEL=<path>` env var writes a timestamp file the moment the ctrlc handler fires. Lets future diagnostics prove handler delivery without needing to capture a hidden-console stdout stream.

### What this means for the 25H2/26200 story

The Session-3 verdict ("everything that touches ConPTY on 26200 is broken") was overstated, and the "external corroboration" that supported it was a misread (see the section above). For agend, the CPR-never-replied bug was the entire story: it was the dominant symptom on 26200, it was *still* the symptom on 23H2 after rollback, and fixing vterm's event listener fixed both. We have no remaining evidence of a 26200-specific problem affecting this repo.

### Diagnostic scripts committed under `scripts/`

- `tui_dump.py` — connect to an agent's TUI socket, print handshake version + every framed read. Use to observe whether the PTY reader is actually getting bytes (as opposed to vterm faking an empty screen in its dump).
- `tui_send.py` — same protocol in the write direction; useful for injecting raw bytes (e.g., `\x03` ETX) into an agent without going through `inject_to_agent`'s prefix/submit-key wrapping.
- `test_ctrlc_v2.py` — the AttachConsole/GenerateConsoleCtrlEvent harness that isolated Bug 2.

## Open questions for next session

1. `agend-terminal app` default shell is still `cmd.exe`; consider changing Windows default to PowerShell for a less spartan first-run experience.
2. `AGEND_DEBUG_PTY_READ` and `AGEND_CTRLC_SENTINEL` env vars are kept for future Windows diagnostics. Strip them only if the diagnostic noise bothers a reviewer — they cost nothing when unset.
3. If a user later reports a new Windows-only symptom on 26200/next-Insider, start by reproducing with `AGEND_DEBUG_PTY_READ=1` — don't assume "OS regression" until the PTY byte stream proves it. The Session 3 misdiagnosis cost a rollback that turned out not to be needed.
