# Telegram topic cleanup RCA + scope brief — (α) auto-cleanup + (γ) doctor topics

**Sprint 59 Wave 2 PR-1 — Path B doc-only RCA.**
Operator-surfaced 2026-05-09 (telegram topic cleanup smoke incident):
- Daemon restart recreates topics; old topics aren't `deleteForumTopic`'d.
- Same-named duplicates accumulate after each `delete_instance` + recreate cycle.
- Stale topics linger across daemon restart loops.
- Bot can only call `deleteForumTopic` when it has `can_manage_topics` permission.

This RCA traces each pathology to source, scopes the (α) auto-cleanup
+ (γ) `agend-terminal doctor topics` IMPL, and pins the IMPL gate so
downstream dispatch is auto-triggered (or surface-blocked when scope
exceeds estimate).

---

## 1. Symptom inventory

| # | Symptom | Reproduction |
|---|---|---|
| **S1** | Same-named duplicate topics accumulate. | Operator: `delete_instance dev`, then `create_instance dev`. The original topic is NOT deleted; a new topic with the same name is created alongside. After 3 cycles: 3 "dev" topics in the chat. |
| **S2** | Stale topics survive daemon restart. | Operator: `agend-terminal stop` mid-sprint with N agents, then restart with `fleet.yaml` listing only N-1 agents (one was retired without a clean delete). The retired agent's topic remains in the chat indefinitely. |
| **S3** | Topic ID drift between `topics.json` and `fleet.yaml.topic_id`. | Operator hand-edits `fleet.yaml.<agent>.topic_id` to a stale value; daemon restart picks up the stale ID, creates a NEW topic anyway, and now both IDs are tracked separately (one in `topics.json`, one in `fleet.yaml`). |
| **S4** | Bot lacking `can_manage_topics` permission produces silent failure on cleanup attempts. | Operator's bot was originally added without forum permissions. `delete_topic()` calls `bot.delete_forum_topic` which returns API error, current code silently swallows via `let _ = ... .await`. Operator sees no signal. |
| **S5** | Operator has no operator-callable surface to inspect or batch-clean topic state — must read `topics.json` + cross-reference Telegram chat manually. |

---

## 2. Root cause(s)

### S1 — Same-named duplicates on recreate
- **Source**: `src/mcp/handlers/instance_lifecycle.rs:82` calls `telegram::delete_topic(home, tid)` ONLY when the instance had a `topic_id` recorded in `fleet.yaml`. If `delete_instance` is called on an instance whose topic_id wasn't yet flushed to `fleet.yaml` (race: fast delete after create), no cleanup fires.
- **Source**: `src/channel/telegram/adapter.rs:264` runs `delete_topic` on the `Channel::delete_instance` outbound op — but the op fires only when the adapter is informed of the lifecycle event, which doesn't always happen synchronously with `instance_lifecycle::handle_delete_instance`.
- **Gap**: there's no canonical "instance N deleted → topic-cleanup" guarantee path that holds for ALL deletion code paths (MCP handler / fleet.yaml hand-edit / bootstrap orphan cleanup).

### S2 — Restart-resurrected stale topics
- **Source**: `src/channel/telegram/bootstrap.rs:71-78` runs orphan-cleanup, but only matches against `config.instances.keys()` from the CURRENT `fleet.yaml`. If a topic was created for an instance, that instance was hand-removed from `fleet.yaml` between two daemon runs, AND the `topics.json` registry retained the topic_id → orphan-cleanup deletes it. **However**, the cleanup excludes `tid != 1` and `inst_name != FLEET_BINDING_SENTINEL`, AND requires `topics.json` to know about the topic. Topics created via direct API call but never recorded in `topics.json` are never enumerated.
- **Source**: `src/channel/telegram/bootstrap.rs:104-122` re-creates topics for instances missing `topic_id` UNCONDITIONALLY, even when a topic for that name already exists in the chat (Telegram allows duplicate-named topics). No "look up topics by name in chat" pre-check.

### S3 — Three-source-of-truth drift
- **Source**: `src/channel/telegram/topic_registry.rs::register_topic` (line 40-56) writes to ALL THREE on creation: `topics.json`, `fleet.yaml.<inst>.topic_id`, in-memory state. **Good**.
- **Source**: But there's no symmetric "reconcile read-side" that flags drift. If operator hand-edits `fleet.yaml.topic_id`, `bootstrap::init_from_config` reads it (line 87-91) AND `topics.json` (line 94-102), creating a merge that may double-track the same instance. The "reuse existing topic from topics.json if present" check in `create_topic_for_instance` (line 76-79) only catches the topics.json path, not the fleet.yaml path.

