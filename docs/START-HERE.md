[繁體中文](START-HERE.zh-TW.md)

# Start here

> **Status:** Current goal router
> **Audience:** Operators, agents, reviewers, and incident responders
> **Authority:** The linked topic guide plus protected-main source/tests
> **Last verified:** 2026-09-22 at `main@62b28f36`

Choose the goal that matches the work. Open one guide first; follow its links
only when the task requires more detail.

## Operator

**REQUIRED:** Start with [Quick Start](FEATURE-quickstart.md), then [Fleet
Configuration](FEATURE-fleet.md). Use [CLI](CLI.md) for commands and [Usage](USAGE.md)
for daily operation.

**STOP:** For a production symptom, use the Incident route below instead of
changing fleet state from a feature guide.

## Agent

**REQUIRED:** Read [Fleet Development Protocol](FLEET-DEV-PROTOCOL.md). Then use
[Communication](FEATURE-communication.md) for messages, [Task Board](FEATURE-task-board.md)
for durable work, and [Worktree Isolation](FEATURE-worktree.md) for branch-bound work.

**OPTIONAL:** Use [MCP Tools](MCP-TOOLS.md) when you need exact actions or fields.

## Reviewer

**REQUIRED:** Start with [Source-of-Truth Matrix](SOURCE-OF-TRUTH.md), then check
[MCP Tools](MCP-TOOLS.md) and the relevant feature guide.

**OPTIONAL:** Use [Health](FEATURE-health.md), [Recovery Stages](RECOVERY-STAGES.md),
and the bilingual invariant when validating operational claims.

## Incident

**REQUIRED:** Start with [Runbook](RUNBOOK.md), then [Diagnostics](FEATURE-diagnostics.md)
and [Health](FEATURE-health.md). For restart or hung-agent behavior, read
[Recovery Stages](RECOVERY-STAGES.md) and [Worktree Isolation](FEATURE-worktree.md).

**STOP:** Treat historical audit sections as evidence, not live instructions.
