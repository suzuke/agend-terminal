# PLAN: Channel UX layer — delivery confirmation, fleet visibility, input mirroring

**Branch:** `feat/channel-ux-layer` (to be created when implementation starts)
**Date:** 2026-04-22
**Related:** `docs/PLAN-channel-abstraction.md` (this plan sits ON TOP of the Channel trait / Capabilities layer defined there)

---

## 1. Background

`PLAN-channel-abstraction.md` solves the **transport** problem: one trait can
speak Telegram, Discord, Slack, etc. What it does not cover is the **UX
layer** that sits above transport:

- Does the user know their message was **delivered** to the agent?
- When agent A delegates to agent B, is the user's phone kept in the loop, or
  does all A2A coordination vanish from every channel?
- When the user types in the local TUI and later picks up their phone, can
  they see their own half of the conversation?

These are platform-independent concerns that need a platform-independent
design. The naïve fix ("mirror everything to Telegram") was tried in an
earlier iteration and retired once we realized Telegram already offers a
client-side "View as Messages" toggle that covers part of the gap. That
lesson is the motivation for this plan:

1. Describe the scenarios in channel-agnostic language.
2. Express them as typed events × channel capabilities.
3. Degrade gracefully when a channel can't express a given UX primitive.
4. Resist the urge to grow `fleet.yaml` with knobs.

## 2. Design principles

1. **Typed semantic events in core, platform-specific rendering in adapters.**
   Daemon never knows about Telegram reactions or Slack typing indicators —
   it emits `UserMsgReceived`, `AgentThinking`, etc. Each channel adapter
   maps those to whatever it can express.
2. **Capability-driven degradation.** If a channel lacks `react`, fall back to
   `edit`; if it lacks `edit`, fall back to a new short ack message; if
   sending an ack would be too noisy (SMS), batch or drop.
3. **Sensible defaults over configuration.** Every scenario must have a
   working out-of-the-box behaviour. `fleet.yaml` only grows when a user
   needs to **override** that default — and the override surface is minimal.
4. **No fleet.yaml monster.** Any new key must pass the test: *"if this key
   is absent, does the system still behave reasonably?"* If not, the default
   is wrong. See §7 for the exact surface we allow.
5. **One-way flow: events are additive, not normative.** Events describe what
   happened. Adapters decide whether to render. A channel that renders
   nothing is still valid (e.g., an audit-only sink).

## 3. Cross-platform "see all" reality check