### S4 — Silent permission failure
- **Source**: `src/channel/telegram/topic_registry.rs:106-120` — `delete_topic` swallows the `delete_forum_topic` error with `let _ = ...`. No tracing emitted with the error itself; only the "deleted topic" success-log fires regardless.
- **Source**: No pre-call check for `can_manage_topics` in chat-admin permissions. Bot API would return a structured permission error which is currently dropped on the floor.

### S5 — Operator has no diagnostic surface
- **Source**: `src/cli.rs::run_doctor` (line 97) calls `bootstrap::doctor::validate_fleet_config` + `helper-staleness` (Sprint 58 Wave 2 PR-1) but has zero hooks for telegram-topic state. Operators must `cat ~/.agend-terminal/topics.json` + manually compare against the Telegram chat.

---

## 3. (α) Topic auto-cleanup design

Goal: every code path that retires an instance triggers a guaranteed,
permission-checked, observable topic-cleanup operation.

### 3.1 Trigger detection

Three trigger sites:
- **(α-a) MCP `delete_instance` handler** — `src/mcp/handlers/instance_lifecycle.rs::handle_delete_instance`. Already calls `delete_topic` at line 82; ensure it ALSO catches the case where `topic_id` is in `topics.json` but not yet in `fleet.yaml` (S1 root cause). Look up via `lookup_topic_for_instance` if `fleet.yaml.topic_id.is_none()`.
- **(α-b) `bootstrap::init_from_config` orphan-cleanup pass** — already exists at line 71-82. Extend the loop to ALSO query the live Telegram chat for forum topics, compare against `topics.json` + `fleet.yaml`, and delete any "in chat but not tracked" topics that match an `instance_names` historical pattern. Defer cross-chat orphans to (γ) doctor surface (out of scope for (α) auto-path).
- **(α-c) Fleet-config-change watcher (NEW)** — when `fleet.yaml` is mutated (instance removed via hand-edit), trigger a reconciliation pass on next daemon tick. Implementation: leverage existing `fleet::FleetConfig::load` reload pattern; on detected diff, schedule `delete_topic(stale_tid)` for any removed instance. *Possible scope risk: file-watch infra.* Alternative: defer to next `bootstrap::init_from_config` run (less responsive but zero new infra).

### 3.2 Permission check

Pre-call gate inside `topic_registry::delete_topic`:
```rust
fn delete_topic(home: &Path, topic_id: i32) {
    // Pre-flight: query bot's chat-admin permissions.
    if !can_manage_topics(&ch).await {
        tracing::warn!(
            topic_id,
            "delete_topic skipped: bot lacks can_manage_topics — \
             grant via Telegram → Chat → Manage admins → bot name → \
             enable 'Manage topics'"
        );
        return; // unregister() also skipped; topic stays in chat AND registry
    }
    // ... existing code ...
}
```

The existing `let _ = bot.delete_forum_topic(...)` becomes `match bot.delete_forum_topic(...)`:
- `Ok(_)` → tracing::info "deleted topic"
- `Err(e)` if e is a permission error → tracing::warn with actionable hint (same as pre-flight)
- `Err(e)` other → tracing::error with full error chain

### 3.3 Daemon restart guard

Already exists at `bootstrap.rs:87-91` (read `fleet.yaml.<inst>.topic_id` first), `bootstrap.rs:94-102` (merge `topics.json`), `bootstrap.rs:104-122` (auto-create only if BOTH absent). The S2 root cause is the `bot.create_forum_topic` call at line 110-112 — it doesn't first ask "does a topic with this exact name already exist in the chat?" before creating.

Fix: add `bot.get_forum_topic_icon_stickers` OR `bot.get_chat` + iterate live topics to check for name collision BEFORE create. *Possible scope risk: Telegram API surface availability.* Alternative: rely on operator's `(γ) doctor topics` to surface duplicates post-hoc + manual cleanup — keeps (α) shape simpler, defers to (γ).

### 3.4 Permission-check helper

New shared helper:
```rust
async fn can_manage_topics_for(bot: &teloxide::Bot, chat_id: ChatId) -> bool {
    // Get bot's own chat member info → check admin rights.
    let me = bot.get_me().await.ok()?;
    let member = bot.get_chat_member(chat_id, me.id).await.ok()?;
    matches!(member, ChatMember::Administrator(admin) if admin.can_manage_topics)
}
```

Return value: `bool` (true/false), used by all three trigger sites
+ the new helper module.

---

## 4. (γ) `agend-terminal doctor topics` design

Goal: operator-callable diagnostic surfacing topic state + opt-in
batch cleanup.

### 4.1 New CLI subcommand

