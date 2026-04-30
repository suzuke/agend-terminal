# PLAN: Discord channel — second-adapter dogfood for the Channel trait

**Date:** 2026-04-29
**Status:** plan-first; awaiting operator GO before any `src/*` impl
**Branch:** `docs/discord-channel-plan` → PR for plan-doc only
**Origin:** operator directive via general m-20260429055609094093-37
**Process:** 4-perspective challenge round (Sprint 26-30 model) — synthesis below
**Scope decision:** project decision `d-20260429060155052366-0`

---

## 0. KISS gate (§0)

- **What real problem does this solve?** The Channel trait shipped in Stage A (`docs/PLAN-channel-abstraction.md`) is the *first* adapter only. Without a second adapter, the trait is a single-implementation interface — its claims of platform-neutrality are unverified. Stage B's stated job (per `PLAN-channel-abstraction.md` §5) is the **first real pressure test**.
- **Would deletion break anyone?** No external user is on Discord today (codex git archaeology: only commit `0c15614` ever touched Discord, and that was bootstrap-only — Phase 2 never started; reviewer P1). Deletion of this plan does not affect users. The deletion *would* leave the abstraction unvalidated, accumulating speculative-design debt.

**Therefore**: this PR ships the plan, not the impl. Impl wave gated by operator GO.

---

## 1. Verified current state

Grep / file-inventory at HEAD `eea15f2` on `main`:

| Artifact | State |
|---|---|
| `src/channel/discord.rs` | 8 LOC `_placeholder()` stub (commit `0c15614`, "Discord Phase 1 bootstrap") |
| `Cargo.toml` line 40 | `discord = []` empty feature |
| `src/channel/mod.rs::ChannelKind` | enum has `Discord` variant (line 119) — forward-declared |
| `src/channel/mod.rs::BindingOpts.extra` | doc comment cites Discord `category_name` (line 332) |
| `src/channel/contract.rs` | 269 LOC contract harness; explicit "future Discord/Slack adapters add their own call site" (lines 4-9) |
| `src/channel/telegram.rs` | 4114 LOC full reference adapter |
| `docs/PLAN-channel-abstraction.md` | Stage A ✓ shipped, Stage B = Discord = not started |
| `docs/PLAN-channel-ux-layer.md` | UX layer separation; transport trait separate |

**Git archaeology** (codex P1): no deleted Discord files; no failed past attempt. Discord is *unfinished*, not *disproven*.

---

## 2. Channel trait gap analysis

### 2.1 Synthesis verdict

Both perspectives that examined the trait surface concluded **zero breaking signature changes required**:

- **kiro structural (S1, S5)**: every `pub trait fn` in `src/channel/mod.rs` (lines 159-253) and every assertion in `src/channel/contract.rs` is adequate for Discord. Telegram-side leaks (e.g. `submit_key` in `record_binding`, `silent` in `notify`) are **harmless** — Discord adapter ignores them.
- **codex prior-art (P3)**: no BLOCKING leak found. Trait surface contains no `chat_id` / `topic_id` / `bot_token` / `update_id` / `from_id` fields. No production `match` on string `kind` outside adapters.

**Per `PLAN-channel-abstraction.md` §5 abort signal: "if Channel trait requires >2 breaking signature changes during Stage B, stop and redesign."** Initial analysis says we are at 0. If implementation surfaces >2 → STOP per protocol.

### 2.2 Per-method gap table (from kiro S1)

