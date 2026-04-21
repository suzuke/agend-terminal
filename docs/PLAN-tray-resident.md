# Plan: tray-resident (menu-bar icon + autostart)

Status: not started (2026-04-21).
Prereq: `docs/PLAN-daemon-resident.md` §3.4 — Attached app opens one
`PaneSource::Remote` per agent. That is what makes tray "Open App" work
against a live daemon.

## Why this exists

Today there is no always-visible surface. The daemon runs headlessly after
`start --detached`; users forget it exists and have to re-run each login.
Target: Ollama-style menu-bar icon with status at a glance, one-click
"Open App", a login-toggle, and "Quit" that stops the daemon. Closing the
TUI never touches the daemon; Telegram polling keeps running because it
lives in the daemon process.

## Functional scope

**In:**
- **A. Tray UI.** Menu-bar / system-tray icon on macOS + Linux + Windows
  with status label, Open App, Launch-at-login toggle, Quit.
- **B. Autostart.** Per-platform persistence of the Launch-at-login toggle.
  Exposed only through the tray menu — no CLI `service install` command.
  A user who can `cargo install` can also write a plist/desktop/registry
  entry by hand, so a CLI wrapper earns nothing; the toggle exists because
  a tray menu checkbox is the UX unlock.

**Out:**
- **C. Native packaging + code signing.** No `.app` bundle, no Developer ID,
  no Authenticode, no `.dmg` / `.msi` / notarization. Distribution stays at
  `cargo install`, a Homebrew formula (not cask — CLI install, no signing
  needed), and existing GitHub Release tarballs. Linux AppImage is
  release-pipeline-only and tracked separately.
- Tauri GUI frontend — `docs/PLAN-gui-frontend.md`.
- Multi-machine / cross-host — deferred in daemon-resident plan.
- In-tray preferences UI — v1 edits `$AGEND_HOME/tray.toml` in `$EDITOR`.

## Layout

```
src/tray/                  # gated on feature = "tray"
├── mod.rs                 # TrayApp: event loop, menu wiring, daemon lifecycle
├── config.rs              # tray.toml load/save
├── icon.rs                # embedded PNG assets, platform-correct sizing
├── autostart/
│   ├── mod.rs             # trait Autostart { enable/disable/is_enabled }
│   ├── macos.rs           # LaunchAgent plist + launchctl bootstrap/bootout
│   ├── linux.rs           # ~/.config/autostart/agend-terminal.desktop
│   └── windows.rs         # HKCU\...\Run value via windows-sys
└── terminal/
    ├── mod.rs             # trait OpenInTerminal { open(cmd) }
    ├── macos.rs           # open -na Terminal / iTerm / Ghostty
    ├── linux.rs           # $TERMINAL → x-terminal-emulator fallback chain
    └── windows.rs         # wt.exe with conhost fallback

assets/tray/               # PNG icons bundled via include_bytes!
```

## Cargo feature

```toml
[features]
default = []
tray = ["dep:tray-icon", "dep:tao"]
```

`tray-icon` loads PNG directly via `Icon::from_rgba` — no `image` crate
needed. The existing `windows-sys` entry in `Cargo.toml` already lists
`Win32_Foundation`; add `Win32_System_Registry` to that same entry
rather than a second declaration.

Release binaries ship `--features tray`. `cargo install agend-terminal`
users opt in with `--features tray`. Lean default keeps contributor
`cargo build` fast.

## CLI surface

One new subcommand: `agend-terminal tray` (foreground event loop). No
other CLI additions.

## tray.toml (MVP schema)

Path: `$AGEND_HOME/tray.toml`. Optional. Missing → defaults. Malformed →
warn and use defaults (never crash the tray on parse).

```toml
terminal = "default"
# "default" | "Terminal" | "iTerm" | "Ghostty" | "Alacritty"
#           | "wt" | "conhost"
#           | "gnome-terminal" | "konsole" | "xterm"
# "default" auto-detects per platform. Any other value is invoked as an
# executable name.
```

Status refresh interval, agent-list display, icon variants are all
hardcoded in v1.

## Runtime flow

