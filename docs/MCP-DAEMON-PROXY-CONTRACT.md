# MCP Daemon Proxy Contract

> Status: implemented. This document describes the current contract between an MCP client, `agend-mcp-bridge`, and the AgEnD daemon.

## Architecture

```text
MCP client
  │ stdin/stdout: newline-delimited JSON-RPC
  ▼
agend-mcp-bridge
  │ loopback TCP: authenticated newline-delimited JSON
  ▼
AgEnD daemon (`/mcp` dispatcher)
```

The daemon is the only authority for the tool registry, authorization, task state, and side effects. The bridge does not contain a second tool implementation and never executes a tool locally when the daemon is unavailable.

## Client-Side Framing

The bridge reads and writes one JSON object per line on stdin/stdout. `Content-Length` framing is not supported. Removing that fallback avoids blocking reads, unbounded allocations, negative-length edge cases, and drip-feed attacks.

The bridge handles these protocol requests locally:

- `initialize`
- `ping`
- JSON-RPC notifications

It proxies `tools/list` and `tools/call` to the daemon. `tools/list` includes the calling instance so the daemon can return the role-filtered tool surface.

## Daemon Transport and Authentication

The bridge discovers the active run directory, rejecting stale directories whose daemon PID is no longer alive on Unix. It then opens a persistent loopback TCP connection and authenticates with the daemon cookie. The handshake includes the bridge PID.

The TCP stream also uses one JSON object per line. Authentication must succeed before any MCP request is accepted.

Timeouts are layered:

| Boundary | Timeout | Purpose |
|---|---:|---|
| Daemon, before authentication | 5 seconds | Bound idle or partial authentication attempts |
| Bridge, waiting for a daemon response | 120 seconds | Bound a stalled proxy request |
| Daemon, after authentication | No session read timeout | Permit long-lived idle MCP sessions |
| Daemon tool execution | 5 / 30 / 60 seconds | Per-tool fast, default, and slow execution bands |

The daemon checks the authenticated bridge PID approximately every two seconds and closes the session if that process exits. TCP EOF remains the normal clean-close path.

## Request Identity and Retry

Every proxied request receives a UUIDv4 `request_id`. On a retryable transport failure, the bridge reconnects and retries exactly once with the same ID. The daemon deduplicates that ID, so the retry cannot apply the same side effect twice.

Startup transport discovery and connection failures are retried every 100 ms for up to 30 seconds. Daemon application errors are returned immediately and are not treated as transport failures.

The bridge is deliberately near-zero-state, not stateless. It retains:

- the persistent daemon connection; and
- one successful recent `tools/call` result for 500 ms, used only to deduplicate an identical consecutive call.

Failed calls never seed the recent-call cache.

## Tool Execution Timeouts

The daemon assigns each registered tool to a fast (5 s), default (30 s), or slow (60 s) execution band.

- If a read-only or idempotent operation exceeds its band, the daemon returns a retryable timeout error.
- If a side-effecting operation exceeds its band, execution continues in the background and the daemon returns `accepted_in_progress`. The caller must not resend that operation; it should observe the task, inbox, or relevant status tool instead.

The 120-second bridge response timeout is a transport safety net, not the tool execution budget.

## Degradation Behavior

| Failure | Observable behavior |
|---|---|
| Daemon unavailable at startup | Bridge retries for up to 30 seconds, then returns a visible JSON-RPC error |
| Connection breaks during a proxied request | Bridge reconnects and retries once with the same `request_id` |
| Retry also fails | Caller receives a visible JSON-RPC error |
| Daemon returns an application error | Error is propagated without a transport retry |
| Bridge process exits | Daemon detects EOF or PID death and closes the session |

There is no in-process or filesystem fallback for `tools/list` or `tools/call`. A successfully persisted operation remains durable in the daemon, but a call that never reached the daemon is not executed elsewhere.

## Security and Consistency Invariants

1. Only the authenticated daemon dispatches MCP tools.
2. The bridge never keeps a divergent local registry or side-effect path.
3. Both stdio and TCP framing are bounded newline-delimited JSON.
4. A transparent retry reuses the original request ID and occurs at most once.
5. Role filtering and authorization are evaluated by the daemon on the current state.
6. Side-effect timeout responses explicitly prevent blind resubmission.
7. Dead bridge processes do not leave indefinitely authenticated sessions.

## Source Pointers

- `src/bin/agend-mcp-bridge.rs` — stdio framing, discovery, authentication, connection reuse, request IDs, and transparent retry
- `src/api/mod.rs` — socket authentication, pre-auth timeout, and peer-PID monitoring
- `src/api/handlers/mcp_proxy.rs` — daemon MCP proxy dispatcher and execution timeout bands
- `src/mcp/registry.rs` — authoritative tool registry and execution classes
