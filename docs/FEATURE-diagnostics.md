# Diagnostics and Evidence: `agend-terminal doctor / bugreport / capture`

This document groups the operator-facing tools that help you inspect the system before you change it.

## Usage Scenarios

> **Target audience:** Operators — used through CLI or TUI.

When an agent starts behaving oddly, the operator can run `agend-terminal doctor` to check whether the problem is in `fleet.yaml`, the backend binaries, stale helper files, or a dead agent process. The goal is to narrow the blast radius before changing anything.

If Telegram topics have drifted away from the registry, `doctor topics` shows which entries are live and which are orphaned. That gives the operator a safe way to decide whether cleanup is appropriate before touching chat state.

When the problem needs to be handed off, `bugreport` collects the runtime snapshot, logs, and redacted config into one file. That makes it much easier to ask another person for help without reassembling the environment by hand.

The shared rule is simple:

- default to read-only
- when state changes are allowed, show the operator the output first
- fail loudly or warn loudly instead of silently "fixing" things

## Feature overview

| Command | Purpose | Changes state by default? |
|---|---|---|
| `doctor` | Global health check | No |
| `doctor topics` | Telegram topic diagnostic | No; only `--cleanup` mutates |
| `bugreport` | Produce a report bundle for issue filing | No |
| `capture backend` | Capture raw backend PTY output | Yes, it writes a capture file |
| `capture promote` | Turn a capture into a fixture | Yes, it writes fixture and manifest files |

These commands map to different questions:

- `doctor`: is this machine healthy right now?
- `doctor topics`: are Telegram topics consistent with the registry?
- `bugreport`: can I hand this state to someone else?
- `capture`: do I want raw replayable output?

## `doctor`: global health check

```bash
agend-terminal doctor
```

### What it checks

`doctor` prints the following checks in order:

1. whether `AGEND_HOME` exists
2. whether `.env` exists
3. whether `fleet.yaml` exists, parses correctly, and contains instances
4. whether each instance can be probed through the runtime helper
5. the thread census
6. whether backend binaries are present in `PATH`
7. helper staleness under `$AGEND_HOME/bin`

### How to read the output

`doctor` is not trying to say "everything is perfect". It is trying to tell you which layer is broken.

Common signals:

- `✗ (not found)`: file or directory missing
- `✗ (parse error: ...)`: config is broken
- `✗ (port stale)`: the agent name exists but the PTY or IPC endpoint looks dead
- `✓ (port responsive)`: daemon sees the agent and the probe succeeded
- `patterns may need update`: backend version and calibrated version do not match

### What `doctor` does not do

It does not:

- auto-repair fleet.yaml
- auto-update backend binaries
- restart the daemon
- auto-rebuild helper binaries

That is intentional. `doctor` observes; it does not heal.

## `doctor topics`: Telegram topic diagnostic

```bash
agend-terminal doctor topics
agend-terminal doctor topics --cleanup
agend-terminal doctor topics --cleanup --yes
agend-terminal doctor topics --format json
```

### What it is for

This command checks whether the Telegram topic registry still matches the live chat state.

The report classifies topics into at least two buckets:

- `live`: registry and live Telegram state still agree
- `orphan`: a registry entry or chat topic has drifted and needs cleanup

### `--cleanup`

`--cleanup` is not a cosmetic option. It performs actual repair work:

1. print the diagnostic report first
2. ask for confirmation unless `--yes` is set
3. synchronize registry state and chat-side deletion when allowed

The cleanup covers both sides of the mismatch:

- registry updates
- chat-side delete operations

### Permission check

`doctor topics` probes whether the bot has `can_manage_topics`.

That affects the cleanup outcome:

- permission present → chat-side deletion is allowed
- permission missing → the operation is skipped and reported as a warning

If the probe fails, the command stays conservative and avoids destructive guessing.

### Human vs JSON output

`--format human`

- multi-line text table
- good for direct terminal use

`--format json`

- machine-readable output
- good for piping into scripts or other tooling

### Cleanup reporting

After cleanup, the CLI prints a per-topic result line such as:

- `deleted topic ... — chat + registry`
- `skipped ... — bot lacks can_manage_topics`
- `skipped ... — API error: ...`

That makes it obvious whether the fix succeeded or was skipped for permission/API reasons.

## `bugreport`: one-shot diagnostic bundle

```bash
agend-terminal bugreport
```

### Output location

The report is written to:

- the current working directory, if available
- otherwise `AGEND_HOME`

The filename looks like this:

```text
bugreport-YYYYMMDD-HHMMSS.txt
```

### What it includes

`bugreport` is designed to be attached to an issue or sent to another operator. It gathers:

1. version information
2. `AGEND_HOME`
3. fleet config, with secrets redacted
4. `schedules.json`
5. daemon status
6. the latest snapshot
7. the last 50 lines of the event log
8. installed backends
9. active sockets
10. `.env`, with secrets redacted

### Why some fields are redacted

The report avoids leaking sensitive values such as:

- tokens
- keys
- secrets
- passwords
- bearer tokens
- authorization data
- credentials
- `group_id`

That keeps the report shareable without exposing private material.

