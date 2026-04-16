# Bug: mcp-server.js orphan processes cause CPU spin (100% per process)

## Summary

When the parent CLI process (Claude Code, Kiro, etc.) exits or crashes, `mcp-server.js` child processes become orphaned (reparented to PID 1/launchd) and enter a **CPU busy-loop at ~87-93% each**. The stdin EOF detection mechanism at lines 235-246 fails to trigger on macOS, so the process never self-terminates.

Over time, multiple restarts of CLI sessions accumulate orphan processes. In this instance: **12 orphan processes × ~90% CPU = ~1080% total CPU** (on a 16-core machine), sustained for up to 38 hours.

## Environment

- **OS**: macOS 15.5 (24F74), Apple Silicon (arm64)
- **Node.js**: v25.9.0 (Homebrew)
- **agend MCP server**: v0.3.0
- **MCP SDK**: `@modelcontextprotocol/sdk` (StdioServerTransport)

## Reproduction

1. Start an agend instance (creates a CLI + mcp-server.js child process)
2. Kill the parent CLI (e.g., `kill <cli-pid>`, or it crashes/exits)
3. Observe: mcp-server.js continues running at ~90% CPU indefinitely

## Diagnostic Evidence

### Orphan vs Healthy Process Comparison

| Attribute | Orphan (12 processes) | Healthy (8 processes) |
|---|---|---|
| PPID | `1` (launchd) | actual parent CLI PID |
| CPU | 83–93% each | 0.0% |
| State | `R` (running/spinning) | `S` (sleeping) |
| stdin (fd 0) pipe | **no peer** (broken) | has peer (`->0x...`) |
| stdout (fd 1) pipe | **no peer** (broken) | has peer |
| stderr (fd 2) pipe | **no peer** (broken) | has peer |
| Unix socket (IPC) | **0 sockets** | 1+ active sockets |
| fd 4↔5 self-pipe | intact (bidirectional) | intact (bidirectional) |

### Key Observation

Orphaned processes have **broken stdio pipes** (fd 0/1/2 have no peer) and **zero Unix sockets** (IPC to daemon lost). Despite this, the process never exits.

### Affected Processes at Time of Diagnosis (2026-04-15 07:24 UTC+8)

| PID | Workspace | Started | Elapsed | CPU | RSS |
|-----|-----------|---------|---------|-----|-----|
| 69667 | kiro-researcher-t17077 | Mon 18:00 | 1d 14h | 93.2% | 396 MB |
| 69055 | agend-feat-next | Mon 18:00 | 1d 14h | 91.0% | 401 MB |
| 69296 | agend-ts-review | Mon 18:00 | 1d 14h | 93.0% | 408 MB |
| 28575 | agend-feat-next | Tue 09:47 | 22h | 92.8% | 376 MB |
| 28948 | agend-ts-review | Tue 09:47 | 22h | 92.8% | 380 MB |
| 29290 | kiro-researcher-t17077 | Tue 09:47 | 22h | 92.9% | 397 MB |
| 45480 | agend-feat-next | Tue 13:43 | 18h | 92.9% | 371 MB |
| 45914 | agend-ts-review | Tue 13:43 | 18h | 87.1% | 387 MB |
| 46128 | kiro-researcher-t17077 | Tue 13:43 | 18h | 87.5% | 381 MB |
| 46524 | agend-feat-next | Tue 13:43 | 18h | 93.0% | 354 MB |
| 46628 | agend-ts-review | Tue 13:43 | 18h | 87.8% | 380 MB |
| 47039 | kiro-researcher-t17077 | Tue 13:43 | 18h | 89.1% | 357 MB |

**Total impact**: ~1080% CPU, ~4.6 GB RSS, sustained over 18–38 hours.

Each workspace (agend-feat-next, agend-ts-review, kiro-researcher-t17077) accumulated **4 orphans** across 3 restart cycles, indicating the bug triggers on every CLI exit.

## Root Cause Analysis

### The intended safety net (mcp-server.js:227-246)

```js
// Detect parent death:
mcp.onclose = () => { process.exit(0); };            // line 231-234
process.stdin.on("end", () => { process.exit(0); }); // line 235-237
process.stdin.on("close", () => { process.exit(0); });// line 239-241
process.stdin.on("error", () => { process.exit(0); });// line 243-246
```

### Why it fails

1. **`StdioServerTransport` owns stdin**: The MCP SDK's `StdioServerTransport` sets up its own reader on `process.stdin`. On macOS with Node.js, once stdin's write end (parent) closes, the pipe becomes a broken orphan pipe — but `StdioServerTransport`'s internal readline/stream handling may enter a tight read loop instead of emitting `'end'` or `'close'` to the process-level listeners.

2. **`mcp.onclose` never fires**: As noted in the code comment at line 227-228: *"StdioServerTransport only listens for 'data'/'error' on stdin — it never detects EOF. mcp.onclose only fires on explicit transport.close(), NOT on stdin EOF."* This is a known limitation acknowledged in the code itself.

3. **No IPC-based exit**: The IPC reconnect logic (`scheduleReconnect`) has `MAX_RECONNECT_ATTEMPTS = 20` (≈60s), after which `process.exit(1)` is called. However, if the daemon is still running and accepts the reconnection, the counter resets — even though the process is orphaned and useless (stdio is broken). The IPC layer doesn't know stdio is dead.

4. **CPU spin mechanism**: With stdin as a broken pipe and the transport in a tight loop, the Node.js event loop spins at ~90% CPU. The fd 4↔5 self-pipe keeps the event loop alive. The process is effectively stuck in an infinite busy-wait.

## Suggested Fixes

### Fix 1: Periodic parent-alive check (most robust)

```js
// Poll whether parent process is still alive (works on all platforms)
const parentPid = process.ppid;
setInterval(() => {
  if (process.ppid === 1 || process.ppid !== parentPid) {
    process.stderr.write("agend: parent process died — exiting\n");
    process.exit(0);
  }
}, 5000);
```

### Fix 2: Monitor stdin readability directly

```js
const net = require('net');
// Wrap stdin fd in a net.Socket to get reliable close/error events
const stdinSocket = new net.Socket({ fd: 0, readable: true, writable: false });
stdinSocket.on('close', () => process.exit(0));
stdinSocket.on('error', () => process.exit(0));
```

### Fix 3: Couple IPC reconnect to stdio health

In `scheduleReconnect()`, before attempting reconnect, verify stdio is still connected:

```js
function isStdioAlive() {
  try {
    // Writing to a broken pipe throws EPIPE
    process.stdout.write('');
    return true;
  } catch {
    return false;
  }
}

function scheduleReconnect() {
  if (!isStdioAlive()) {
    process.stderr.write("agend: stdio broken + IPC lost — exiting\n");
    process.exit(0);
  }
  // ... existing reconnect logic
}
```

### Fix 4: Daemon-side cleanup on instance restart

When the daemon restarts an instance's CLI, read `channel.mcp.pid` and kill the old mcp-server process before spawning a new one. (The PID file mechanism at lines 249-257 already exists but may not be consumed on restart.)

## Recommended Approach

Use **Fix 1 + Fix 4** together:
- Fix 1 is a universal safety net — no dependency on stdio or IPC behavior
- Fix 4 prevents accumulation even if Fix 1 has edge cases
- Fix 2/3 are defense-in-depth additions
