# Decisions — Decision Traceability

The Decisions system records important architecture and process choices so the team can answer a simple question later: why did we choose this?

## Usage Scenarios

> **Target audience:** Agent infrastructure — agents use this via MCP tools; operators typically don't interact directly.

A lead agent makes an architecture choice such as preferring worktrees over direct branch checkout. Recording that choice through `decision action=post` creates a durable explanation that future agents can query instead of rediscovering the same discussion.

When a previous choice becomes obsolete, a new decision can supersede it with a clear replacement path. That keeps review history understandable without forcing anyone to guess which note is current.

An operator or reviewer can search for decisions by tag when they need to understand why a particular gate, policy, or migration rule exists. The record becomes a shared memory layer for the fleet.

## Design Goals

In a multi-agent workflow, decisions end up scattered across chat logs, PR descriptions, and commit messages. Decisions pulls them into one place and makes them queryable.

It gives you:

- **Centralized records**: important decisions live in one place.
- **Structured data**: title, content, scope, tags, and authorship are explicit.
- **Revision tracking**: new decisions can supersede older ones.
- **Automatic expiry**: TTL keeps stale decisions from hanging around forever.

---

## Quick Start

```json
// Record a decision
{
  "action": "post",
  "title": "Use prefix match when checking SHA",
  "content": "reviewed_head uses a prefix match of at least 7 characters instead of a full SHA comparison. Reason: `git log --oneline` commonly shows 7 characters.",
  "scope": "project",
  "tags": ["sha-gate", "sprint-58"]
}

// List active decisions
{
  "action": "list"
}

// Replace a previous decision
{
  "action": "post",
  "title": "Require a minimum SHA prefix length of 7",
  "content": "The original rule allowed empty strings, which could bypass the gate. The new rule requires at least 7 characters.",
  "scope": "project",
  "tags": ["sha-gate", "sprint-58"],
  "supersedes": "d-20260525040000000000-1"
}
```

---

## Operations

### `post` — record a decision

Create a new decision entry.

| Parameter | Type | Required | Description |
|---|---|---:|---|
| `title` | string | yes | Human-readable title |
| `content` | string | yes | Decision text and rationale |
| `scope` | string | yes | `project` or `fleet` |
| `tags` | string[] | no | Classification tags |
| `ttl_days` | number | no | Auto-expiry window in days, default 90 |
| `supersedes` | string | no | Decision ID being replaced |

Response includes the generated ID:

```json
{
  "id": "d-20260525040000000000-1",
  "status": "created"
}
```

### `list` — query decisions

List active decisions.

| Parameter | Type | Required | Description |
|---|---|---:|---|
| `tags` | string[] | no | Filter by tag; any match counts |
| `include_archived` | bool | no | Include archived decisions, default false |

Results are sorted newest-first.

### `update` — edit an existing decision

Use this when the content needs to be corrected without changing the overall record identity.

Typical uses:

- fix a wording mistake
- append a missing rationale
- mark a decision as superseded
- archive or unarchive a record depending on the policy being applied

---

## Scopes

### `project`

Project-scoped decisions are local to this repository and this codebase.

Examples:

- a specific handler contract
- a CLI argument rule
- a one-off migration choice

### `fleet`

Fleet-scoped decisions are broader and affect multiple agents or the operator workflow.

Examples:

- message routing policy
- worktree discipline
- release / merge conventions

Use the narrowest scope that still tells the truth.

---

## When to Record a Decision

Record a decision when the choice matters later.

Good candidates:

- behavior changes that will be reviewed again
- a rejected alternative that people may keep asking about
- a rule that affects multiple agents
- a migration policy that the next person will need to understand

Don't record every implementation detail. If the question will disappear once the PR is merged, it probably does not need a decision record.

---

## Why Supersedes Exists

The `supersedes` field is the main mechanism for keeping the history useful instead of merely long.

It lets you say:

- this new decision replaces the old one
- the old one is still visible, but it is no longer authoritative
- readers can follow the chain instead of guessing which note is current

That is especially important for policy changes that happen in stages.

---

## TTL and Archiving

Decisions can expire automatically.

This is not a deletion mechanism; it's a way to reduce noise in the active query surface while preserving old records for audit.

Use TTL when the decision is naturally time-bound. For example:

- a temporary migration workaround
- a short-lived rollout policy
- a sprint-specific operating rule

---

## Common Usage Patterns

### Architecture choice

Record the shape of a shared abstraction, especially if it replaces or rejects a competing model.

### Review finding

If a review conclusion changes behavior or future expectations, turn it into a decision record so the same debate doesn't repeat every sprint.

### Migration guidance

If operators need a sequence of steps to move from one format to another, record the decision that defines the path.

---

## Querying Strategy

Use `list` when you need a quick lookup.

Use tags to narrow the field:

- `sha-gate`
- `watchdog`
- `topic-routing`
- `mcp-config`
- `worktree`

If you are unsure which tag to search first, start with the subsystem name and then refine.

---

## Operational Boundaries

Decision records are for traceability, not as a substitute for code comments.

If the reasoning is local to a function, comment the code.
If the reasoning is about a system-level tradeoff, record it here.

---

## Source Pointers

- `src/decisions.rs`: storage and query implementation
- `src/store.rs`: shared JSON persistence helpers
- `src/mcp/handlers/decision.rs`: MCP handler surface
- `src/mcp/tools.rs`: tool registration

---

## Practical Advice

1. Use a title that will still make sense in six months.
2. Put the decision and its reason in the same record.
3. Add tags early; search quality depends on them.
4. Supersede rather than overwrite when the meaning has changed.
5. Prefer concise records that capture the real tradeoff.
