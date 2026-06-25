# #2466 SRL real-fix — Phase 1 SPIKE (read-only; awaiting lead vet)

Base: HEAD 43c6114f (includes #2465 merge). All line refs are this base.

## #1 — Confirmed: which path set blocked_reason=rate_limit in the dev-3 incident
**Setter = `watchdog.rs:107` via `classify_pty_output`.** Evidence:
- `classify_pty_output` (patterns.rs:320) = `StatePatterns::for_backend(backend).detect_with_match(output)`
  → maps `AgentState::RateLimit|ServerRateLimit` ⇒ `BlockedReason::RateLimit`. It runs the SAME pattern
  engine as the StateTracker BUT WITHOUT the StateTracker's extra gates (#919 red-anchor, #1769 positional
  "working_state_below", is_high_fp_state). So it matches the throttle banner as plain text.
- `run_watchdog_pass` (watchdog.rs:94-108): for `RateLimit` it calls `health.set_blocked_reason(reason)`
  UNGUARDED by current_state (only QuotaExceeded is state-guarded). → sets blocked_reason=RateLimit even
  with screen state=Idle.
- supervisor.rs:1995 (the SRL arm's own set) is gated on `state==ServerRateLimit` → did NOT fire (state=Idle).
- Net: in the incident the throttle banner matched `detect_with_match` (→ classify → blocked_reason=RateLimit)
  but the StateTracker AgentState stayed Idle (positional/#1769 defeat) → strict `state==ServerRateLimit` arm
  never fired.

### Three signal strengths (strict → loose)
1. `AgentState::ServerRateLimit` — strictest (pattern + #919 red + #1769 positional). DID NOT fire (Idle).
2. `classify_pty_output` ⇒ `blocked_reason==RateLimit` — pattern-only, no red/positional gate. **FIRED** (set by watchdog). The "looser signal that already fired."
3. `screen_has_throttle_hint` (THROTTLE_HINT_TOKENS 429/limiting/Overloaded) — loosest token match. true.

## #2 — Proposed arm condition + FP guard
ApiError is high-FP, so the looser clause must corroborate a genuine failed-throttled turn:
```
srl_arm_eligible =
    state == ServerRateLimit                                  // (strict, UNCHANGED — zero regression)
 || ( blocked_reason == RateLimit                             // (loose: classify already matched a RL pattern)
      && hook_last_event == "StopFailure"                     //   turn actually FAILED (claude; see note)
      && !recovered && !self_cleared                          //   not awake/progressing
      && productive_silent >= <threshold> )                   //   genuinely stuck
```
- Primary signal = `blocked_reason==RateLimit` (already read in the arm at supervisor.rs:1991; most SPECIFIC —
  matched a real RL error pattern, not just a token). Alternative/corroborator = `has_throttle_hint`
  (already captured, supervisor.rs:1974).
- Feedback-safe: the arm's own blocked_reason set (1995) stays gated on `state==ServerRateLimit` (the strict
  clause), so the loose clause never feeds itself.
- Non-claude backends have no StopFailure hook → fall back to `blocked_reason==RateLimit + throttle_hint +
  productive_silent + !recovered + !self_cleared` (no hook corroboration). (Flag for lead.)
- Residual: `blocked_reason==RateLimit` doesn't distinguish server(retriable) vs user rate-limit; bounded by
  the existing 12-retry backoff + exhaustion + abort escalation. (Flag.)

## #4 — arm/clear synchronization (the sharp edge)
`clears_server_rate_limit_retry(state) = (state == Idle)` (supervisor.rs:1638). The incident's state IS Idle,
so a naive loose-arm would be CLEARED the same tick by the Idle terminal-recovery path. Fix: the clear must
consult the SAME loose signal — `state==Idle` is terminal recovery ONLY when the throttle signal is GONE:
```
clears = state == Idle && blocked_reason != RateLimit && !has_throttle_hint
```
i.e. while the banner is still classify-matchable, Idle is NOT terminal recovery → keep the track armed.
When the banner scrolls off, watchdog auto-clears blocked_reason (69-76) → loose clause stops → track clears.

## #3 — Single ownership vs dev-2's 529-recovery (critical)
- `recovery_shadow.rs` is **Phase-0 shadow-only** (AGEND_RECOVERY_SHADOW-gated, "takes NO action"). supervisor
  only `record_recovery_shadow` + `retain_live` — NO production arm consumes `would_fire`. So dev-2's ACTIVE
  expectation-keyed fix is NOT merged yet → no live double-arm TODAY.
- Boundary proposal = the `expectation` flag (arm_expectation called only at the [AGEND-RESUME]/recovery-inject
  site = a daemon recovery turn):
  - dev-2's recovery owns `expectation==true` (a daemon-injected recovery turn that fails).
  - this #2466 loose-arm owns `expectation==false` (a WORK-turn 529, the dev-3 incident).
  - Optionally guard the loose clause with `!has_expectation(agent)` so they can never both arm one episode.
- ⚠ MUST align with dev-2 BEFORE their active fix lands (lead coordinates). Until then this arm is independent.

## #5 — Load-bearing test proposal (real path, not synthetic)
Drive the real supervisor arm classifier with a representative fixture:
- raw screen = throttle banner positioned so `detect_with_match` matches (→ classify=RateLimit) but StateTracker
  AgentState resolves Idle (the #1769 shape); hook last_event="StopFailure"; productive-silent; no expectation
  → assert the SRL retry track ARMS.
- Regression: a real ServerRateLimit banner still arms (unchanged).
- FP guard: bare ApiError with NO throttle/blocked_reason → does NOT arm.
- Clear sync: Idle with throttle gone (blocked_reason cleared) → track clears.
- NEUTER/RM: drop the loose clause → the work-turn case no longer arms (proves it load-bearing).

## Open questions for lead
A. Primary loose signal: `blocked_reason==RateLimit` (recommended, specific) vs `throttle_hint` (looser) vs both?
B. Require StopFailure-hook corroboration (claude-strong, but non-claude falls back weaker) — acceptable?
C. Ownership boundary = expectation flag — confirm + you align dev-2 before their active fix.
D. server-vs-user RateLimit conflation (blocked_reason can't tell) — accept (bounded by retry budget) or add a
   ServerRateLimit-specific classify variant (more invasive)?
