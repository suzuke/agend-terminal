# Competitor Comparison: AgEnD-Terminal vs Multica vs OpenAB

## Overview

| Project | Positioning | Stars | Language |
|---------|-------------|-------|----------|
| **AgEnD-Terminal** | Operator real-time command system — TUI multi-agent fleet coordination with instant control and review chains | — | Rust |
| **[Multica](https://github.com/multica-ai/multica)** | Agent HR management platform — kanban-style task assignment, progress tracking, multi-user shared agent pool | 32.2k | Go + TypeScript |
| **[OpenAB](https://github.com/openabdev/openab)** | Chat-first ACP bridge — routes Discord/Slack messages to any ACP-compatible coding CLI over stdio JSON-RPC | 515 | Rust |

## Architecture

```
AgEnD-Terminal:
┌─────────────┐
│  Operator   │ (TUI / Telegram)
└──────┬──────┘
       │ direct PTY control + MCP tools
┌──────┴──────┐
│   Daemon    │ (single process, file-based state)
├─────────────┤
│ agent-1..N  │ PTY sessions + inter-agent messaging
└─────────────┘

Multica:
┌──────────┐     ┌──────────┐     ┌────────────┐
│ Web UI   │────►│ Go Server│────►│ PostgreSQL │
│ Next.js  │     │ REST+WS  │     └────────────┘
└──────────┘     └────┬─────┘
                      │ poll/heartbeat
                 ┌────┴─────┐
                 │  Daemon  │ (per machine, PTY spawn)
                 └──────────┘

OpenAB:
┌──────────┐  WebSocket/webhook  ┌──────────┐  ACP (JSON-RPC/stdio)  ┌─────────────┐
│ Discord  │◄───────────────────►│  openab  │◄──────────────────────►│ coding CLI  │
│ Slack    │                     │  (Rust)  │                        │ (any ACP)   │
│ Telegram │                     └──────────┘                        └─────────────┘
└──────────┘                     Session Pool
```

## Feature Matrix

| Feature | AgEnD-Terminal | Multica | OpenAB |
|---------|---------------|---------|--------|
| **Communication Protocol** | PTY + MCP tools | REST + WebSocket | ACP (JSON-RPC over stdio) |
| **Chat Platforms** | Telegram | Web UI only | Discord, Slack, Telegram, LINE, Feishu, Google Chat, WeCom |
| **Multi-agent Coordination** | Fleet protocol (lead/dev/reviewer chain) | Squads + Leader routing | Bot-to-bot messaging (basic) |
| **Task Management** | Task board + event sourcing | Issue board + full lifecycle | ❌ None (session pool only) |
| **Git/Branch Management** | Deep integration (worktree bind/release/gc) | ❌ None | ❌ None |
| **CI Integration** | CI watch + auto-notify chain | Not mentioned | ❌ None |
| **Review Chain** | ✅ Structured (VERIFIED/REJECTED) | ❌ None | ❌ None |
| **Real-time Control** | ✅ interrupt/pane_snapshot/replace | ❌ (assign-and-wait) | ❌ (fire-and-forget) |
| **TUI Dashboard** | ✅ Multi-pane, tab/split layout | ❌ | ❌ |
| **Deployment Model** | Local daemon (zero infra) | Docker self-host / SaaS | Kubernetes (cloud-native) |
| **Session Management** | Per-agent worktree | Per-task workspace dir | Pool (max_sessions, TTL) |
| **Cron/Schedule** | ✅ Cron + one-shot | ✅ Autopilots | ✅ Config-driven cron |
| **Supported Backend CLIs** | 5-6 (via PTY) | 11 (via PTY) | 12+ (via ACP) |
| **Voice/Media** | Images (Telegram) | ❌ | STT, images, files |
| **Skill System** | ✅ SKILL.md + skills-lock.json | ✅ .agents/skills/ | ❌ None |

## Philosophy Comparison

| | AgEnD-Terminal | Multica | OpenAB |
|---|---|---|---|
| **What is an agent?** | Special forces member | Employee | Chatbot |
| **Control model** | Commander-driven | Task-assignment-driven | Conversation-driven |
| **Complexity** | High (operator → fleet → review chain → result) | Medium (team → agent pool → report) | Low (1 user → 1 session → response) |
| **Target user** | Single power user | Team PM + engineers | Any Discord/Slack user |
| **Scaling direction** | Deeper fleet intelligence | More people, more agents | More chat platforms |

## Unique Strengths

### AgEnD-Terminal

1. **Real-time TUI control** — Multi-pane view of all agents simultaneously. `pane_snapshot` for live terminal output, `interrupt` to stop bad work, `replace_instance` for instant reset. No other tool offers this level of operator-in-the-loop control.
2. **Structured review chain** — VERIFIED/REJECTED verdict protocol, auto-dispatch reviewer, internal retry transparent to operator.
3. **Deep git/worktree integration** — Per-agent branch isolation, daemon-managed bind/release lifecycle, automated GC.
4. **Inter-agent direct messaging** — Agents query/report to each other without going through a central server. Suitable for complex collaboration (review chains, fixup loops).
5. **Zero infrastructure** — No PostgreSQL, no Docker, no web server. Single binary daemon with file-based state.

### Multica

1. **Product completeness** — Full web UI, issue board, comment threads, execution history, workspace isolation. 3,236 commits, 75 releases.
2. **Multi-user / multi-workspace** — Built for teams. Multiple people can assign work to shared agent pools.
3. **Non-technical accessibility** — Web UI means PMs and non-engineers can interact with agents.
4. **Ecosystem scale** — 32k stars, active community, extensive documentation.
5. **Desktop + Mobile + Web** — Three-platform coverage with shared component architecture.

### OpenAB

1. **ACP protocol standardization** — Uses Agent Client Protocol (JSON-RPC over stdio) instead of PTY hacks. Structured tool calls, thinking, and permissions.
2. **Chat platform breadth** — 7 platforms (Discord/Slack/Telegram/LINE/Feishu/Google Chat/WeCom) through a unified gateway architecture.
3. **Cloud-native (K8s)** — Helm charts, PVC, per-backend Dockerfiles. Designed for multi-tenant scaling.
4. **Edit-streaming** — Live-updates Discord messages every 1.5s as tokens arrive.
5. **Lowest barrier to entry** — @mention a bot in Discord and you're using a coding agent. Zero setup for end users.

## What AgEnD Can Learn

### From Multica

- **Autopilot (schedule → auto-task → auto-assign)** — Completed partially; schedule can trigger messages but not auto-create tasks with lifecycle tracking.
- **Workspace GC (artifact-only cleanup)** — ✅ Implemented.
- **Per-agent timeout** — ✅ Implemented.
- **Task metadata KV** — ✅ Implemented.
- **Boot orphan sweep** — Crash recovery for in_progress tasks whose daemon died.
- **Max concurrent agents** — Resource protection guard.

### From OpenAB

- **ACP protocol support** — As coding CLIs converge on ACP (`--acp` mode), AgEnD's PTY-only approach may miss structured output (tool calls, thinking tokens, permissions). Consider optional ACP backend mode alongside PTY.
- **Session pool TTL + max sessions** — Safer than unlimited agent spawning.
- **Gateway architecture for chat platforms** — OpenAB's standalone Custom Gateway is cleaner than hardcoding Telegram into the daemon. AgEnD already has `channel/` abstraction but could benefit from a more pluggable design.

## What AgEnD Should NOT Copy

| From Multica | Why Not |
|---|---|
| Web UI | TUI is the core differentiator; adding web dilutes focus |
| PostgreSQL | File-based state is correct for single-operator use |
| Multi-user auth | Single-operator by design |
| REST API | YAGNI — no second network client exists |

| From OpenAB | Why Not |
|---|---|
| K8s deployment | Local tool, no cloud infra needed |
| Discord/Slack as primary UX | AgEnD is not a chatbot |
| Fire-and-forget execution | Defeats the purpose of operator control |

## Summary

The three projects occupy different abstraction layers:

- **OpenAB** = Communication layer (chat ↔ agent bridge)
- **Multica** = Management layer (task → agent → report)
- **AgEnD-Terminal** = Control layer (operator → fleet → quality gate → result)

AgEnD-Terminal's moat is **real-time TUI control + structured fleet coordination**. This requires deep daemon-PTY integration (`vterm.rs`, `pane_factory.rs`, `layout/`, `keybinds.rs`, `render/`) that cannot be retrofitted into the other architectures — it was designed from day one for this purpose.
