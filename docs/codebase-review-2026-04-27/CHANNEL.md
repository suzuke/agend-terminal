# CHANNEL Code Review — 2026-04-27 (Track A)

## Methodology

- **audited_head**: `1485e85eab70ceeb43d794ecb586ee0b72d0bf04` (origin/main at audit start)
- **scope_source**: fleet decision `d-20260426210724891457-5` (Sprint 20 final scope, post-challenge round 11 修正項)
- **audit_mode**: `codebase_audit`
- **auditor**: `dev-impl-1`
- **time-box**: 2h hard cap (2026-04-26 21:09 → ~23:09 UTC)
- **commands_run** (top 8 grep / inspection invocations):
  1. `find src/channel -type f -name '*.rs' -exec wc -l {} \;` → file-size inventory
  2. `git log --diff-filter=A --follow --pretty=format:'%h %ad %s' --date=short -- <file>` per file → comfort-zone first-pass (oldest add date / last touch)
  3. `grep -nE 'fn check|fn verify|fn validate|fn audit|fn authorize|token|secret|signature|allowlist|allowed_user' src/channel/telegram.rs` → path-keyword auto-Critical scan
  4. `grep -rn 'active_channel\|register_active_channel' --include='*.rs' src/` → process-wide channel registration consumers
  5. `grep -rn 'impl Channel for\|impl crate::channel::Channel' --include='*.rs' src/` → trait impl sites
  6. `grep -rn 'ChannelCapabilities\|::caps()' --include='*.rs' src/` → caps consumers
  7. `grep -rn 'ChannelKind::\|ChannelKind ' --include='*.rs' src/` → discriminator leak surface
  8. `grep -nE 'tracing::|debug!|info!|warn!|error!|format!.*token' src/channel/telegram.rs | grep -i token` → token-logging risk audit

## Scope audited (file list / lines / audit-tier)

| File | Lines | Tier | Last touch (date) | Add date |
|---|---:|---|---|---|
| `src/channel/telegram.rs` | 3320 | hot | 2026-04-27 | 2026-04-10 |
| `src/fleet_broadcast.rs` (in scope per dispatch) | 717 | hot | 2026-04-27 | author of #199 |
| `src/channel/mod.rs` (trait surface) | 334 | hot | 2026-04-26 | 2026-04-22 |
| `src/channel/ux_event.rs` | 1050 | control | 2026-04-22 | 2026-04-22 |
| `src/channel/contract.rs` | 267 | control | 2026-04-22 | 2026-04-22 |
| `src/channel/event.rs` | 227 | control | 2026-04-26 | 2026-04-22 |
| `src/channel/sink_registry.rs` | 182 | peripheral | 2026-04-22 | 2026-04-22 |
| `src/channel/caps.rs` | 166 | peripheral | 2026-04-22 | 2026-04-22 |
| `src/channel/binding.rs` | 160 | peripheral | 2026-04-22 | 2026-04-22 |
| `src/channel/discord.rs` | 8 | peripheral (placeholder) | 2026-04-26 | 2026-04-26 |

**Comfort-zone first-pass**: 6 files in `src/channel/` (binding/caps/contract/sink_registry/ux_event/discord) have not been touched since 2026-04-22 (≥ 4 days legacy at audit time). Tier-1 prioritised the trait surface plus the only file touched today (telegram.rs); tiers 2/3 captured the legacy scaffold via deliberate first-pass scan.

## Findings

### Critical (path-keyword auto-Critical applied) — 1 entry

#### C1 — `is_user_allowed` legacy `None` accepts every Telegram user

