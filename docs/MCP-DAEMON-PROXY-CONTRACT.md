# MCP Daemon Proxy Contract

Sprint 25 P0 Option F — architectural contract for the MCP subprocess↔daemon proxy.

## Architecture

```
┌─────────────┐    stdio     ┌──────────────────┐    TCP/cookie    ┌─────────────┐
│ Agent Backend│◄────────────►│ agend-mcp-bridge │◄────────────────►│   Daemon    │
│ (Claude/Kiro)│  MCP JSON-RPC│  (zero state)    │  NDJSON + auth   │ handle_tool │
└─────────────┘              └──────────────────┘                  └─────────────┘
```

### Subprocess (agend-mcp-bridge)

- **Zero state**: no globals, no file I/O beyond daemon discovery, no channel state
- **Pure transport**: Content-Length framed stdin/stdout ↔ NDJSON over TCP
- **Handles locally**: `initialize`, `ping`, `notifications/*` (no daemon state needed)
- **Proxies to daemon**: `tools/call`, `tools/list`
- **Persistent connection**: single TCP connection reused across calls, rebuilt on error

### Daemon (/mcp endpoint)

- **`mcp_tool` API method**: receives `{tool, arguments, instance}`, calls `handle_tool` directly
- **`mcp_tools_list` API method**: returns tool definitions
- **Process-global state available**: ACTIVE_CHANNEL, heartbeat_pair, home_dir, save_metadata
- **Auth**: existing cookie handshake (32-byte random, filesystem-permission-gated)

### Short-circuit (daemon-internal)

When MCP runs inside the daemon process (TUI mode), `is_running_inside_daemon_process()` returns true and `proxy_or_local` calls `handle_tool` directly — no TCP round-trip.

## Auth Contract

| Transport | Auth mechanism | Trust model |
|-----------|---------------|-------------|
| TCP loopback | 32-byte cookie handshake | Filesystem permissions (mode 0600) |
| instance_name | Set by daemon via AGEND_INSTANCE_NAME env | Daemon-controlled, not agent-controlled |

The cookie is issued per daemon startup and stored in `{run_dir}/api.cookie`. Only same-user processes can read it.

## Anti-bypass Invariant (5 rules)

Enforced by `tests/mcp_subprocess_is_zero_state.rs`:

1. **No state file reads**: bridge must not reference fleet.yaml, topics.json, tasks.json, etc.
2. **No crate:: imports**: bridge is a standalone binary, no daemon library dependencies
3. **No globals**: no OnceLock, lazy_static, static Mutex/RwLock/HashMap
4. **No state file paths**: no agents/, inbox, metadata references
5. **No channel state**: no active_channel, ACTIVE_CHANNEL, TelegramState

## Degradation Matrix

| Failure mode | Bridge behaviour | Operator impact |
|-------------|-----------------|-----------------|
| Daemon not running | JSON-RPC error response per call | Agent sees tool errors, can retry |
| Daemon restarts | Bridge reconnects on next call | One failed call, then recovery |
| Bridge crash | Agent backend restarts MCP server | Transparent to operator |
| Cookie mismatch | Auth rejected, error response | Restart agent to pick up new cookie |
| Network timeout | 30s read timeout, error response | Slow tool call fails cleanly |

## Performance

- **Per-call overhead**: ~0.1ms TCP loopback + cookie auth (amortized over persistent connection)
- **Connection setup**: ~1ms (TCP connect + cookie handshake, once per session)
- **Short-circuit (daemon-internal)**: 0ms overhead (direct function call)
