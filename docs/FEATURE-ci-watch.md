[繁體中文](FEATURE-ci-watch.zh-TW.md)

# CI Watch — Automated PR CI Monitoring

CI Watch polls a forge CI provider for a repository and branch, records PR/CI state, and delivers terminal results to subscribers and optional follow-up agents.

## Usage Scenarios

> **Target audience:** agent infrastructure. Agents normally receive an auto-armed feature-branch watch from task dispatch, or create one explicitly through MCP.

- **CI-to-review handoff:** a branch task carries `next_after_ci`; when the current head passes, the daemon sends `[ci-ready-for-action]` to the named target or targets.
- **Subscriber notification:** every subscribed agent receives informational pass/fail updates without multiplying provider polls.
- **Conflict warning:** a mergeability transition to conflicting emits `[ci-conflict-detected]`.
- **Post-merge validation:** an authorized exact-head watch can pin one immutable commit on `main`/`master` without allowing a later push to satisfy the original task.

CI Watch evaluates all latest CI runs returned for the selected head. The MCP schema does not expose a `required_checks` filter.

## Create a Feature-Branch Watch

```json
{
  "tool": "ci",
  "action": "watch",
  "repository": "owner/repo",
  "branch": "feat/my-feature",
  "next_after_ci": ["reviewer-a", "reviewer-b"],
  "task_id": "t-...",
  "interval_secs": 60
}
```

| Parameter | Required | Description |
|---|---|---|
| `repository` | Conditional | Forge repository slug. `watch` can derive it from the caller's binding; otherwise supply it explicitly. |
| `branch` | No | Branch to watch; defaults to `main`, but a generic protected-branch watch is rejected. |
| `interval_secs` | No | Baseline polling interval; defaults to 60 seconds. |
| `next_after_ci` | No | One instance or an array to receive `[ci-ready-for-action]` after success. |
| `task_id` | No | Durable back-link copied into the CI handoff. |
| `review_class` | No | `single` or `dual` review threshold for PR readiness. |
| `ci_provider` | No | Provider override, normally `github` or `bitbucket_cloud`; `bitbucket_server` is rejected. |
| `ci_provider_url` | No | Custom provider base URL; credentials are sent only to trusted hosts. |
| `head_sha` | Protected only | Full 40/64-hex SHA for an exact-head protected-ref watch. Ignored on feature branches. |

The argument name is `repository`, not `repo`.

Calling `watch` again for the same key is append-idempotent: it preserves poll state and other subscribers while adding the caller if needed. It also refreshes the requested interval/provider settings and clears a prior explicit-unwatch tombstone.

## Unwatch and Status

```json
{
  "tool": "ci",
  "action": "unwatch",
  "repository": "owner/repo",
  "branch": "feat/my-feature"
}
```

`unwatch` requires explicit `repository`. It removes only the caller's subscription, resolves that caller's matching CI-handoff obligation, and preserves co-subscribers. When the final subscriber is removed, the daemon keeps an opt-out tombstone instead of deleting the file, preventing automatic re-arm until the PR becomes terminal or someone explicitly calls `watch` again.

```json
{
  "tool": "ci",
  "action": "status",
  "repository": "owner/repo",
  "branch": "feat/my-feature"
}
```

Both status filters are optional. The result exposes persisted watch and latest-poll diagnostics.

Releasing a worktree does not implicitly unsubscribe CI Watch. Watch lifetime is managed by terminal state, TTL, explicit `unwatch`, and its own cleanup rules.

## Dispatch Auto-Arm

An actual `send` task dispatch with a feature `branch` auto-arms CI Watch as part of the dispatch lease. `next_after_ci` is optional: when absent, the subscriber still receives informational CI results, but no extra handoff target is inferred from names or roles.

```json
{
  "tool": "send",
  "instance": "dev",
  "request_kind": "task",
  "task_id": "t-...",
  "branch": "feat/my-feature",
  "next_after_ci": "reviewer",
  "message": "Implement the task and open a PR"
}
```

`bind_self` and `repo action=checkout bind:true` used as self-claim/recovery operations do not silently arm a new watch. Call `ci action=watch` explicitly if those flows need monitoring.

## Protected-Ref Exact-Head Watch

Generic watches on protected refs such as `main` and `master` remain rejected. The narrow post-merge exception pins one exact immutable GitHub SHA:

