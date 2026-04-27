# Test Coverage Audit — 2026-04

Sprint-level audit of all `#[test]` / `#[cfg(test)]` across the repo.
Triggered by PR #255 exposing mock-pair self-validation confirmation-bias
(NDJSON framing bug passed 30 min dual VERIFIED review + 3 PR rounds).

**Baseline**: 1301 tests (1198 in `src/`, 103 in `tests/`).

---

## Finding 1: Bridge binary has zero unit tests

**Severity**: CRITICAL  
**File**: `src/bin/agend-mcp-bridge.rs` (307 LOC, 0 `#[test]`)  
**Pattern**: External-fixture coverage gap  

**Why it's wrong**: The bridge is the MCP subprocess — every agent backend
talks through it. It has its own message framing (`read_message`,
`write_message`), connection management (`connect_daemon`,
`ensure_connection`), and response unwrapping (`proxy_tool_call`). None of
these have unit tests. The NDJSON framing bug (PR #255) was in this exact
code path.

**Concrete trace**: `read_message` auto-detects Content-Length vs NDJSON.
If a backend sends Content-Length framing but the bridge responds in
Content-Length (pre-PR #255 it did), the backend hangs. No test caught this
because `mcp_roundtrip.rs` tests `agend-terminal mcp` (old path), not
`agend-mcp-bridge` (new path).

**Fix**: Add `#[cfg(test)] mod tests` to `agend-mcp-bridge.rs` with:
- `read_message` unit tests (NDJSON input, Content-Length input, mixed, EOF)
- `write_message` format verification (Content-Length output)
- `extract_id` edge cases
- `wrap/unwrap` daemon response roundtrip

**LOC est**: ~80

---

## Finding 2: mcp_proxy_parity.rs is source-reading self-validation

**Severity**: HIGH  
**File**: `tests/mcp_proxy_parity.rs` (81 LOC, 5 tests)  
**Pattern**: Self-validation — tests read source code and assert on string patterns  

**Why it's wrong**: `proxy_handler_calls_handle_tool_directly` reads
`src/api/handlers/mcp_proxy.rs` and asserts it contains the string
`"crate::mcp::handlers::handle_tool("`. This is a tautology — it verifies
the source text, not the runtime behavior. If `handle_tool` is called but
with wrong args, or if the response wrapping is broken, this test passes.

PR #255's NDJSON bug would have passed all 5 parity tests because they
check source strings, not wire output.

**Concrete trace**: `proxy_handler_wraps_result_correctly` asserts
`src.contains(r#"json!({"ok": true, "result": result})"#)` — this is
literally checking that the source code contains a string. The actual
runtime behavior (JSON serialization, Content-Length framing, error
handling) is untested.

**Fix**: Replace with `mcp_proxy_behavioral_parity.rs` (already exists,
PR #253). Delete or demote `mcp_proxy_parity.rs` to a structural lint
(rename to `mcp_proxy_structural_lint.rs`).

**LOC est**: ~10 (delete/rename)

---

## Finding 3: ci_watch mock tests don't validate against real GitHub API

**Severity**: MEDIUM  
**File**: `src/daemon/ci_watch.rs` (7 mock tests)  
**Pattern**: Mock-pair confirmation — mocks construct JSON that matches
the parser, but no test verifies the JSON matches real GitHub API responses  

**Why it's wrong**: `mock_success_run_updates_watch_state` constructs a
`serde_json::json!({...})` response and feeds it to `classify_response`.
If GitHub changes their API shape (field rename, nesting change), the mock
still passes but production breaks.

**Fix**: Add 1-2 golden-file tests with captured real GitHub API responses
(sanitized). Store as `tests/fixtures/github_ci_*.json`.

**LOC est**: ~40

---

## Finding 4: vterm.rs tests don't cover resize-race panic path

**Severity**: CRITICAL  
**File**: `src/vterm.rs` (19 tests, 0 cover resize-during-render)  
**Pattern**: External-fixture coverage gap  

**Why it's wrong**: The Sprint 25 P0 HOTFIX vterm panic
(`index out of bounds: len 26, idx 107`) is a resize race between
`self.cols` update and `grid.columns()`. No test exercises concurrent
resize + render. The 19 existing tests are all single-threaded sequential
operations.

**Concrete trace**: `dump_screen` (L344) uses `self.cols as usize` without
`grid.columns()` cap. If `resize(120, 40)` is called while `dump_screen`
is iterating with the old `cols=26`, the grid has 26 columns but the loop
iterates to 120 → panic.

**Fix**: Add a stress test that spawns a thread calling `resize()` while
the main thread calls `render_to_buffer` / `dump_screen` / `tail_lines`
in a loop. Assert no panic (use `catch_unwind`).

**LOC est**: ~60

---

## Finding 5: state.rs `replay_session` is `#[ignore]` with no CI path

**Severity**: LOW  
**File**: `src/state.rs:2051` (`#[ignore]`)  
**Pattern**: Skip / ignore — test exists but never runs  

**Why it's wrong**: `replay_session` is a valuable regression test (replays
a real agent session trace) but is permanently `#[ignore]`. No CI job runs
ignored tests. The test may have bitrotted.

**Fix**: Either remove `#[ignore]` and fix any failures, or add a CI job
that runs `cargo test -- --ignored` periodically.

**LOC est**: ~5

---

## Finding 6: backend_harness.rs 4 tests are `#[ignore]` (require installed backends)

**Severity**: LOW  
**File**: `src/backend_harness.rs:453-485` (4 `#[ignore]` tests)  
**Pattern**: Skip — tests require kiro-cli/codex/gemini/claude installed  

**Why it's wrong**: These are the only tests that verify real backend
spawning. They never run in CI. Backend spawn regressions are caught only
by manual testing.

**Fix**: Add a CI matrix job that installs at least one backend (claude)
and runs `cargo test -- --ignored backend_harness`. Or: add a mock backend
that exercises the spawn path without requiring a real CLI.

**LOC est**: ~30 (mock backend)

---

## Finding 7: mcp_subprocess_is_zero_state.rs comment-stripping is fragile

**Severity**: MEDIUM  
**File**: `tests/mcp_subprocess_is_zero_state.rs` (164 LOC)  
**Pattern**: Self-validation — custom comment stripper may miss edge cases  

**Why it's wrong**: The `strip_comments` function is a hand-rolled parser
that strips `//` and `/* */` comments before grepping for forbidden
patterns. If a forbidden pattern appears in a doc comment (`///`) or a
raw string (`r#"..."#`), the stripper may not handle it correctly. The
test validates itself (its own comment stripper) rather than the bridge's
actual behavior.

**Fix**: Use `syn` crate to parse the AST and check for forbidden
identifiers in non-comment positions. Or: simplify to line-level grep
that skips lines starting with `//`.

**LOC est**: ~30

---

## Finding 8: inbox.rs tests share READONLY_TEST_LOCK global

**Severity**: MEDIUM  
**File**: `src/inbox.rs` (4 tests use `READONLY_TEST_LOCK`)  
**Pattern**: Self-validation — tests serialize via shared lock, masking
concurrency bugs  

**Why it's wrong**: `READONLY_TEST_LOCK` prevents tests from running
concurrently. This masks potential race conditions in the inbox code
itself. If inbox operations are supposed to be thread-safe, the lock
hides bugs. If they're not supposed to be concurrent, the lock is
correct but should be documented.

**Fix**: Document why the lock exists. If inbox is supposed to be
thread-safe, remove the lock and fix any races.

**LOC est**: ~5 (documentation)

---

## Finding 9: mcp_roundtrip.rs tests old `agend-terminal mcp` path, not bridge

**Severity**: HIGH  
**File**: `tests/mcp_roundtrip.rs` (33 tests)  
**Pattern**: External-fixture coverage gap — tests the deprecated code path  

**Why it's wrong**: After PR #250 (Option F), the production MCP path is
`agend-mcp-bridge` → daemon API. But `mcp_roundtrip.rs` spawns
`agend-terminal mcp` (the old in-process path). These 33 tests don't
exercise the production code path.

`mcp_bridge_client_handshake.rs` and `mcp_proxy_behavioral_parity.rs`
partially cover the bridge, but only 2-3 tool calls each — not the 33
comprehensive scenarios in `mcp_roundtrip.rs`.

**Fix**: Port `mcp_roundtrip.rs` to spawn `agend-mcp-bridge` instead of
`agend-terminal mcp`. Or: add a parallel `mcp_bridge_roundtrip.rs` that
mirrors the 33 tests against the bridge binary.

**LOC est**: ~50 (port existing tests to bridge binary)

---

## Finding 10: No wire-protocol capture tests for Telegram channel

**Severity**: MEDIUM  
**File**: `src/channel/telegram.rs` (7 tests, all unit-level)  
**Pattern**: External-fixture coverage gap  

**Why it's wrong**: Telegram channel tests construct mock responses
in-process. No test verifies against real Telegram Bot API wire captures.
If Telegram changes their API response format, tests still pass.

**Fix**: Add golden-file tests with captured real Telegram API responses
(sanitized). Similar to PR #255's `mcp_bridge_client_handshake.rs` pattern.

**LOC est**: ~40

---

## Backlog Priority Table

| # | Severity | Finding | Est LOC | Sprint |
|---|----------|---------|---------|--------|
| 1 | CRITICAL | Bridge binary zero unit tests | 80 | Next sprint |
| 4 | CRITICAL | vterm resize-race test missing | 60 | Next sprint |
| 2 | HIGH | mcp_proxy_parity source-reading self-validation | 10 | Next sprint |
| 9 | HIGH | mcp_roundtrip tests old path, not bridge | 50 | Next sprint |
| 3 | MEDIUM | ci_watch mocks not validated against real API | 40 | P3 backlog |
| 7 | MEDIUM | zero-state invariant comment-stripping fragile | 30 | P3 backlog |
| 8 | MEDIUM | inbox READONLY_TEST_LOCK masks concurrency | 5 | P3 backlog |
| 10 | MEDIUM | Telegram channel no wire captures | 40 | P3 backlog |
| 5 | LOW | state.rs replay_session #[ignore] bitrot | 5 | P3 backlog |
| 6 | LOW | backend_harness #[ignore] no CI path | 30 | P3 backlog |

**Total next-sprint LOC**: ~200 (4 CRITICAL/HIGH findings)  
**Total P3 backlog LOC**: ~150 (6 MEDIUM/LOW findings)
