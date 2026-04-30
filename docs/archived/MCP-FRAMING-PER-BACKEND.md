# MCP Framing Per-Backend Audit

Sprint 25 P3 — audit of MCP stdio framing convention per backend.

## Audit Results

| Backend | MCP Client | Framing | Evidence |
|---------|-----------|---------|----------|
| Claude Code | `claude` CLI | **NDJSON** | Wire capture in `tests/mcp_bridge_client_handshake.rs` (Claude Code 2.1.119) |
| Kiro CLI | `kiro-cli` | **NDJSON** | Same MCP SDK as Claude Code; `mcp_config.rs` generates identical server entry |
| Codex | `codex` CLI | **NDJSON** | OpenAI MCP implementation uses stdio NDJSON per MCP spec |
| Gemini | `gemini` CLI | **NDJSON** | Google MCP implementation uses stdio NDJSON per MCP spec |
| OpenCode | `opencode` CLI | **NDJSON** | Community MCP client, stdio NDJSON per MCP spec |

**Conclusion**: All 5 backends use NDJSON. No backend sends Content-Length (LSP-style) framing.

## Decision: KILL Content-Length fallback

Content-Length input fallback removed from both:
- `src/bin/agend-mcp-bridge.rs` (`read_message`)
- `src/mcp/mod.rs` (`read_message`)

### Attack surfaces closed

1. **Drip-feed DoS**: `Content-Length: 999999\r\n\r\n` + slow byte feed → `read_exact` blocks indefinitely with no timeout → thread pinned
2. **Negative Content-Length crash**: `Content-Length: -1` → `parse::<usize>` returns Err → bridge crashes (pre-fix: `unwrap_or(0)` silently desynced stream)
3. **OOM**: `Content-Length: 999999999` → `vec![0u8; 999999999]` allocation → OOM kill

### New behavior

Non-JSON lines (including `Content-Length:` headers) are logged and skipped. Only lines starting with `{` are parsed as JSON-RPC requests. This is strictly more defensive than the previous auto-detect behavior.
