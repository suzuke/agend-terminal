[繁體中文](FEATURE-decisions.zh-TW.md)

# Decisions — Decision Traceability

The Decisions system lets the team record important architecture and process choices, providing a queryable decision history so anyone can trace "why did we make this choice?"

## Usage Scenarios

> **Target audience:** Agent infrastructure — agents use this via MCP tools; operators typically don't interact directly.

A lead agent makes an architecture choice, such as deciding to use a worktree instead of checking out a branch directly. By recording the reasoning through `decision action=post`, future agents can query it directly instead of digging through the discussion thread again.

When a previous decision no longer applies, a new decision can explicitly supersede the old version, preserving history while keeping the current rule clear. That way everyone knows which entry is current, without guessing.

If an operator or reviewer wants to know why a particular gate, policy, or migration rule exists, they can search for decisions by tag. This becomes a shared memory layer for the fleet.

## Design Goals

In a multi-agent workflow, decisions end up scattered across conversations, PR descriptions, and commit messages, making them hard to trace. Decisions provides:

- **Centralized records**: all important decisions live in one place
- **Structured data**: title, content, scope, tags, author
- **Revision tracking**: a new decision can explicitly replace an old one (supersedes)
- **Automatic expiry**: a TTL mechanism keeps stale decisions from causing confusion

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

### post — record a decision

Create a new decision entry.

| Parameter | Type | Required | Description |
|------|------|------|------|
| `title` | string | yes | Decision title |
| `content` | string | yes | Decision content and rationale |
| `scope` | string | yes | `"project"` (project-scoped) or `"fleet"` (fleet-scoped) |
| `tags` | string[] | no | Classification tags |
| `ttl_days` | number | no | Auto-expiry window in days (default 90) |
| `supersedes` | string | no | Decision ID being replaced |

The response includes the auto-generated decision ID:

```json
{
  "id": "d-20260525040000000000-1",
  "status": "created"
}
```

### list — query decisions

List all active decisions.

| Parameter | Type | Required | Description |
|------|------|------|------|
| `tags` | string[] | no | Filter by tag (any match counts) |
| `include_archived` | bool | no | Whether to include archived decisions (default false) |

Results are sorted by creation time in descending order (newest first).

### update — edit a decision

Edit the content, tags, or status of an existing decision.

| Parameter | Type | Required | Description |
|------|------|------|------|
| `id` | string | yes | Decision ID |
| `content` | string | no | New content |
| `tags` | string[] | no | New tags |
| `ttl_days` | number | no | New expiry window in days |
| `archive` | bool | no | Set to true to archive manually |

Edit permission: only the original author or the orchestrator of their team can edit.

---

## Decision Structure

Each decision includes the following fields:

```json
{
  "id": "d-20260525040000000000-1",
  "title": "使用 prefix match 比對 SHA",
  "content": "reviewed_head 使用 7 字元以上的 prefix match...",
  "scope": "project",
  "author": "fixup-dev-2",
  "tags": ["sha-gate", "sprint-58"],
  "ttl_days": 90,
  "created_at": "2026-05-25T04:00:00Z",
  "updated_at": "2026-05-25T04:00:00Z",
  "archived": false,
  "supersedes": null
}
```

### Decision ID Format

`d-<microsecond timestamp>-<sequence>`, for example `d-20260525040000000000-1`. Microsecond precision plus an atomic counter guarantees uniqueness.

### Scope

- `project`: project-scoped decisions, tied to the current working directory
- `fleet`: fleet-scoped decisions, shared rules that span projects

Scope is currently used as metadata and does not affect access permissions.

Use the narrowest scope that still tells the truth.

Project-scoped examples:

- a specific handler contract
- a CLI argument rule
- a one-off migration choice

Fleet-scoped examples:

- message routing policy
- worktree discipline
- release / merge conventions

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

## Supersedes

When you need to revise a previous decision, use `supersedes` to create a link between the old and new entries:

```json
{
  "action": "post",
  "title": "SHA 最短長度改為 7 字元",
  "content": "修正 d-20260525040000000000-1，加入最短長度檢查",
  "supersedes": "d-20260525040000000000-1"
}
```

Execution flow:

1. Acquire the lock on the old decision
2. Mark the old decision as `archived: true`
3. Update the old decision's `updated_at`
4. Create the new decision, recording `supersedes` pointing to the old ID

This whole flow runs atomically under a file lock, so there is no race condition where two agents supersede the same decision at the same time.

By default, `list` does not show archived decisions. To see the full history (including superseded old decisions), use `include_archived: true`.

