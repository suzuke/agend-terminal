[繁體中文](README.zh-TW.md)

# AgEnD Terminal — Documentation

Everything beyond the [project README](../README.md), mapped by topic. Most
user-facing guides are bilingual — pick **EN** or **中文**. Internal design and
process notes are English-only.

> New here? Start with the [Quick Start Guide](FEATURE-quickstart.md), then the
> [Fleet Configuration](FEATURE-fleet.md) reference.

## Getting Started

| Document | EN | 中文 | What it covers |
|---|---|---|---|
| Quick Start | [EN](FEATURE-quickstart.md) | [中文](FEATURE-quickstart.zh-TW.md) | First-run walkthrough, from install to a live fleet |
| Fleet Configuration | [EN](FEATURE-fleet.md) | [中文](FEATURE-fleet.zh-TW.md) | The `fleet.yaml` schema — backends, roles, teams, working dirs |
| Usage Guide | [EN](USAGE.md) | [中文](USAGE.zh-TW.md) | Day-to-day operation, Telegram binding, common flows |
| CLI Reference | [EN](CLI.md) | [中文](CLI.zh-TW.md) | Every `agend-terminal` subcommand and flag |

## Feature Guides

### Core

| Document | EN | 中文 | What it covers |
|---|---|---|---|
| Agent Interaction | [EN](FEATURE-agent-interaction.md) | [中文](FEATURE-agent-interaction.zh-TW.md) | Driving an agent: input injection, output capture, prompts |
| TUI Interface | [EN](FEATURE-tui.md) | [中文](FEATURE-tui.zh-TW.md) | The multi-pane terminal UI, panes, tabs, keybindings |
| Skills System | [EN](FEATURE-skills.md) | [中文](FEATURE-skills.zh-TW.md) | Installing and using skills across backends |
| Communication | [EN](FEATURE-communication.md) | [中文](FEATURE-communication.zh-TW.md) | `send` / `inbox` / `reply` — how agents talk |
| Task Board | [EN](FEATURE-task-board.md) | [中文](FEATURE-task-board.zh-TW.md) | Shared work tracking: create, claim, done |
| Teams | [EN](FEATURE-teams.md) | [中文](FEATURE-teams.zh-TW.md) | Grouping agents and scoping coordination |
| Git Worktree Isolation | [EN](FEATURE-worktree.md) | [中文](FEATURE-worktree.zh-TW.md) | Per-agent worktrees and how they are pooled |

### Advanced

| Document | EN | 中文 | What it covers |
|---|---|---|---|
| CI Watch | [EN](FEATURE-ci-watch.md) | [中文](FEATURE-ci-watch.zh-TW.md) | Monitoring GitHub Actions from the fleet |
| Health & Monitoring | [EN](FEATURE-health.md) | [中文](FEATURE-health.zh-TW.md) | Liveness, hung detection, blocked-reason reporting |
| Dispatch Idle Tracking | [EN](FEATURE-dispatch-idle.md) | [中文](FEATURE-dispatch-idle.zh-TW.md) | When and how idle agents pick up work |
| Channels (Telegram/Discord) | [EN](FEATURE-channels.md) | [中文](FEATURE-channels.zh-TW.md) | Remote control and notifications over chat |
| Decision Records | [EN](FEATURE-decisions.md) | [中文](FEATURE-decisions.zh-TW.md) | Recording scope decisions and corrections |
| Schedules & Deployments | [EN](FEATURE-schedules.md) | [中文](FEATURE-schedules.zh-TW.md) | Cron-style routines and template deployments |

### Maintenance

| Document | EN | 中文 | What it covers |
|---|---|---|---|
| Service Management | [EN](FEATURE-service.md) | [中文](FEATURE-service.zh-TW.md) | Installing the daemon as a system service |
| Diagnostics | [EN](FEATURE-diagnostics.md) | [中文](FEATURE-diagnostics.zh-TW.md) | Logs, snapshots, and troubleshooting tools |
| Configuration | [EN](FEATURE-configuration.md) | [中文](FEATURE-configuration.zh-TW.md) | Runtime config knobs and where they live |

## Reference

| Document | EN | 中文 | What it covers |
|---|---|---|---|
| Architecture | [EN](architecture.md) | [中文](architecture.zh-TW.md) | Daemon design, worktree pool, health monitor, channel lifecycle |
| MCP Tools | [EN](MCP-TOOLS.md) | [中文](MCP-TOOLS.zh-TW.md) | All 30 agent-coordination MCP tools, by category |
| Environment Variables | [EN](env-vars.md) | [中文](env-vars.zh-TW.md) | Every `AGEND_*` variable and its effect |
| Git Behavior | [EN](GIT-BEHAVIOR.md) | [中文](GIT-BEHAVIOR.zh-TW.md) | What the daemon changes in a spawned agent's git env |
| Compatibility Policy | [EN](COMPATIBILITY.md) | [中文](COMPATIBILITY.zh-TW.md) | On-disk format stability guarantees under `$AGEND_HOME` |
| Known Issues | [EN](KNOWN_ISSUES.md) | [中文](KNOWN_ISSUES.zh-TW.md) | Intentionally-deferred items — check before filing an issue |
| Skills Reference | [EN](SKILLS.md) | [中文](SKILLS.zh-TW.md) | The skills catalog and lock format |
| Launch Flows | [EN](launch-flows.md) | [中文](launch-flows.zh-TW.md) | Every way to start the daemon; cold vs warm start |
| Recipe: Clean Claude Instance | [EN](RECIPE-clean-claude-instance.md) | [中文](RECIPE-clean-claude-instance.zh-TW.md) | Spawning a Claude Code agent without global instructions |
| Competitor Comparison | [EN](competitor-comparison.md) | [中文](competitor-comparison.zh-TW.md) | How AgEnD-Terminal compares to similar tools |

