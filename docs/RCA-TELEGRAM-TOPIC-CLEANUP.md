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
| **S1** | Daemon restart auto-create produces duplicate-named topics when registry state is lost or partial. | Operator: corrupt or delete `topics.json` between daemon runs (e.g. `~/.agend-terminal/topics.json` accidentally truncated, OR fleet.yaml hand-edited to clear `topic_id`). On restart, bootstrap reads neither source has a topic_id for instance "dev", calls `bot.create_forum_topic` unconditionally — but the OLD "dev" topic still exists in the chat from the previous run. Telegram allows duplicate-named topics; now the chat has two "dev" topics (one stale, one fresh). Over multiple lossy-restart cycles: 3+ "dev" topics accumulate. |
| **S2** | Stale topics survive daemon restart when `topics.json` mapping is lost. | Operator: `agend-terminal stop` mid-sprint with N agents, then restart with `fleet.yaml` listing only N-1 agents (one was retired without a clean delete) AND `topics.json` no longer has the retired agent's mapping (registry corruption / manual cleanup). The bootstrap orphan-cleanup at `bootstrap.rs:71-78` only enumerates `topics.json` entries — topics that exist on the chat side but lack a `topics.json` mapping are NEVER detected. The retired agent's chat topic remains indefinitely. |
| **S3** | Topic ID drift between `topics.json` and `fleet.yaml.topic_id`. | Operator hand-edits `fleet.yaml.<agent>.topic_id` to a stale value; daemon restart reads BOTH sources at `bootstrap.rs:87-102`, merging them into `topic_map`. If the same instance is keyed in fleet.yaml with one tid and `topics.json` with another, the merge collapses to fleet.yaml's value (later overwrite), but `topics.json` retains the stale entry — the next daemon restart re-merges the stale entry, perpetuating the drift. |
| **S4** | Bot lacking `can_manage_topics` permission produces silent failure on cleanup attempts. | Operator's bot was originally added without forum permissions. `delete_topic()` calls `bot.close_forum_topic` (`let _ = ... .await`) and `bot.delete_forum_topic`, both of which return API errors that the code does not match-on or surface. Operator sees no signal; topics linger in the chat and the registry. |
| **S5** | Operator has no operator-callable surface to inspect or batch-clean topic state — must read `topics.json` + cross-reference Telegram chat manually. |

---

## 2. Root cause(s)

### S1 — Bootstrap restart auto-create doesn't pre-check chat for same-named existing topic
- **Source**: `src/channel/telegram/bootstrap.rs:104-122` (auto-create loop) calls `bot.create_forum_topic(chat_id, name, ...)` whenever an instance has no `topic_id` in either `fleet.yaml.<inst>.topic_id` (line 87-91) or `topics.json` (line 94-102). It does NOT first query the chat to ask "does a topic with this exact name already exist?".
- **Telegram API behaviour**: `create_forum_topic` succeeds even when a same-named topic already exists in the chat — Telegram allows duplicate names. There is no client-side dedup.
- **Trigger condition**: any registry-state loss between daemon runs that drops the `topic_id` for an instance whose topic still exists on chat. Real-world causes: corrupted `topics.json` (truncated mid-write before atomic rename, manual `rm`), operator hand-edit of `fleet.yaml` clearing `topic_id`, fresh daemon working dir against a chat that already has topics.
- **Gap**: bootstrap auto-create needs a "search live chat for same-named topic" pre-check OR a "query existing forum topics" enumeration to reuse rather than recreate.

