# CI Watch — Automated PR CI Monitoring

## Usage Scenarios

> **Target audience:** Agent infrastructure — agents use this via MCP tools; operators typically don't interact directly.

**Automatic CI-to-reviewer handoff.** A dev agent finishes implementation and creates a PR. The daemon automatically attaches a CI watch to the PR's branch. When all GitHub Actions checks pass, the daemon sends a `[ci-pass]` notification to the reviewer agent, who begins the code review immediately — no human intervention required.

**Selective workflow monitoring.** A repository has multiple CI workflows, but only "build" and "test" are required for merge. The dev specifies `required_checks: ["build", "test"]` when the watch is created, so a flaky "windows-compat" workflow does not block the reviewer handoff.

**Conflict early warning.** While monitoring CI, the daemon also checks the PR's mergeable status. If upstream changes cause a merge conflict, a `[ci-conflict-detected]` notification is sent to the dev agent immediately, allowing it to rebase before the reviewer wastes time on a conflicted branch.

## Design Rationale

In a multi-agent workflow, after a dev agent pushes a PR it typically needs to
wait for CI to pass before notifying the reviewer. Without automation, the
operator must manually watch GitHub Actions and ping the reviewer when the
checks go green.

CI Watch eliminates this manual step: when a PR is created, the daemon
automatically attaches a CI monitor. It polls the CI provider periodically and,
upon completion, notifies all subscribers. The optional `next_after_ci`
parameter chains the next agent automatically — for example, dispatching the
reviewer as soon as CI passes.

```
dev pushes PR → daemon attaches ci watch →
CI finishes  → daemon notifies reviewer →
reviewer starts review automatically
```

---

## Usage

### Subscribe to CI Monitoring

Via the MCP tool `ci action=watch`:

```json
{
  "tool": "ci",
  "action": "watch",
  "repo": "owner/repo",
  "branch": "feat/my-feature",
  "next_after_ci": "reviewer"
}
```

| Parameter | Required | Description |
|-----------|----------|-------------|
| `repo` | Yes | GitHub repository (`owner/repo` format) |
| `branch` | Yes | Branch to monitor |
| `next_after_ci` | No | Agent to notify automatically when CI passes |
| `interval_secs` | No | Polling interval (default 60 seconds) |
| `task_id` | No | Associated task board ID |
| `required_checks` | No | Only track these workflow names (ignore others) |

### Unsubscribe

```json
{
  "tool": "ci",
  "action": "unwatch",
  "repo": "owner/repo",
  "branch": "feat/my-feature"
}
```

### Query Status

```json
{
  "tool": "ci",
  "action": "status"
}
```

Returns all active CI watches and their latest poll results.

### Auto-Attach

When a lead dispatches a task (`kind=task`) via `send` with both `branch` and
`next_after_ci`, the daemon automatically creates a CI watch for that branch.
The dev does not need to call `ci action=watch` manually.

---

## How It Works

### File Structure

Each CI watch maps to a JSON file under `$AGEND_HOME/ci-watches/`. The
filename is the SHA-256 hash of `{repo}:{branch}` (64 hex chars + `.json`),
avoiding path issues from `/` in repo names.

### Polling Loop

The daemon runs a continuous background polling loop:

1. **Scan** the `ci-watches/` directory for all `.json` files.
2. **Throttle**: compare `last_polled_at` + `effective_interval_secs` to decide whether to poll.
3. **Query the CI provider API** for the latest workflow run status.
4. **Compare** with the previously recorded `last_run_id` / `head_sha`.
5. **Notify**: if a new terminal result (success / failure / cancelled) is detected, notify all subscribers.
6. **Persist**: write the updated state back to the watch JSON.

### Adaptive Polling Interval

To avoid exceeding GitHub API rate limits, the daemon adjusts polling intervals
based on remaining quota:

| Remaining Quota | Zone | Multiplier |
|----------------|------|------------|
| > 50% | Healthy | x1 (configured interval) |
| 10%–50% | Cautious | x2 |
| <= 10% | Critical | x4 |

For example, a 60-second watch in the Critical zone polls every 240 seconds.
The upper bound is 4x the configured value. If the API does not return rate
limit headers (GitLab / Bitbucket), the original interval is used.

### Notification Deduplication

