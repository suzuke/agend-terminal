# Embedder Contract v1

This document is the **versioned integration contract** between `agentic-git`
(the shim + the `agentic-git-core` contract crate) and any orchestrator that
embeds it — agend (the co-evolved reference deployment), the built-in
`agentic-git run` reference orchestrator, or a thin wrapper around any agent
CLI. It is enough to sign a binding, install hooks, put the shim on `PATH`,
and get the same seatbelt behavior agend gets — without reading `main.rs`.

Contract-testing: the binding schema is pinned by golden fixtures
(`crates/agentic-git/tests/fixtures/binding-*.json`) and the event table
below is asserted against the code's single-source `disposition_for` mapping
by `doc_event_disposition_table_matches_code_26`.

## Env

Primary names are `AGENTIC_GIT_*`; every one has a legacy `AGEND_*` twin
(`AGEND_GIT_*` for the bypass family) accepted for zero-change agend
adoption. When both are set the primary wins.

| Primary | Legacy | Required when | Notes |
|---|---|---|---|
| `AGENTIC_GIT_HOME` | `AGEND_HOME` | shim + orchestrator | state root: `runtime/`, key, `fleet_events.jsonl` |
| `AGENTIC_GIT_AGENT` | `AGEND_INSTANCE_NAME` | agent execution path | agent identity; selects `runtime/<agent>/binding.json` |
| `AGENTIC_GIT_REAL_GIT` | `AGEND_REAL_GIT` | always recommended | absolute path to real `git`; the shim rejects a self-referencing value |
| `AGENTIC_GIT_BYPASS` | `AGEND_GIT_BYPASS` | optional, audited | `1` = pass mutating ops through (audited — see the Events table) |
| `AGENTIC_GIT_BYPASS_AGENT` | `AGEND_GIT_BYPASS_AGENT` | optional | scope a bypass to one agent |
| `AGENTIC_GIT_BYPASS_UNTIL` | `AGEND_GIT_BYPASS_UNTIL` | optional | unix-seconds expiry for a bypass window |
| `AGENTIC_GIT_ALLOW_CANONICAL_MUTATE` | `AGEND_GIT_ALLOW_CANONICAL_MUTATE` | optional, audited | permit mutating ops in the canonical checkout |
| `AGENTIC_GIT_SNAPSHOTS` | `AGEND_GIT_SNAPSHOTS` | optional | pre-destructive-op snapshot refs; default off raw / on under `run` |
| `AGENTIC_GIT_SHIM_DEPTH` | `AGEND_GIT_SHIM_DEPTH` | internal | recursion guard — do not set manually |

## Binding

- Path: `$AGENTIC_GIT_HOME/runtime/<agent>/binding.json` + `binding.json.sig`
- Bound predicate: `task_id` present **and** the `worktree` path exists
- HMAC over the exact file bytes with the key at
  `$AGENTIC_GIT_HOME/.config-integrity-key` (`agentic_git_core::integrity_core`:
  `ensure_key` / `sign_binding` / `verify`). Bare-hex tags are the default;
  envelope-scheme tags are accepted (core P1a). Missing/invalid/foreign-scheme
  signatures all fail **closed** to unbound.
- Typed codec: `agentic_git_core::binding` — `BindingV1`, `decode`, `encode`.
  The reference `run` writer and the shim reader share this representation;
  a second orchestrator should too (or write byte-equivalent JSON).

### Schema (v1)

| Field | Type | Notes |
|---|---|---|
| `version` | int | `1`; **absent = legacy v1** (agend zero-daemon-change); any other value → decode fails closed (`UnsupportedVersion`) and the shim treats the agent as unbound, loudly |
| `agent` | string? | agent identity |
| `task_id` | string? | the bound anchor — present ⇒ bound |
| `branch` | string? | the branch this binding authorizes work/push on |
| `worktree` | string? | absolute worktree path; must exist or the binding reads unbound (orphan guard) |
| `source_repo` | string? | canonical repository the worktree belongs to |
| `issued_at` | string? | RFC 3339 issue timestamp |
| *(unknown fields)* | any | bounded extension surface: preserved round-trip, ignored by readers; anything a reader must understand for safety belongs in v2 |

Golden fixtures: `crates/agentic-git/tests/fixtures/binding-agend-v1.json`
(agend daemon shape), `binding-run-v1.json` (reference `run` shape),
`binding-unsupported-v2.json` (must fail closed).

## Events