| Method | TG | Discord | Gap |
|---|---|---|---|
| `kind()` | `"telegram"` | `"discord"` | string swap |
| `caps()` | TG matrix | Discord matrix | new instance |
| `poll_event()` | mpsc drain | WS Gateway → mpsc drain | shape unchanged |
| `send` | `send_message` | `POST /channels/{id}/messages` | downcast `BindingRef` |
| `edit` | `editMessageText` | `PATCH .../messages/{id}` | unchanged |
| `delete` | `deleteMessage` | `DELETE .../messages/{id}` | unchanged |
| `create_binding` | forum topic | thread in category | `opts.extra` carries `category_id` |
| `remove_binding` | delete topic | `DELETE /channels/{id}` | unchanged |
| `has_binding` / `record_binding` / `take_binding` | HashMap<String, i32> | HashMap<String, u64> | adapter-internal |
| `attach_registry` | stores | stores | unchanged |
| `create_topic` | forum topic | thread | unchanged |
| `notify` | send_message | POST | `silent` ignored |
| `outbound_authorized` | user_allowlist | role/user allowlist | bool unchanged |
| `send_from_agent` | 4 ops | 4 ops | custom emoji format adapter-internal |

### 2.3 NEEDS-REFACTOR candidate (codex P3)

`fn kind(&self) -> &'static str` is a string discriminator while `enum ChannelKind { Telegram, Discord, ... }` exists nearby. String-typed return invites magic-string drift in non-impl code. **Severity: NEEDS-REFACTOR**, not blocking. Defer to its own PR (out of scope for Discord plan) — the Discord adapter does not require this fix to ship.

### 2.4 HIGH-RISK runtime-semantics (codex P2)

Event ordering, backpressure, and edit/delete semantics on the trait's runtime behaviour are speculative until a non-Telegram adapter exercises them. The Discord adapter is therefore the *test* for these semantics. Adversarial scenario #4 (gateway burst → core starvation) directly stresses this surface; mitigation in §5.

### 2.5 BindingRef downcast contract

Discord adapter introduces:
```rust
pub(crate) struct DiscordBindingPayload {
    pub channel_id: u64,
    pub thread_id: Option<u64>,
}
```
wrapped in `BindingRef::new("discord", Some(format!("DC#{}", channel_id)), payload)`. Core never inspects the payload — only the adapter downcasts.

---

## 3. Telegram → Discord mapping

| Concern | Telegram | Discord | New infra? |
|---|---|---|---|
| Inbound transport | long-poll updates | WS Gateway IDENTIFY/READY/MESSAGE_CREATE | None — `poll_event()` drains mpsc |
| Outbound transport | bot HTTP | bot HTTP (REST v10) | None |
| Auth | bot token (env var) | bot token (env var) | None |
| Allowlist | user IDs (i64) | user IDs (u64 snowflake) OR role IDs | Adapter-side allowlist parsing; trait unchanged |
| Per-instance binding | forum topic in 1 group | thread in 1 channel of 1 guild | None — `create_binding` delegates |
| Multimedia inbound | `file_id` → `getFile` | `attachments[].url` (CDN) | Minor — adapter eagerly downloads to `$AGEND_HOME/attachments/{instance}/`. `download_attachment` MCP gains URL-download fallback (~15 LOC). |
| Multimedia outbound | `sendDocument` etc. | multipart on `POST .../messages` | Adapter-internal |
| Message edit | `editMessageText` | `PATCH .../messages/{id}` | None |
| Reactions | `setMessageReaction` | `PUT .../reactions/{emoji}/@me` | None — `React { emoji: String }` adequate |
| Topic auto-archive | n/a (forum topics persist) | Discord threads auto-archive 1h/24h/3d/7d | Adapter-side periodic unarchive (~20 LOC) |
| Deletion event | **absent** (TG) → error-driven cleanup fallback | **present** (`channelDelete`) → direct `BindingRevoked` | Capabilities-gated already (`emits_deletion_events`) |

**Auth threat model**: same as Telegram — bot token in env var, single-operator localhost daemon. Per `docs/audit-over-engineering-2026-04-28.md`, no new defensive layer required.

---

## 4. Dependency choice

| Option | Adapter LOC | Transitive deps | Gateway protocol | Audit cost |
|---|---|---|---|---|
| `serenity = "0.12"` | ~400-500 | ~30 (incl. cache, voice, framework) | handled | high (large surface; framework features unused) |
| `twilight-gateway` + `twilight-http` + `twilight-model` | ~350-450 | **~15** | handled | **medium** (modular, only what we use) |
| Raw `tokio-tungstenite` + `reqwest` | ~400-500 (incl. ~200 LOC own gateway) | 0 new (already in tree) | own code | low dep cost, **high own-code maintenance**; gateway heartbeat/resume/reconnect is the hardest part |

