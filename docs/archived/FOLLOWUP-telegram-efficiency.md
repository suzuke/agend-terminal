# Follow-up: Telegram routing efficiency + type safety — RESOLVED

> **Status: SHIPPED** — implementation landed on `main` (branch `feature/telegram-efficiency-followups`, 2026-04-20). Doc retained for historical/provenance.

Status: **all three items closed 2026-04-20** on branch
`feature/telegram-efficiency-followups`.

Original review surfaced three pre-existing smells in `src/telegram.rs` /
`src/daemon/supervisor.rs` (not introduced by the InteractivePrompt forwarding
work, but worth fixing once the raw-keystroke state set grew beyond two
variants). All three are now addressed.

## 1. Per-inbound-message daemon LIST RPC — FIXED

Previously `agent_wants_raw_keystrokes` called `api::call(LIST)` on every
inbound Telegram message just to read one agent's `agent_state`.

Fix: `TelegramState` gained an `Option<AgentRegistry>` field, wired in by
`telegram::attach_registry` after both the registry and `TelegramState` exist
(the two are born in opposite orders — `TelegramState` during bootstrap, the
registry inside `daemon::run_core` / `app::run_app`). Inbound routing now
reads `core.state.current.wants_raw_keystrokes()` directly — one Arc clone,
two mutex acquires, one enum match — no UDS round-trip, no JSON.

## 2. `format!` + `tail_lines` under `core.lock()` — FIXED

`src/daemon/supervisor.rs::tick` now extracts `(tail, silent_secs)` under the
core lock, drops the guard, then runs `format_stall_notice` and
`notify_telegram` outside. `tail_lines` still allocates under the lock (that
allocation lives in vterm and is pre-existing), but `format!` + the Telegram
spawn no longer do.

## 3. Stringly-typed agent_state compare — FIXED

Added `AgentState::wants_raw_keystrokes(self) -> bool` as the canonical
membership check (matches `AwaitingOperator | InteractivePrompt`). The old
`list_response_wants_raw_keystrokes` JSON helper was deleted outright along
with its tests — §1's in-process registry lookup made the JSON parse path
unreachable, and the state enum's predicate is now the single source of truth.