### `bugreport` vs `doctor`

- `doctor` is an instant health summary.
- `bugreport` is a shareable snapshot.

If you plan to hand the problem off to someone else, start with `bugreport`, then add the `doctor` output if needed.

## `capture backend`: capture raw backend output

```bash
agend-terminal capture backend --backend claude --seconds 15
```

### Purpose

This command preserves raw PTY output so you can:

- build fixtures
- replay a session later
- compare real backend behavior across providers

### Behavior

`capture backend`:

1. spawns an agent for the selected backend
2. reads PTY bytes for the requested duration
3. writes them to a capture file
4. writes a `.meta.json` sidecar on drop

### Capture layout

By default the output lands at:

```text
$AGEND_HOME/captures/<agent>/<epoch_ms>.cap
```

The companion sidecar is:

```text
$AGEND_HOME/captures/<agent>/<epoch_ms>.cap.meta.json
```

### When to use it

Use this when you want to:

- reproduce backend prompts and responses
- build the replay corpus
- compare the real terminal stream from different backends

### Important limitations

- if `AGEND_CAPTURE_FIXTURES` is not set, the capture writer is a no-op
- this feature stores raw PTY bytes, not prettified summaries
- capture is an observation tool, not a replay engine by itself

## `capture promote`: turn a capture into a replay fixture

```bash
agend-terminal capture promote \
  $AGEND_HOME/captures/myagent/1234567890.cap \
  sample-scenario \
  --scenario-kind silent_stuck
```

### What it does

`promote` takes a `.cap` file and turns it into canonical fixture data:

1. read the `.cap.meta.json` sidecar
2. copy the capture into `tests/fixtures/state-replay/<scenario>.raw`
3. append a manifest entry to `tests/fixtures/state-replay/MANIFEST.yaml`
4. optionally run an `auto_replay` warning check

### `scenario_kind`

The current allowed values are:

- `productive_marker_fire`
- `productive_silence`
- `silent_stuck`
- `hung`
- `real_capture`

This field is part of the manifest schema, not free-form text.

### `auto_replay`

If you pass `--auto-replay`, the CLI compares the implied hung / not-hung classification with the one the scenario kind suggests.

Important: a mismatch only warns. It does not roll the promotion back.

### `expected_hung`

This option is used for cross-checking:

- `silent_stuck` and `hung` should generally map to `hung`
- `productive_*` should generally map to `not_hung`
- `real_capture` skips the check

## File and data source map

| Command | Main read path | Main write path |
|---|---|---|
| `doctor` | `fleet.yaml`, `$AGEND_HOME/bin`, runtime probes | none |
| `doctor topics` | Telegram channel / registry state | optional registry + chat cleanup |
| `bugreport` | `fleet.yaml`, `schedules.json`, snapshot, event log, `.env` | `bugreport-*.txt` |
| `capture backend` | backend PTY | `$AGEND_HOME/captures/.../*.cap` + `.meta.json` |
| `capture promote` | `.cap` + `.meta.json` | `tests/fixtures/state-replay/*.raw` + `MANIFEST.yaml` |

## Typical workflows

### 1. Run a health check first

```bash
agend-terminal doctor
```

Use it to see whether the issue is in the file layer, the backend binaries, helper staleness, or an agent that has gone stale.

### 2. If the problem is Telegram topics

```bash
agend-terminal doctor topics --format json
```

Look for orphan entries before deciding whether to clean anything up.

### 3. If you need to hand the problem off

```bash
agend-terminal bugreport
```

Attach the generated file to the issue or send it along to the next operator.

### 4. If you want a replay fixture

```bash
export AGEND_CAPTURE_FIXTURES=1
agend-terminal capture backend --backend claude --seconds 20
agend-terminal capture promote <cap> <name> --scenario-kind silent_stuck
```

## Common mistakes

### Treating `doctor` like a repair tool

It is not. `doctor` reports what is broken; it does not change it.

### Ignoring the `doctor topics` permission probe

If the bot lacks `can_manage_topics`, cleanup will intentionally skip the chat mutation step.
That is not a bug; it is a permissions problem.

### Treating bugreport as a backup archive

It is a diagnostic bundle, not a restore format.

### Treating capture as a finished fixture

`capture backend` only creates the raw material. You still need `capture promote` to move it into the replay corpus.

## Source pointers

- `src/cli.rs`: `run_doctor`, `run_doctor_topics`, `capture_backend`
- `src/main.rs`: CLI subcommand routing
- `src/bugreport.rs`: report generation and redaction
- `src/capture.rs`: capture sink, rotation, and promotion
- `src/bootstrap/doctor_topics.rs`: topic classification and cleanup
- `src/bootstrap/doctor.rs`: fleet validation logic

## Practical advice

1. When an agent looks stale, run `doctor` before deleting anything.
2. For Telegram topic drift, inspect with `doctor topics` before cleanup.
3. Use `bugreport` when you need a shareable snapshot.
4. Make sure `AGEND_CAPTURE_FIXTURES=1` is set before trying to capture fixtures.
5. Check `scenario_kind` carefully before promoting; a wrong kind makes the corpus less useful.