- **Where**: `src/channel/telegram.rs:197-203` (`fn is_user_allowed`) + `:572-589` (Authz drop guard) + `:959-969` (init-time warning).
- **Why auto-Critical**: channel auth (`token`) + allowlist semantics — keyword `is_user_allowed` matches the spirit of the path-keyword rule (`check / verify / validate / audit / authorize`) even though the literal name doesn't.
- **Behaviour**: `user_allowlist == None` means "accept anyone in the bound Telegram group". `init_from_config` does `tracing::warn!` once at startup if the field is missing, but a legacy / hand-edited `fleet.yaml` that omits the field still boots and silently runs in accept-all mode forever.
- **Risk**: anyone added to the Telegram group can send messages that get treated as operator input — full agent fleet surface (PTY raw inject path used for interactive prompts at lines 612-633 makes this especially load-bearing).
- **Recommendation**: fail-closed default. Either require explicit `user_allowlist: [...]` (treat absence as parse error in `parse_config`), or accept an explicit `accept_all: true` opt-in. Either reverses the principle of least authority bias from "open until configured" to "closed until configured". Path-keyword auto-Critical: requires explicit operator-wake decision, not an auto-merge fix.

### High — 4 entries

#### H1 — `Channel::send` discards binding routing

`src/channel/telegram.rs:1853` — `let topic_id: Option<i32> = None; // Full binding-based topic routing lands with the dispatcher in T2`. The trait's "send to a binding" promise is silently broken: the send goes to the group only, not to the binding's specific topic. Callers that route through the trait surface (rather than the legacy `try_telegram_*` free fns) get behaviour that does not match the trait doc. Today the only `Channel::send` consumer is the Telegram impl itself and it is not wired into the production hot path — but the trait shape lies about what it does.

#### H2 — `Channel::edit` and `Channel::delete` are `bail!` stubs

`telegram.rs:1888-1898` — both return `Err("not wired yet — use try_telegram_edit")`. Cap matrix advertises `edit: true` (`telegram.rs:1711`); a `select_action` consumer that picks "edit" based on caps will reach the unimplemented method and bail. The mismatch between caps and impl is a runtime trap.

#### H3 — Stale module doc claims scaffold is unused

`src/channel/mod.rs:8-13` reads:

> **Status (T1 prep scaffold):** this module is intentionally unused by any call site. PR2 in the T1 series (the atomic type cut-over) is the one that wires `Arc<Mutex<TelegramState>>` leaks through `Bootstrap` / `Daemon` / `App` onto this trait.

But `register_active_channel` / `active_channel()` is now wired in **6** call sites: `bootstrap/telegram_init.rs:40`, `daemon/supervisor.rs:126,134`, `daemon/ci_watch.rs:622`, `daemon/mod.rs:608`, `api/handlers/team.rs:146`, `api/handlers/instance.rs:193`. The trait is impl'd in `telegram.rs:1825`. The module is no longer scaffold-only — the comment misleads anyone reading mod.rs first.

#### H4 — `MsgRef` returned by `Channel::send` is opaque-empty

`telegram.rs:1869-1872` (media path) and `:1880-1884` (text-only path) both return:

```rust
crate::channel::MsgRef {
    binding: crate::channel::BindingRef::new("telegram", None, ()),
    id: msg_id.to_string(),  // "0" for text-only path (line 1882-1883)
}
```

The text-only path hands back `id = "0"` as a placeholder — caller has no way to address the just-sent message for later edit/delete. The binding payload is `()` (unit) so even the kind discriminator survives but the routing payload is gone. Combined with H1/H2 this means `Channel::send → edit/delete` round-trips do not work today.

### Medium — 5 entries

#### M1 — `caps.rs` doc claims UX readers don't exist yet

`src/channel/caps.rs:15-18`:

> **Status (T1 prep scaffold):** fields are defined; readers for the UX region land with the UX renderer in a later PR.

But `ux_event.rs:345 select_action(event, caps)` reads `caps.react`, `caps.edit` today. Doc lags code.

#### M2 — Module-wide `#![allow(dead_code, unused_imports)]` silences lint

`src/channel/mod.rs:30` blanket-suppresses dead-code lint for the entire `src/channel/` tree. Sprint 19 Track 1.A `cargo clippy --features tray -- -W dead_code` had 0 findings partly because of this opt-out. Once H3 / H4 land and the scaffold is genuinely consumed, this allow can be removed; until then it hides any genuine dead code in the scaffold.