(Note: the `delete_instance` path at `src/mcp/handlers/instance_lifecycle.rs:55-85` is robust — line 64's fallback `topic_id.or_else(|| telegram::lookup_topic_for_instance(home, name))` covers the pre-fleet.yaml-flush race, line 81-85 calls `delete_topic` when EITHER source has the id, line 84 surfaces a tracing warn on the genuine "no record anywhere" case. This path is not the S1 gap source — bootstrap is.)

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
- **(α-a) Bootstrap restart auto-create — pre-check live chat for same-named topic** (S1 fix). At `src/channel/telegram/bootstrap.rs:104-122`, before calling `bot.create_forum_topic`, query the chat for existing forum topics and reuse a same-named match if found (record into `topics.json` + `fleet.yaml.<inst>.topic_id`). The teloxide `Bot::get_chat` may not directly expose forum topic enumeration — investigation note: `getForumTopicIconStickers` is for icons, not topic listing; the Telegram Bot API has limited surface for "list all forum topics". If no native enumeration: fall back to `topics.json` re-scan + the (α-b) orphan-cleanup extension as the dedup mechanism. *Possible scope risk: surface gap if teloxide doesn't expose the needed API.* Alternative if blocked: rely on (γ) operator-driven cleanup as the recovery surface, document the limitation in compat strategy.
- **(α-b) `bootstrap::init_from_config` orphan-cleanup pass** — already exists at line 71-82. Extend the loop to ALSO query the live Telegram chat for forum topics, compare against `topics.json` + `fleet.yaml`, and delete any "in chat but not tracked" topics that match an `instance_names` historical pattern. Defer cross-chat orphans to (γ) doctor surface (out of scope for (α) auto-path).
- **(α-c) Delete path permission surfacing** (S4 fix). `src/channel/telegram/topic_registry.rs:106-120` `delete_topic` currently swallows API errors via `let _ = ... .await`. Replace with explicit match arms that distinguish permission errors (warn-log with actionable hint) from generic errors (error-log with full chain). The MCP `delete_instance` path already has the topic_id resolution robust per code line 64 + 81-83, so this fix surfaces failures rather than re-tracing the resolution path. (NOTE: this is what (α-a) was originally drafted as in r0 — that draft was based on a stale code reading that the line-64 fallback didn't exist; reviewer corrected. r1 reframes (α-a) to bootstrap-side as the actual S1 gap.)

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
- Classifies each topic with **explicit precedence-ordered assignment** (no overlapping classifications):

#### Classification algorithm (precedence-ordered, first-match wins)

For each (topic_id, instance_name) candidate observed across the 3 sources (topics.json / fleet.yaml.topic_id / live chat enumeration), apply rules in this order; assign the first matching class:

1. **`live`** — present in `topics.json` AND `fleet.yaml.<inst>.topic_id == topic_id` AND in chat. (All three anchors agree.)
2. **`drift_fleet`** — present in `topics.json` AND in chat AND `fleet.yaml.<inst>.topic_id` exists BUT `≠ topic_id`. (Two of three agree; fleet.yaml is desynced.)
3. **`stale_registry`** — present in `topics.json` AND `fleet.yaml.<inst>.topic_id == topic_id` (or absent) BUT NOT in chat. (Topic deleted out-of-band; registry retained the mapping.)
4. **`orphan`** — present in `topics.json` mapping to instance name **not in `fleet.yaml`** (regardless of chat presence). (Instance retired without registry cleanup.)
5. **`stale_chat`** — present in chat AND NOT in `topics.json` AND no `fleet.yaml.<inst>.topic_id == topic_id` match. (Topic created out-of-band; never tracked.)

Notes on precedence:
- `drift_fleet` checked BEFORE `stale_registry` because both are tied to a `topics.json` entry; drift is the more specific case (chat-present + fleet mismatch) and should be surfaced separately so the operator can resolve the desync explicitly.
- `orphan` checked BEFORE `stale_chat` because an orphan's chat presence is irrelevant to the classification — the defining property is "instance no longer in fleet.yaml". An orphan that's also chat-deleted would be classified `orphan` (not `stale_registry` + `orphan`).
- `live` and `drift_fleet` are mutually exclusive by construction (live requires fleet.yaml match; drift_fleet requires fleet.yaml mismatch).
- `stale_registry` and `orphan` are mutually exclusive: stale_registry requires the instance to BE in fleet.yaml (else it would be orphan); orphan requires the instance NOT in fleet.yaml.

Each topic appears in exactly one classification. The taxonomy enumerates all observable states across the 3-source state space.

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
| (α-a) bootstrap restart auto-create same-named-topic pre-check | 30-50 | 40-60 | 70-110 | Tier-1 single primary |
| (α-b) bootstrap orphan-cleanup extension (live-chat enumeration) | 30-50 | 40-60 | 70-110 | Tier-1 single primary |
| (α-c) delete path permission surfacing (delete_topic error matches) | 20-30 | 30-50 | 50-80 | Tier-1 single primary |
| (α) shared permission-check helper (`can_manage_topics_for`) | 20-30 | 20-30 | 40-60 | Tier-1 single primary |
| **(α) total** | **~100-160** | **~130-200** | **~230-360** | **Tier-1** |
| (γ) doctor_topics module (classification + format, precedence-ordered) | 110-160 | 70-110 | 180-270 | Tier-1 single primary |
| (γ) cli.rs integration | 10-20 | 10-20 | 20-40 | Tier-1 single primary |
| (γ) `--cleanup` flag (delete_topic per classification, confirmation prompt) | 30-50 | 30-50 | 60-100 | Tier-1 single primary |
| **(γ) total** | **~150-230** | **~110-180** | **~260-410** | **Tier-1** |
| **Combined (α)+(γ)** | **~250-390** | **~240-380** | **~490-770** | **Tier-1** |

(Scope: combined ~490-770 LOC, all Tier-1 single primary. (α-a) bootstrap pre-check ~70-110 LOC, (α-c) delete_topic permission surfacing ~50-80 LOC, (γ) doctor_topics + cli + --cleanup ~260-410 LOC. Threshold-acceptance baseline 770 LOC accepted per §8.1.)

Predicted file touches:
- `src/channel/telegram/topic_registry.rs` (delete_topic permission gate + error surfacing — addresses S4 via (α-c))
- `src/channel/telegram/bootstrap.rs` (auto-create same-named-topic pre-check + orphan-cleanup extension — addresses S1 via (α-a) + S2 via (α-b))
- `src/bootstrap/doctor_topics.rs` (NEW — (γ) classification logic with precedence-ordered taxonomy)
- `src/cli.rs` (subcommand wiring for `agend-terminal doctor topics [--cleanup]`)


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
(combined ~490-770 LOC), IMPL dispatch follows automatically per
Wave 2 sequencing context. The conditional task is
`t-20260509090003452174-17` (referenced in lead's m-20260509152716671697-225).

**Threshold-acceptance baseline**: the operative ceiling is **770 LOC**
combined. Reviewer Tier-1 + lead recommendation accepted this baseline
(reviewer m-20260509161333869887-255 + lead m-20260509161254...).
Split-condition #4 below triggers only if a re-estimate during IMPL
moves materially beyond 770 (e.g. > 850 LOC).

Dispatch shape: single combined IMPL PR for (α) + (γ) (estimated
~490-770 LOC, within the 770 ceiling) OR split into 2 sequential
PRs only if a re-estimate during IMPL materially exceeds 770 OR
reviewer flags a clean (α)/(γ) scope-split during VERIFIED verdict.

### 8.2 Surface-block criteria (escalate to lead)

Surface to lead/general for triage if RCA-stage investigation
reveals:
1. `(α-c)` requires NEW file-watch infrastructure to be reactive (current proposal defers to bootstrap reload) — adds Sprint 60 `notify` crate dependency or similar.
2. `bot.get_chat` doesn't enumerate forum topics in the teloxide API surface — `(α-a)` bootstrap pre-check + `(α-b)` enriched orphan-cleanup design changes shape; may force fallback to `(γ)` operator-driven cleanup as the recovery surface.
3. Permission-check helper requires `bot.get_chat_administrators` enumeration which is rate-limited under heavy fleet sizes — needs caching layer.
4. Combined LOC estimate moves materially beyond the accepted 770 ceiling (e.g. > 850 LOC) — split (α) and (γ) into separate PRs with sequential reviewer cycles.
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
