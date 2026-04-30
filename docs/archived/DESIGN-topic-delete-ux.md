---
status: draft — discussion paused, awaiting user clarification
date: 2026-04-21
related: archived/PLAN-telegram-topic-delete-sync.md (SHIPPED)
---

# DESIGN: Topic-delete UX in app mode

## 1. Problem statement

`archived/PLAN-telegram-topic-delete-sync.md` wired lazy cleanup: when an outbound
Telegram call fails with "message thread not found", the instance is killed via
`cleanup_deleted_topic` (registry remove + fleet.yaml remove + pane close).

This works correctly end-to-end (verified at
`~/.agend-terminal/app.log` 2026-04-20 15:32:29 — `handle_tui_event
InstanceDeleted` → `remove_agent_pane: closing tab (single pane)`).

However, the UX is bad in **app mode**:

```
T0  User is typing into pane kiro-cli-2 locally (not yet sent)
T1  Someone deletes the Telegram topic for kiro-cli-2
T2  kiro-cli-2 agent emits something → send fails (thread not found)
T3  handle_send_failure → cleanup_deleted_topic → InstanceDeleted event
T4  App closes the pane → user's in-flight input vanishes with the pane
```

User framing: "用戶在已經被刪除 topic 的 instance 中打字，結果突然就被關閉，
這是很違反使用者體驗的事情".

## 2. Constraint from daemon-only mode

The fix **cannot** simply make all paths detach-only. User framing:
"不能只考慮 app，單純 daemon 模式下，刪除 topic 就應該等同於關閉該 instance".

Rationale: in daemon-only mode, Telegram is the sole control channel; there is
no local user to be surprised and no UI affordance for manually terminating an
instance. If a topic is deleted and we detach-without-kill, the instance leaks.

So any design must:

- **daemon mode**: delete topic ⇒ instance terminates (current behaviour is
  correct).
- **app mode**: avoid abrupt mid-typing closure.

## 3. Telegram semantics (clarification)

User corrected an earlier misreading:

- **Close topic** (Telegram UI → "Close topic"): **reversible**, topic preserved,
  has a "Reopen" affordance.
- **Delete topic** (Telegram UI → "Delete topic"): **permanent**, topic removed,
  cannot be recovered.

Current bot-side mapping (both paths trigger the same `cleanup_deleted_topic`)
treats them identically destructively. This is a separate question from the UX
fix but worth revisiting once the UX design settles: arguably Close should be
reversible on our side too (detach, keep instance alive, allow re-bind on
reopen) while Delete is the permanent kill.

## 4. Options considered

### Option A — Unified kill with countdown + PTY-injected warning

Detect topic-delete → inject a system message into the PTY above the current
input line (`⚠ Telegram topic deleted — instance will terminate in 30s`) →
start 30s countdown → local input still accepted during countdown → on
timeout, run current kill flow.

- **Pros**: symmetric across modes; daemon mode unaffected (countdown elapses
  silently); app user gets warning + time to copy/save.
- **Cons**: PTY injection may corrupt agent state (agent reads the warning as
  input); daemon mode has a 30s residency window for a dead-on-arrival topic.

### Option B — Mode-aware: daemon kills, app asks

- `is_app_attached() == false`: current behaviour (immediate
  `cleanup_deleted_topic`).
- `is_app_attached() == true`: mark pane `pending_kill`, render a red banner
  `topic deleted, press Enter to close / Esc to keep local`, wait for key.

- **Pros**: explicit user agency in app mode.
- **Cons**: new state machine (`pending_kill`), new key handler, "keep local"
  creates an orphan instance with no Telegram binding — needs a separate
  re-attach path to be coherent.

### Option C — Uniform kill, save a rescue transcript before pane close

On topic-delete detection, still kill; but before `remove_agent_pane` closes
the pane, dump scrollback + unsent input line to
`~/.agend-terminal/rescued/<agent>-<ts>.txt`, then toast the path in the TUI.

- **Pros**: minimal diff; no new state; both modes behave identically;
  recoverable content.
- **Cons**: **user rejected** — "不覺得 C 有改善任何使用者體驗噎". Does not
  address the disorientation of the pane vanishing mid-task; only makes the
  text loss recoverable after the fact.

### Option D — App pane becomes the new lifeline; close pane to kill

Reframe: "who authorises the kill" depends on whether a control channel
remains.

- **daemon mode** (no pane exists for the agent): Telegram was the only
  channel → on delete, run `cleanup_deleted_topic` fully. Unchanged from
  today.
- **app mode** (pane exists for the agent): pane itself is now the control
  channel → on delete, only clear Telegram-side state (topic_to_instance /
  instance_to_topic / `fleet.yaml` `topic_id` field). Keep registry + PTY +
  pane alive. Render a banner `⚠ Telegram detached — close pane to terminate`.
- User-initiated pane close (`close_tab` / `close_pane_by_id`) already reaches
  the existing kill path → registry remove + fleet.yaml instance removal.

Decision rule at `handle_send_failure`:

```text
if topic_deleted_error:
    if app_attached && pane_exists_for(agent):
        detach_telegram_only(state, agent, tid, home)
    else:
        cleanup_deleted_topic(state, agent, tid, home)   # current behaviour
```

- **Pros**: no new state machine — "topic gone" and "pane open" are already
  two independent facts, D just stops conflating them; pane-close semantics
  already mean "terminate this instance" and are reused unchanged; daemon
  mode byte-for-byte identical to today.
- **Cons**: requires `detach_telegram_only` helper (split from current
  `cleanup_deleted_topic`); banner rendering path in pane title/header; need
  a way to answer `pane_exists_for(agent)` from `handle_send_failure`
  (likely via the existing `api` channel or a new query).

## 5. Status

- Options A, B, C, D presented.
- User rejected C explicitly.
- User paused the discussion before picking A / B / D: "先把這些討論方案都
  記錄到文件裡，待之後我釐清再繼續".

## 6. Open questions for the next session

1. Daemon-mode constraint (delete = kill) holds — confirmed.
2. In app mode, should Close and Delete diverge? (See §3: Close is reversible
   on Telegram's side; should we mirror that and only detach on Close while
   Delete stays destructive?)
3. Preferred option: A (countdown), B (explicit key prompt), or D (pane
   becomes lifeline)?
4. If D: how does `handle_send_failure` (in `src/telegram.rs`) query whether
   the app is attached and has a pane for this agent? Options:
   - Route cleanup through a trait that has two impls (daemon vs app).
   - Add a query endpoint on the `api` server (`GET /panes`) and consult it
     before deciding.
   - Emit a new `TuiEvent::TelegramDetached { agent }` and let the app decide
     whether to kill or keep (daemon has no TUI listener, so naturally no-ops
     — but then daemon needs a separate kill trigger).

## 7. Scratch state to clean up when work resumes

- Diagnostic tracing added in `src/app/tui_events.rs` on branch
  `fix/single-pane-tab-close` (commit 165b902, locally merged to main, not
  pushed). Was added to confirm the single-pane close path; confirmed
  working. Revert once the real UX fix lands.
- No other in-flight code for this feature.