#### M3 — Documented but not yet ingested edit-events

`telegram.rs:1716-1725`: `receives_edit_events: false` with TODO comment. Bot API does push `edited_message`, but the teloxide dispatcher only registers `Update::filter_message()`. UX renderer cannot react to edits; this is a known gap with explicit TODO. Tracked as functional debt.

#### M4 — `MsgPayload` lossy at trait surface

`event.rs:78-82`:

```rust
pub struct MsgPayload {
    pub text: String,
    // TODO(T1b+): attachments, reply-to metadata, inline entities.
}
```

Today the Telegram adapter sidesteps this by populating `inbox::InboxMessage::attachments` directly (`telegram.rs:716`). The trait-level `ChannelEvent::MessageIn { payload: MsgPayload }` therefore does not faithfully describe the inbound event surface — any future adapter that goes through the trait will lose attachment / reply metadata.

#### M5 — `TelegramState` parallel maps require manual bidirectional sync

`telegram.rs:116-118`: `topic_to_instance: HashMap<i32, String>` + `instance_to_topic: HashMap<String, i32>`. `record_binding` (line 1942-1945) updates both; `take_binding` (line 1949-1952) removes from both. Any future direct mutation site must remember to update both sides — drift produces silent topic-routing bugs. Could be replaced by a single `HashMap<i32, String>` with reverse lookup helper, or a small bidi-map wrapper that enforces invariants.

### Low — 4 entries

#### L1 — `notify` impl drops `severity`

`telegram.rs:1978-1992`: the trait passes `severity: NotifySeverity`, but the impl branches only on `silent: bool`. Warn / Error / Info all render identically. If callers expect severity to influence formatting (emoji prefix, mention escalation), they get nothing.

#### L2 — `discord.rs` placeholder is single-noop, no contract test

`src/channel/discord.rs` is 8 lines, 1 `_placeholder()` fn. Feature gate compiles cleanly but nothing exercises a Discord `impl Channel`. When Phase 2 lands, expect the contract harness from `contract.rs` to grow a Discord call site.

#### L3 — Sentinel `"__fleet__"` for fleet-binding topic registry

`telegram.rs` references a `FLEET_BINDING_SENTINEL` (`docs/DESIGN-stage-b-ux.md` §3/§5) for storing the fleet-binding topic id in the on-disk topic registry. Sentinel-string-as-key is a magic value. Future fleet-event subscriptions (multi-fleet?) would need a richer schema; for now it's an explicit tradeoff documented in design. Minor refactor candidate, not a bug.

#### L4 — `init_from_config` allowlist legacy-vs-empty messaging

`telegram.rs:959-969` — three log paths:
- `None` → warn: "any group member can command the fleet"
- `Some([])` → warn: "all inbound messages will be rejected"
- `Some([...])` → info: "user_allowlist active"

The `Some([])` case is described as "rejects all" — operationally identical to "channel is offline for command intake". Could be a hard error at init (operator typo guard), but reasonable people may disagree. Cosmetic.

## Praise — patterns worth replicating

### Replicate

- **`ChannelCapabilities::default()` is conservative "nothing supported"** (`caps.rs:66-90`). Every new adapter must explicitly opt-in per capability — this surfaces the feature matrix at review time. Excellent defensive default; replicate this idiom anywhere a new variant otherwise risks silent wider-than-intended behaviour.
- **`BindingRef` opaque payload** (`binding.rs:23-67`) — `Arc<dyn Any + Send + Sync>` keeps core code from peeking at platform-specific shapes. `kind` + optional `display_tag` are the only public field surfaces. The opacity contract is cheap to clone (Arc) and reviewable (no public payload accessor).
- **`contract.rs` registry-side trait harness** (`contract.rs:1-50`) — verifies the subset of `Channel` invariants observable without a real backend. New adapters add a single `run_registry_contract` call site instead of duplicating invariant logic. Future Discord / Slack adapters will benefit immediately.
- **`UxSinkRegistry::emit` snapshot-then-emit pattern** (`sink_registry.rs:67-76`) — clones `Vec<Arc<dyn UxEventSink>>` under the lock, releases lock, then iterates. A slow sink can't block registration or other emits, and the Arc clone is cheap. Apply anywhere a fan-out registry exists.
- **`select_action` exhaustive on the event axis** (`ux_event.rs:345`) — purely-functional, cap-blind, easy to diff against `PLAN-channel-ux-layer.md` §6 table. Reviewers can verify the table-to-code mapping 1-to-1. Replicate for any cap-degradation decision.