**Recommendation: twilight-* (modular)**

Rationale (kiro S2):
- Aligns with audit doc posture: trust 3rd-party deps less than `teloxide` (which is also "trusted-but-large"); `serenity` framework runtime / cache / voice all unused.
- Modular cuts dep count in half vs `serenity`.
- Avoids re-implementing Discord gateway protocol (heartbeat/resume/reconnect) — the most fragile part to roll our own.
- `reqwest` already in tree → twilight-http reuses it.
- Gateway burst backpressure (adversarial #4) is handled at twilight's mpsc boundary; we add bounded queue at adapter→core edge.

**Counter-evidence considered**: codex prior-art (P1) found *no* historical record of dep-choice decision in Discord-related commits or design docs. `PLAN-channel-abstraction.md` §5 Stage B says "backed by `serenity`" but with no rationale. Therefore the recommendation here departs from the original plan with explicit reasoning; reviewer should challenge this swap during impl-PR review.

---

## 5. Adversarial scenarios + mitigations (from codex P5)

| # | Scenario | Localhost-real? | Cheapest mitigation |
|---|---|---|---|
| 1 | Trait surgery during Discord cutover regresses Telegram | Yes (reliability, not security) | Freeze trait sigs for experiment window; Telegram golden behavioural regression test before Discord wiring (§7 PR1 fixture) |
| 2 | Discord crate (twilight) introduces vuln/maint surface | Moderate | Feature-gate Discord off by default; no runtime activation unless `channels: [discord-*]` configured |
| 3 | Binding model mismatch (guild/channel/thread) → routing bugs | Yes (semantic > infra likelihood) | Adapter-owned payload invariants; negative tests for wrong-kind binding downcast |
| 4 | Discord gateway burst starves daemon main loop | Yes — single process can starve regardless of localhost | Bounded queue + explicit drop/backpressure policy at adapter→core boundary BEFORE dispatch |
| 5 | `channel:` (legacy singular) vs `channels:` (plural) ambiguous fleet.yaml in Discord-enabled paths | Yes (operator ergonomics) | Hard-fail on ambiguous mixed config in Discord paths; document one canonical migration in `docs/USAGE.md` |

All 5 are real and have cheap mitigations. None are blocking; each is a §3.5.10 / §3.5.11 fixture target during impl wave.

---

## 6. Scope tiers — operator picks

The 3 perspectives diverge slightly on MVP breadth. **Operator chooses one**.

### TIER-A — narrowest validation (codex P4 GO-NARROW)
**1-2 PRs, ~300-450 LOC.** Single inbound (`MessageCreate`) + single outbound (`send`) + single revoke (`channelDelete` → `BindingRevoked`) + contract test extension.

- Defer ALL: edit, delete, reactions, attachments, threads/binding lifecycle, auto-archive, multi-guild, slash commands.
- Validates trait + WS gateway scaffold + auth handshake.
- Cheapest "does the trait crack" test.

### TIER-B — minimal viable Discord (lead minimal-delta synthesis)
**4 PRs, ~600-700 LOC.** TIER-A + binding lifecycle + outbound `edit` + outbound `delete`.

- Defer: reactions, attachments, slash commands, multi-guild.
- Adds enough to be operator-usable for one-off Discord agent without parity.
- Each defer item answers "wait until user asks".

### TIER-C — kiro structural recommendation
**4 PRs, ~900 LOC.** TIER-B + reactions + attachments (URL-download + multipart) + auto-archive keepalive.

- Defer: slash commands, multi-guild, voice.
- Closest to feature parity with current Telegram impl.
- Highest LOC; risks scope creep but adapter LOC well-bounded by twilight choice.

**Decision criterion**: which scope minimally validates "the trait is not Telegram-shaped" while answering KISS "real problem solved"? My recommendation is **TIER-B**: TIER-A is too narrow to surface threading/binding-lifecycle semantics (the highest-risk speculative surface per codex P2); TIER-C bundles parity-feel work that has no current user demand.

**§3.5.12(d) framing**: this is additive, not removal — counter-example construction does not directly apply. Instead, the dogfood deliverable per scope tier is the surfaced trait-design failure modes during impl. If TIER-B's 4 PRs land with **0 trait-signature changes**, the abstraction is validated. If >2 changes are needed (`PLAN-channel-abstraction.md` §5 abort signal), STOP and redesign.

---

## 7. PR sequencing for chosen tier (TIER-B reference; adjust if TIER-A/C)

| PR | Scope | LOC est | §3.5.10 fixture | §3.5.11 test-first |
|---|---|---|---|---|
| **PR1** | DiscordState + DiscordChannel skeleton, twilight gateway connect, IDENTIFY+READY, auth handshake, `Connected` event emit, config parsing for `type: discord` | ~250 | **wire-format**: captured Gateway JSON (IDENTIFY / READY / HELLO / HEARTBEAT_ACK) replayed against mock WS | YES — RED commit asserts `Connected` emitted from fixture |
| **PR2** | Inbound `MESSAGE_CREATE` → `ChannelEvent::MessageIn`; outbound `send` (text only); minimal `notify` (silent ignored) | ~200 | **wire-format**: captured `MESSAGE_CREATE` payload + REST `POST /messages` response | YES — RED commit asserts MessageIn shape |
| **PR3** | Outbound `edit`, `delete`; `send_from_agent` (Reply / Edit; React/Provenance defer to TIER-C) | ~150 | **wire-format**: captured PATCH/DELETE responses | YES |
| **PR4** | `create_binding` / `remove_binding` (thread per instance), auto-archive periodic unarchive, `channelDelete` → `BindingRevoked`, contract harness extension (`run_registry_contract`) for Discord | ~200 | **wire-format**: captured thread lifecycle events; **persistence-replay**: binding map round-trip across daemon restart | YES — contract harness call site |

**Total TIER-B: ~800 LOC across 4 PRs** (slightly above original 600-700 estimate after dev structural detail).

Dependencies: PR1 → PR2 → PR3 → PR4 (linear; pipeline-dispatch §10.1 applies — impl pushes PR1 then immediately picks up PR2 scaffold).

Each PR § review: single-reviewer Tier-1 if no `src/api/` / `src/daemon/` core touched; AUTO-CRITICAL Tier-2 if PR4 binding-registry semantics touch core dispatch. Dispatch decision per orchestrator at PR time.

---

## 8. Risks (Discord-specific, beyond §5 adversarial table)

| Risk | Mitigation |
|---|---|
| Gateway reconnect amplification (storms) | twilight handles backoff; cap retry count, surface error to operator |
| Discord rate-limit per-bucket bursting under React/Edit churn | RateBudget wrapper `{ per_second: 5, per_minute: 50 }` (kiro S5 caps recommendation) — same token-bucket layer as Telegram |
| Multi-guild support (out of scope) creep | Plan **explicitly excludes**; adapter parses `guild_id: u64` single value, errors on array. fleet.yaml schema enforces this in PR1. |
| DM (direct message) vs channel/thread topic mapping | Out of scope: only thread-in-channel binding supported. DM path raises `Err(NotSupported)`. |
| Custom emoji `name:id` format | Adapter-internal handling; trait `React { emoji: String }` accepts both Unicode and Discord custom syntax |

---

## 9. Out of scope (operator decided 2026-04-29)

- **Slack** — wait until user asks
- **Matrix** — wait until user asks
- **Discord voice channels** — never
- **Discord slash commands** — defer (Telegram has no equivalent; scope-creep risk)
- **Multi-guild fleet** — defer; single-guild is the parity floor
- **Discord stickers / activities / forum-channel ForumTag** — defer
- **`kind() -> &'static str` → `ChannelKind` typed return refactor** — separate PR, not gated on Discord ship

Each out-of-scope item answers "real problem solved? = no current user, defer per KISS."

---

## 10. Migration & backward compatibility

- Existing fleet.yaml `channel:` (singular legacy) parses unchanged; adapter wraps as named `default`.
- New `channels: [tg-main, discord-ops]` plural form already documented in `PLAN-channel-abstraction.md` §3.6 — no schema break for Discord landing.
- Telegram impl untouched by Discord PRs (zero trait surgery → zero TG callsite changes).
- Mixed `channel:` + `channels:` in same fleet.yaml → hard-fail with operator-actionable error message (adversarial #5 mitigation).

---

## 11. Verification

### Per-PR (within TIER-B)
- §3.5.10 wire-format: fixture per PR1-PR4 (specified §7 table)
- §3.5.10 persistence-replay: PR4 binding map round-trip
- §3.5.11 test-first: RED commit before GREEN per PR
- §3.5.12 deferred-defense: not applicable (additive feature, no defer); §3.5.12(d) counter-example does not apply (no removal)
- §3.5.13 verdict mirror: every reviewer verdict mirrored to GH PR comment

### TIER-B exit criteria
- Discord agent spawns, receives messages, replies, can edit own message, binding revoked when thread deleted
- All Telegram tests pass unchanged (Stage B abort signal not triggered)
- Trait signature unchanged at end of TIER-B (§5 abort signal not triggered)

### Stage C (third channel) trigger
- TIER-B trait stable → Slack/Matrix candidacy revisits via fresh plan-first round
- TIER-B requires trait surgery → STOP, redesign per `PLAN-channel-abstraction.md` §5

---

## 12. Implementation checklist (for impl wave, after operator GO)

- [ ] Cargo.toml: `discord = ["dep:twilight-gateway", "dep:twilight-http", "dep:twilight-model"]`
- [ ] Cargo.toml: optional twilight deps (versions pinned to current 0.16 series; minor-bump policy = manual review)
- [ ] PR1 — gateway scaffold + auth + Connected event
- [ ] PR2 — inbound MessageIn + outbound send + notify
- [ ] PR3 — edit + delete + send_from_agent (Reply/Edit)
- [ ] PR4 — binding lifecycle + auto-archive + BindingRevoked + contract harness
- [ ] `tests/channel_contract_discord.rs` — `run_registry_contract(DiscordChannel::new_for_contract_test(...), discord_make_binding)`
- [ ] `docs/USAGE.md` — Discord setup walkthrough + canonical fleet.yaml example
- [ ] `docs/PLAN-channel-abstraction.md` — Stage B checklist line items checked

---

## 13. Open questions for operator

1. **TIER selection** — A (narrowest validation) / B (recommended) / C (closest parity)?
2. **Dep choice** — accept twilight-* recommendation, or hold to original `PLAN-channel-abstraction.md` §5 `serenity`?
3. **Stage B abort signal** — confirm 0 trait surgery as exit gate, OR allow up to 2 sigs (per §5 §abort signal text)?
4. **Sprint placement** — Sprint 32 candidate, or fold into in-flight sprint?
5. **Dispatch ownership** — TIER-B impl wave dispatched same `dev` (kiro) or new impl agent?

---

## Cross-references

- `docs/PLAN-channel-abstraction.md` — Stage A history, §5 Stage B Discord
- `docs/PLAN-channel-ux-layer.md` — UX layer separation
- `docs/audit-over-engineering-2026-04-28.md` — single-operator localhost threat model (defensive surface justification)
- `docs/FLEET-DEV-PROTOCOL-v1.md` §0 KISS / §3.5.10 / §3.5.11 / §3.5.13 / §10.1 / §10.4
- general m-20260429055609094093-37 — operator directive
- decision `d-20260429060155052366-0` — challenge round scope
- task `t-20260429060158846927-1` — master task