CI Watch uses a two-layer dedup mechanism:

**Layer 1: Run Selection**
`select_runs_to_notify` compares the latest workflow run against the recorded
`last_run_id` and selects only new terminal runs.

**Layer 2: SHA Dedup**
`dedupe_notifications_by_head_sha` ensures the same `head_sha` conclusion
(success / failure) is notified only once. This handles the `gh run rerun --failed`
scenario where the run_id stays the same but the conclusion changes.

### Multi-Subscriber Support

A single CI watch can have multiple subscribers. Polling runs once; notifications
are sent to each subscriber's inbox individually. Each notification carries a
supersede token — if the inbox already contains an older notification for the
same repo@branch, the new one replaces it (preventing notification pileup).

### Zombie Subscriber Filtering

If a subscriber has been removed from the fleet (agent deleted), the daemon
skips that subscriber during notification. The check requires the subscriber to
be absent from both the agent registry and fleet.yaml instances.

---

## PR Conflict Detection

CI Watch also monitors the PR's mergeable status.

### At Watch Creation

`ci action=watch` immediately queries the PR's mergeable state. If it detects
`CONFLICTING`, a `[ci-conflict-detected]` notification is sent to all
subscribers.

### During Polling

The polling loop periodically rechecks the mergeable state. If the status
transitions from non-conflicting to conflicting, a conflict notification is
sent.

---

## Stall Detection

If a CI watch is consecutively skipped due to rate limiting (unable to poll),
the daemon tracks the skip count. After 3 consecutive skips, a
`[ci-watch-stalled]` notification is sent to all subscribers, including:

- When the stall started
- Estimated next poll time (rate limit reset time)
- Configuration advice (how to obtain a higher API quota)

When polling resumes normally, a `[ci-watch-resumed]` notification is sent.

---

## TTL and Auto-Cleanup

### Absolute TTL

Each watch has an `expires_at` set at creation (default 72 hours). The watch is
unconditionally removed after this time.

### Inactivity TTL

If `last_terminal_seen_at` (the last time a terminal result was observed)
exceeds the configured hours, the watch is removed due to inactivity.

### Startup Sweep

The daemon runs a one-time startup sweep to clean up watches that expired while
the daemon was stopped.

### Protected Branch Filtering

Watches on protected branches (`main` / `master`) are automatically removed,
as these are not PR branches.

---

## CI Provider Support

| Provider | Detection | API |
|----------|-----------|-----|
| GitHub | Default, or remote URL contains `github` | GitHub Actions API |
| GitLab | Remote URL contains `gitlab` | GitLab CI/CD API |
| Bitbucket | Remote URL contains `bitbucket` | Bitbucket Pipelines API |

The provider is auto-detected from the repo URL, or can be specified manually
via the `ci_provider` parameter.

### GitHub Token

GitHub API requires authentication to avoid strict rate limits. The daemon uses
`GITHUB_TOKEN` or `GH_TOKEN` environment variables. Without a token, CI Watch
still works but is more likely to trigger rate limit stalls. Authenticated
requests get 5,000 requests/hour (vs. 60 unauthenticated).

---

## FAQ

### Q: What does CI Watch monitor?

It monitors GitHub Actions workflow runs (or the equivalent on GitLab CI /
Bitbucket Pipelines). When all checks complete, it sends a notification based
on the aggregate conclusion (success / failure).

### Q: Can I monitor only specific workflows?

Yes. Use the `required_checks` parameter to specify workflow names. Only those
workflows are considered for pass/fail judgment; others (e.g., a flaky Windows
CI) are ignored.

### Q: Does CI Watch auto-cancel?

Yes. A watch is automatically removed when:
1. It exceeds the absolute TTL (default 72 hours).
2. It exceeds the inactivity TTL.
3. The PR reaches a terminal state (merged / closed) and conditions are met.

### Q: Can multiple agents subscribe to the same watch?

Yes. Multiple `ci action=watch` calls for the same repo + branch with different
agent names add each agent to the subscriber list. Polling runs once;
notifications go out separately.

### Q: What about rate limits?

The adaptive polling interval slows down automatically. If rate limiting
persists, a stall notification is sent. Setting the `GITHUB_TOKEN` environment
variable is recommended — authenticated requests have a much higher rate limit.