### Preserve as-is (load-bearing complexity, do not emulate)

- **`Arc<Mutex<TelegramState>> + lock_state` synchronization** (`telegram.rs` throughout). Matches established crate convention (`src/sync.rs::lock_poisoned`). Don't reach for `RwLock` here — there's no read-heavy contention, and the contention model is dominated by short bursts of inbound polling + outbound send, not concurrent reads.
- **`telegram_runtime().block_on(...)` from sync trait methods** (`telegram.rs:1810`, etc.) — intentional separation between the teloxide private tokio runtime and the main thread. Sprint 14 PR-AK already moved the worst nested-runtime hazards to `async fn`. Remaining `block_on` paths are either init/teardown (cheap) or sync-trait-impl seams (necessary glue), not re-introducible bugs.
- **Free-fn `try_telegram_*` API surface coexisting with the trait impl** (`telegram.rs:1281, 1318, 1366, 1388, 1422`) — explicit doc comment at `:2003-2008` calls out the rationale: trait `send/edit/delete` are not yet wired to the dispatcher, so UxEventSink uses free fns. Keep until the T2 cut-over; merging now would blow scope.

### Earmarked for refactor (works now, plan to revisit)

- **`Channel::send/edit/delete` stubs** — H1/H2/H4 capture this. Wire properly in the T2 dispatcher PR; the trait's actual contract is correct, just unimplemented.
- **`#![allow(dead_code, unused_imports)]` blanket** — once the scaffold is genuinely consumed, demote to per-item allows so future dead code surfaces in clippy.
- **`MsgPayload` minimal fields** — expand alongside attachment / reply-to support at the trait surface (M4).
- **`receives_edit_events: false`** — flip when the teloxide `filter_edited_message` ingest path lands (M3).

## Coverage

### Test coverage

