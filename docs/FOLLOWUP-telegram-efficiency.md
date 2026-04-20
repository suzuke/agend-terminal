# Follow-up: Telegram routing efficiency + type safety

Surfaced during the `/simplify` review of commits `dd3d7c3` (InteractivePrompt
forwarding) and `5c9d85a` (shared formatter). These are all **pre-existing**
patterns in `src/telegram.rs` / `src/daemon/supervisor.rs` — not introduced by
the InteractivePrompt work — but they become worth fixing as the raw-keystroke
state set grows beyond two variants.

Deliberately deferred from the InteractivePrompt commit because each one
reaches into abstraction boundaries (TelegramState's data source, AgentState's
serde derives, lock granularity) that deserve their own scoped change.

## 1. Per-inbound-message daemon LIST RPC

**Where:** `src/telegram.rs:340-365` — `agent_wants_raw_keystrokes` calls
`crate::api::call(..., method::LIST)` on **every** inbound Telegram message
just to read one agent's `agent_state`. Response is the full agent list,
JSON-parsed, then one field extracted.

**Cost:**
- UDS round-trip + JSON serialize/parse per message
- O(N) scan inside daemon for each incoming Telegram reply
- Scales poorly once a fleet has many agents / chatty Telegram groups

**Call graph (verified 2026-04-20):** Telegram is only initialized on the
`OwnedFleet` bootstrap path (`src/bootstrap/mod.rs:160-164` →
`telegram::init_from_config`). Both daemon mode and app mode go through this
path when they own the registry; the `Attached` path skips Telegram init
entirely. So Telegram always lives in-process with `AgentRegistry` — the RPC
here is effectively the process talking to itself over UDS. No pluggable
source needed; just pipe the registry through.

**Proposed fix:**
- Add `Arc<AgentRegistry>` parameter to `telegram::init_from_config`
  (callers already have one — see `bootstrap::OwnedFleet.agents` / the
  registry used by `daemon::supervisor::spawn`)
- Store it on `TelegramState` alongside `bot`, `group_id`, etc.
- Replace `agent_wants_raw_keystrokes` with
  `registry.get(name).map(|c| c.lock().state.current).map(AgentState::wants_raw_keystrokes)`
  (assumes §3 fix applied; otherwise inline the matches!)
- Keep `list_response_wants_raw_keystrokes` as a pure JSON helper **only if**
  we still need it for an external-client code path — otherwise delete it
  along with its tests

**Severity:** must-fix when fleet sizes grow; nice-to-have today.

## 2. `format!` + `tail_lines` under `core.lock()`

**Where:** `src/daemon/supervisor.rs` tick loop holds the per-agent `core`
mutex across `vterm.tail_lines(TAIL_LINES)` **and** `format_stall_notice`.
Affects both branches (`AwaitingOperator` and `InteractivePrompt`).

**Cost:**
- UTF-8 Chinese formatting + 40-line tail copy under a lock that also gates
  PTY reads/writes on that agent
- Blocks the per-agent pipeline for ~microseconds per tick where a notice
  fires — not catastrophic but unnecessary

**Proposed fix:**
```rust
// Extract tail + decision data under the lock, drop guard, format outside:
let payload_kind = { let core = core.lock(); ... };  // enum { None, Awaiting { silent, tail }, Interactive { tail } }
drop(core_guard);
let msg = match payload_kind { ... };
```

**Severity:** nice-to-have. Lock hold time today is well under any realistic
contention window.

## 3. Stringly-typed agent_state compare in routing

**Where:** `src/telegram.rs:358-364` — compares the daemon's JSON string
against `AgentState::AwaitingOperator.display_name()` and
`AgentState::InteractivePrompt.display_name()`. Every new state added to the
raw-keystroke set needs a new `||` branch.

**Risk:**
- No compile-time check that the string set stays in sync with what
  `display_name()` actually emits — a refactor to display_name silently
  breaks routing
- Tests pin current behavior, but only for the two hardcoded strings

**Proposed fix (option A — minimal):** centralize the set membership on the
enum:
```rust
impl AgentState {
    pub fn wants_raw_keystrokes(self) -> bool {
        matches!(self, Self::AwaitingOperator | Self::InteractivePrompt)
    }
}
```
Then `list_response_wants_raw_keystrokes` parses the string back via a small
`from_display_name(&str) -> Option<AgentState>` helper (also added to
`state.rs` so `display_name` ↔ parse are colocated), then calls
`.wants_raw_keystrokes()`. No serde changes needed.

**Proposed fix (option B — bigger):** add `Deserialize` to `AgentState`, let
serde handle the parse. Widens the blast radius but makes the JSON wire
format a first-class concern.

**Severity:** moderate. The `from_display_name` helper in option A is the
right scope for this codebase's size.

## Suggested ordering

1. Option A from §3 first — small, isolates the `display_name` pairing, buys
   type safety with minimal blast radius.
2. §1 after — needs an architectural decision about daemon-mode telegram
   worker first.
3. §2 last — pure micro-optimization, likely not worth doing unless a
   concrete contention issue shows up in tracing.