`agend-terminal doctor topics [--cleanup] [--format human|json]`

Default mode: read-only inspection.
- Loads `topics.json`, `fleet.yaml.instances.<>.topic_id`, queries live Telegram chat for current topic list.
- Classifies each topic into:
  - **`live`** — present in `topics.json` AND `fleet.yaml.topic_id` AND in chat.
  - **`stale_registry`** — in `topics.json` but NOT in chat (topic deleted out-of-band).
  - **`stale_chat`** — in chat but NOT in `topics.json` (topic created out-of-band).
  - **`drift_fleet`** — `fleet.yaml.topic_id` ≠ `topics.json[<inst>]` (write-side desync from S3).
  - **`orphan`** — in `topics.json` mapping to instance name not in `fleet.yaml`.

Output: human-readable table OR JSON list.

### 4.2 `--cleanup` flag

Opt-in batch sweep. For each classification:
- `stale_registry` → remove from `topics.json` (no chat operation)
- `stale_chat` → call `bot.delete_forum_topic` (gated by `can_manage_topics` check, prints warn+skip if permission missing)
- `drift_fleet` → unify by source-of-truth (operator chooses via prompt OR `--prefer-fleet|--prefer-registry` flag)
- `orphan` → call `delete_topic` (gated as above)

`--cleanup` requires interactive confirmation by default (operator
sees the list before action). `--yes` flag to skip the prompt.

### 4.3 Output format examples

**Human** (default):
```
Telegram topic state:
  4 live (alpha:42, bravo:43, charlie:44, dev:45)
  1 stale_registry (deleted-agent:99 — in topics.json, not in chat)
  2 stale_chat (manually-created:120, manually-created:121 — in chat, not tracked)
  0 drift_fleet
  1 orphan (gone-instance:88 — in topics.json, instance not in fleet.yaml)

Bot can_manage_topics: ✓ enabled (cleanup operations available)

Run with --cleanup to act on stale/orphan entries.
```

**JSON**: structured with the same classification keys, suitable for
piping into other tools.

### 4.4 Permission-check surfacing

Both modes report `can_manage_topics` status at top of output. If
`false`, all chat-mutating cleanup operations are skipped with the
actionable hint. Read-only inspection still works.

### 4.5 CLI integration point

`src/cli.rs::run_doctor` adds a `topics` subcommand branch (matches
existing pattern — Sprint 58 Wave 2 PR-1 helper-staleness lives in
the same surface). The sub-handler lives in a new module
`src/bootstrap/doctor_topics.rs` (~150-200 LOC) so `cli.rs` stays
under the LOC ceiling.

---

## 5. Backwards-compat strategy

### 5.1 Missing `can_manage_topics`
- **Pre-PR behaviour**: silent failure (delete_forum_topic returns Err, swallowed)
- **Post-PR behaviour**: `tracing::warn!` with actionable hint at every cleanup attempt site (delete_topic / `(α-c)` reconciliation / `(γ) --cleanup`). Read-only `(γ)` inspection works regardless.

### 5.2 No panic / no crash
- All API errors handled gracefully. `delete_topic` returns silently on permission-denied (registry untouched, topic stays in chat — operator can manually delete via Telegram UI OR fix bot permissions).
- `(γ) doctor topics` read-only mode never makes a write call.

### 5.3 Migration
- No schema changes to `topics.json` or `fleet.yaml`.
- Existing daemon restarts continue to work; first restart post-(α) merge runs the enriched orphan-cleanup pass (can be observed via tracing logs).
- If permission gate is missing on the bot, daemon logs warn + continues; operator can retroactively grant permissions and the next restart cleans up.

---

## 6. Scope estimate per part

| Part | Prod LOC | Test LOC | Total | Tier |
|---|---|---|---|---|
| (α-a) `delete_instance` topic_id fallback (lookup_topic_for_instance) | 10-20 | 30-50 | 40-70 | Tier-1 single primary |
| (α-b) bootstrap orphan-cleanup extension (live-chat enumeration) | 30-50 | 40-60 | 70-110 | Tier-1 single primary |
| (α-c) fleet-config-change watcher (deferred to bootstrap reload, NOT file-watch) | 10-20 | 30-50 | 40-70 | Tier-1 single primary |
| (α) Permission check helper + delete_topic surfacing | 30-50 | 40-60 | 70-110 | Tier-1 single primary |
| **(α) total** | **~80-140** | **~140-220** | **~220-360** | **Tier-1** |
| (γ) doctor_topics module (classification + format) | 100-150 | 60-100 | 160-250 | Tier-1 single primary |
| (γ) cli.rs integration | 10-20 | 10-20 | 20-40 | Tier-1 single primary |
| (γ) `--cleanup` flag (delete_topic per classification, confirmation prompt) | 30-50 | 30-50 | 60-100 | Tier-1 single primary |
| **(γ) total** | **~140-220** | **~100-170** | **~240-390** | **Tier-1** |
| **Combined (α)+(γ)** | **~220-360** | **~240-390** | **~460-750** | **Tier-1** |

