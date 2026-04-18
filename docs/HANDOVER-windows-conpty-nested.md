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

## Open questions for next session

1. Does `FreeConsole()` before spawn make the child's output flow? (direction A verification)
2. If yes, does `ctrlc` handler survive `FreeConsole`? If not, how does `SetConsoleCtrlHandler` registered pre-FreeConsole behave?
3. Is there a Windows version-specific code path in WezTerm's vendored portable-pty that upstream `portable-pty` 0.9 is missing?
4. Should agend-terminal ship `conpty.dll` + `OpenConsole.exe` in release artifacts even though sideload didn't help 26200? (It may still help future builds with different regressions.)