## Contributing & Operations

| Document | EN | 中文 | What it covers |
|---|---|---|---|
| Contributing | [EN](../CONTRIBUTING.md) | [中文](../CONTRIBUTING.zh-TW.md) | Dev setup, branch/PR workflow, review expectations |
| Releasing | [EN](RELEASING.md) | [中文](RELEASING.zh-TW.md) | Cutting a release, versioning, publishing |
| Release Smoke Checklist | [EN](release-smoke-checklist.md) | [中文](release-smoke-checklist.zh-TW.md) | Manual checks before a release goes out |
| Runbook | [EN](RUNBOOK.md) | [中文](RUNBOOK.zh-TW.md) | Operational playbook for common incidents |
| CI-Down SOP | [EN](CI-DOWN-SOP.md) | [中文](CI-DOWN-SOP.zh-TW.md) | What to do when CI is unavailable |
| GitLab Mirror Setup | [EN](GITLAB-MIRROR-SETUP.md) | [中文](GITLAB-MIRROR-SETUP.zh-TW.md) | Mirroring the repo to GitLab CI |
| Lint Discipline | [EN](LINT-DISCIPLINE.md) | [中文](LINT-DISCIPLINE.zh-TW.md) | Clippy/rustfmt conventions enforced in CI |
| Changelog | [EN](../CHANGELOG.md) | [中文](../CHANGELOG.zh-TW.md) | Release-by-release change history |

## Internal Design & Process

Deep-dive design notes and the multi-agent development protocol. These track
implementation detail for contributors working inside the codebase and are
maintained in English only.

<details>
<summary><strong>Architecture deep-dives</strong></summary>

- [Architecture Groups](ARCHITECTURE-GROUPS.md) — subsystem grouping of the source tree
- [Architecture Quick Start](ARCHITECTURE-QUICK-START.md) — orientation for new contributors
- [Daemon Lock Ordering](DAEMON-LOCK-ORDERING.md) — lock acquisition order and deadlock avoidance
- [Hung State Transitions](HUNG-STATE-TRANSITIONS.md) — the hung-detection state machine
- [Recovery Stages](RECOVERY-STAGES.md) — crash-recovery staging
- [MCP↔Daemon Proxy Contract](MCP-DAEMON-PROXY-CONTRACT.md) — the bridge protocol
- [Skill System Architecture](skill-system-architecture.md) — how skills are resolved per backend
- [Project Feature Groups](project-feature-groups.md) — feature-to-module mapping
- [Loop Engineering Mapping](loop-engineering-mapping.md) — supervisor loop responsibilities
- [F685 Fixture Corpus](F685-FIXTURE-CORPUS.md) — state-replay fixture corpus
- [F9 Productive-Output Gate](F9-PRODUCTIVE-OUTPUT-GATE.md) — the productive-output gate design
</details>

<details>
<summary><strong>Fleet development protocol & process</strong></summary>

- [Fleet Dev Protocol](FLEET-DEV-PROTOCOL.md) — the multi-agent coordination contract
- [Reviewer Contract v0.1](REVIEWER-CONTRACT-v0.1.md) — reviewer verdict/evidence rules
- [Protocol: Verdict Delivery Confirmation](PROTOCOL-VERDICT-DELIVERY-CONFIRMATION.md)
- [Protocol: Parallel Filler Opt-In Schema](PROTOCOL-PARALLEL-FILLER-OPT-IN-SCHEMA.md)
- [Process: Lead Closeout & Claim-State Discipline](PROCESS-LEAD-CLOSEOUT-CLAIM-STATE-DISCIPLINE.md)
- [Process: LOC Estimation Methodology](PROCESS-LOC-ESTIMATION-METHODOLOGY.md)
- [Process: systemd / loginctl Operator Hardening](PROCESS-SYSTEMD-LOGINCTL-OPERATOR-HARDENING.md)
- [Process: Tier-2 Dual-Review Lessons Learned](PROCESS-TIER2-DUAL-REVIEW-LESSONS-LEARNED.md)
- [Refactor Plan](REFACTOR-PLAN.md) — the active architecture-pass plan
</details>

> Superseded and point-in-time documents live under [`archived/`](archived/).
