[繁體中文](FEATURE-configuration.zh-TW.md)

# Configuration Layers: `AGEND_HOME`, `fleet.yaml`, runtime config, and MCP config

This document explains how AgEnD's configuration is layered, who writes each layer, and where you should make a change depending on the type of setting.

## Usage Scenarios

> **Target audience:** Operators — used through CLI or TUI.

An operator is trying to understand where the current behavior comes from. `AGEND_HOME`, `.env`, `fleet.yaml`, and `runtime-config.json` each control a different layer, and this section helps explain which file should be edited for which kind of change.

When an instance needs a long-term policy change, the right place is usually `fleet.yaml`. When the change is only a live tuning adjustment, `runtime-config.json` is the better fit because it reloads every daemon tick.

If a backend's project-local MCP config looks wrong, the operator should treat it as a derived file rather than the source of truth. Re-running the generator is usually safer than hand-editing the backend-specific artifact.

The main rule is:

- **Keep one source of truth, and make derived files rebuildable.**

In practice that means:

- `fleet.yaml` is the primary human-editable fleet source.
- `runtime-config.json` holds live, runtime-tunable numbers.
- backend `mcp.json` or `settings.local.json` files are derived, not primary sources.
- service-manager artifacts are also derived files.

## Configuration layers at a glance

| Layer | Typical file / env | Written by | Used when |
|---|---|---|---|
| Process environment | `AGEND_HOME`, `AGEND_POINTER_ONLY_INJECT`, `AGEND_CAPTURE_FIXTURES` | operator / launcher | startup |
| Fleet source | `$AGEND_HOME/fleet.yaml` | operator + daemon | startup / reload |
| Runtime config | `$AGEND_HOME/runtime-config.json` | MCP `config` tool | each daemon tick |
| Derived MCP config | `.claude/settings.local.json`, `.kiro/settings/mcp.json`, `mcp-config.json` | daemon / generator | before backend launch |
| Service artifact | plist / unit / task XML | `service install` | OS login |
| Diagnostic output | `bugreport-*.txt`, `captures/*` | operator / capture tools | on demand |

## `AGEND_HOME`

`AGEND_HOME` is the root of almost everything.

### Resolution order

The code resolves the home directory in this order:

1. if the `AGEND_HOME` environment variable is set, use it
2. otherwise fall back to the user's home directory:
   - prefer `~/.agend`
   - keep `~/.agend-terminal` as a compatibility fallback

### Why it matters

Most persisted state hangs off this directory:

- `fleet.yaml`
- `runtime-config.json`
- `captures/`
- `service/`
- `skills/`
- `bin/`
- `protocol/`
- `workspace/`
- `worktrees/`

If you are moving a setup, backing it up, or trying to understand where state lives, start here.

## `.env`

The daemon loads environment variables from `AGEND_HOME/.env`.

### Supported formats

- `KEY=value`
- `export KEY=value`
- single-quoted and double-quoted values

### Notes

- `#` inside quoted values is preserved.
- Inline comments are stripped from unquoted values.
- `.env` is a supplement to startup-time environment, not the only configuration source.

### Good uses

- bot token variable names
- API key variable names
- local-only feature flags

### Bad uses

- large structured fleet definitions
- live tuning numbers that should be editable at runtime
- user-global CLI settings

## `fleet.yaml`

`fleet.yaml` is the main fleet definition file.

### Default path

```text
$AGEND_HOME/fleet.yaml
```

### What belongs here

This file describes each instance's source, role, and startup behavior. Common fields include:

- `backend`
- `working_directory`
- `role`
- `instructions`
- `source_repo`
- `repo`
- `github_login`
- `args`
- `model`
- `env`
- `ready_pattern`
- `command`
- `worktree`
- `skills`
- `topic_binding_mode`
- `topic_id`
- `id`

### Daemon-managed fields

The daemon treats the following fields as managed by the system:

