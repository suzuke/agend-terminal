[繁體中文](FEATURE-communication.zh-TW.md)

# Inter-Agent Communication System

AgEnD Terminal's communication system enables structured message passing between agents — delegating tasks, asking questions, reporting results, and broadcasting updates.

## Usage Scenarios

> **Target audience:** Agent infrastructure — agents use this via MCP tools; operators typically don't interact directly.

**Dev reports completion to lead.** After finishing a PR, the dev agent calls `send` with `kind=report` and the task's `correlation_id`. The lead agent's inbox receives the report, and the dispatch idle tracker clears the pending sidecar — no human coordination needed.

**Lead delegates a task.** The lead agent creates a task on the board, then uses `send` with `kind=task`, a `branch` name, and `next_after_ci=reviewer`. The daemon auto-provisions a worktree for the dev, and once CI passes, the reviewer is automatically notified — the entire handoff chain is wired up in a single `send` call.

**Cross-team status broadcast.** An agent needs to inform the whole team about a merge freeze. It calls `send` with `team=fixup` and `kind=update`. Every team member receives the message in their inbox; no reply is expected. The sender is automatically excluded from the broadcast list.

## Design Philosophy

Multi-agent collaboration requires a reliable communication channel. Passing messages through raw terminal output is neither structured nor traceable. The communication system provides:

- **Structured messages**: Every message has a clear type (task/query/report/update)
- **Persistent inbox**: Messages are never lost, even if the recipient is temporarily offline
- **Task tracking**: Delegated tasks can be tracked for progress and results
- **Multiple delivery modes**: Single target, multi-target broadcast, team broadcast

---

## Three Core Tools

### `send` — Unified Sender

All message sending goes through the `send` tool. It automatically routes to the appropriate handler based on parameters.

```json
{
  "instance": "dev",
  "message": "Please fix the regression in #123",
  "request_kind": "task",
  "task_id": "t-20260525..."
}
```

### `inbox` — Receive Messages

Check the inbox for pending messages.

```json
{}
```

Calling with no parameters returns all unread messages. You can also query a specific message's status or fetch a thread.

### `reply` — Reply to External Channels

Reply to users or operators through external channels like Telegram or Discord.

```json
{
  "text": "Task complete, PR has been created"
}
```

---

## Message Types (request_kind)

Every message has a `request_kind` that determines the recipient's handling obligation and system behavior:

| Type | Purpose | Reply Obligation |
|------|---------|-----------------|
| `task` | Delegate work to another agent | Must report results on completion |
| `query` | Ask another agent a question | Must reply with an answer |
| `report` | Report task results or review conclusions | Usually no reply needed |
| `update` | Status notification | No reply needed |

### task — Task Delegation

Used to assign work to another agent. Must include a `task_id` (obtained from the task board).

```json
{
  "instance": "dev",
  "message": "Fix the empty-string bypass in sha_gate.rs",
  "request_kind": "task",
  "task_id": "t-20260525040842727169-9",
  "branch": "fix/1177-sha-gate-empty",
  "success_criteria": "Fix complete + cargo test passes + PR created"
}
```

Task type supports additional management parameters:

| Parameter | Description |
|-----------|-------------|
| `task_id` | Task board ID (required; obtain via `task action=create`) |
| `branch` | Target git branch (auto-binds worktree) |
| `success_criteria` | Completion criteria |
| `eta_minutes` | Expected completion time |
| `force` / `force_reason` | Override busy gate (reason required) |
| `expect_reply_within_secs` | Timeout monitoring (seconds) |
| `next_after_ci` | Agent to auto-notify after CI passes |

### query — Questions

Ask another agent a question. The recipient must reply.

```json
{
  "instance": "reviewer",
  "message": "Is this race condition fix correct?",
  "request_kind": "query"
}
```

### report — Result Reporting

Report task results or review conclusions. Typically paired with `correlation_id` referencing the original task.

```json
{
  "instance": "lead",
  "message": "Review complete. VERIFIED — 4M/2L/1I.",
  "request_kind": "report",
  "correlation_id": "t-20260525040842727169-9",
  "parent_id": "m-20260525044640824746-72",
  "reviewed_head": "1c78314"
}
```

| Parameter | Description |
|-----------|-------------|
| `correlation_id` | Corresponding task ID (for tracking correlation) |
| `parent_id` | Message ID being replied to (thread linking) |
| `reviewed_head` | Git HEAD SHA at review time |

### update — Status Updates

Informational messages that don't require a reply.

```json
{
  "instance": "lead",
  "message": "PR #1187 CI passed, awaiting review",
  "request_kind": "update"
}
```

---

## Delivery Modes

### Single Target

```json
{
  "instance": "dev",
  "message": "..."
}
```

Message is delivered directly to the specified agent's inbox.

### Multi-Target Broadcast

```json
{
  "instances": ["dev", "reviewer", "tester"],
  "message": "..."
}
```

The same message is delivered to all specified agents.

### Team Broadcast

```json
{
  "team": "fixup",
  "message": "..."
}
```

Delivered to all members of the specified team.

### Tag Broadcast

```json
{
  "tags": ["backend"],
  "message": "..."
}
```

Delivered to agents filtered by tags.

In broadcast mode, the sender is automatically excluded from the recipient list. Each message includes `broadcast_context` so recipients know it's a one-to-many message.

---

## Message Delivery Mechanism

### PTY Injection (Default)

When the target agent is running:

1. Message is written to the inbox (append-only JSONL)
2. A notification line is simultaneously injected into the agent's terminal:
   ```
   [AGEND-MSG-PENDING] id=m-20260525... kind=task from=lead inbox=1
   ```
