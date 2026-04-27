# Migration: `outbound_capabilities` (Sprint 22 P0 → Sprint 23 hard-cut)

**Status**: Sprint 22 P0 transition window — operators MUST add `outbound_capabilities` to user-authored instances in `fleet.yaml` before Sprint 23 ships, or `agend-terminal start` will refuse to load fleet.yaml.

## TL;DR

For every instance in your `fleet.yaml` that you authored (anything not auto-injected as `general`):

```yaml
instances:
  <your-instance-name>:
    backend: claude
    outbound_capabilities: [reply, react, edit, inject_provenance]   # ← ADD THIS
    # … other existing fields …
```

For inbound-only / relay agents that should NOT emit MCP→Channel ops:

```yaml
    outbound_capabilities: []                                          # ← explicit "no outbound"
```

## What changed

`outbound_capabilities` is a per-instance gate for **agent-callable** outbound MCP→Channel operations (`reply` / `react` / `edit_message` / `delegate_task` provenance side-channel). The field shipped declaratively in [PR #223](https://github.com/suzuke/agend-terminal/pull/223) but defaulted to permissive when absent. Sprint 22 P0 (this sprint) starts the 2-stage hard-cut transition.

### Two-stage transition

| State | Sprint 22 P0 | Sprint 23 |
|---|---|---|
| `Some([reply, …])` | only listed ops permitted | same |
| `Some([])` | fail-closed (no agent outbound; explicit) | same |
| `None` (absent) | **FATAL warn-but-permit one daemon cycle** + migration template logged | **hard parse error** |

### Built-in instances

`general` (and any future auto-created coordinator) get auto-injected `[reply, react, edit, inject_provenance]` via `bootstrap::fleet_normalize::auto_create_general` — operator never has to author this field for first-class fleet members.

### Independence from `user_allowlist`

`outbound_capabilities` only gates **agent-callable** MCP→Channel ops (the four MCP tools above). The `channel.user_allowlist` field continues to gate inbound message acceptance + daemon-internal stall/recovery/CI notifications (per [PR #216](https://github.com/suzuke/agend-terminal/pull/216) outbound fail-closed default at the daemon notify call sites). The two gates compose — both must be satisfied for an agent's reply to actually reach the bound Telegram group.

## Why this matters (security)

Without the per-instance gate, any agent in the fleet could emit any MCP→Channel op to the bound Telegram group. The Sprint 20.5 cross-validation audit (see `docs/codebase-review-2026-04-27/SYNTHESIS.md`) flagged this as a per-instance authorization hole even after PR #216 closed the daemon-side outbound info-leak.

The fix is to require operators to declare per-instance outbound intent explicitly. The Sprint 22 P0 transition window (warn-but-permit) gives operators time to update fleet.yaml without breaking running deployments; Sprint 23 closes the hole completely.

## What you'll see during the transition

On the first agent outbound call from an instance missing `outbound_capabilities`, daemon logs (at `error` level for visibility):

```
ERROR: FATAL (warn-but-permit one daemon cycle): instance '<name>' \
       outbound_capabilities NOT SET. Sprint 22 P0 grants this <op> call \
       under gradual-migration grace. Sprint 23 will fail-closed (hard \
       parse error on missing field). Add to fleet.yaml NOW:
  instances.<name>.outbound_capabilities: [reply, react, edit, inject_provenance]
See docs/USAGE.md "Channel: Telegram" + docs/MIGRATION-OUTBOUND-CAPS.md for details.
```

The op is permitted this cycle (no operator-visible breaking) but the warn is rate-limited to **once per instance per process lifetime** so you'll see one error per instance per daemon restart — not log spam.

In Sprint 23, the same fleet.yaml will fail to load with a `serde` parse error pointing back to this migration doc.

## ChannelOpKind enum reference

Source: `src/channel/auth.rs::ChannelOpKind` (Rust enum, snake_case YAML).

| YAML token | MCP tool | Description |
|---|---|---|
| `reply` | `reply` | Agent sends a free-form message into its bound Telegram topic |
| `react` | `react` | Agent attaches an emoji reaction to an existing message |
| `edit` | `edit_message` | Agent edits a previously-sent message |
| `inject_provenance` | `delegate_task` (provenance side-channel) | Daemon-internal injection of "who delegated this" tag to the receiving agent's topic |

## Architectural note (cross-channel)

The shared `crate::channel::auth::gate_outbound_for_agent` helper extracted in Sprint 22 P0 is what every `Channel::send_from_agent` impl calls (Telegram + future Discord/Slack/Teams). Future channel adapters cannot accidentally bypass the per-instance gate — the helper is the single source of truth.

## References

- [`docs/USAGE.md`](USAGE.md) — operator setup guide, `outbound_capabilities` semantics section
- [`src/fleet.rs`](../src/fleet.rs) — `InstanceConfig.outbound_capabilities` field doc-comment with full 2-stage transition table
- [`src/channel/auth.rs`](../src/channel/auth.rs) — `ChannelOpKind` enum + `gate_outbound_for_agent` shared helper
- [PR #223](https://github.com/suzuke/agend-terminal/pull/223) — gradual bridge introducing the field
- [PR #224](https://github.com/suzuke/agend-terminal/pull/224) — `Channel::send/edit/delete` real dispatcher
- [PR #216](https://github.com/suzuke/agend-terminal/pull/216) — `user_allowlist` outbound fail-closed default
- Sprint 22 P0 dispatch decision `d-20260427042738203707-13`