- `id`
- `topic_id`
- `git_branch`
- `source_repo`

Meaning:

- the daemon will overwrite them
- operators should not treat them as permanent manual edits
- if operator and daemon values diverge, merge logic will favor the daemon value or surface a conflict

### Operator-hand-edited fields

Typical operator-controlled fields include:

- `backend`
- `working_directory`
- `role`
- `instructions`
- `repo`
- `github_login`
- `args`
- `model`
- `env`
- `ready_pattern`
- `command`
- `worktree`
- `skills`
- `topic_binding_mode`

### `None` vs `Some(empty)`

This distinction matters for fields like `args`, `env`, and `skills`.

- `None`: do not override the default
- `Some(empty)` or an empty collection: explicit opt-out

Examples:

- `args: null` → use the backend default argument list
- `args: []` → explicitly request an empty argument list
- `skills: null` → install every shared skill
- `skills: []` → install no skills at all

### `skills`

`skills` is a per-instance allowlist.

Semantics:

- `null`: install all shared skills
- `[]`: install no skills
- `["foo", "bar"]`: install only the named skills

This affects what is installed into each backend's skill directory under the agent's working directory.

### `topic_binding_mode`

This field controls whether a Telegram topic is created during spawn.

- `auto` / `null`: current default behavior
- `skip`: never create a topic
- `deferred`: do not create one at spawn, but allow later binding

### `repo` vs `source_repo`

These fields are related but not identical.

- `source_repo`: part of the binding-derived source identity
- `repo`: an `owner/name` override at the GitHub-repo level

If you are making a manual operator edit, avoid treating daemon-managed fields as the long-term source of truth.

### Merge behavior

Fleet merge is not a blind overwrite. Field classification matters.

- daemon-managed fields: daemon value overwrites operator value
- operator-hand-edit fields:
  - missing existing value → write daemon value
  - same value → no-op
  - different value → conflict
- fields the daemon does not provide: preserve the operator's existing value

That keeps daemon-owned bookkeeping from clobbering operator intent.

## `runtime-config.json`

This is the live runtime config.

### Default path

```text
$AGEND_HOME/runtime-config.json
```

### How it is used

The daemon reloads it every tick, which makes it suitable for live tuning.

### Tunable keys

| Key | Default | Meaning |
|---|---|---|
| `dev_idle_threshold_secs` | `3600` | Idle threshold for a single agent |
| `fleet_idle_threshold_secs` | `1800` | Idle threshold for the fleet as a whole |
| `hang_auto_recovery_enabled` | `false` | Whether hang auto-recovery is enabled |

### How to change it

Use the MCP `config` tool:

- `config get`
- `config set`
- `config list`

This is not usually a file you edit by hand.

### Failure behavior

- unknown key → error
- integer parse failure → error
- boolean parse failure → error
- JSON parse failure → fall back to defaults

That means a broken runtime config does not usually crash the daemon. It falls back to defaults.

### When to use it

- adjust watchdog thresholds
- temporarily raise fleet idle thresholds
- enable or disable hang auto-recovery shadow behavior

### What not to put here

- fleet structure
- agent identity
- backend paths
- long-term policy that should go through review

## `DaemonConfig`

`src/daemon_config.rs` contains process-wide runtime flags.

### Current field

- `pointer_only_inject`

### Source

- `AGEND_POINTER_ONLY_INJECT=1` turns it on
- otherwise the default is `false`

### Use case

This is appropriate for startup-time feature flags or experiment toggles that should not live on disk.

### Important note

- it is not persisted
- it is not automatically synchronized with `fleet.yaml`
- if you want a long-lived policy, use a real config file or the MCP config path

## MCP config: derived settings for each backend

`src/mcp_config.rs` generates backend-specific MCP configs.

### Scope rule

There is a hard rule here:

- writes must stay inside `$AGEND_HOME` or the project working directory
- do not mutate the user's global CLI config directories