Predicted file touches:
- `src/channel/telegram/topic_registry.rs` (delete_topic permission gate + error surfacing)
- `src/channel/telegram/bootstrap.rs` (orphan-cleanup extension)
- `src/mcp/handlers/instance_lifecycle.rs` (lookup_topic_for_instance fallback)
- `src/bootstrap/doctor_topics.rs` (NEW — classification logic)
- `src/cli.rs` (subcommand wiring)

---

## 7. Out-of-scope explicit

- **Not changing `fleet.yaml` schema** — `topic_id` field already exists.
- **Not changing `telegram::inbound`** — inbound message routing uses topic_id but doesn't manage lifecycle.
- **Not changing mirror layer** — fleet binding topic and per-instance topic mechanisms are separate concerns; mirror handles message broadcast, not topic lifecycle.
- **Not changing `chat_id` management** — single-chat-id assumption preserved; multi-chat support is Sprint 60+ candidate.
- **Not adding file-watch infrastructure for fleet.yaml** — (α-c) reuses existing `bootstrap::init_from_config` reload pattern; live-watch is operator's manual restart trigger.
- **Not auto-recreating deleted-by-operator topics** — if operator manually deletes a topic via Telegram UI, daemon detects on next restart via `(γ)` classification but doesn't auto-recreate; operator's manual delete is treated as authoritative.

---

## 8. IMPL gate / conditional escalation

### 8.1 IMPL dispatch criteria (auto-trigger)

If RCA reviewer verdict = VERIFIED AND scope estimate from §6 holds
(<= ~750 LOC combined for both parts), IMPL dispatch follows
automatically per Wave 2 sequencing context. The conditional task
is `t-20260509090003452174-17` (referenced in lead's m-20260509152716671697-225).

Dispatch shape: single combined IMPL PR for (α) + (γ) (estimated
~460-750 LOC) OR split into 2 sequential PRs if reviewer flags a
clean (α)/(γ) scope-split during VERIFIED verdict.

### 8.2 Surface-block criteria (escalate to lead)

Surface to lead/general for triage if RCA-stage investigation
reveals:
1. `(α-c)` requires NEW file-watch infrastructure to be reactive (current proposal defers to bootstrap reload) — adds Sprint 60 `notify` crate dependency or similar.
2. `bot.get_chat` doesn't enumerate forum topics in the teloxide API surface — `(α-b)` enriched orphan-cleanup design changes shape.
3. Permission-check helper requires `bot.get_chat_administrators` enumeration which is rate-limited under heavy fleet sizes — needs caching layer.
4. Combined LOC estimate exceeds 750 — split (α) and (γ) into separate PRs with sequential reviewer cycles.
5. `(γ)` `--cleanup` interactive prompt conflicts with daemon-mode semantics (doctor runs in non-TTY contexts) — needs `--yes` mandatory + UX redesign.

### 8.3 Q2=(C) protocol compliance

- All daemon-side worktree operations via `bind_self` / `release_worktree` / `force_release_worktree` (Sprint 59 Wave 1 PR-5).
- Force-push only to self-owned PR feature branches (NEVER to main).
- NO `AGEND_GIT_BYPASS=1` use anywhere.
- File-overlap audit: (α)+(γ) IMPL touches `src/channel/telegram/*` + `src/cli.rs` + `src/bootstrap/doctor_topics.rs` (new). Verified non-overlapping with concurrent Wave 2 filler candidates (#14 IME / #5 dedup orphan operate on distinct subsystems).

---

## History

- **2026-05-09 telegram topic cleanup smoke incident** — operator surfaced 4 pathologies (S1-S4) via telegram thread.
- **2026-05-09 Sprint 59 PLAN draft** — (α) and (γ) listed as Wave 2 P0 candidates.
- **Sprint 59 Wave 2 PR-1 (this RCA, Path B doc-only)** — captures the symptom inventory + root cause + design + scope estimate; gates the IMPL dispatch.
- **Sprint 59 Wave 2 PR-IMPL** (conditional, post-RCA-verdict) — ships (α) + (γ) per scope estimate. Task `t-20260509090003452174-17` pre-allocated.

The combined Wave 2 dispatch closes the operator's reported S1–S5
with structural fixes at every triggering layer + diagnostic surface
for ongoing observability.
