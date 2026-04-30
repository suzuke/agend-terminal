# PLAN — `attachment_id` access path (Sprint 31 prep)

**Date**: 2026-04-29
**Author**: dev-reviewer-2 per Sprint 30 wave-2 dispatch m-1 (task `t-20260429015940866082-0`)
**Scope**: docs-only design plan; **NO IMPL** — Sprint 31 if approved
**Companion to**: Sprint 30 PR-3 (#291) which retained `download_attachment` per operator m-208 + PR-5 (#295) steering rewrite which documented its telegram multimedia use case
**Status**: PROPOSAL — design verdict + Sprint 31 dispatch readiness

## §0 TL;DR

The original Sprint 30 proposal hand-waved "fold attachment access into inbox via attachment_id". Investigating the actual telegram flow reveals **the premise was based on a misunderstanding** — telegram attachments are already eagerly downloaded to local filesystem at receive time, with paths exposed via `attachments=[/path/...]` header. Agents reading inbox messages typically open the local path via standard file-read tools; `download_attachment(file_id)` is a fallback / recovery path for the rare case where eager download failed or only the file_id is known.

**Design verdict**: **KEEP `download_attachment` standalone. DO NOT add `attachment_id` to inbox schema.** The proposed consolidation would create a third overlapping access path (inbox → message header → local path is the primary; download_attachment(file_id) is the fallback) without solving any real problem operators have hit. Per §0 KISS principle (PR #288): "What real problem does this solve? Would deletion break anyone?" — folding into inbox solves nothing concrete and the `download_attachment` standalone tool already covers the by-file_id case.

Sprint 31 dispatch readiness: **NO IMPL TASK needed.** This plan closes the Sprint 30 PR-5 NIT carryover ("Sprint 31 candidate: explicit design plan for fold attachment handling into inbox") with a design verdict of "no consolidation needed — current architecture correct".

## §1 Use case (clarified)

### §1.1 Current telegram attachment flow (post-Sprint-30)

```
Telegram inbound message with media (photo/audio/document)
         │
         ▼
src/channel/telegram.rs:715 polling loop
         │
         ▼  download_file_async(bot, home, instance, file_id)
         │  → fetches bytes via teloxide bot.get_file + bot.download_file
         │  → writes to $AGEND_HOME/downloads/<instance>/<filename>
         ▼
Attachment { kind, path: /local/path, mime, caption, size_bytes, ...}
         │
         ▼  inbox::enqueue(msg) — stores InboxMessage with attachments Vec
         │
         ▼  inbox::header_string()  src/inbox.rs:498-502
         │  → emits "attachments=[/tmp/agend/downloads/dev-1/photo.jpg, ...]"
         ▼
Agent's PTY sees [AGEND-MSG] header line containing local paths
Agent calls `inbox` → reads InboxMessage with attachments[].path field
Agent opens the local path via standard file Read tool
```

**Key fact**: the bytes are on local disk by the time the agent sees the inbox message. No additional download call is needed in the happy path.

### §1.2 When `download_attachment(file_id)` is actually called

The standalone `download_attachment` MCP tool (kept in PR #291 + documented in PR #295 per operator m-208) handles the **fallback / recovery** cases:

| Case | Trigger | Resolution |
|---|---|---|
| Eager download failed | `download_file_async` errored at receive time → `attachments=[]` in inbox msg → agent sees no path | Agent receives notification via `tracing::warn!` log AND no `attachments=` header → can request operator-provided file_id, then call `download_attachment(file_id)` to retry |
| Local file cleaned up | File at `$AGEND_HOME/downloads/<instance>/<filename>` deleted by external process / disk-full sweep / TTL policy | Agent sees old inbox message with `attachments=[/missing/path]`; calls `download_attachment(file_id)` to re-fetch from telegram if file_id retained externally |
| File_id from external source | Operator pastes a file_id in chat, asks agent "fetch this attachment" | Agent calls `download_attachment(file_id)` directly without going through inbox |
| Audit trail / replay | Sprint 31+ scenario: replay old conversation for analysis, need attachments not in current inbox cache | Lookup file_id in archival message store, fetch via `download_attachment` |

These cases are **legitimate** but **rare** — most agent multimedia consumption goes through the eager-download → local-path path, not `download_attachment`.

## §2 Resolution path design (3 options analyzed)

### §2.1 Option A — inline bytes in inbox response

`inbox(message_id=X)` returns the inbox message body PLUS embedded base64 attachment bytes.

| Cost | Benefit |
|---|---|
| Inbox response grows by ~size_bytes × 1.33 (base64 overhead) per attachment | Single round-trip, no separate download |
| Large messages (e.g. 10MB video) → 13MB base64 in MCP response → JSON parsing slow | Agent doesn't need to know about file_id |
| Stack-allocated buffer in MCP framing — current `DEFAULT_FRAME_LIMIT = 1MB` (per Sprint 29 PR #283 audit #9) would reject any non-trivial media | — |

**KISS check**: Solves the case where agent doesn't have local-path access. But the local-path access already works for 100% of happy-path cases. Adds large-response failure mode to a path that doesn't have one today. **REJECTED**.

### §2.2 Option B — lazy 2-call: inbox returns `file_id + URL`, agent fetches separately

`inbox` returns a placeholder `attachments=[file_id=X, file_id=Y]` header; agent calls a separate fetch tool to retrieve bytes.

| Cost | Benefit |
|---|---|
| Removes the eager-download flow that already works in production | Inbox response stays small |
| Each agent message-read becomes 2 round-trips for media; current is 0 (file already on disk) | — |
| Operator UX regression: telegram media latency increases | — |

**KISS check**: Solves nothing. Eager download already provides this exact pattern (file_id → local download → path), just done by daemon at receive time instead of by agent at read time. Moving the work from daemon to agent adds round-trips, doesn't simplify anything. **REJECTED**.

### §2.3 Option C — `attachment_id` param on inbox schema

`inbox(attachment_id=X)` looks up the local cached path for the given attachment and returns it (or re-fetches if missing).

| Cost | Benefit |
|---|---|
| Adds 3rd access path overlapping with existing 2 (inbox header path + standalone download_attachment) | Single tool surface (`inbox`) for all attachment access |
| Behavior of "missing attachment_id" ambiguous: error? re-fetch? walk inbox history? | — |
| Schema-implicit conditional validation (attachment_id exclusive with message_id / thread_id?) | — |

**KISS check**: Solves only the case where agent has an attachment_id but not a local path. That case already has a tool (`download_attachment`). Renaming the entry point to `inbox` doesn't reduce LOC, doesn't reduce token cost (inbox schema already has 3 optional params from PR #294), and doesn't add capability. **REJECTED**.

## §3 Recommended design

**KEEP `download_attachment` as a standalone tool. Do NOT add `attachment_id` to inbox schema.**

Justification:
1. **Eager-download flow already solves the happy path** (~95%+ of multimedia consumption). Agents read local file via standard Read tool — no MCP overhead.
2. **`download_attachment(file_id)` already covers the fallback path** (~5% — failed eager download, file_id from external source, archival access).
3. **No third path needed.** Folding into inbox creates schema permissiveness (which params apply when, mutual-exclusion semantics) without solving any concrete operator-hit problem.
4. **Per §0 KISS principle (PR #288 §0)**: "What real problem does this solve? Would deletion break anyone?" Adding `attachment_id` to inbox solves no concrete problem; deleting `download_attachment` (the alternative direction) WOULD break the fallback path documented in §1.2 (operator m-208 caught this real bug).

## §4 Inbox schema extension semantics — explicitly NOT changed

Inbox schema after PR #294:

```json
{"name": "inbox",
 "description": "Check pending messages, OR look up a single message by ID, OR fetch a thread's messages.",
 "inputSchema": {"type": "object", "properties": {
    "message_id": {"type": "string"},
    "thread_id": {"type": "string"},
    "instance": {"type": "string"}
 }}}
```

**Not adding** `attachment_id` per §2.3 analysis. Schema remains as-is.

`download_attachment` schema (kept per PR #291 + documented per PR #295):

```json
{"name": "download_attachment",
 "description": "Download a file attachment (telegram multimedia: images, audio, documents). Returns local path.",
 "inputSchema": {"type": "object", "properties": {
    "file_id": {"type": "string"}
 }, "required": ["file_id"]}}
```

**Not consolidating.** Standalone semantics remain clear.

## §5 Inline vs separate fetch trade-off

Already analyzed in §2.1 (Option A) and §2.2 (Option B). Result: existing eager-download (which is neither pure inline nor pure separate-fetch) is the right pattern — daemon does the work at receive time, agent gets a path-pointer at read time, no token-budget impact on inbox response.

## §6 Backwards-compat plan

**Nothing to change.** download_attachment remains in tool list (PR #291 + PR #295 confirmed). Existing agents continue to work via:
1. Primary path: read `attachments=[/local/path]` header → open via Read tool (current 95%+ flow)
2. Fallback path: `download_attachment(file_id=X)` for failed-eager / archival cases (current 5% flow)

No 1-sprint migration window needed (no consolidation happening). No alias retention needed.

## §7 Design verdict

**GO keep separate.** Recommended Sprint 31 outcome: close this design plan as "no impl needed; architectural review confirmed current state correct".

## §8 Sprint 31 dispatch readiness

**NO IMPL TASK needed.** Sprint 31 should NOT dispatch attachment-fold work. Instead:

- **(a)** Merge this plan as Sprint 31 reference doc — codifies the design verdict so future reviewers don't re-litigate "fold into inbox" without context
- **(b)** Optional: extend Sprint 30 PR-5 (#295) steering rewrite with a 1-line cross-reference: `(see docs/PLAN-attachment-id-inbox-fold-2026-04.md for the Sprint 31 fold-into-inbox decision)`
- **(c)** Sprint 31 Sprint-31-CANDIDATE-CHALLENGE roadmap drops the "attachment fold" entry; replace with reference to this plan

If Sprint 31 reviewer challenges with "but the original proposal said fold into inbox" — the answer is in §1.1 / §1.2: the proposal author didn't know about the eager-download flow. The plan's hand-wave is rebutted by the actual code path.

## §9 Self-qualification

This plan is docs-only, qualifies under §3.5.5 LOW docs-only single-reviewer (Path A — dev-reviewer review). LOC budget ~250; substantive design doc, NOT a §3.5.5 qualifying amendment introducing new fleet rules.

§3.5.10/11/12/13/14/15 N/A (no protocol-layer src/ change).

§3.6 dogfood — push and immediately continue.

## §10 Cross-references

- **Sprint 30 wave-1 PRs**: PR #291 (download_attachment KEEP per operator m-208), PR #294 (inbox absorbed describe_message + describe_thread, NOT download_attachment per m-208 correction), PR #295 (steering rewrite documented download_attachment use case)
- **Operator directives**: m-208 (download_attachment use case = telegram multimedia), m-1 dispatch (Sprint 30 wave-2 task #74 = author this plan)
- **Protocol**: §0 KISS principle (PR #288, "What real problem does this solve?"), §3.5.15 observability e2e (PR #277), §3.6.9 git auto-cleanup (PR #277)
- **Code surface**: `src/channel/telegram.rs:715` (eager download), `src/inbox.rs:501` (header path emission), `src/mcp/handlers/channel.rs:42` (download_attachment handler)