```
agend-terminal tray
  │
  ├─ load $AGEND_HOME/tray.toml                             (non-fatal)
  ├─ api::call(LIST) → probe daemon
  │   ├─ success → adopt running daemon
  │   └─ failure → bootstrap::daemon_spawn::spawn_detached  (with wait for readiness)
  │
  ├─ tray_icon::TrayIconBuilder::new()
  │     .with_icon(...)
  │     .with_menu(build_menu())
  │     .build()
  │
  └─ tao::event_loop::EventLoop::run(...)
        ├─ every 2s → api::call(LIST), rebuild status label
        └─ menu events:
              • Open App       → OpenInTerminal::open(["agend-terminal", "app"])
              • Launch toggle  → Autostart::{enable,disable}()
              • Quit           → api::call(SHUTDOWN); exit(0)
```

Constraints:
- `tray-icon` requires the main thread on macOS. Status polling runs on a
  worker thread and sends menu-label updates through a channel drained
  inside the event loop.
- macOS: `tao::platform::macos::ActivationPolicy::Accessory` — menu-bar
  only, no Dock, no Cmd-Tab. Matches Ollama.

## Autostart per platform

All three derive `absolute_path_of_current_exe` via
`std::env::current_exe()?.canonicalize()?`. Whatever install channel
the user is on (`~/.cargo/bin`, `/opt/homebrew/bin`, future `.app`
bundle), the autostart entry points at that binary. `cargo install
--force` upgrade is picked up on next login with zero ceremony.

### macOS

Write `~/Library/LaunchAgents/io.github.suzuke.agend-terminal.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>Label</key>             <string>io.github.suzuke.agend-terminal</string>
  <key>ProgramArguments</key>
  <array>
    <string>{{ absolute_path_of_current_exe }}</string>
    <string>tray</string>
  </array>
  <key>RunAtLoad</key>         <true/>
  <!-- KeepAlive: non-zero exit respawns (crash recovery); exit(0) stays down (Quit button) -->
  <key>KeepAlive</key>         <dict><key>SuccessfulExit</key><false/></dict>
  <!-- Without Interactive, launchd can drop the tray to Background QoS and menu events feel laggy -->
  <key>ProcessType</key>       <string>Interactive</string>
  <key>StandardOutPath</key>   <string>{{ $AGEND_HOME/tray.log }}</string>
  <key>StandardErrorPath</key> <string>{{ $AGEND_HOME/tray.log }}</string>
  <key>EnvironmentVariables</key>
  <dict><key>AGEND_HOME</key>  <string>{{ $AGEND_HOME }}</string></dict>
</dict></plist>
```

Enable: `launchctl bootstrap gui/$(id -u) <plist>` (modern; `load` is
deprecated).
Disable: `launchctl bootout gui/$(id -u)/io.github.suzuke.agend-terminal`.

`io.github.suzuke.agend-terminal` is a net-new identifier — no existing
bundle ID or label in the repo. The `io.github.*` form is the
open-source convention when the project does not own a dedicated
domain. A future signed `.app` bundle (Phase 2) must reuse this exact
string as its `CFBundleIdentifier` so Login Items entries don't split
and the old LaunchAgent plist gets reclaimed on upgrade.

### Linux

Write a standard XDG autostart `.desktop` at
`~/.config/autostart/agend-terminal.desktop` with
`Exec={absolute_exe} tray`, `Type=Application`, `Terminal=false`,
`Icon=agend-terminal`, `X-GNOME-Autostart-enabled=true`. Enable = write;
disable = delete. XDG autostart works across GNOME, KDE, XFCE, Cinnamon,
MATE, LXQt — no systemd user service needed.

### Windows

