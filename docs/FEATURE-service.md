[繁體中文](FEATURE-service.zh-TW.md)

# Service Management: `agend-terminal service`

This document explains how the `service` subcommand hands daemon lifecycle management to the operating system, and where each supported platform writes its service artifact.

## Usage Scenarios

> **Target audience:** Operators — used through CLI or TUI.

An operator wants the daemon to start automatically when the machine logs in, instead of having to run `agend-terminal start` every time. `service install` turns the daemon into a managed OS service so the platform owns startup and restart behavior.

After upgrading the binary, the operator wants the service manager to point at the new executable path. Re-running `service install` regenerates the artifact with the current binary path and `AGEND_HOME`.

When the machine is being decommissioned or the service is no longer needed, `service uninstall` removes the registration cleanly and leaves the platform in a known state.

## What this feature solves

`agend-terminal` can run in the foreground, background, TUI, or daemon mode, but if you want it to start automatically after login and come back up after a crash, you need more than a manual command.

`service` is the entry point that says: "let the OS manage the lifecycle for me."

You can think of it as two steps:

1. Record the absolute path to the currently running `agend-terminal` binary in the host service manager.
2. Let the platform start it at login and attempt restarts when the daemon exits unexpectedly.

Important constraints:

- This is **user-level** setup. No root or admin is required.
- The daemon does not supervise itself. The OS is the supervisor.
- `install` and `uninstall` are idempotent. Re-running them should not damage an existing setup.

## Supported platforms

| Platform | Service manager | Artifact path | Registration command |
|---|---|---|---|
| macOS | `launchd` | `~/Library/LaunchAgents/com.agend-terminal.daemon.plist` | `launchctl load -w` |
| Linux | `systemd --user` | `~/.config/systemd/user/agend-terminal-daemon.service` | `systemctl --user enable --now` |
| Windows | Task Scheduler | `\AgendTerminalDaemon` | `schtasks /Create /XML` |

If you run this on a platform without the matching helper, `install` will return a platform-not-supported error.

## The three subcommands

```bash
agend-terminal service install
agend-terminal service uninstall
agend-terminal service status
```

### `install`

`install` does the following:

1. Resolves the absolute path to the currently running `agend-terminal` binary.
2. Renders the platform-specific template with that binary path and `AGEND_HOME`.
3. Writes the platform artifact.
4. Invokes the service manager to register and start the service.

If you run `install` again, it regenerates the template. That is useful after binary upgrades or when `AGEND_HOME` changes.

### `uninstall`

`uninstall` tries to:

1. Remove the platform registration.
2. Delete the artifact file.

If nothing was installed, it succeeds as a no-op.

### `status`

`status` only reports one of three values:

- `running`
- `stopped`
- `not_installed`

It queries the platform service manager, not daemon internals.

## macOS behavior

macOS uses a `launchd` user agent.

### Paths

- plist: `~/Library/LaunchAgents/com.agend-terminal.daemon.plist`
- label: `com.agend-terminal.daemon`

### Install flow

macOS install works like this:

1. Render the plist template.
2. Write the plist into `LaunchAgents`.
3. Run `launchctl unload -w` first.
4. Run `launchctl load -w` next.

That sequence keeps re-installations idempotent.

### Status flow

Status checks are:

- plist missing → `NotInstalled`
- `launchctl list <label>` succeeds and output contains a PID → `Running`
- plist exists but the agent is not loaded or not running → `Stopped`

### Notes

- `launchctl` failures do not magically mean the daemon is down. They only tell you the service manager call failed.
- If the plist exists but launchd is not managing it, status still reports `stopped`.
- Standard output and standard error are fixed to `/dev/null`; real logs go through the daemon tracing pipeline.

## Linux behavior

Linux uses `systemd --user`.

### Paths

- unit: `~/.config/systemd/user/agend-terminal-daemon.service`
- `XDG_CONFIG_HOME` is honored if set.

### Install flow

Linux install does this:

1. Render the unit template.
2. Write the unit file into the user-level systemd directory.
3. Run `systemctl --user daemon-reload`.
4. Run `systemctl --user enable --now agend-terminal-daemon.service`.

### Status flow

Status checks are:

- unit file missing → `NotInstalled`
- `systemctl --user is-active` succeeds → `Running`
- unit exists but is not active → `Stopped`

### Notes

