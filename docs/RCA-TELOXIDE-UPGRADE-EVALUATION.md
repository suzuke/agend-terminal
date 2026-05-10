# RCA — teloxide upgrade evaluation (Phase 1)

**Sprint 62 W2 PR-1 — Path B doc-only.** Closes the Sprint 59 W2 PR-IMPL #574 F2 deferral by evaluating whether a teloxide upgrade unblocks `stale_chat` class detection. Phase 1 RCA only — no IMPL ships in this PR.

---

## 1. Background

Sprint 59 W2 PR-IMPL #574 shipped telegram topic cleanup (α + γ) F2 path with a 4-class taxonomy:

- `live` — present in `topics.json` AND `fleet.yaml.topic_id` matches
- `drift_fleet` — both sources present but `topic_id` differs
- `stale_registry` — present in `topics.json` AND fleet.yaml absent or matches
- `orphan` — present in `topics.json` mapping to instance NOT in fleet.yaml

The original RCA proposed a 5-class taxonomy that included `stale_chat` (topic exists in chat but not in `topics.json`). F2 dropped `stale_chat` per surface-block #1: teloxide-core 0.11.2 + Telegram Bot API offered no `listForumTopics`-equivalent method. Operators must verify chat-side topics via the Telegram UI directly.

`docs/RCA-TELEGRAM-TOPIC-CLEANUP.md` (Sprint 59 W2 PR-1 #573) noted Sprint 60+ candidate: re-evaluate if a future Bot API version exposes forum-topic enumeration.

---

## 2. teloxide 0.13.0+ evaluation

### 2.1 Current dependency state

`Cargo.toml` already pins `teloxide = "0.15"` (with `default-features = false, features = ["macros", "ctrlc_handler", "rustls"]`). teloxide 0.15 transitively uses teloxide-core 0.13.x, which is the latest published as of 2025-07-11.

The dispatch's "0.13.0+ upgrade evaluation" framing predates this state — we are already on the latest framework version that covers Telegram Bot API 9.1.

### 2.2 Forum-topic methods in teloxide-core 0.13.0

Per `~/.cargo/registry/src/*/teloxide-core-0.13.0/src/payloads/`, the available forum-topic payload methods are:

- `create_forum_topic` (operator-initiated creation)
- `edit_forum_topic`
- `close_forum_topic` / `reopen_forum_topic`
- `delete_forum_topic`
- `unpin_all_forum_topic_messages`
- `edit_general_forum_topic`
- `close_general_forum_topic` / `reopen_general_forum_topic`
- `hide_general_forum_topic` / `unhide_general_forum_topic`
- `unpin_all_general_forum_topic_messages`
- `get_forum_topic_icon_stickers` (icon catalog only — does NOT enumerate topics)

**No `get_forum_topics` / `list_forum_topics` method is present.** Every forum method is a targeted single-topic operation; none returns a paginated list of topics in a chat.

### 2.3 Telegram Bot API survey

teloxide-core 0.13.0's CHANGELOG (released 2025-07-11) documents Bot API support through TBA 9.1, including:

- TBA 8.1: star transactions / affiliate program
- TBA 8.2: gift / verification methods
- TBA 8.3: `send_gift_chat` / video cover timestamps
- TBA 9.0: business gifts + stories
- TBA 9.1: checklists + `get_my_star_balance` + direct-message price-change service message

None of these versions added a forum-topic enumeration method. The Bot API itself has not exposed this surface as of the most recent release.

### 2.4 Conclusion

**Forum-topic enumeration is NOT available in teloxide 0.15 / teloxide-core 0.13.0 / Telegram Bot API through TBA 9.1.** The F2 design's `stale_chat` exclusion remains correct on technical grounds, not on dependency-version grounds. Upgrading further is unnecessary because we are already on the latest stable; the limitation is upstream (Bot API itself).

---

## 3. Phase 2 IMPL scope estimate (conditional)

**Not applicable per §2.4 finding.** With no enumeration API available, there is no Phase 2 IMPL to scope. The conditional branch in this RCA's structure does not trigger.

If a future Bot API version (TBA 9.2+) adds forum-topic enumeration, a re-evaluation Phase 1.1 RCA would re-scope. Per #582 5-category framework as a placeholder for that hypothetical:

- Boilerplate: ~10 (Telegram method registration in `src/channel/telegram/*`)
- Test density: ~80 (3-4 enumeration tests + cross-platform fixture)
- New module: ~120 (`stale_chat` detection in `src/bootstrap/doctor_topics.rs`)
- Cross-platform: 0 (Bot API is HTTP-only)
- Schema/API: ~30 (5-class taxonomy reintroduction + DriftResolution variant)
- Honest range: 200-350 LOC. Tier-2 likely.

---

## 4. Phase 2 IMPL gate

Auto-trigger criteria (when forum-topic enumeration becomes available):

- Telegram Bot API releases a `getForumTopics` / `listForumTopics` method (or equivalent)
- teloxide-core releases a payload + impl for that method
- Operator confirms chat-side garbage from offline-creation periods is impacting productivity (drives Phase 2 priority)

Surface-block conditions (forcing further deferral):

- Bot API exposes enumeration but with restrictive permissions (e.g. requires `can_manage_topics` chat-admin privilege the bot doesn't have — would force a separate operator-grant flow first)
- Pagination API has rate-limit characteristics that make full-chat enumeration impractical for large chats
- teloxide upgrade beyond 0.15 introduces breaking changes that require coordinated migration in `src/channel/telegram/*` (>200 LOC churn)

Until any auto-trigger criterion is met, Phase 2 stays deferred.

---

## 5. Out of scope

- Implementing `stale_chat` detection via webhook-event accumulation. The F2 IMPL #574 already ships "track-on-create" via `forum_topic_created` updates; topics created when the bot is offline remain undetectable until the bot is present at re-creation time. A more robust offline-discovery mechanism (e.g. operator-supplied chat-export parse) is itself a Sprint 63+ candidate, not in scope here.
- Migrating teloxide beyond 0.15. We are at the latest published; further upgrade evaluation is moot until a new release lands.
- Modifying the existing F2 4-class taxonomy. Per §2.4, the F2 design is correct; no taxonomy change is justified by this RCA's findings.
- Changing the `agend-terminal doctor topics` CLI surface. The CLI works as designed against the 4-class taxonomy.

---

## 6. Recommendation

**Cancel Phase 2 IMPL** as a teloxide-upgrade-driven follow-up. The dependency upgrade premise has been disproven: we are already on the latest, and the upstream Bot API has not added the required surface.

Document the current state as the steady-state F2 design:

- The `stale_chat` exclusion is permanent until upstream Bot API changes.
- Operators must verify chat-side garbage via the Telegram UI directly.
- Future re-evaluation triggers are §4 above; until any fires, no further work in this area.

Sprint 59 W2 PR-IMPL F2 deferral is **closed by determination**, not by Phase 2 ship. The F2 design remains correct.

If a future Bot API release adds enumeration, a new Phase 1.1 RCA re-scopes; this RCA's §3-§4 estimates serve as a starting reference for that hypothetical work.

---

**Summary.** teloxide 0.15 + teloxide-core 0.13.0 (latest as of 2025-07-11, covering TBA 9.1) do not expose forum-topic enumeration. The Telegram Bot API itself has not added this method. F2 4-class taxonomy is correct on technical grounds; `stale_chat` exclusion is permanent until upstream Bot API changes. Phase 2 IMPL **cancelled**, not deferred — Sprint 59 W2 PR-IMPL F2 deferral closed by RCA determination. Future re-evaluation triggers documented in §4.