| Area | Test surface | Gap |
|---|---|---|
| `Channel` trait registry-side invariants | `contract.rs::run_registry_contract` covers Telegram | No Discord call site yet (deferred to Phase 2) |
| Default caps conservatism | `caps::tests::default_caps_are_conservative` ✓ | — |
| `BindingRef` opacity | `binding::tests::*` (5 tests, downcast / display_tag / clone) ✓ | — |
| `OutMsg` serde | `event::tests::*` (5 tests including backward-compat) ✓ | — |
| `select_action` table | `ux_event::tests` covers caps_react / edit / both / neither × 4 events | Edge case: react capability with empty origin_msg id (would degrade where?) |
| `UxSinkRegistry` | `sink_registry::tests` covers register / emit / multi-sink / empty / singleton ✓ | — |
| `fleet_broadcast` | `fleet_broadcast::tests` covers render / compute_targets / dispatch / append_event_log (after #199) | — |
| `Channel::send/edit/delete` actual transport | not tested | Would crash if invoked through trait today (H1/H2/H4) |
| `is_user_allowed` matrix | `telegram.rs::tests` covers None / Some / Some(empty) | Boundary: enormous list (perf), missing user_id field → falls through to `None` allowlist branch (test missing) |
| Inbound attachment download fail | `t-20260426024342801229-20` (backlog) tracks UX-side gap | Reported failure path is `tracing::warn!` only — no test |

### Doc-vs-code drift

- **`mod.rs:8-13`** — claims module is unused; actually wired in 6 call sites. **(H3)**
- **`caps.rs:15-18`** — claims UX caps readers "land in a later PR"; `select_action` reads them today. **(M1)**
- **`telegram.rs:2003-2008`** "trait methods are still bail! stubs" — accurate; but the module-level mod.rs comment doesn't reference this nuance, and a reader who skips telegram.rs will miss it. Cross-doc consistency.

### Dead code / unused

- Module-wide `#![allow(dead_code, unused_imports)]` (`mod.rs:30`) hides any organic dead code. **(M2)**
- Telegram free-fn surface (`try_telegram_*`) duplicates trait methods — by design today, but trackable.

## Refactor opportunities (medium-effort, mid-sprint)

| Id | Description | Effort | Priority |
|---|---|---|---|
| R1 | Wire `Channel::send/edit/delete` to actual binding routing (close H1/H2/H4) | M-L | High |
| R2 | Update `mod.rs` + `caps.rs` doc comments to match post-T1-prep state (H3 + M1) | XS | High (operator-visible) |
| R3 | Remove module-wide `#![allow(dead_code)]` once R1 lands; demote to per-item allows | S | Medium |
| R4 | Promote `is_user_allowed` to channel-level "auth" abstraction so future Discord adapter has the same fail-closed contract | M | Medium (couples to C1 fix) |
| R5 | Replace `TelegramState` parallel maps with bidi-map helper or single map + lookup (M5) | S | Low |

## Cross-area dependencies

Each entry double-labelled with `reported_from: A, primary_owner: <area>` per challenge #4. Findings duplicated in the target area's `<AREA>.md` for visibility.

### CD1 — `active_channel()` registration gap silent fall-through  
- `reported_from: A`, `primary_owner: B (daemon)`  
- `daemon/supervisor.rs:126,134` + `daemon/ci_watch.rs:622` + `daemon/mod.rs:608` all do `if let Some(ch) = active_channel() { ch.notify(...) } else { tracing::debug!(...) }`. When the channel never registers (e.g. fleet.yaml has no `channel:` block, or token env var missing), every stall / recovery / crash notification silently drops to `debug` (not visible at default log level). For an operator who deployed without realising the channel didn't initialise, the first symptom is "the daemon never tells me anything" — silent failure. Track B should consider whether a missing channel registration is a setup error worth surfacing once at boot.

### CD2 — `active_channel().create_topic()` ignores Err  
- `reported_from: A`, `primary_owner: D (MCP / API handlers)`  
- `api/handlers/instance.rs:193-195` and `api/handlers/team.rs:146-147` use `.and_then(|ch| ch.create_topic(name).ok())` and discard the error. A failed topic creation (e.g. Telegram API rate limit) silently boots the instance with no Telegram surface. Track D should at minimum `tracing::warn!` the error.

### CD3 — `ChannelKind` discriminator leak into `inbox.rs`  
- `reported_from: A`, `primary_owner: B (daemon — inbox layer)`  
- `inbox.rs:139-140` matches `ChannelKind::Telegram` / `ChannelKind::Discord` to render a string. Acceptable today (small enum); flag as future refactor when more variants land — converting via `AsRef<str>` on `ChannelKind` would localise the mapping. Not a bug.

### CD4 — `fleet_broadcast.rs` writes daemon-level path  
- `reported_from: A`, `primary_owner: A (channel layer / fleet event log)`  
- `<home>/fleet_events.jsonl` is written by `fleet_broadcast::append_event_log`. Path lives outside `src/channel/` but logically is a fleet event log. The Phase 2 read API (deferred per Sprint 18.5 long-term backlog `t-20260426164120257127-1`) needs a clear ownership boundary — either move path constant to `channel/` or document that fleet event log is daemon-state, not channel-state.

## Sprint 21 actionable tasks (proposed)

Pulled from Critical / High / Medium findings. priorities reflect risk; final ordering is operator's call.

| Id | Task title | Priority | primary_owner |
|---|---|---|---|
| **F1** | Make `is_user_allowed` fail-closed (require explicit `user_allowlist` or `accept_all: true`) — covers C1 | **high** | A |
| **F2** | Wire `Channel::send/edit/delete` to actual binding routing in T2 dispatcher — covers H1/H2/H4 | **high** | A |
| **F3** | Update `mod.rs` + `caps.rs` doc-vs-code drift (H3 + M1) | normal | A |
| **F4** | Demote module-wide `#![allow(dead_code)]` to per-item once F2 lands (M2) | low | A |
| **F5** | Surface `active_channel().create_topic()` Err in api/handlers (CD2) | low | D |
| **F6** | Surface `active_channel()` registration failure once at daemon boot (CD1) | normal | B |
| **F7** | Add edit-event ingest path (M3) — flip `receives_edit_events: true` after wiring | low | A |
| **F8** | Expand `MsgPayload` to carry attachments / reply-to / entities (M4) — couples to F2 | normal | A |
| **F9** | Replace `TelegramState` parallel maps with bidi-map helper (M5) | low | A |

(F2 + F8 cluster well as a single "T2 dispatcher cut-over" PR; F1 is operator-decision-required so not auto-merge per challenge #2 path-keyword.)

---

## Audit complete

- 1 Critical (auto-Critical via channel auth path-keyword)
- 4 High (trait contract gaps + doc drift)
- 5 Medium (lint suppression / minor doc drift / functional debt)
- 4 Low (cosmetic / placeholder)
- 5 Praise — replicate
- 3 Praise — preserve as-is
- 4 Praise — earmarked for refactor
- 4 Cross-area dependencies (3 to other Tracks, 1 self-Track)
- 9 Sprint 21 actionable tasks proposed

Per challenge #10, peer pass against `dev-impl-2`'s Track B (DAEMON.md) — see section below.

---

## Peer-pass: reading Track B `DAEMON.md` from a channel angle

(Per dispatch §10 + `m-20260426213832561994-93`. 1 paragraph blindspot critique, in-tree append.)

**Acknowledging B's peer-pass to me first**: B's 5 critiques back at CHANNEL.md (§7 of DAEMON.md) all land. Agreed: (1) C1 fail-closed inbound does NOT close the outbound notify info-leak — `supervisor::tick` lines 126/134 + `ci_watch.rs:622` push stall/recovery/CI notices to the bound Telegram group with no allowlist gate, leaking PTY tails (40 lines per stall) regardless of who's been added; my F1 (Sprint 21) needs an explicit "outbound gate is separate" sentence. (2) CD4 ownership split (persistence=B / format=A) is the right resolution and supersedes my "either / or" framing. (3) H1+H4 daemon consequence — opaque-empty `MsgRef` makes any future "clean up bot-sent messages on `delete_instance`" impossible; this is exactly the kind of cascade my channel-internal lens missed. (4) M5 contention from the tick layer during `crash_tx`-bounded(64) bursts is a real load profile I didn't articulate; "keep Mutex" still stands as advice but the contention regime needs noting. (5) `set_waiting_on` audit gap accepted — my 8-command methodology missed it.

**Blindspots in DAEMON.md from a channel angle (4 critiques)**:

1. **B's "11 spawn sites / 0 graceful-shutdown" inventory is scope-correct but the systemic claim under-states**: Track A holds **two more spawn surfaces** that share the same shape and should be co-counted at fleet-summary time. (a) `src/channel/telegram.rs:78-89` initialises a private `tokio::runtime::Runtime` via `OnceLock<Runtime>`; that runtime hosts the entire teloxide dispatcher (`start_polling` line 348) plus every `block_on(send_with_topic / send_media)` call. On daemon shutdown the runtime is dropped silently with no graceful-task-drain — same shape as B's pty_read_loop F4 (no shutdown-flag observation in the read path). (b) `start_polling` itself spawns `telegram_runtime().spawn(...)` for the dispatcher loop; no JoinHandle stored, no shutdown signal. So the daemon-wide claim should be **"13+ spawn sites / 0 graceful systemic"** including Track A's two — worth a footnote in B's inventory or a Sprint 21 cross-track unification task (S21-A/B joint).

2. **F1–F4 cascade into channel binding state** (B's lifecycle traces stop at `agent.rs` / `api/handlers/instance.rs`, but the partial-state extends into Track A persistence): F1 spawn_agent partial-failure post-`record_binding` leaves an orphan **Telegram binding** (`topics.json` row + `instance_to_topic` map) for an agent that doesn't exist — symmetric to B's "orphan PID / phantom registry" but at the channel persistence layer. F2 delete-step-4 (registry mutate before child exit) compounds: if the dying child still holds the bound topic via teloxide's pending API call, a re-`spawn` of the same name through `record_binding` collides on the topic registry. F3 (app-mode `kill_agent` regression) doesn't call `take_binding`, so the Telegram topic reference leaks in `instance_to_topic` even after the agent is killed in TUI mode — operator who kills via TUI sees the topic stay in the group, no auto-removal. None of these are corruptly Critical (no data loss) but they're concrete A-side effects of B's lifecycle invariants; B's S21-B1/B2/B3 should explicitly cover Telegram-binding rollback as part of the lifecycle transaction.

3. **`fleet_broadcast::broadcast` partial-failure step 8 in B's `handle_delete` trace deserves narrower framing**: B notes "crash here → other agents never get InstanceDeleted; their fleet view stays stale until next ad-hoc sync". After PR #199 (Sprint 18.5 HOTFIX B Hybrid), `fleet_broadcast::append_event_log` writes to `<home>/fleet_events.jsonl` **before** the per-target inject loop (`fleet_broadcast.rs:248-251`). So step 8 is actually two sub-steps: 8a = persistent log (always succeeds barring fs error, which warns-not-fails), 8b = per-target inject (best-effort warn). A crash between 8a and 8b means "log shows InstanceDeleted, no agent received it" — but on next daemon restart, the log is the source of truth and any future replay-API consumer can detect the gap. B's "stale until next ad-hoc sync" is overstated for the post-PR-199 architecture; recovery is bounded by Phase 2 read API rather than unbounded ad-hoc sync. Worth a 1-line correction in B's lifecycle trace once Sprint 21 lands the read API.

4. **C↔D joint sub-track (`app/api_server.rs`) is also an A↔D bridge** worth surfacing: TUI.md flags `app/api_server.rs` (130 lines) for cross-pass with Track D. From the channel angle, MCP handlers route to channel via `try_telegram_reply` / `try_telegram_react` / `try_telegram_edit` / `inject_provenance` (`mcp/handlers.rs:74,88,102,399`) — the MCP→channel bridge is not in `app/api_server.rs` but in `mcp/handlers.rs`. So the cross-pass should be **C↔D for app-bridge AND A↔D for channel-bridge**, both surfaces flowing into the same MCP request handler layer. If the proposed sub-track audits `app/api_server.rs` only, it'll miss the channel-side outbound surface. Recommend the sub-track scope explicitly include the `mcp/handlers.rs` channel-bridge entries (≤200 lines if scoped tight) so A↔D coupling is exercised in the same pass.

**Cross-pollination Sprint 21 backlog ideas (from this peer-pass)**:

- **S21-A/B joint** — unify spawn-site inventory across `src/daemon/` + `src/channel/telegram.rs` + `src/instance_monitor.rs`; document graceful-shutdown stance once in `daemon/mod.rs`.
- **S21-A** add Telegram-binding rollback to B's S21-B1 lifecycle transaction (so `record_binding` participates in spawn rollback / `take_binding` participates in delete cleanup).
- **S21-A/D joint** — extend C↔D `app/api_server.rs` audit to the `mcp/handlers.rs` channel-bridge entry points (`try_telegram_reply` / `try_telegram_react` / `try_telegram_edit` / `inject_provenance`); ~200 LOC cross-pass.

(End peer-pass section. CHANNEL.md complete.)