In other words, do not write to `~/.claude`, `~/.codex`, `~/.gemini`, or similar personal settings directories.

### What gets generated

The generated MCP config includes:

- `AGEND_HOME`
- sometimes `AGEND_INSTANCE_NAME`
- the bridge binary path

### Common output paths

Depending on the backend, generated files commonly include:

- `.claude/settings.local.json`
- `mcp-config.json`
- `.kiro/settings/mcp.json`

Other backends have their own project-local configuration targets as well.

### Recovery and error handling

- malformed JSON is backed up to a `.corrupt.<timestamp>` file
- the generator then starts from an empty object
- unlike `runtime-config.json`, the MCP path prefers backup-first recovery when the existing file is broken

### Why this is a derived layer

The real source of truth is:

- the fleet instance definition
- `AGEND_HOME`
- the current binary path

The MCP config is just the backend-specific rendering of those inputs.

## Service artifacts are also derived

The service output is not a source file. It is rendered from the current binary and `AGEND_HOME`.

| Platform | Artifact |
|---|---|
| macOS | plist |
| Linux | systemd user unit |
| Windows | Task Scheduler XML |

If the binary path changes, re-run `service install` so the artifact is regenerated.

## What happens when a config layer breaks

### Broken `fleet.yaml`

- `doctor` reports a parse error
- the daemon may fail to start cleanly
- use `bugreport` first, then fix the file

### Broken `runtime-config.json`

- the daemon falls back to default values
- the problem may not be obvious immediately
- inspect logs or `doctor` output if the behavior looks wrong

### Broken MCP config

- the old file is backed up
- a new derived file is written
- if backend behavior is weird, verify whether a generated config was rewritten

### Broken service artifact

- `service status` may say `stopped`
- re-running `service install` is usually faster than hand-editing the artifact

## Recommended order of operations

### New machine or fresh install

1. Set `AGEND_HOME` or accept the default.
2. Prepare `.env`.
3. Edit `fleet.yaml`.
4. Run `service install`.
5. Run `doctor`.

### Adjusting idle / watchdog thresholds

1. Change the runtime values with `config set`.
2. Wait for the next daemon tick.
3. Verify with `doctor` or the event log.

### Changing an agent's behavior

1. Edit the instance fields in `fleet.yaml`.
2. Regenerate backend MCP settings if needed.
3. Reinstall the service if the binary path changed.

### Tracking a problem

1. Run `doctor`.
2. Run `bugreport`.
3. If the issue is backend interaction, consider `capture` next.

## Common misunderstandings

### Treating MCP config as the primary source

Wrong. It is derived and will be overwritten.

### Treating runtime config as fleet configuration

Wrong. It only controls live tunables.

### Expecting daemon-managed fields to survive manual edits forever

Wrong. Those values are overwritten by design.

### Treating empty collections and `null` as the same thing

Wrong. For many fields they mean opposite things.

## Source pointers

- `src/main.rs`: `AGEND_HOME`, CLI defaults, `Doctor` and `Service`
- `src/fleet.rs`: `FleetConfig`, `InstanceYamlEntry`, field merge rules
- `src/runtime_config.rs`: runtime-config read/write and tick reload
- `src/daemon_config.rs`: process-wide runtime flags
- `src/mcp_config.rs`: backend MCP config generation
- `src/store.rs`: file locking, atomic writes, corrupt-file backup behavior
- `src/service/*`: service artifact generation
- `src/bugreport.rs`: diagnostic report generation
- `src/capture.rs`: capture and promote output

## Practical advice

1. Put long-term policy in `fleet.yaml`, not in runtime config.
2. Put temporary tuning values in `runtime-config.json`, not in source code.
3. Keep the original source whenever a file is generated from it.
4. If you suspect config pollution, inspect `bugreport` first and then check whether a derived file was rewritten.
5. Remember the rule: **source files are editable, derived files are rebuildable.**