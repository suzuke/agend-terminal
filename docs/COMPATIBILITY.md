# On-Disk Format Compatibility Policy

agend-terminal reads and writes a number of files under `$AGEND_HOME` (and a
few inside agent working directories). Now that external users exist, these
are product surface, not implementation detail. This document declares what
you can rely on, per tier, based on the 2026-06-11 format inventory (#1989).

The short version: **tier (a) and (b) changes are additive-only** until a
real migration framework exists. "Additive-only" means:

- New fields are always **optional with a serde default** — a file written
  by an older version keeps deserializing identically.
- Existing fields are never renamed, retyped, repurposed, or removed.
- A change that can't be expressed additively is a **breaking change**: it
  bumps the relevant schema version, ships a migration (or an explicit
  refuse-with-instructions), and is called out in the CHANGELOG migration
  notes.

## Tier (a) — stable public interfaces (hand-edited or user-visible)

You may hand-edit these; upgrades must never silently change their meaning.

| Surface | Notes |
|---|---|
| `fleet.yaml` | The primary hand-edited interface. Carries an optional `schema_version:` (omitted = `1`, the version of every pre-#1989 file). The daemon **warns** when a file declares a version newer than it supports (unknown fields are silently ignored by serde — the warning is your signal the daemon is too old) and never refuses to start over it. The daemon never injects `schema_version:` into a file that doesn't have it. |
| Service templates | launchd plist / systemd unit / Task Scheduler XML written by `agend-terminal service install`. Regenerate with `service install` after upgrading rather than hand-porting; hand edits survive only until the next `service install`. |
| Instruction blocks | The marker-delimited blocks injected into agent instruction files (e.g. `CLAUDE.md`). The markers are the interface: content between them is daemon-owned and rewritten; everything outside them is user-owned and never touched. |
| MCP config | Backend MCP wiring written into agent working directories (e.g. `.mcp-config.json`, `.claude/settings.local.json` entries). Daemon-owned keys are upserted in place; user-added keys in the same files are preserved. |

## Tier (b) — internal persisted state (versioned)

Daemon-owned state that must survive restarts and upgrades: inbox messages,
task-board entries and events, decision log, and the sidecar stores
(escalation persist, ci-handoff tracks, pending dispatches, …). Schemas
either carry an explicit `schema_version` field or evolve additive-only
under the same rule as tier (a). Hand-editing these is unsupported; an
upgraded daemon must read state written by any prior release of the same
major version. How a store treats *newer*-than-supported records is
per-store (#1992): the inbox skips the unknown record with a warning and
keeps serving the rest (degrade); the task-events store fail-closes on the
whole file (deliberate — board integrity outranks availability, stricter
than the tier floor). Either way: never a crash, never a silent drop.

`runtime-config.json`, the decision log (`decisions/*.json`), and
`binding.json` carry an explicit `schema_version` (#1990): an older file
without it reads normally; a newer-than-supported file is fail-closed
(runtime-config keeps the last-known-good per #1576 and refuses an overwriting
write; a decision is skipped on read and refused for update; a binding reads as
absent to **daemon-side** readers — the git shim has its own reader that
HMAC-verifies and treats a parseable future binding as bound, so the agent stays
restricted to its own worktree).

**Two tier (b) stores are unversioned free-form key-value bags** that cannot
carry a `schema_version` without a breaking shape change: `topics.json` (the
telegram topic registry — a bare `topic_id → instance` map) and
`metadata/*.json` (per-instance operator metadata — an open KV bag). Their
compatibility rule is narrower and explicit: **only new keys may be added;
existing keys are never renamed, retyped, or repurposed.** Versioning them is
deferred (#1990) until a non-additive change actually needs it — wrapping a
bag in a versioned envelope is itself a breaking change, and both are low
risk (topics self-heals via the boot orphan-sweep; metadata is
operator-cosmetic).

## Tier (c) — regenerable / ephemeral (no commitment)

Caches, lock files, PTY transcripts, logs, runtime sockets/PID files, and
anything the daemon can rebuild from scratch. No format commitment; any
release may change or delete these. If deleting a tier (c) file changes
behavior beyond a one-time rebuild cost, that's a bug — report it.

## Versioning mechanics (tier (a) fleet.yaml)

- `FLEET_SCHEMA_VERSION` (`src/fleet/mod.rs`) is the version the daemon
  reads and writes; `FleetConfig::effective_schema_version()` resolves an
  omitted field to `1`.
- Additive optional fields do **not** bump the version.
- A future breaking change bumps the constant, and the then-current daemon
  must ship explicit handling for older files (migration or documented
  refusal). Until such a framework exists, breaking changes to fleet.yaml
  are simply not allowed.
