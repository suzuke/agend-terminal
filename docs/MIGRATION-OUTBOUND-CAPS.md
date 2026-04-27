# Migration: `outbound_capabilities` (Sprint 22 P0 fail-closed → Sprint 23 P1 default-open reversal)

**Status (Sprint 23 P1)**: **default-open**. Missing `outbound_capabilities` field permits all ops; declare the field only if you want to opt out (`[]`) or restrict (selective list). The Sprint 22 P0 hard-cut described in earlier revisions of this doc is **reversed** per operator philosophy override.

## TL;DR (Sprint 23 P1 onward)

You don't need to add `outbound_capabilities` to anything. Existing fleets work. If you want to restrict an instance:

```yaml
instances:
  <your-instance-name>:
    backend: claude
    outbound_capabilities: [reply]      # only `reply` permitted; everything else rejected
```

Or block all agent outbound (relay / read-only roles):

```yaml
instances:
  <your-instance-name>:
    backend: claude
    outbound_capabilities: []           # explicit "no agent outbound"
```

## Sprint 23 P1 reversal — why

**Original Sprint 22 P0 design**: fail-closed default with FATAL warn ("operator MUST declare outbound_capabilities or daemon refuses to load fleet.yaml"). The intent was to defend against a cascade attack chain where a compromised agent in the fleet could emit MCP→Channel ops freely.

**Operator philosophy override (Sprint 23 P1)**: the cascade-attack-chain defence is over-spec for the actual single-operator threat model. The TUI is already full machine access; if an agent in the fleet is compromised, the operator already has bigger problems than gated MCP outbound ops. Operator explicitly accepts the security trade-off (telegram 11:00 UTC routed via `general` m-20260427115706155870-88, dispatch m-20260427115825059656-89, task t-20260427115754474312-3).

**What changed in code**:
- `src/channel/auth.rs::evaluate_outbound_capability` — `None` (missing field) now returns `OutboundCapabilityDecision::OpenDefault` instead of `PermissiveLegacyMissing`.
- `OutboundCapabilityDecision::PermissiveLegacyMissing` variant renamed to `OpenDefault` (no longer a transitional grace).
- `warn_once_outbound_capabilities_missing` helper retired entirely. Missing field is silent — no FATAL log, no operator-actionable migration template, no friction.
- `bootstrap::fleet_normalize::auto_create_general` no longer auto-injects an explicit cap list onto `general`. Built-ins inherit default-open like operator-authored instances do.

**What stayed the same**:
- `Some([reply, …])` — selective allow-list still works.
- `Some([])` — explicit opt-out still rejects everything (operator can still actively block agent outbound).
- `channel.user_allowlist` (PR #216) — **still fail-closed**. Different threat model: notification fan-out to the operator's bound group. Missing allowlist drops every daemon-driven notification (stall / crash / CI alerts) silently. The `warn_once_user_allowlist_unconfigured` helper from Sprint 22 P1.5 remains active and surfaces a FATAL log.

## Decision matrix (current contract)

| State | Behaviour |
|---|---|
| field absent | **all ops permitted** |
| `[reply, react, edit, inject_provenance]` | only listed ops permitted |
| `[reply]` | only `reply` permitted; `react`/`edit`/`inject_provenance` rejected |
| `[]` (explicit empty) | **all ops rejected** (operator opt-out) |

## Built-in instances

`general` (and any future auto-created coordinator) inherits default-open — no auto-injected list. The persisted `fleet.yaml` for a fresh `general` no longer carries an `outbound_capabilities:` line.

## Migration from Sprint 22 P0

If you previously added `outbound_capabilities: [reply, react, edit, inject_provenance]` to an instance to silence the FATAL log: you can remove that line and the instance still works. Or keep it — it's now equivalent to default-open.

If you set `outbound_capabilities: []` to opt out: that still works as expected.

## Independence from `user_allowlist` (unchanged)

`outbound_capabilities` only gates **agent-callable** MCP→Channel ops (`reply` / `react` / `edit_message` / `delegate_task` provenance). The `channel.user_allowlist` field continues to gate inbound message acceptance + daemon-internal notifications. **It is still fail-closed**, and the `warn_once_user_allowlist_unconfigured` helper still emits a FATAL log when missing. The two gates compose independently.

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
