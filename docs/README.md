[繁體中文](README.zh-TW.md)

# AgEnD Terminal — Documentation

Everything beyond the [project README](../README.md), mapped by topic. Every
repository-authored Markdown document is maintained as an English /
Traditional-Chinese pair; neither language is allowed to silently omit a
section, table, code block, or link target.

> New here? Start with [Quick Start](FEATURE-quickstart.md), then
> [Fleet Configuration](FEATURE-fleet.md).

## Placement and maintenance policy

- The repository root contains only the `README`, `CONTRIBUTING`,
  `CHANGELOG`, and runtime `CLAUDE` pairs.
- Maintained general documentation is flat in `docs/`. Stable filename
  prefixes such as `FEATURE-` identify a family without creating
  link-heavy directory nesting.
- Platform-owned templates stay under `.github/`; loader-owned
  specifications stay beside their consumers under `skills/` or
  `tests/fixtures/`.
- Every English `name.md` has a same-directory
  `name.zh-TW.md` twin with reciprocal language navigation and matching
  structure.
- `vendor/agentic-git` is a pinned git submodule with independent
  documentation ownership. Its files are not superproject documents.
- Plans, audits, review snapshots, and completed incident notes do not remain
  in the active tree. Durable current rules belong in the maintained document
  that owns the topic.

The `docs_bilingual_invariant` integration test enforces the placement,
pairing, navigation, heading, code-fence, table, link-target, and index rules.
Natural-language quality still requires bilingual review.

## Historical records

Git, pull requests, and issues are the history system; the current branch is
not a second archive. The last tree before the documentation consolidation is:

```sh
git show fb7ba195854e12d787d8daf9999c2dd128b7bfd2:<former-path>
```

Use that command to retrieve any former file under `docs/archived/`,
`docs/audit/`, `docs/design/`, or a removed point-in-time document.
For the decision trail, use the issue/PR named in that artifact or
`git log -- <former-path>`. Historical text is evidence of what was
believed then, not a current contract.

## Getting started

| Document | EN | 中文 | Covers |
|---|---|---|---|
| Quick Start | [EN](FEATURE-quickstart.md) | [中文](FEATURE-quickstart.zh-TW.md) | Install through first live fleet |
| Fleet Configuration | [EN](FEATURE-fleet.md) | [中文](FEATURE-fleet.zh-TW.md) | `fleet.yaml`, backends, roles, teams, and workspaces |
| Usage Guide | [EN](USAGE.md) | [中文](USAGE.zh-TW.md) | Daily operation and common flows |
| CLI Reference | [EN](CLI.md) | [中文](CLI.zh-TW.md) | Commands and flags |

## Feature guides

### Core

| Document | EN | 中文 | Covers |
|---|---|---|---|
| Agent Interaction | [EN](FEATURE-agent-interaction.md) | [中文](FEATURE-agent-interaction.zh-TW.md) | Input, output, prompts, and capture |
| TUI Interface | [EN](FEATURE-tui.md) | [中文](FEATURE-tui.zh-TW.md) | Panes, tabs, rendering, and keybindings |
| Skills System | [EN](FEATURE-skills.md) | [中文](FEATURE-skills.zh-TW.md) | Installing and using skills |
| Communication | [EN](FEATURE-communication.md) | [中文](FEATURE-communication.zh-TW.md) | `send`, `inbox`, and `reply` |
| Task Board | [EN](FEATURE-task-board.md) | [中文](FEATURE-task-board.zh-TW.md) | Durable work tracking |
| Teams | [EN](FEATURE-teams.md) | [中文](FEATURE-teams.zh-TW.md) | Agent grouping and coordination scope |
| Worktree Isolation | [EN](FEATURE-worktree.md) | [中文](FEATURE-worktree.zh-TW.md) | Branch-bound managed worktrees |

### Advanced

| Document | EN | 中文 | Covers |
|---|---|---|---|
| CI Watch | [EN](FEATURE-ci-watch.md) | [中文](FEATURE-ci-watch.zh-TW.md) | Hosted-CI monitoring and handoff |
| Health & Monitoring | [EN](FEATURE-health.md) | [中文](FEATURE-health.zh-TW.md) | Liveness, hung detection, and blocked reasons |
| Dispatch Idle | [EN](FEATURE-dispatch-idle.md) | [中文](FEATURE-dispatch-idle.zh-TW.md) | Idle work pickup and tracking |
| Channels | [EN](FEATURE-channels.md) | [中文](FEATURE-channels.zh-TW.md) | Telegram and Discord adapters |
| Decisions | [EN](FEATURE-decisions.md) | [中文](FEATURE-decisions.zh-TW.md) | Scope decisions and corrections |
| Schedules & Deployments | [EN](FEATURE-schedules.md) | [中文](FEATURE-schedules.zh-TW.md) | Recurring work and deployment templates |

### Maintenance

