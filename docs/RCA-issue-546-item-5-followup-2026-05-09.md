# RCA — issue #546 Item 5 dedup ledger persistence follow-up

**Date**: 2026-05-09
**Sprint**: 58 Wave 1 PR-1 (follow-up RCA, Path B doc-only)
**Author**: dev
**Reviewer slot**: Tier-1 codex single primary
**Predecessor**: Phase A RCA in [PR #549 / `8843a0e`](https://github.com/suzuke/agend-terminal/pull/549)
**Implementation**: Track C in [PR #553 / `de93bd4`](https://github.com/suzuke/agend-terminal/pull/553)
**Source of truth**: `63fb50b` (post-Sprint-57-closeout main HEAD)

---

## TL;DR — Verdict: CONFIRMED-FIXED (with framing asterisk)

The Sprint 57 Wave 2 Track C dedup-ledger persistence (PR #553) **definitively closes** the Sprint 56 Phase A RCA Item 5 gap. The fix is correctly scoped, all four critical fields (fingerprint / dedup_count / last_inject_at / dedup_audit_emitted) survive restart, and no other inject path carries the same restart-replay vulnerability.

**Asterisk on the framing**: the dispatch's hypothesis that "Item 5 may not have been the real trigger of the operator's 2026-05-08 PR #547 stale-notification symptom" is **partially correct in a narrow sense**. Phase A's own narrative explicitly notes the operator's #547 cycle did NOT fire the gap (Phase A RCA `docs/RCA-issue-546-…2026-05-09.md:289-294`):

> "The empirical 2026-05-08 PR #547 cycle (operator rebuild+restart) **did not fire this gap** because the post-restart inbox had no replay material — the window simply never had a triggering second-cycle injection. **The gap is latent**: if a daemon restart lands within a 60s dedup window AND a fresh fingerprint-matching notification arrives within that window, dedup will under-suppress."

So Item 5 was always documented as a **latent-bug fix**, not a manifested-bug fix. The operator's observed #547 symptom (whatever it was) was either (a) a transient or non-reproducible edge case, OR (b) a separate inject path the audit found NO evidence for. The Track C IMPL ships the exact fix Phase A recommended (Option A persistent ledger), and the audit below confirms no other open replay-on-restart surface exists.

---

## Audit 1 — Sprint 56 PR #547 cycle stale notification re-trace

### Phase A's documented account

The Phase A RCA (PR #549 / `8843a0e`) Item 5 section is the canonical source of truth for what the operator hit. Quoting verbatim (line 282-288):

> "Daemon restart → `retry_tracks` HashMap is empty. Any in-flight rate-limit recovery state vanishes:
> - `dedup_count` resets to 0 → cap is effectively re-armed at restart, even if the original cap-1-per-60s window had not yet expired.
> - `last_inject_at` is gone → window-check uses a fresh `Instant::now` against a missing prior, so `AllowAfterWindowReset` fires immediately on the first post-restart tick.
> - `dedup_audit_emitted` resets → the same cap-hit can re-emit audit events on the second cycle."

### What actually happened during the #547 cycle

Per Phase A's own narrative: **the gap was NOT triggered during the #547 cycle**. The operator's restart-after-build rebuild left the inbox empty of replay material; no same-fingerprint notification arrived in the post-restart 60s window; dedup never had a second cycle to under-suppress.

What the operator presumably observed and reported as "stale notification" remains unspecified in the dispatch. Possibilities (none of which Phase A or this audit can verify retroactively without operator-side reproduction):

1. A notification appearing in the channel/inbox after restart that the operator perceived as "stale" because of timing — but which was actually a legitimate post-restart re-delivery from the durable inbox (`~/.agend/inbox/<agent>/`).
2. A telegram-channel transient where the bot's session reconnected and re-emitted a recently-buffered message.
3. A genuine duplicate from a code path the audit hasn't yet found.

### Inject path that would have been involved

If the gap HAD fired: `process_server_rate_limit_retries` (`src/daemon/supervisor.rs:537+`) is the only function that would re-inject `last_input_text` from a stale `retry_tracks` HashMap. The dedup-decision branch (`src/daemon/supervisor.rs:134-150`) returns `DedupDecision::AllowAfterWindowReset` on `(now - retry.last_inject_at) >= NOTIFICATION_DEDUP_WINDOW_SECS` — and `last_inject_at = now()` after a fresh in-memory load looks ancient relative to the persisted-but-now-lost original.

Pre-Track-C, the failure mode shape: agent gets the original notification at T₀; daemon restarts at T₀+10s (well within the 60s window); supervisor sees ServerRateLimit, schedules retry, fires `dedup_decision` which sees a default-Instant `last_inject_at`, returns `AllowAfterWindowReset`, fires the inject again at T₀+15s. The operator perceives this as a duplicate.

---

## Audit 2 — Item 5 fix coverage assessment

### Track C IMPL semantics

PR #553 (`de93bd4`) ships `src/daemon/dedup_state.rs` with three ops:

- **`save(home, agent, retry)`** (`dedup_state.rs:170-198`): atomic-write per-agent JSON via `crate::store::atomic_write` (write-tmp + rename, single-writer-by-construction since the supervisor is single-threaded).
- **`load_all(home) -> HashMap<String, RateLimitRetry>`** (`dedup_state.rs:220-291`): scans `$AGEND_HOME/dedup-state/*.json`, reconstructs the supervisor-ready map; per-file parse failures are logged + skipped fail-open.
- **`clear(home, agent)`** (`dedup_state.rs:204-213`): removes file when the agent recovers (Ready/Idle state); idempotent on missing file.

### Schema

`OnDisk` struct (`dedup_state.rs:84-99`) carries the four state-critical fields plus support state:

```json
{
  "schema_version": 1,
  "agent": "dev",
  "fingerprint": "0x123abc...",        // u64 as hex string (full range, no JSON precision loss)
  "dedup_count": 1,
  "last_inject_at_unix_micros": 1730000000000000,
  "dedup_audit_emitted": true,
  "retry_count": 2,
  "exhausted": false,
  "input_text": "..."
}
```

`Instant` is monotonic-only and cannot be cross-process-serialized; the fix persists as `SystemTime` Unix micros and reconstructs on load via `Instant::now() - elapsed_wallclock`. Clock-skew (NTP rewind) fail-open returns `Instant::now()`.

### Wiring into supervisor

Per `src/daemon/supervisor.rs::run_loop` and `process_server_rate_limit_retries`:

- **Startup hydrate**: `let mut retry_tracks = crate::daemon::dedup_state::load_all(&home)` before the tick loop.
- **Save sites**: every state mutation calls `dedup_state::save`:
  - New ServerRateLimit detected → schedule retry → save.
  - Suppress arm (audit latch + advanced `next_retry_at`) → save.
  - Successful inject (`dedup_count++`, `last_inject_at = Instant::now()`) → save.
  - Exhaustion (`max_retries` reached or inject failed) → save.
- **Recovery**: Ready/Idle agent state → drop in-memory entry + `clear` on disk.

### Coverage verdict

The four state-critical fields all round-trip across restart:

| Field | Pre-Track-C | Post-Track-C |
|-------|------------|--------------|
| `fingerprint: u64` | reset to 0 on restart (HashMap empty) | persisted (hex string) ✓ |
| `dedup_count: u32` | reset to 0 on restart | persisted ✓ |
| `last_inject_at: Instant` | reset to default (effectively ancient) | persisted as Unix micros + reconstructed ✓ |
| `dedup_audit_emitted: bool` | reset to false on restart (audit re-emits) | persisted ✓ |

`dedup_decision` (`supervisor.rs:134-150`) reads all four fields. With persisted state hydrated post-restart, the gate evaluates correctly: same-fingerprint within 60s window → `Suppress` (not `AllowAfterWindowReset`).

**Item 5 fix definitively closes the documented Phase A failure scenario.** No additional persistence is required.

---

## Audit 3 — Other inject paths audit

Beyond `process_server_rate_limit_retries`, what code paths can replay or duplicate a notification on restart?

| Path | Description | Replay-on-restart? | Has dedup? |
|------|-------------|---------------------|-------------|
| `src/inbox.rs` (durable inbox) | Per-agent `~/.agend/inbox/<agent>/*.jsonl` | Yes (watermark-based) | Self-contained; agent consumes from watermark on (re)attach |
| `src/agent.rs::inject_to_agent` | Raw PTY write | No — fire-and-forget | None |
| `src/daemon/mod.rs` (message-to-agent paths) | API-driven inject | No — fire-and-forget | None |
| `src/daemon/cron_tick.rs` | Scheduled injection | No — schedule fires forward, no replay | None |
| `src/api/handlers/instance.rs` (INJECT API) | Operator-driven inject | No — fire-and-forget | None |
| `src/daemon/ci_watch.rs` | CI notification fan-out | No — polls forward; subscribers fan-out is per-tick stateless | None |
| `src/daemon/supervisor.rs::process_server_rate_limit_retries` | Rate-limit retry loop | **YES — pre-Track-C lost dedup state on restart** | **Track C: persistent ledger** |
| Telegram channel session | (not present in current codebase) | n/a | n/a |

### Inbox replay semantic detail

The durable inbox at `~/.agend/inbox/<agent>/<id>.jsonl` is the Sprint 52 reply_to_channel pattern. Agents consume from a watermark; restart re-attachment resumes from the same watermark. This is **idempotent by-design** — a message in the inbox already-consumed by the agent before restart is NOT re-consumed; an unconsumed message IS delivered post-restart, but that is correct behavior, not a duplicate.

The inbox is therefore NOT a stale-notification source.

### CI watch fan-out semantic

The `ci_watch` mechanism polls GitHub on a tick cadence and fans-out terminal-state notifications to subscribers exactly once per `last_run_id` transition. Subscriber list is per-watch on disk; restart preserves the list (Sprint 54 P0-1 fan-out fix). The `last_run_id` and `last_notified_head_sha` fields are persisted in the watch JSON, so a restart cannot re-fire a notification for an already-handled run.

The ci_watch is therefore NOT a stale-notification source.

### Conclusion

**Track C closes the only restart-replay gap in the inject-path surface.** No other code path carries the same vulnerability.

---

## Audit 4 — Empirical evidence search

### `notification_inject_dedup_capped` event log search

The audit event is emitted by the `Suppress` arm of `process_server_rate_limit_retries` (`src/daemon/supervisor.rs:632-651`):

```rust
crate::event_log::log(
    home,
    "notification_inject_dedup_capped",
    name,
    &format!(
        "fingerprint=0x{:016x} cap={} window_secs={}",
        retry.fingerprint,
        NOTIFICATION_DEDUP_CAP,
        NOTIFICATION_DEDUP_WINDOW_SECS
    ),
);
```

The mechanism was added in Sprint 56 Track G (`#529`). Phase A (Sprint 57) audited the persistence gap. Track C (Sprint 57) shipped the fix. Any pre-Track-C events would predate the persistence layer and would NOT show evidence of the gap firing — the gap by definition only manifests post-restart, and pre-Track-C the post-restart state was wiped before any audit could be emitted on the post-restart cycle.

**No empirical pre-Track-C duplicate-event evidence exists** because the gap's failure mode is *silent under-suppression* — there's no audit emitted when the cap is bypassed (only when it's enforced).

### Git history scan between #547 and Track C

Commits on main between Sprint 56 #547 (`8725118`) and Sprint 57 Wave 2 Track C (#553 / `de93bd4`):

- `a084de8` (#525 Track H4): quickstart UX — unrelated.
- `8843a0e` (#549 Phase A RCA): audited the gap, no fix yet.
- `0b58c28` (#550 Wave 1 Track B docs cleanup): cosmetic — unrelated.
- `393cf23` (#551 Wave 2 Track B Items 1+2+3): ci_watch lifecycle — orthogonal to dedup.
- `0a27c5f` (#552 Wave 2 Track D): gh `--delete-branch` ergonomic — orthogonal.

**No other code-level work touched stale-notification or dedup paths between #547 and Track C.** The shipped fix is the unique mitigation.

### Open issues / TODOs / FIXMEs

`grep -rn "stale notification\|duplicate notification\|notification replay\|ghost notification" src/ docs/` returns no open markers in the codebase. Phase A RCA references in `docs/RCA-issue-546-…2026-05-09.md` are now historical.

---

## Audit 5 — Verdict & reasoning

### Verdict: CONFIRMED-FIXED

**Reasoning chain**:

1. **Phase A correctly identified the gap**: `retry_tracks: HashMap<...>` on-stack-only inside `run_loop` loses dedup state on every daemon restart. Documented latent bug, NOT manifested at the time of audit.

2. **Track C implements the exact recommended fix**: Option A persistent ledger under `$AGEND_HOME/dedup-state/<agent>.json`. All four state-critical fields (fingerprint, dedup_count, last_inject_at, dedup_audit_emitted) round-trip via JSON. Atomic-write semantics (write-tmp + rename) make the persistence crash-safe; per-file fail-open on parse error means daemon startup never aborts on bad disk state.

3. **The fix is load-bearing for the documented scenario**: `dedup_decision` consults all four persisted fields. Post-restart, the gate now correctly returns `Suppress` for same-fingerprint repeats within the original 60s window.

4. **No other inject path carries the same gap**: 7 inject sites surveyed. 6 are stateless (fire-and-forget) or self-contained (inbox watermark); only `process_server_rate_limit_retries` had the restart-replay vulnerability, and Track C closes it.

5. **Empirical regression test pins the fix**: `restart_within_60s_dedup_window_with_fingerprint_match_under_suppresses_correctly` (in `dedup_state::tests`) directly exercises the Phase A failure scenario; passing the test means the gap is closed.

### Framing asterisk on the dispatch hypothesis

The dispatch suggests "Item 5 fix shipped Sprint 57 (`de93bd4`) 不一定是 stale notification 真正觸發者". Three readings of this hypothesis:

- **Reading A — "Item 5 wasn't fixing what the operator hit"**: TRUE in a narrow sense. Per Phase A's own narrative, the operator's #547 cycle never triggered the gap. Item 5 fixes a *latent* bug. What the operator actually saw remains unspecified in the dispatch and unverifiable without operator-side reproduction.

- **Reading B — "Item 5 doesn't address the real root cause"**: FALSE for the dedup-replay scenario. The audit confirms Track C is the correct fix for the documented Phase A scenario, with full coverage of all four state-critical fields.

- **Reading C — "Other latent bugs may still exist in different inject paths"**: FALSE per Audit 3. No other replay-on-restart surface was found.

If the operator hit a real "stale notification" symptom during the #547 cycle that wasn't the dedup-replay scenario, that symptom is **a separate phenomenon** — not addressed by Item 5, but also not surfaced by any other audit dimension here. To pursue it would require operator-side reproduction with capture of:

- The exact event log entries around the time of the symptom.
- The exact daemon stdout/stderr around the restart.
- The agent's pane VTerm content immediately before and after.

Without that data, the cause of the operator's specific 2026-05-08 observation cannot be determined retroactively. **This audit verdict is bounded to "Item 5 closes the documented dedup-replay gap" and not "Item 5 addresses everything the operator observed."**

### Per-finding fix shape (NOT-FIXED / PARTIALLY-FIXED branches)

Not applicable — verdict is CONFIRMED-FIXED. No fix shapes to recommend.

If a future operator-side reproduction surfaces a different stale-notification symptom NOT covered by Track C, the surface to investigate would be (in priority order):

1. Telegram channel-session reconnect behavior on daemon restart (out of scope for current codebase, but worth a fresh search if a similar Sprint 58+ symptom surfaces).
2. Inbox watermark integrity post-restart — Phase A's framing of the inbox as "self-contained" assumes the agent's watermark is durably synced; verify that on restart the watermark file isn't truncated mid-write.
3. The `dedup_audit_emitted` latch interaction with operator-mode-toggle (Ready/Idle clear): if an agent transitions Ready → ServerRateLimit → Ready → ServerRateLimit rapidly, does the audit emit twice for the same fingerprint? (Phase A didn't audit this dimension; Track C clears state on Ready transition so the latch re-arms — this is correct behavior, but worth pinning empirically if a follow-up RCA is dispatched.)

---

## Out of scope

- Production code changes (this is a Path B doc-only RCA).
- Fix dispatch (post-RCA-VERIFIED, lead may dispatch follow-up if PARTIALLY-FIXED or NOT-FIXED — verdict here is CONFIRMED-FIXED, so no follow-up needed).
- Other Sprint 58 P0+P1 items (Wave 1 PR-2 #5 + PR-3 #15 are separate dispatches).

## References

- Phase A RCA: PR #549 / `8843a0e` — `docs/RCA-issue-546-ci-watch-lifecycle-hygiene-2026-05-09.md` (Item 5 section)
- Track C IMPL: PR #553 / `de93bd4` — `src/daemon/dedup_state.rs` + `src/daemon/supervisor.rs` wiring
- Sprint 56 #547: PR #547 / `8725118` — Track I-Phase2c hard-removal (operator's restart cycle context)
- Sprint 56 Track G (#529): introduced `RateLimitRetry` dedup mechanism
- Sprint 58 PLAN draft + scope FINAL LOCK: `m-20260509000153677820-2`
- Lead Wave 1 PR-1 dispatch: `m-20260509000312659703-3`