- Path: `$AGENTIC_GIT_HOME/fleet_events.jsonl` (the name is historical —
  treat it as the shim's append-only audit log)
- Every record carries: `kind` (`git_event`), `event`, `disposition`,
  `agent`, `subcommand`, `timestamp`, plus event-specific fields
  (`reason`, `target_branch`, `argv`, `cwd`, `ppid`, `process_ancestry`,
  `bypass_layer`, `allow_empty`, `git_user_email`, …)
- `disposition` is the agent's stop-vs-continue routing axis. An event type
  absent from this table fails **closed** to `deny` in code — treat unknown
  events as "stop and check".

| Event | Disposition | Meaning |
|---|---|---|
| `deny` | deny | mutating op refused (unbound / policy) |
| `deny_trust_root` | deny | write into the trust root refused |
| `deny_protected_ref` | deny | push/update of a protected ref refused |
| `deny_snapshot_ref_push` | deny | push of internal snapshot refs refused |
| `cwd_worktree_drift` | warn | op ran, but cwd disagreed with the bound worktree |
| `git_conflict` | warn | conflict state detected after a conflict-capable op |
| `snapshot_failed` | warn | pre-op snapshot failed; the op itself still ran (fail-open) |
| `post_merge_cleanup_exempt` | info | recognized `gh pr merge` cleanup checkout exempted |
| `bypass_mutating_op` | warn | audited bypassed mutating op (instrument-only) |
| `canonical_passthrough_checkout` | warn | unattributed canonical-cwd HEAD-touching checkout (instrument-only) |
| `init_heartbeat_forensics` | info | backend heartbeat `commit --allow-empty` forensics (instrument-only) |

## Hooks

Orchestrators must install (or let `agentic-git run` install) into the
**worktree-scoped** hooks dir and point `core.hooksPath` at it — never the
repository-global hooks, so an operator's own checkout is untouched:

- `prepare-commit-msg` (+ `prepare-commit-msg.ps1` on Windows): stamps the
  provenance trailers below onto every commit made in a bound worktree.
- `reference-transaction`: the ref-update seatbelt backstop.

## Trailers

Stable provenance surface stamped by the hooks — parse these, do not scrape
commit bodies:

- `Agentic-Agent`
- `Agentic-Task`
- `Agentic-Branch`
- `Agentic-Issued-At`

## Orchestrator responsibility checklist

| Responsibility | Owner |
|---|---|
| Provision the worktree (branch checkout) | orchestrator |
| Write + HMAC-sign `binding.json` (typed codec) | orchestrator (daemon or `run`) |
| Provision the integrity key (`ensure_key`) | orchestrator, once per home |
| Put the shim first on `PATH` as `git` | orchestrator |
| Set `AGENTIC_GIT_REAL_GIT` | orchestrator |
| Install hooks + worktree `core.hooksPath` | orchestrator (or `run`) |
| Grant/expire bypass windows | orchestrator/operator only |
| Enforce deny/warn routing from events | agent runtime / orchestrator |

`agentic-git run` is the **reference implementation** of the orchestrator
side, not the only one.

## Core crate boundary

- **In `agentic-git-core`:** key provision + HMAC sign/verify
  (`integrity_core`), the typed binding codec (`binding`), and the
  protected-ref predicate (`protected_refs`).
- **Not in core (deliberately):** the full subcommand classification matrix
  and push denylist — policy stays in the shim so embedders are not forced
  onto one binary policy blob.

## Minimal generic embed recipe

```sh
HOME_DIR=/tmp/agentic-home; AGENT=my-agent
WT=/tmp/agentic-home/worktrees/my-agent   # any provisioned git worktree

# 1. one-time: state root + integrity key (or call core's ensure_key)
mkdir -p "$HOME_DIR/runtime/$AGENT"

# 2. sign a binding for the worktree (the reference writer does all of this):
agentic-git run --agent "$AGENT" --branch feat/x -- your-agent-command
#    …or write binding.json yourself with core's BindingV1 + sign_binding,
#    install hooks, then launch:
# 3. launch anything under the seatbelt:
env AGENTIC_GIT_HOME="$HOME_DIR" \
    AGENTIC_GIT_AGENT="$AGENT" \
    AGENTIC_GIT_REAL_GIT="$(command -v git)" \
    PATH="/path/to/shim-dir:$PATH" \
    your-agent-command
```

Version policy: this document freezes v1. Breaking changes to any surface
above (binding fields readers must understand, event record shape, hook or
trailer names, env semantics) require a v2 contract, a new doc, and an
explicit `version: 2` binding that v1 readers reject closed.