Registry path `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value
`AgendTerminal` (REG_SZ) = `"{absolute_exe}" tray`. Enable = set; disable
= delete. `HKCU` needs no admin. Same mechanism Ollama uses on Windows.

## OpenInTerminal per platform

### macOS

| `terminal` | Command |
|---|---|
| `"default"` / `"Terminal"` | `open -na Terminal --args agend-terminal app` |
| `"iTerm"` | osascript creates a new iTerm2 window and runs the command |
| `"Ghostty"` | `open -na Ghostty --args -e 'agend-terminal app'` |
| other path | `open -na <path> --args agend-terminal app` |

Terminal.app ships with every macOS, so `"default"` is always safe.

### Linux

Resolve order:
1. `tray.toml` `terminal` if not `"default"`
2. `$TERMINAL` env
3. `x-terminal-emulator` (Debian alternatives)
4. First in PATH: `gnome-terminal` / `konsole` / `xfce4-terminal` / `kitty`
   / `alacritty` / `xterm`

Invocation shape varies per terminal (`gnome-terminal -- cmd` vs
`konsole -e cmd` vs `xterm -e cmd`) — encoded in a small table keyed by
binary name.

### Windows

| `terminal` | Command |
|---|---|
| `"default"` / `"wt"` | `wt.exe` invoked with `agend-terminal app` as the starting commandline |
| `"conhost"` | `cmd /c start "agend-terminal" agend-terminal.exe app` |
| other | treated as executable, invoked with `app` as arg |

The exact `wt.exe` flag form differs between cold-start and
already-running sessions; prototype before pinning the command string.

## Quit semantics

Menu "Quit (stops daemon)":

```
api::call(SHUTDOWN)     # best-effort; ignore errors
event_loop.exit()       # drop tray_icon, release menu-bar slot
process::exit(0)        # explicit zero so KeepAlive does not respawn
```

Tray crash or OS-level kill → non-zero exit → KeepAlive respawns.

## Interaction with existing modules

Tray never calls `bootstrap::prepare` and never owns the daemon flock.
It spawns via `bootstrap::daemon_spawn::spawn_detached` and otherwise
speaks only through `api::call`. App launched from "Open App" goes
through the shipped Attached path.

## Acceptance criteria

- `cargo check --all-targets` (no tray feature) unchanged: green, no new
  deps, no new warnings.
- `cargo check --all-targets --features tray` green on macOS, Linux,
  Windows CI runners.
- `cargo clippy --all-targets --features tray -- -D warnings` green on
  all three.
- Manual smoke per platform:
  1. `agend-terminal tray` → menu-bar icon appears. No Dock icon
     (macOS).
  2. Status label updates within 2s of `agend-terminal inject shell
     'hi'`.
  3. "Open App" opens the configured terminal running `agend-terminal
     app` in Attached mode (tab per agent).
  4. Launch-at-login toggle on → autostart file/key present; off → gone.
  5. "Quit" → `api::call(LIST)` fails afterward (daemon is down), tray
     icon gone, process exits 0.
  6. Kill `agend-terminal tray` with `kill -9` → after launchd respawn
     delay the tray returns (with autostart enabled).
- `scripts/e2e/tray-lifecycle.sh` (Unix only for v1) runs headless: daemon
  probe/spawn/adopt, autostart enable/disable, shutdown.
- Closing the TUI launched from "Open App" does NOT kill the daemon
  (regression guard).

## Known limitations

- **GNOME ≥ 3.26**: no tray without AppIndicator extension. README links
  install instructions. Not a code bug.
- **Unsigned binary**: first launch on macOS / Windows shows Gatekeeper /
  SmartScreen warning. README documents the bypass (right-click → Open on
  macOS; "Run anyway" on Windows). Phase 2 territory.
- **Login Items UI on macOS**: the binary appears under "Allow in the
  Background" by its executable path, not as "Agend Terminal". Cosmetic.
  Phase 2 fixes it with SMAppService + signed `.app`.
- **No in-tray preferences UI**: edit `$AGEND_HOME/tray.toml` and restart
  the tray.

## Distribution

### Now (this arc)

- `cargo install agend-terminal --features tray`
- Homebrew formula (not cask) at `suzuke/homebrew-tap`. Pulls the
  existing release tarball; Homebrew strips the macOS quarantine → no
  Gatekeeper dialog.
- Existing GitHub Release tarballs (5 targets via `release.yml`). First
  run: Gatekeeper / SmartScreen warning.

### Additive (cheap follow-ons)

- Linux AppImage in `release.yml` — one extra job, no signing, no code
  changes.

### Out of scope for this arc

- macOS: `.app` bundle + Developer ID + notarization + `.dmg` + Homebrew
  cask.
- Windows: `.msi` via WiX + Authenticode + winget manifest.

Both are release-engineering tasks, not code. Enabling them later does
not require refactoring anything in this plan.