- In CI or sessions without a user systemd bus, `enable --now` can fail even though the file was written successfully.
- That is treated as a warning, not as a hard failure for the file-rendering part.
- No sudo is required because this is all user-level.

## Windows behavior

Windows uses Task Scheduler.

### Paths

- task name: `\AgendTerminalDaemon`
- XML cache: `$AGEND_HOME/service/scheduler.task.xml`

### Install flow

Windows install works like this:

1. Render the XML template.
2. Encode the XML as UTF-16 LE with a BOM.
3. Run `schtasks /Create /XML <path> /F`.

The XML is entity-escaped before writing so characters like `&`, `<`, `>`, `"`, and `'` do not corrupt the template.

### Status flow

Status checks are:

- XML cache missing → `NotInstalled`
- `schtasks /Query /TN \AgendTerminalDaemon /FO LIST` succeeds and contains `Running` → `Running`
- XML exists but the task is not active → `Stopped`

### Notes

- `schtasks` failures can still leave the XML file behind, which is useful for debugging the rendered content.
- The task name is fixed. It does not vary per instance.
- No admin rights are required as long as the current user can create scheduled tasks.

## Idempotency rules

| Operation | Existing state | Missing state |
|---|---|---|
| `install` | Re-renders and re-registers | Installs normally |
| `uninstall` | Removes registration and artifact | Succeeds as no-op |
| `status` | Reports `running` or `stopped` | Reports `not_installed` |

Idempotent here means the action can be repeated safely, not that every underlying platform command is completely silent. `launchctl`, `systemctl`, and `schtasks` may still print warnings while the operation remains functionally repeatable.

## How this relates to daemon lifecycle

`service` only wires the daemon into the OS supervisor. It does not run the daemon logic itself.

In practice:

- `service install` says: "OS, please start this binary for me."
- `agend-terminal start` actually runs the daemon.
- `service status` asks whether the OS still has the service registration.

If you upgrade the binary, re-run `service install` so the service manager gets the new absolute path and current `AGEND_HOME`.

## Common workflows

### First install

```bash
agend-terminal service install
agend-terminal service status
```

Check that:

- the artifact was written
- the service manager accepted the registration
- status is `running` or `stopped`, not `not_installed`

### Reinstall after a binary update

```bash
cargo build --release
agend-terminal service install
```

This refreshes the binary path embedded in the artifact.

### Uninstall

```bash
agend-terminal service uninstall
agend-terminal service status
```

If the uninstall worked, status should move to `not_installed`.

## Troubleshooting

### `this platform is not supported`

You are on a target that does not have a matching service manager implementation. Make sure you are on macOS, Linux, or Windows.

### `status` returns `stopped`

The OS still knows about the service artifact, but the daemon is not currently active. Check:

- whether the binary still exists
- whether `AGEND_HOME` is correct
- the service manager logs
- whether the daemon exits immediately after startup

### Windows XML writes, but the task does not appear

Common causes:

- `schtasks` is unavailable
- the current user lacks permission to create scheduled tasks
- the XML was not escaped correctly

### Linux unit writes, but nothing starts

Common causes:

- no systemd user session bus
- `systemctl --user` is not usable in the current environment
- the unit points to a missing binary

### macOS plist exists, but launchd does not load it

Common causes:

- `launchctl load -w` failed
- the plist path moved
- the binary changed and the plist still points to the old path; rerun `install`

## Relationship to other settings

`service` only manages the OS supervisor integration.

It does **not** manage:

- fleet.yaml agent configuration
- runtime-config.json thresholds
- MCP JSON backend settings
- bugreport or capture output

Those are different layers. If daemon behavior looks wrong after changing fleet or runtime config, run `doctor`, then check `service status`, and only then consider reinstalling the service.

## Source pointers

- `src/main.rs`: CLI text and `Commands::Service`
- `src/service/mod.rs`: cross-platform helper logic
- `src/service/macos.rs`: launchd implementation
- `src/service/linux.rs`: systemd user implementation
- `src/service/windows.rs`: Task Scheduler implementation
- `src/daemon/restart.rs`: supervisor detection and restart semantics

## Practical advice

1. Run `status` immediately after installation.
2. Re-run `install` after binary upgrades instead of assuming the artifact will update itself.
3. In CI or test environments, it is often more useful to verify the generated files than to force the OS service to start.
4. If `status` is not `not_installed` but the daemon still looks dead, check the platform manager logs first.
5. This feature is an external supervisor entry point, not a self-healing mechanism.