The `supersedes` field is the main mechanism for keeping the history useful instead of merely long.

It lets you say:

- this new decision replaces the old one
- the old one is still visible, but it is no longer authoritative
- readers can follow the chain instead of guessing which note is current

That is especially important for policy changes that happen in stages.

---

## Tag System

Tags are an arbitrary array of strings, used for classification and filtering:

```json
{
  "tags": ["sha-gate", "sprint-58", "security"]
}
```

When querying, the `tags` filter uses "any match" logic — a decision is included as long as it contains any one of the filter tags.

### Protected Tags

You can define protected tags in `fleet.yaml`:

```yaml
retention:
  protected_decision_tags:
    - SPRINT_99
    - ARCHITECTURE
```

Decisions carrying a protected tag are never auto-expired, regardless of the TTL setting. This is suitable for long-lived architecture decisions.

---

## Automatic Expiry

Decisions have a TTL (Time To Live) mechanism; expired decisions are automatically archived:

| Parameter | Default | Description |
|------|--------|------|
| Default TTL | 90 days | Expiry window when `ttl_days` is not specified |
| Minimum protection period | 14 days | No matter how short the TTL, kept for at least 14 days |
| Protected tags | — | Decisions with a protected tag never expire |

Expiry flow:

1. The daemon periodically scans all decisions
2. Skips: already archived, created less than 14 days ago, carries a protected tag
3. Decisions meeting the expiry criteria are moved to `decisions/.archive/`

You must set the environment variable `AGEND_RETENTION_DECISIONS_CUTOVER=1` to enable the auto-expiry scan (the old `AGEND_RETENTION_CUTOVER=1` still works for compatibility, but is deprecated; please switch to the new flag).

This is not a deletion mechanism; it's a way to reduce noise in the active query surface while preserving old records for audit.

Use TTL when the decision is naturally time-bound. For example:

- a temporary migration workaround
- a short-lived rollout policy
- a sprint-specific operating rule

---

## Storage

- Location: `$AGEND_HOME/decisions/`
- Format: one JSON file per decision (`{id}.json`)
- Locking: each decision has its own flock (`{id}.lock`), which does not affect concurrent operations on other decisions
- Writes: use `atomic_write()` (temp file → fsync → rename), crash-safe
- Archiving: expired decisions are moved to the `decisions/.archive/` subdirectory

---

## TUI View

In the TUI, press `Ctrl+B D` (capital D) to open the decisions panel:

- `j` / `k` or `↑` / `↓`: scroll up and down
- `PgUp` / `PgDn`: fast scroll
- `q` / `Esc`: close the panel

The panel shows each decision's title, author, timestamp, content, and tags. The selected decision expands to show the full content.

---

## Decision Timeout

An agent can set an automatic decision in `reply`:

```json
{
  "text": "是否要繼續使用精簡方案？",
  "default_action": "proceed-with-lean",
  "timeout_secs": 1800
}
```

If the operator does not reply within 30 minutes, the daemon automatically executes the `default_action`.

Flow:
1. The agent calls `reply` with `default_action` and `timeout_secs`
2. Creates a pending decision sidecar (`pending-decisions/{id}.json`)
3. The operator replies → marked as `resolved`, canceling the auto-execution
4. Timeout → marked as `timeout`, sends a notification with the default action to the agent's inbox

The same agent can only have one pending decision at a time. A new pending decision automatically cancels the previous one.

---

## Modification Permissions

| Role | Permission |
|------|------|
| Original author | Can edit decisions they created |
| Team orchestrator | Can edit decisions created by members of their team |
| Other agents | Cannot edit; returns an authorization error |

---

## Common Usage Patterns

### Record an architecture decision

```json
{
  "action": "post",
  "title": "Agent 間通訊使用 inbox JSONL 而非 RPC",
  "content": "選擇 append-only JSONL 因為：1) crash-safe 2) 離線 agent 可延遲讀取 3) 除錯時 cat 就能看。RPC 需要兩端都在線，且 crash 時訊息遺失。",
  "scope": "fleet",
  "tags": ["architecture", "communication"]
}
```

### Trace the reason for a decision

```json
{
  "action": "list",
  "tags": ["sha-gate"]
}
```

### Fix an incorrect decision

```json
{
  "action": "post",
  "title": "SHA gate 需要最少 7 字元（修正）",
  "content": "原決策未考慮空字串情境。空字串是任何字串的 prefix，會繞過所有驗證。",
  "supersedes": "d-20260525040000000000-1",
  "tags": ["sha-gate", "security"]
}
```

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