| Document | EN | 中文 | Covers |
|---|---|---|---|
| Service Management | [EN](FEATURE-service.md) | [中文](FEATURE-service.zh-TW.md) | Launch paths and OS supervisors |
| Diagnostics | [EN](FEATURE-diagnostics.md) | [中文](FEATURE-diagnostics.zh-TW.md) | Logs, snapshots, and troubleshooting |
| Configuration | [EN](FEATURE-configuration.md) | [中文](FEATURE-configuration.zh-TW.md) | Runtime settings and ownership |

## Architecture and reference

| Document | EN | 中文 | Covers |
|---|---|---|---|
| Architecture Map | [EN](architecture.md) | [中文](architecture.zh-TW.md) | Current subsystems and reading path |
| Architecture-14 Ledger | [EN](ARCHITECTURE-14-LEDGER.md) | [中文](ARCHITECTURE-14-LEDGER.zh-TW.md) | Current convergence outcomes and evidence |
| Daemon Lock Ordering | [EN](DAEMON-LOCK-ORDERING.md) | [中文](DAEMON-LOCK-ORDERING.zh-TW.md) | Lock hierarchy and deadlock prevention |
| Hung-State Contract | [EN](HUNG-STATE-TRANSITIONS.md) | [中文](HUNG-STATE-TRANSITIONS.zh-TW.md) | Detection transitions and productive-output gate |
| Recovery Stages | [EN](RECOVERY-STAGES.md) | [中文](RECOVERY-STAGES.zh-TW.md) | Staged automatic recovery |
| Backend Matrix | [EN](BACKEND-CAPABILITY-MATRIX.md) | [中文](BACKEND-CAPABILITY-MATRIX.zh-TW.md) | Backend signals, providers, resume, and MCP |
| MCP Tools | [EN](MCP-TOOLS.md) | [中文](MCP-TOOLS.zh-TW.md) | Tool registry and bridge/proxy contract |
| Environment Variables | [EN](env-vars.md) | [中文](env-vars.zh-TW.md) | Supported environment contract |
| Git Behavior | [EN](GIT-BEHAVIOR.md) | [中文](GIT-BEHAVIOR.zh-TW.md) | Agent git policy and shim behavior |
| Compatibility | [EN](COMPATIBILITY.md) | [中文](COMPATIBILITY.zh-TW.md) | On-disk format guarantees |
| Known Issues | [EN](KNOWN_ISSUES.md) | [中文](KNOWN_ISSUES.zh-TW.md) | Deliberately deferred user-visible issues |
| Skills Reference | [EN](SKILLS.md) | [中文](SKILLS.zh-TW.md) | Skill catalog and lock format |
| Source of Truth | [EN](SOURCE-OF-TRUTH.md) | [中文](SOURCE-OF-TRUTH.zh-TW.md) | Code, docs, and evidence authority |

## Operations and governance

| Document | EN | 中文 | Covers |
|---|---|---|---|
| Fleet Development Protocol | [EN](FLEET-DEV-PROTOCOL.md) | [中文](FLEET-DEV-PROTOCOL.zh-TW.md) | Normative multi-agent workflow |
| Solo Profile | [EN](SOLO-PROFILE.md) | [中文](SOLO-PROFILE.zh-TW.md) | Proportional application for one agent |
| Incident Runbook | [EN](RUNBOOK.md) | [中文](RUNBOOK.zh-TW.md) | Incidents, CI outage, and clean-agent recipe |
| GitLab Mirror | [EN](GITLAB-MIRROR-SETUP.md) | [中文](GITLAB-MIRROR-SETUP.zh-TW.md) | Backup CI mirror setup |
| Releasing | [EN](RELEASING.md) | [中文](RELEASING.zh-TW.md) | Versioning, publishing, and smoke checks |

## Repository and co-located documents

| Document | EN | 中文 | Location |
|---|---|---|---|
| Project README | [EN](../README.md) | [中文](../README.zh-TW.md) | repository root |
| Contributing | [EN](../CONTRIBUTING.md) | [中文](../CONTRIBUTING.zh-TW.md) | repository root |
| Changelog | [EN](../CHANGELOG.md) | [中文](../CHANGELOG.zh-TW.md) | repository root |
| Claude Runtime Notes | [EN](../CLAUDE.md) | [中文](../CLAUDE.zh-TW.md) | repository root |
| Telegram Setup Skill | [EN](../skills/setup-telegram/SKILL.md) | [中文](../skills/setup-telegram/SKILL.zh-TW.md) | loader-owned skill directory |
| Fixture Capture Playbook | [EN](../tests/fixtures/state-replay/CAPTURE-RECIPES.md) | [中文](../tests/fixtures/state-replay/CAPTURE-RECIPES.zh-TW.md) | fixture directory |
| Bug Template | [EN](../.github/ISSUE_TEMPLATE/bug_report.md) | [中文](../.github/ISSUE_TEMPLATE/bug_report.zh-TW.md) | GitHub convention |
| Feature Template | [EN](../.github/ISSUE_TEMPLATE/feature_request.md) | [中文](../.github/ISSUE_TEMPLATE/feature_request.zh-TW.md) | GitHub convention |
| Pull Request Template | [EN](../.github/PULL_REQUEST_TEMPLATE.md) | [中文](../.github/PULL_REQUEST_TEMPLATE.zh-TW.md) | GitHub convention |