Earlier iteration assumed Telegram's forum-group "General topic" would serve
as a universal cross-topic view. Correction after verification
([Telegram Forums API](https://core.telegram.org/api/forum)):

| Platform | "See all across threads" | Type | Daemon can enable? |
|---|---|---|---|
| Telegram | **View as Messages** toggle (`channels.toggleViewForumAsMessages`) | Per-user client-side setting | No — user-local, not bot-addressable |
| Slack | None | — | No native equivalent |
| Discord | None | — | No native equivalent |
| Matrix | Element "Home" (partial) | Per-user client view, not per-room | No |
| LINE | None | — | No |
| SMS / IRC | None | — | No |

**Implication:** We cannot rely on any server-side "see all" feature. A
daemon-created **fleet binding** is the only portable solution for users who
want A2A visibility outside the TUI. Telegram users who prefer the client-side
View as Messages toggle can simply not configure a fleet binding.

## 4. Event taxonomy (daemon core)

These event kinds are emitted by the daemon and consumed by channel adapters.
They are platform-agnostic and live alongside the existing `ChannelEvent` stream
(which is inbound-from-platform). These are **outbound semantic events**.

```rust
enum UxEvent {
    // User → agent, observed at daemon ingress
    UserMsgReceived { binding: BindingRef, origin_msg: MsgRef, agent: AgentName },
    AgentPickedUp  { origin_msg: MsgRef, agent: AgentName },

    // Agent lifecycle observable
    AgentThinking     { agent: AgentName },
    AgentIdle         { agent: AgentName },
    AgentRateLimited  { agent: AgentName, retry_after: Option<Duration> },
    AgentCrashed      { agent: AgentName, reason: CrashReason },
    AgentRestarted    { agent: AgentName, attempt: u32 },

    // Agent output
    AgentReplied { agent: AgentName, binding: BindingRef, payload: MsgPayload },

    // Fleet coordination (see §6 scenarios S2c / S2d)
    Fleet(FleetEvent),
}

enum FleetEvent {
    DelegateTask   { from: AgentName, to: AgentName, summary: String, task_id: TaskId },
    ReportResult   { from: AgentName, to: AgentName, summary: String, task_id: TaskId },
    PostDecision   { by: AgentName, title: String, decision_id: DecisionId },
    Broadcast      { from: AgentName, recipients: Vec<AgentName>, summary: String },
    // Explicitly NOT mirrored: raw send_to_instance acks — see §8 non-goals
}
```

`MsgRef` is a channel-adapter-owned reference (Telegram `(chat_id, message_id)`,
Slack `thread_ts`, etc.) that the adapter uses to later react/edit/delete.

## 5. Capability extensions

`PLAN-channel-abstraction.md` §3.4 defines a minimal `ChannelCapabilities`.
This plan extends it with UX-layer capabilities:

```rust
struct ChannelCapabilities {
    // ... existing fields (emits_deletion_events, threads, buttons,
    //     attachments, markdown, max_msg_bytes, rate_budget)

    // UX layer additions
    react: bool,                    // Telegram ✓ / Slack ✓ / SMS ✗
    edit:  bool,                    // most IMs ✓, SMS/IRC ✗
    typing_indicator: bool,         // TG sendChatAction, Slack presence, etc.
    receives_edit_events: bool,     // Discord MESSAGE_UPDATE — TG no
    receives_delete_events: bool,   // already covered by emits_deletion_events
    mention_parsing_hint: MentionStyle, // @username (Slack), <@uid> (Discord), none
    bot_sees_read_receipts: bool,   // TG private yes, TG group no, Slack no
    has_native_multi_thread_view: Option<NativeSeeAllHint>, // TG "View as Messages"
    ephemeral: bool,                // TUI adapter is ephemeral-by-nature
}

enum NativeSeeAllHint {
    TelegramViewAsMessages, // documented at PLAN-channel-ux-layer.md §3
    // (Matrix Element Home is per-account client setting, not useful to
    //  suggest to the user at bot level; omit)
}
```

`has_native_multi_thread_view` only informs the CLI / `agend-terminal doctor`
output, so the daemon can suggest to the user at first run: *"Telegram users:
enable View as Messages on your client if you prefer a unified view — no
config required. Otherwise set `channels.telegram.fleet_binding` below."*

## 6. UX scenarios & resolutions

Each scenario is expressed as **event → capability-aware rendering**.

### Q1 — Delivery confirmation for inbound user messages

Event chain on a single user message:

```
UserMsgReceived  →  AgentPickedUp  →  AgentThinking  →  AgentReplied
   (daemon ingress)   (inbox dequeue)   (tool_use)       (user-visible)
```

Rendering per capability:

| Event | `react` | `edit` only | `typing_indicator` only | None |
|---|---|---|---|---|
| `UserMsgReceived` | 👀 on origin msg | edit origin → `[delivered]` | start typing animation | short ack `✓ delivered` |
| `AgentPickedUp` | stack ✅ on origin | edit origin → `[read]` | keep typing | no-op (already acked) |
| `AgentThinking` | (typing indicator if also available) | — | keep typing | no-op |
| `AgentRateLimited` | ⏳ on origin | edit → `[queued, retrying in Xs]` | pause typing | short ack `⏳ queued` |
| `AgentReplied` | send reply (clear reactions optional) | send reply | stop typing | send reply |

**Anti-feature:** never mirror `AgentThinking` into a text "agent is typing..."
message when no typing-indicator capability exists. Signalling status via
text messages turns every turn into two-plus messages and creates more noise
than it relieves. Silence is an acceptable fallback.

**T3 scope narrowing.** The shipped `UxAction` enum covers only
`React | EditText | SendText | Noop`. The `typing_indicator` column of
the Q1 table — plus the `AgentThinking` / `AgentRateLimited` rows that
lean on it — are **deferred to T12** (typing-indicator action + the
daemon-side dispatcher that ticks `sendChatAction` on a timer). T3 also
only wires `UserMsgReceived` / `AgentPickedUp` / `AgentReplied`; the
other rows come online when their producers (inbox dequeue, rate-limit
gate) land.

### Q2 — A2A fleet visibility

Split into the two concrete scenarios confirmed as real pain:

#### S2c — Live A2A on a secondary device (phone)

`FleetEvent::DelegateTask / ReportResult / PostDecision / Broadcast` is
rendered into the **fleet binding** (§7) as a compact one-liner:

```
[at-dev-1 → at-dev-2] DELEGATE  task #9 Option C scoping
[at-dev-2 → at-dev-1] REPORT    DONE  src/utils.rs consolidation landed (#21)
[at-dev-3 → *]         BROADCAST  CI green post-rebase
```

Explicitly **not mirrored**:
- Raw `send_to_instance` that is a pure ack (covered by the
  [stop-ack-loop fleet rule](https://github.com/suzuke/agend-terminal)).
- Inbox routing messages (internal plumbing).
- Agent-side thinking / status (those are per-agent, not fleet-wide).

If the channel lacks native threading and message length allows, the one-liner
is all that's rendered. For long payloads, the full body lives behind a link
or `<details>`-style disclosure per channel capability (`max_msg_bytes`).

#### S2d — Provenance in the receiving agent's topic

When agent B receives a delegated task, B's own binding (topic/channel)
receives a *system-flavored* injected message:

```
⬅️ from at-dev-1 — DELEGATE  task #12 follow-up
   (brief: "audit docs/ for Task #9 post-merge staleness")
```

This gives context to a user who opens B's topic directly without having to
cross-reference the fleet binding. Always-on, no config. Fully consumes the
agent's output capability (`send` only, no special APIs).

### Q3 — TUI input mirrored to IM

Treat the local TUI as a first-class channel adapter
(`src/channel/tui.rs`, `kind() == "tui"`). Its capabilities:

```rust
ChannelCapabilities {
    react: false,
    edit: true,          // scrollback can re-render lines
    typing_indicator: false,
    ephemeral: true,     // not persisted across daemon restart
    has_native_multi_thread_view: None,
    // ...
}
```

Once TUI is a channel, each agent's `Vec<BindingRef>` can include both the
TUI binding and the Telegram/Slack binding. Inbound `ChannelEvent::MessageIn`
from the TUI binding is routed to the agent **and** fan-out to every other
binding subscribed to that agent — exactly the same mechanism
`PLAN-channel-abstraction.md` §3.7 already describes for cross-channel
mirroring.

Solving the scratchpad case:

- **Prefix `!`** (exclamation at line start) → do not fan-out to non-ephemeral
  bindings. Hard-coded protocol, no config.
- **Session toggle** (`Ctrl+B m`) → mute mirror for the current TUI session.
  TUI runtime state, not persisted, not in `fleet.yaml`.

Device switch (desktop → phone): once mirror is on, the phone sees
user-input messages as they happen. If the user was offline during the
earlier part of the conversation, their IM app's own scrollback shows the
earlier TUI-originated messages — no special "replay" feature required.

### Q4 — Additional gaps

| Gap | Event / capability | Resolution |
|---|---|---|
| a. Typing indicator | `AgentThinking` × `typing_indicator` | Emit platform typing API; silently skip if cap absent |
| b. Inbound edit events | `receives_edit_events` | Expose to agent inbox as annotated update (see §8 design) |
| c. Attachment inbound | `attachment_*` caps | Signal via Q1 react/edit on the attachment's origin msg |
| d. Read receipts | `bot_sees_read_receipts` | Pass to agent layer; agent retry / escalate logic must branch on this cap, not assume universal seen-tracking |
| e. Device switch | — | Solved by Q3 + S2d provenance; no new mechanism |
| f. @mention cross-channel routing | `mention_parsing_hint` | Channel adapter parses mention → sets `target_agent` on inbound event; router is cap-blind |
| g. Crash / restart routing | `AgentCrashed`, `AgentRestarted` | Sent to fleet binding + the agent's user-facing binding (two sinks, different purposes) |
| h. Oversized payload | `max_msg_bytes` | Adapter truncates + provides "view full" hook (URL, inline-file upload, or `<details>`) |

## 7. `fleet.yaml` surface (minimal, opt-in)

Default behaviour with **zero new config**:

- No fleet binding is auto-created. `FleetEvent`s are emitted in-process but
  have no sink; the fleet binding is opt-in per channel.
- Q1 delivery confirmation is always on (cap-degraded).
- Q3 TUI mirror is on by default once TUI is a channel; `!` prefix and
  `Ctrl+B m` are hard-coded escapes.
- S2d provenance injection is always on.

Only when the user wants an explicit fleet binding:

```yaml
# OPTIONAL — only when user wants fleet visibility in an IM
channels:
  telegram:
    fleet_binding:
      type: topic
      name: "fleet-activity"
  slack:
    fleet_binding: "#agend-ops"
  discord-ops:
    fleet_binding:
      type: channel
      name: "fleet-activity"
```

Rules:
- The `fleet_binding` key is the **only** new field this plan adds to
  `fleet.yaml`.
- Absent block = no fleet sink for that channel. Daemon logs a one-line hint
  at first start.
- `doctor` subcommand reminds Telegram users about the View as Messages
  client-side toggle so they know an IM-side option exists.
- Per-event, per-agent, per-kind routing knobs are **not added**. If users
  ask, the answer is "use a different channel adapter" (e.g., separate a
  noisy fleet from a quiet one by routing through two channels).

## 8. Non-goals

- **Per-fleet-event filtering in `fleet.yaml`.** The kind filter is fixed
  (`DelegateTask | ReportResult | PostDecision | Broadcast`). Users cannot
  say "I want Delegates but not Decisions." If that ever becomes a real need,
  reconsider — but only then.
- **Auto-creating fleet bindings.** Requires user opt-in.
- **Polyfilling missing capabilities.** If a channel lacks `react`, we do not
  simulate reactions with messages. Degrade, don't emulate.
- **Read-receipt emulation.** If `bot_sees_read_receipts == false`, callers
  must handle that — we don't fake it by polling.
- **Mirroring every `send_to_instance`.** Only `FleetEvent`-kinded events mirror.
  Pure acks stay internal.
- **Cross-channel identity unification.** Telegram user ≠ Slack user. Already
  a non-goal of the parent plan; inherited here.
- **Multi-device "last-read" sync for IM itself.** Users' unread state is
  their IM client's responsibility.

## 9. Staged rollout

Aligned with `PLAN-channel-abstraction.md` staging:

### Stage A-UX (co-schedule with Stage A of parent plan)

- Add extended capability fields to `ChannelCapabilities`.
- Define `UxEvent` + `FleetEvent` enums in `src/channel/ux_event.rs`.
- Define UxEvent → rendering mapping at a channel-adapter-callable layer
  (e.g. `src/channel/renderer.rs`).
- Telegram adapter implements Q1 reactions (first real consumer of the
  renderer).
- Add `fleet_binding` optional config parsing — no auto-create, no behavior
  change if absent.

Exit: `UserMsgReceived` causes 👀 → ✅ reactions on existing Telegram
deployments. No config-required behaviour change.

### Stage B-UX (after parent Stage B / concurrent with Discord)

- Discord adapter wires Q1 (with Discord reactions or embed edits).
- Fleet binding logic emits `FleetEvent` one-liners into the configured
  binding — test with both TG and Discord.
- S2d provenance injection: agent inbox receiver → system-flavored message
  to agent's primary user binding.

Exit: fleet binding in a Discord channel shows live A2A traffic; B's topic
shows `⬅️ from A ...` when delegated.

### Stage C-UX (TUI as channel)

- `src/channel/tui.rs` with ephemeral capability set.
- Fan-out routing from TUI binding to agent's other bindings.
- `!` prefix protocol + `Ctrl+B m` toggle.

Exit: typing in TUI on desktop shows up in the user's phone IM conversation;
`!ls` stays local-only.

### Stage D-UX — typing indicators, oversized payload, attachments (demand-driven)

Add one capability at a time, each validated by a real user complaint. Do
not pre-build them speculatively.

## 10. Verification

Per stage, cover:

- **Q1 delivery confirmation:** integration test that sends a user message
  via a mock channel with `react = true`, asserts 👀 → ✅ sequence fires.
  Second test with `react = false, edit = true` asserts origin-msg edit
  pattern. Third with both false asserts short-ack message + no crash.
- **S2c fleet visibility:** integration test with `fleet_binding` configured,
  dispatches a delegation via `delegate_task`, asserts a one-liner lands at
  the binding with the right format. Negative pin: `send_to_instance` with
  pure ack text does **not** produce a fleet-binding render.
- **S2d provenance:** agent B delegation test asserts B's primary binding
  receives the `⬅️ from A` message.
- **Q3 TUI mirror:** contract test on the TUI channel adapter — send via TUI,
  assert fan-out to a mock IM adapter. `!` prefix test: prefixed input does
  not fan-out. Toggle test: after mute, no fan-out until unmute.

## 11. Success criteria

- User never again has to ask *"did my message reach the agent?"* — delivery
  status is visible in every channel that supports any form of
  message-level feedback.
- Opening the user's phone after an hour of agent-to-agent work yields a
  readable fleet sink with the top-level coordination events, without
  requiring the user to attach the TUI.
- Agent B's primary binding surfaces the upstream brief so a user opening
  just that topic can reconstruct why B is doing what it's doing.
- Desktop TUI conversations survive a device switch: input and replies both
  visible on the phone.
- `fleet.yaml` gains exactly one new optional key family
  (`channels.<name>.fleet_binding`). No per-event / per-agent / per-kind
  routing knobs are introduced.
