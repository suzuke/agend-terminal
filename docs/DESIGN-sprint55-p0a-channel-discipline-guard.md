# Sprint 55 P0-A — Channel Discipline Daemon-Level Enforcement (FINAL)

**Status**: Phase 3 lead-synthesized design — pending operator review per m-3637 design-first directive
**Date**: 2026-05-08
**Phase 1 input**: dev RCA `docs/internal/DESIGN-sprint55-p0a-channel-discipline-guard-v1-dev-rca.md`
**Phase 2 input**: reviewer challenge `docs/internal/CHALLENGE-sprint55-p0a-channel-discipline-guard-reviewer.md`
**Authors**: dev (primary RCA) + reviewer (challenge) + lead (synthesis)

## §1 Executive summary

**Problem**: `handle_reply` MCP tool handler (`src/mcp/handlers/channel.rs:5-28`) uses singleton `crate::channel::active_channel()` instead of consulting per-instance `reply_to_channel` heartbeat_pair field. In multi-channel fleets (telegram + discord), this can silently route replies to the wrong channel — a deterministic correctness gap, latent today (telegram-only production) but locked-in once discord rolls out broadly.

**Premise (verified by dev + lead cross-grep)**: `reply_to_channel` flag DOES exist in current codebase as Sprint 52 redesign (PR #437 + #440) of Sprint 49's reverted `last_input_channel` design. Sprint 52 deadlock-RCA absorbed; production-proven via 60s soak with 2.1B events 0% drift.

**Final design — Variant-extend** (chosen over Variant-refuse per reviewer Phase 2 challenge):
- Add `lookup_channel_by_name(name) -> Option<Arc<dyn Channel>>` to channel registry
- `handle_reply` reads sender's `HeartbeatPair.reply_to_channel`
- Prefers tagged channel when present + currently-registered
- Falls back to `active_channel()` singleton when `reply_to_channel == None`
- Returns structured error `code: "reply_channel_unavailable"` when tagged channel unregistered/offline
- Warn-log only on actionable divergence (mismatch / refusal / unavailable) — NOT happy-path equality

## §2 Chosen variant: Variant-extend (vs Variant-refuse)

| Aspect | Variant-refuse (rejected) | Variant-extend (CHOSEN) |
|---|---|---|
| LOC | ~30-40 | ~50-85 |
| Multi-channel correctness | Refuses on mismatch | Routes to tagged channel |
| Agent UX in healthy multi-channel state | Forces retry churn | Transparent attribution preserved |
| Future `target_channel` arg compatibility | Limited | Natural extension |
| Locks routing invariant before discord broadens | Partial | Full |

**Rejection rationale for Variant-refuse**: discord integration shipped Sprint 54 P2-8/P2-8b (#512/#513); multi-channel fleet timing is no longer purely hypothetical. Refuse-only creates avoidable retry churn when channel exists but isn't singleton. Extend keeps attribution correctness AND preserves delivery in healthy multi-channel states.

**Counterpoint acknowledged**: Variant-refuse is acceptable as INTERIM step under extreme schedule pressure, with explicit follow-up ticket to migrate to Variant-extend. Operator's call. Default recommendation: Variant-extend now (small marginal LOC, cleaner long-term contract).

## §3 Implementation surface

### Code sites (verified at `26e7331`)

1. **`src/channel/mod.rs`** — add registry lookup-by-name:
   ```rust
   pub fn lookup_channel_by_name(name: &str) -> Option<Arc<dyn Channel>> { ... }
   ```
   Plus a `name(&self) -> &str` accessor on the `Channel` trait OR equivalent struct field.

2. **`src/mcp/handlers/channel.rs:5-28`** — replace singleton lookup with prefer-chain:
   ```rust
   pub fn handle_reply(...) -> Value {
       let pair_snapshot = heartbeat_pair::snapshot_for(instance_name);
       let target = match pair_snapshot.reply_to_channel.as_deref() {
           Some(name) => match crate::channel::lookup_channel_by_name(name) {
               Some(ch) => ch,
               None => return json!({
                   "error": format!("reply_to_channel '{name}' not registered or offline"),
                   "code": "reply_channel_unavailable"
               }),
           },
           None => match crate::channel::active_channel() {
               Some(ch) => ch,
               None => return json!({"error": "no active channel", "code": "no_active_channel"}),
           },
       };
       // ... existing send_from_agent logic
   }
   ```

3. **Tests** in `src/mcp/handlers/tests.rs`:
   - Match: `reply_to_channel` set + lookup returns Some → uses tagged channel
   - Mismatch: `reply_to_channel` set + lookup returns None → returns structured error
   - Absent: `reply_to_channel = None` → falls back to `active_channel()`
   - Capability check: tagged channel exists but doesn't support reply → structured error

### Telemetry

Warn-log on **actionable divergence only**:
- `reply_to_channel` set + tagged channel unregistered → `WARN`
- `reply_to_channel` set + tagged channel returns send error → `WARN`
- `reply_to_channel == None` → no log (happy-path fallback to singleton)
- `reply_to_channel == active_channel().name()` (single-channel fleet steady state) → no log

Rationale: always-log creates noise floor in single-channel fleet; never-log blocks forensic debugging. Actionable-only balances.

## §4 Edge case adjudication (7 + 4 reviewer-added = 11 total)

### EC1 — Mixed-channel (telegram + TUI in same window) [INHERIT Sprint 52]
- Sprint 52 `src/layout/pane.rs:111` clears `reply_to_channel` on TUI keyboard input
- Guard reads cleared `None` → falls back to `active_channel()` (which on TUI may be telegram or whichever singleton)
- No new logic needed; intentional UX (operator typing locally signals "working here now")

### EC2 — Mid-message channel drop (offline mid-stream)
- Guard returns `code: "reply_channel_unavailable"`
- **Bounded retry policy** (per reviewer m-25): agent attempts max 2 retries, backoff 200ms then 800ms, then fail loud
- Prevents infinite retry loops + handles transient reconnects deterministically

### EC3 — Multi-turn (channel changes between turns same task)
- Sprint 52 `reply_to_input_id` monotonic counter ensures per-turn freshness
- Guard reads `snapshot_for` at handler-call-time → current turn's `reply_to_channel` correct
- No new logic needed

### EC4 — Daemon restart (persistence?)
- Sprint 52 contract: ephemeral (in-memory `OnceLock<Mutex<HashMap>>`)
- Post-restart `reply_to_channel = None` → fallback to `active_channel()`
- INHERIT Sprint 52 ephemeral semantics; persistence adds write-amplification + stale-attribution risks

### EC5 — Concurrent (two channels racing same agent inbox)
- Sprint 52 inbox.rs:482 "last channel-sourced message wins" semantic
- Mirror infrastructure auto-sends PTY output to whichever `reply_to_channel` won
- **Largely benign post-Sprint-52** (mirror covers second channel's expected reply receipt)
- Document explicitly

### EC6 — Channel disconnect at reply time
- Same as EC2: structured error `code: "reply_channel_unavailable"`
- Bounded retry policy per reviewer m-25

### EC7 — "Turn" definition
- INHERIT Sprint 52's Idle→Ready transition + 3s silence fallback
- Backend-specific quirks (e.g. KiroCli pause-states) covered by per-backend state-tracker work (Sprint 50-52)

### EC8 (reviewer-added) — Agent-peer-only mode (no external channel registered)
- Daemon running headless (no telegram, no discord registered)
- Agent calls `reply` after agent-peer `send` input → `reply_to_channel = None` (only telegram/TUI sources set this) → falls back to `active_channel()` → `None` → structured error `code: "no_active_channel"`
- **Document explicit behavior**: `reply` requires registered external channel; agent-peer message routing is via `send` MCP tool, not `reply`

### EC9 (reviewer-added) — Channel name aliasing/migration
- If channel identifier changes (e.g. `telegram` → `telegram-main`), `lookup_channel_by_name` fails despite same logical sink
- **Recommendation**: stable canonical channel IDs; if rename needed, daemon-side alias map handling (not exposed in P0-A scope)
- Out-of-scope mitigation: future `target_channel` arg with alias support (Sprint 56)

### EC10 (reviewer-added) — Snapshot/send TOCTOU race
- `reply_to_channel` snapshot valid at read-time; channel unregistered between snapshot and send call
- **Branch to same `code: "reply_channel_unavailable"` error** — do not silently fall back to singleton
- Same retry policy as EC2/EC6 applies

### EC11 (reviewer-added) — Mixed capability channels
- Some channels may support edit/react but not reply-like semantics
- `lookup_channel_by_name` MUST validate operation capability via `Channel::supports_reply()` accessor (existing trait method per channel/mod.rs caps probe)
- If incapable → structured error `code: "channel_capability_unsupported"`

## §5 Cross-P0 integration testing (per reviewer m-25)

When P0-A + P0-B both land:

1. **Restart + binding + reply attribution**: Bind agent (`source_repo` set per P0-B), inject channel input, restart daemon, verify post-restart reply behavior is explicit-error not silent
2. **Concurrent dispatch + channel switch**: while binding state mutates branch/watch, issue channel-originated messages, verify reply guard preserves attribution OR fails with structured code
3. **Multi-agent shared repo + mixed channels**: 2 agents same source_repo + different branches; telegram input to one, discord input to other; verify zero cross-agent/channel bleed

Implementation order: **P0-A first, then P0-B** (per reviewer + lead consensus). Channel-discipline guard is smaller, de-risks user-facing routing correctness before broader binding lifecycle refactors.

## §6 LOC + Tier estimate

| Component | LOC est | Notes |
|---|---|---|
| `lookup_channel_by_name` registry add | ~10-15 | HashMap lookup pattern |
| `Channel::name()` accessor | ~5-10 | Trait method or struct field |
| `handle_reply` prefer-chain refactor | ~25-35 | Snapshot read + lookup + branch |
| Tests | ~30-50 | 11 edge case unit tests |
| **Total** | **~70-110** | Slightly above general's 30-50 nominal due to Variant-extend choice |

**Tier**: Tier-1 single primary review (codex). No Tier-2 escalation expected.

**LOC ceiling**: 150 nominal, 200 hard escalate. Past 200 → flag scope drift.

## §7 Out of scope (deferred)

- **`target_channel` explicit arg** — Sprint 56 follow-up after P0-A telemetry establishes mismatch/offline rate baseline
- **Persistent `reply_to_channel` across daemon restart** — Sprint 52 ephemeral contract preserved
- **Channel name aliasing infra** — separate refactor if needed
- **Sprint 49's prompt-injection design** — superseded by Sprint 52 router-layer mirror (production-proven)

## §8 Risks

| Risk | Severity | Mitigation |
|---|---|---|
| Variant-extend introduces channel registry race (concurrent register/lookup) | LOW-MED | Use existing lock-tier-3 pattern (heartbeat_pair precedent) |
| Warn-log volume in single-channel fleet | LOW | Only-on-actionable-divergence policy (no happy-path noise) |
| Bounded retry creates user-visible delay on transient channel blips | LOW | 200+800ms backoff is sub-second total; agent decides ultimate retry |
| Future `target_channel` arg widens scope mid-Sprint | LOW | Out-of-scope this PR; Sprint 56 follow-up explicit |
| Channel capability check breaks edit/react-only channel implementations | LOW | Capability accessor already exists per `Channel` trait caps probe |

## §9 Status / next steps

- **Phase 1 dev RCA**: Complete (`docs/internal/DESIGN-sprint55-p0a-channel-discipline-guard-v1-dev-rca.md`)
- **Phase 2 reviewer challenge**: Complete (`docs/internal/CHALLENGE-sprint55-p0a-channel-discipline-guard-reviewer.md`)
- **Phase 3 lead synthesis**: This document
- **Operator review**: Pending m-3637 directive
- **Phase 4 IMPL** (post-approval): dev primary, reviewer Tier-1, ~70-110 LOC, ~1.5-2hr cycle

**Disagreement summary** (resolved per Phase 3 synthesis):
- Variant choice: dev deferred → reviewer Variant-extend → **lead concurs Variant-extend**
- Observed bug evidence framing: dev neutral → reviewer "latent deterministic gap" → **lead concurs "latent deterministic"**
- Cross-P0 test surface: dev 0 cross-tests → reviewer 3 scenarios → **lead concurs 3 scenarios**

Sprint 52 prior-art absorbs deadlock + lock-ordering risk. P0-A delta is small + bounded.