```json
{
  "tool": "ci",
  "action": "watch",
  "repository": "owner/repo",
  "branch": "main",
  "head_sha": "0123456789abcdef0123456789abcdef01234567",
  "task_id": "t-...",
  "next_after_ci": "release-owner"
}
```

All of these conditions are required:

1. `head_sha` is a full 40- or 64-hex commit ID, not an abbreviation.
2. `task_id` is non-empty.
3. `next_after_ci` is explicit and non-empty.
4. The provider is GitHub.
5. The caller is an operator or the orchestrator of every target's team.

The sidecar key includes repository, branch, and SHA. The poller queries runs for that SHA only, so a newer `main` run cannot falsely complete the pinned episode.

## Polling and Result Aggregation

Each watch is persisted below `$AGEND_HOME/ci-watches/`. The daemon groups due watches by repository where possible, polls once, then fans results out to subscribers.

For each head, it selects the latest attempt per workflow and derives an aggregate terminal result. Pending runs keep the watch active. A head change resets feature-branch run tracking; notifications are deduplicated by immutable head and terminal episode.

The configured interval is adapted to provider quota:

| Remaining quota | Effective interval |
|---|---:|
| More than 50% | 1× baseline |
| 10%–50% | 2× baseline |
| 10% or less | 4× baseline |

The multiplier is capped at 4×. Providers without usable quota headers keep the baseline interval.

After three consecutive repository-level rate-limit/provider skips, subscribers receive `[ci-watch-stalled]`. A later successful poll emits `[ci-watch-resumed]`.

## Subscribers and Delivery

- Multiple instances share one watch and one poll stream.
- Delivery skips a subscriber that is absent from both the runtime registry and fleet roster.
- Newer branch notifications supersede older pending rows for the same delivery class where applicable.
- `next_after_ci` produces the action handoff; ordinary subscribers receive informational CI events.
- A terminal exact-head watch is removed after its pinned run reaches a terminal result.

The `send` field `triaged:{head,job,reason?}` currently records a durable triage ledger entry. It requires both `head` and `job`. That ledger is an audit/data-layer surface today; it does not yet promise that every duplicate notification path will be suppressed.

## Conflict Detection

On watch creation and later polling, the daemon examines PR mergeability when the provider supports it. A transition to `CONFLICTING` emits `[ci-conflict-detected]` to subscribers. Unknown mergeability fails safe and does not fabricate a conflict result.

## Lifetime and Cleanup

- A subscription refreshes its 72-hour expiry.
- Terminal inactivity is subject to the same 72-hour cleanup window.
- A seven-day absolute age cap prevents continuously refreshed leaked watches.
- Startup sweep removes watches that expired while the daemon was down.
- Terminal PR/CI paths may remove their watch earlier.
- Explicit `unwatch` removes only the caller; removing the final subscriber leaves a non-polled opt-out tombstone until terminal cleanup, re-watch, or its tombstone age backstop.

## Provider and Credential Rules

Provider detection supports GitHub, GitLab, and Bitbucket Cloud. Explicit `bitbucket_server` is currently rejected. Exact-head protected watches are GitHub-only.

For GitHub, token discovery is:

1. `GITHUB_TOKEN`;
2. authenticated `gh` CLI (`gh auth status`, then `gh auth token`);
3. unauthenticated access.

`GH_TOKEN` is not part of this discovery chain. Discovery is cached once per daemon process, so restart the daemon after `gh auth login` or token rotation. Without a token, `watch` returns a `setup_warning`; GitHub's usual unauthenticated allowance is about 60 requests/hour versus 5,000/hour when authenticated.

Custom `ci_provider_url` credentials are sent only to trusted HTTPS SaaS hosts, loopback, or hosts explicitly allowed by `AGEND_CI_TRUSTED_HOSTS`. An untrusted custom host is polled without the forge token and produces a warning rather than receiving credentials.

## Source Pointers

- `src/mcp/handlers/ci/watch.rs` — watch/unwatch validation, persistence, subscriber removal, and opt-out tombstone
- `src/daemon/ci_watch/` — polling, providers, aggregation, delivery, stall detection, and cleanup
- `src/github_token.rs` — GitHub credential discovery and cache
- `src/mcp/handlers/dispatch_hook/` — task-dispatch auto-arm