3. The agent sees the notification and calls the `inbox` tool to read the full message

### Inbox Fallback

When the target agent is offline (not started, cross-team, daemon offline):

1. Message is written directly to the inbox JSONL file
2. Agent receives it the next time it comes online and calls `inbox`

### Failure Degradation

When the daemon API call fails:

1. Automatically degrades to direct inbox file write
2. Resolves threading (derives `thread_id` from `parent_id`)
3. Records delivery mode as `inbox_fallback`

Regardless of mode, messages are never lost. The inbox uses append-only JSONL format with file locking to ensure safe concurrent writes.

### Idempotent Retry (#1341)

All three MCP→daemon `api::call` sites (`send`, `delegate_task`,
`report_result`) include a UUIDv4 `request_id` in the JSON envelope. The
daemon's `request_dedup::DedupCache` uses this to deduplicate retries: if
the same `request_id` arrives while the first call is still processing (or
has already completed), the duplicate is suppressed or returns the cached
result. Without `request_id`, the legacy at-least-once path applies.

---

## Inbox Operations

### Drain Unread Messages

```json
// inbox (no parameters)
{}
```

Returns all unread messages and marks them as read. Read messages do not reappear on subsequent calls.

### Query a Specific Message

```json
// inbox (with message_id)
{
  "message_id": "m-20260525042040527659-39"
}
```

Returns the message's delivery status: read (with timestamp and delivery mode), unread/expired, or not found.

### Fetch a Thread

```json
// inbox (with thread_id)
{
  "thread_id": "m-20260525035931943006-17"
}
```

Returns all messages in the specified thread, including both read and unread.

---

## Threading

Messages can form threads through `thread_id` and `parent_id`:

```
Message A (id=m-001, thread_id=null)       <- Thread root
  └─ Message B (parent_id=m-001)           <- thread_id auto-inherited as m-001
      └─ Message C (parent_id=m-002)       <- thread_id inherits m-001
```

Rules:
- If `parent_id` is set but `thread_id` is not, `thread_id` is automatically inherited from the parent message
- If the parent message itself has no `thread_id`, the parent's `id` becomes the thread root

---

## Task Board Integration

### task_id Requirement

Sprint 58 Wave 4 introduced the anti-stall contract:

- `kind=task` in broadcast mode (team/targets/tags) **must** include `task_id`
- `kind=task` in single-target mode auto-creates a task if `task_id` is omitted

How to obtain a `task_id`:

```json
// Create a task first
{"action": "create", "title": "Fix #1177", "assignee": "dev"}
// Returns task_id = "t-20260525..."

// Then send a message with task_id
{"instance": "dev", "request_kind": "task", "task_id": "t-20260525..."}
```

### task_id Format

- Prefix `t-`
- Length 4-128 characters
- Alphanumeric, hyphens, and underscores only

### Timeout Monitoring

Enable timeout monitoring with `expect_reply_within_secs`:

```json
{
  "instance": "dev",
  "request_kind": "task",
  "task_id": "t-...",
  "expect_reply_within_secs": 600
}
```

If no matching `kind=report` (paired by `correlation_id`) arrives within the specified time, the daemon sends a `dispatch_idle_threshold_exceeded` notification to the sender's inbox.

Fixup team tasks automatically enable a 10-minute timeout by default. Other teams must specify explicitly.

---

## Busy Gate

When the target agent already has a claimed or in-progress task, `kind=task` delivery is blocked by the busy gate.

```json
// Override the busy gate
{
  "instance": "dev",
  "request_kind": "task",
  "force": true,
  "force_reason": "Urgent fix, needs immediate attention"
}
```

`force` requires a `force_reason` explaining why, which is recorded in the audit log.

---

## Automatic Worktree Binding

When `kind=task` includes a `branch` parameter, the system automatically creates and binds a git worktree for the target agent:

```json
{
  "instance": "dev",
  "request_kind": "task",
  "task_id": "t-...",
  "branch": "fix/1177-sha-gate-empty"
}
```

Binding flow:
1. Checks out the specified branch into a worktree from the source repo
2. Binds the target agent to that worktree
3. If `next_after_ci` is set, automatically monitors CI results

Set `"bind": false` to skip automatic binding (e.g., for notifications that don't need a working directory).

---

## CI Notification Chaining

`next_after_ci` implements automatic handoff after CI passes:

```json
{
  "instance": "dev",
  "request_kind": "task",
  "branch": "fix/1177",
  "next_after_ci": "reviewer"
}
```

Flow:
1. Lead delegates a task to dev with `next_after_ci=reviewer`
2. Dev completes the work and pushes the PR
3. After CI passes, the daemon automatically notifies the reviewer
4. Reviewer receives a `[ci-ready-for-action]` message

No manual CI watch setup needed — everything is wired up at dispatch time.

---

## Typical Communication Flows

### Complete Task Delegation Cycle

```
Lead: task(create, title="Fix #1177") -> task_id="t-..."
Lead: send(target=dev, kind=task, task_id="t-...", branch="fix/1177", next_after_ci="reviewer")
Dev:  inbox() -> receives the task
Dev:  send(target=lead, kind=report, correlation_id="t-...", message="PR created")
CI passes -> Reviewer auto-receives [ci-ready-for-action]
Reviewer: send(target=lead, kind=report, correlation_id="t-...", message="VERIFIED")
```

### Team Broadcast / Cross-Agent Queries

```
send(team="fixup", kind=update, message="Merge freeze today")
send(target=reviewer, kind=query, message="When should the M3 session be deleted?")
```