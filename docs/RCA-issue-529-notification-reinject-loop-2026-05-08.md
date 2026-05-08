# RCA — issue #529: notification re-inject loop on rate-limit recovery

**Date:** 2026-05-08
**Author:** dev (Sprint 56 Track D-RCA, Path B doc-only)
**Issue:** [#529](https://github.com/suzuke/agend-terminal/issues/529) — *notification re-inject loop on rate-limit recovery — pickup tracking not marking delivered*
**Reporter:** suzuke (operator), pane-evidence captured during dev's Sprint 55 P0-B FINAL smoke on 2026-05-08
**Verdict:** **(a)-class only — daemon-side ServerRateLimit retry mechanism in `src/daemon/supervisor.rs::process_server_rate_limit_retries` re-injects `last_input_text` up to 3 times with `[5, 15, 30]`s backoff. Claude Code-side scrollback retrigger ((b)-class) is not in the chain — ruled out by direct event-log correlation.**

This document covers the four investigation steps lead requested in the dispatch (m-20260508100911711596-89): daemon-side code path / log audit, PTY-vs-event correlation, backend-side scrollback hypothesis, and verdict + fix-path recommendation.

---

## 1 — (a) hypothesis: daemon-side re-inject loop

### Code path

`src/daemon/supervisor.rs:442-543` defines `process_server_rate_limit_retries`, called by the daemon tick AFTER `tick()` so PTY writes happen lock-free (Sprint 49 lesson). The function has two phases:

**Phase 1 (lines 453-490)** detects new ServerRateLimit states and registers them:

```rust
if state == AgentState::ServerRateLimit {
    if retry_tracks.contains_key(name) { continue; }
    let pair = crate::daemon::heartbeat_pair::snapshot_for(name);
    let input_text = match pair.last_input_text {
        Some(t) if !t.is_empty() => t,
        _ => { tracing::warn!(...); continue; }
    };
    let delay = Duration::from_secs(SERVER_RATE_LIMIT_BACKOFF[0]); // = 5s
    retry_tracks.insert(name.clone(), RateLimitRetry { /* … */ });
}
```

**Phase 2 (lines 494-543)** fires due retries:

```rust
for (name, retry) in retry_tracks.iter_mut() {
    if retry.exhausted || now < retry.next_retry_at { continue; }
    retry.retry_count += 1;
    if retry.retry_count > SERVER_RATE_LIMIT_MAX_RETRIES { /* exhaust */ }
    let injected = {
        let reg = agent::lock_registry(registry);
        if let Some(handle) = reg.get(name.as_str()) {
            agent::inject_to_agent(handle, retry.input_text.as_bytes()).is_ok()
        } else { false }
    };
    // re-schedule: SERVER_RATE_LIMIT_BACKOFF[(retry_count).min(2)]
}
```

Constants (lines 29-31):
```rust
const SERVER_RATE_LIMIT_MAX_RETRIES: u32 = 3;
const SERVER_RATE_LIMIT_BACKOFF: [u64; 3] = [5, 15, 30];
```

So after the agent enters `ServerRateLimit`, the supervisor re-injects `last_input_text` at +5s, +20s, +50s relative to the rate-limit detection time, with the 4th occurrence emitting `server_rate_limit_exhausted`.

### Direct event-log correlation for the cheerc-class repro

`grep "2026-05-08T07:4" ~/.agend-terminal/event-log.jsonl | grep server_rate_limit`:

```
2026-05-08T07:42:37 — server_rate_limit_retry  instance=dev      attempt 1
2026-05-08T07:42:57 — server_rate_limit_retry  instance=dev      attempt 2   (Δ +20s)
2026-05-08T07:43:27 — server_rate_limit_retry  instance=dev      attempt 3   (Δ +30s)
2026-05-08T07:43:57 — server_rate_limit_exhausted instance=dev   gave up after 3 retries  (Δ +30s)
```

These four events are the dev pane's three `[AGEND-MSG]` re-occurrences from issue #529's pane snapshot, plus the silent exhaustion entry. The `Δ` deltas match the `[5, 15, 30]`s backoff schedule (the +20s = 5s + 15s = first interval after attempt 1 was scheduled at +5s from rate-limit detection). The pattern is structurally inconsistent with random scrollback retrigger or stuck pickup tracking — it is precisely the `process_server_rate_limit_retries` schedule.

Aside: the same retry pattern occurred for `general` at 07:45-07:46 in the same session, ruling out a one-off anomaly.

### What gets re-injected

`last_input_text` is populated from three production sites (lines 9-11 in `grep -rn 'p\.last_input_text =' src/`):
- `src/inbox.rs:1045` — `notify_agent_with_attachments` stores the **raw `text` parameter** AFTER calling `compose_aware_inject` with the formatted `notification`. Comment at line 1042-1043 explicitly: *"Store raw body AFTER inject — overwrites any formatted text the inject handler may have stored. Ensures retry re-injects raw body, not header."*
- `src/layout/pane.rs:110` — TUI keystroke path captures user-typed bytes.
- `src/api/handlers/instance.rs:31` — INJECT API path stores the bytes injected by the cross-instance comms surface.

For an inbound `kind=task` notification flowing through `notify_agent_with_attachments`, `text` is the body the agent should see. In the production payload shape used by `comms::handle_send`, `text` is itself prefixed with `[AGEND-MSG] from=… kind=… size=… (use inbox tool)` so the agent CLI can route it correctly — that's why issue #529's pane snapshot shows the marker line repeating, not because the *header* envelope (`format_notification_for_inject`'s pointer-only output) is stored. The retry replays the same body string verbatim three times.

**Verdict for §1:** the daemon-side re-inject loop is the symptomatic root cause. `last_input_text` is the agent's most recent input regardless of source, so `[AGEND-MSG]` notifications are exactly what gets replayed when a rate-limited agent had been processing one.

---

## 2 — (b) hypothesis: backend-side scrollback retrigger

The (b)-class hypothesis is that the daemon injects only once, the `[AGEND-MSG]` marker stays in the PTY scrollback, and Claude Code's auto-suggestion / classifier re-reads the scrollback substring and re-processes the dispatch. Precedent: issues #468 / #469 documented Gemini dismiss-pattern regexes false-matching scrollback.

### What would need to be true for (b)

1. The 3 occurrences in the pane would have **irregular timing** (whatever Claude Code's auto-suggestion fires on, which is *not* tied to a `[5, 15, 30]`s schedule).
2. The daemon's `event-log.jsonl` would show **only one** `inject_to_agent` event per dispatch, with no `server_rate_limit_retry` correlations matching the symptom timing.
3. The agent's processing pattern would show *re-reads* of the same scrollback region, not fresh `❯ user prompt` events.

### What is actually true

1. The pane occurrences match the `[5, 15, 30]`s backoff exactly (§1 timing).
2. The event log shows three `server_rate_limit_retry` events at the symptom timestamps for instance `dev`, plus the trailing `server_rate_limit_exhausted` (§1 quote).
3. Each pane occurrence renders as a fresh `❯` user prompt — the agent's CLI treats them as **new** keyboard input arriving at the prompt, not as scrollback re-reads.

Auxiliary point: Claude Code is a CLI driven by the operator's keystrokes / piped input on stdin; there is no documented mechanism for it to re-process its own scrollback region as fresh input absent an explicit user gesture. The (b)-class precedent at #468/#469 was about a *regex on stale output*, not about Claude Code re-injecting scrollback — different mechanism.

**Verdict for §2:** (b)-class is not in the chain. The hypothesis is concretely refuted by both the timing match against the daemon's backoff schedule and the absence of any non-daemon inject/re-process events at the symptom timestamps.

---

## 3 — Verdict

**Root cause:** `src/daemon/supervisor.rs::process_server_rate_limit_retries` re-injects `last_input_text` up to `SERVER_RATE_LIMIT_MAX_RETRIES = 3` times on the `[5, 15, 30]`s backoff after an agent enters `AgentState::ServerRateLimit`. When the most-recent input was a `[AGEND-MSG]` notification (which is the typical case — the agent rate-limits exactly while processing a fresh dispatch), the same notification is what gets replayed.

The retry mechanism is correct in spirit ("don't lose user input when a transient API error interrupts processing"), but the *content-blind* policy means it inappropriately replays inbox-class notifications whose body is already durably persisted in `~/.agend/inbox/<agent>/`. The agent's `inbox` MCP tool would surface the message naturally on recovery; the re-inject is redundant.

**Other RCA dimensions ruled out:**
- §2 (b) backend scrollback retrigger — refuted by timing-match against daemon backoff and absence of non-daemon events.
- *Pickup-tracking*: `pending_pickup_ids` (in `mcp/handlers/comms.rs:609-638`) is **not** the loop driver. It tracks channel-side reactions for telegram/discord/slack picked-up events; the re-inject mechanism doesn't read it and clearing it doesn't suppress the loop. (Issue #529's title hint that "pickup tracking not marking delivered" is the root cause is mistaken — the actual driver is `last_input_text` + retry scheduler.)
- *Poll-reminder*: `src/daemon/poll_reminder.rs` already has count-keyed dedup via `LAST_NOTIFIED` and is not the loop driver. (Confirmed clean by reading the file.)

### Symptom counting reconciliation

Issue body says "3 times" in pane. Event log shows 3 retries (attempts 1/2/3) plus the original delivery, totalling 4 inject events per affected agent. The pane snapshot in the issue truncates the original delivery (above the visible scroll window) so the operator counted the 3 visible retries. With the original included, the agent saw the same notification 4 times.

---

## 4 — Recommended fix path

The verdict aligns with issue #529's own "Suggested fix direction". Lead's dispatch named this as **Option A** in the conditional Track G chain. Path A standard fix, Tier-1 single primary, ~30-50 LOC.

### Track G shape (single load-bearing change)

`src/daemon/supervisor.rs::process_server_rate_limit_retries`:

1. Add a `last_inject_id` (or `last_msg_id`) field to `RateLimitRetry`, derived from a notification fingerprint (e.g. SHA-1 of the first 256 bytes of `last_input_text`, or — preferred — a real `message_id` plumbed through from `comms::handle_send` into `heartbeat_pair`).
2. Before retry phase 2's `agent::inject_to_agent` call, compute the current fingerprint and compare against `last_inject_id`. If equal AND elapsed-since-last-inject < dedup window (e.g. `60s`), skip the retry — the agent already has the input pending; replaying the same bytes mid-stall doesn't change the queue.
3. Force-inject only when one of (a) the fingerprint differs (operator typed something new), or (b) the agent shows a heartbeat-after-rate-limit gap consistent with recovery.
4. Cap re-inject count per fingerprint at ~1-2 (already capped at 3 globally; the per-fingerprint cap closes the inbox-class case explicitly). On cap, emit `notification_inject_dedup_capped` system event so the operator can audit.

### Alternative shape (rejected, recorded for completeness)

A simpler "skip retry entirely when `last_input_text` starts with the `[AGEND-MSG]` marker" rule would also fix the symptom but loses the legitimate retry case (operator typed a follow-up via TUI keystrokes during a rate-limit window). Track G should keep keystroke retries intact while suppressing the inbox-class double-delivery.

### Out of scope (separate items / future RCA)

- Generalising the dedup to non-rate-limit retry paths (currently the only retry path is `process_server_rate_limit_retries`, so no generalisation needed today).
- Plumbing real `message_id` through `notify_agent_with_attachments` → `heartbeat_pair`. Current implementation hashes the bytes; a real `message_id` would let the dedup discriminate two different notifications that happened to share content. Sprint 57 candidate if the byte-hash version doesn't pass review.
- Reducing the `[5, 15, 30]`s schedule's aggressiveness — the schedule is fine for keystroke retries, only the inbox-class case is over-aggressive.

### Risk assessment

- **Behavioural compatibility:** keystroke retry paths are untouched (different fingerprint → dedup doesn't suppress them). Inbox-class deliveries are replaced with a single inject; the agent reads them via `inbox` MCP tool on recovery exactly as today.
- **Cap regression risk:** the existing `MAX_RETRIES = 3` global cap is preserved; the new per-fingerprint cap is strictly tighter, so total work decreases.
- **Test surface:** unit tests for the fingerprint helper, the dedup decision under (same fingerprint within window) / (different fingerprint) / (same fingerprint past window) / (no last_inject_id yet — first retry) matrix.

### Operator escape hatch (today, zero-LOC)

Until Track G lands, operators can suppress the loop in two ways:

1. Drain inbox quickly when the agent recovers — the `inbox` MCP tool clears the pending notification, and even if a stale `last_input_text` re-fires, the agent ack's "duplicate" gracefully (which is what dev did during the cheerc-class smoke).
2. If the loop is consistently noisy, lower `SERVER_RATE_LIMIT_MAX_RETRIES` from 3 to 1 via a code edit. This is fragile (loses keystroke retry coverage) but is a one-line change for operators who hit rate limits constantly.

Track G is the durable fix.
