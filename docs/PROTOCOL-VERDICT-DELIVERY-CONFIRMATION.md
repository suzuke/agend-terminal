# Reviewer verdict-delivery confirmation protocol

**Sprint 62 W1 PR-3 — process protocol, Sprint 62 W1 closeout PR.**
Formalizes the lead-status-query recovery procedure for the verdict-
delivery-miss failure mode observed ≥2 times in Sprint 60-61. Pairs
with `feedback_ping_stalled_dispatch.md` (60-min stall ping) and the
parallel-PR conflict resolution memory.

---

## 1. Failure mode

The reviewer agent has produced a verdict locally but the dispatch
hasn't reached the requesting agent. Observable from the requester
side as: PR open, CI green, no verdict in inbox after the typical
review window (~30-60 min). From the reviewer side: agent_state
shows `ready` + health_state `idle_long` despite work being complete.

**Observed incidents (Sprint 60-61, ≥2):**

- Sprint 60 W1 PR-1 #578 — verdict produced + queued but not delivered;
  resolved via lead-status-query at ~60min stall threshold per
  m-20260509204037132974-356.
- Sprint 61 W2 PR-1 dispatch task_id mismatch
  (m-20260510004827404099-471) — dispatch typo recovered via branch
  binding lookup.

These are distinct from genuine reviewer-still-working cases (where
silence reflects in-progress) and from `feedback_reviewer_stale_content_diagnostic.md`
(reviewer's grep-style verification cache stale).

---

## 2. Detection signals

Stack these in order; the more that match, the higher the
delivery-miss probability:

1. **Time elapsed** ≥ 60 min since CI green (per
   `feedback_ping_stalled_dispatch.md` threshold).
2. **No reviewer activity in PR/branch**: `gh pr view <num> --json
   reviews,statusCheckRollup` shows no review event.
3. **Reviewer agent state**: `describe_instance reviewer` returns
   `agent_state: ready` + `health_state: idle_long`.
4. **No reviewer message in own inbox** after refreshing.

If 1+2+3 all match, treat as delivery-miss candidate (not in-progress
silence).

---

## 3. Lead-status-query recovery procedure (de facto established Sprint 60 #578)

When detection signals indicate delivery-miss:

```
1. dev → send kind=query to lead:
   "Sprint <N> W<M> PR-<X> #<num> — reviewer-verdict ping (~60min stall threshold).
    PR <num> / commit <sha> / r0 / branch <name>.
    [List CI green status, last-update timestamp.]
    Per memory feedback_ping_stalled_dispatch.md, surfacing at the ~60min mark
    rather than idle-polling so silence isn't misread as in-progress.
    Standing by for direction — happy to continue idle if reviewer is queued,
    or pivot to <interim work> if PR-<X> review is gated."
2. Lead inspects reviewer state; if delivery-miss confirmed:
   2a. Lead sends kind=query to reviewer asking for verdict status.
   2b. Reviewer self-acknowledges + re-sends verdict via send to dev.
3. dev receives verdict via inbox push; proceeds to merge per dispatch.
```

The pattern is fully bypass-free (relies on `send` + `inbox` MCP
tools only).

---

## 4. Reviewer-side ack convention (recommended, opt-in)

When delivering a verdict, reviewer should include a structured ack
line in the dispatch body:

```
[verdict-delivered] <VERIFIED|REJECTED|UNVERIFIED> at <head-sha> + correlation_id <task-id>
```

Examples:

```
[verdict-delivered] VERIFIED at 6c7bf7f + correlation_id t-20260510021509532813-36
[verdict-delivered] REJECTED at 895535f + correlation_id t-20260509173428917538-20
```

Lead can grep for `[verdict-delivered]` across recent dispatches to
audit delivery completeness across reviews. The convention is
recommended; reviewers who omit it still produce valid verdicts —
the ack is purely for delivery-audit grep semantics.

---

## 5. Cross-references

- `feedback_ping_stalled_dispatch.md` — establishes the 60-min stall
  ping threshold; this protocol formalizes the procedure that fires
  at that threshold.
- `feedback_reviewer_stale_content_diagnostic.md` — distinct failure
  mode (reviewer's grep verification cache stale, not delivery-miss).
- `feedback_parallel_pr_conflict_resolution.md` — bypass-free recovery
  pattern; this protocol applies the same bypass-free posture to
  delivery-miss recovery.
- `PROTOCOL-PARALLEL-FILLER-OPT-IN-SCHEMA.md` — Sprint 61 W2 PR-1
  formal schema using the same protocol-doc style.

---

## 6. Out of scope (Sprint 63+ candidates)

- **Automated reviewer ack scanning** — periodic scan of recent
  dispatches grepping for `[verdict-delivered]` markers; missing
  ack on a closed/merged PR triggers a delivery-audit warn. Pure-
  wiring follow-up once the convention has uptake data.
- **Daemon-side verdict-delivery telemetry** — supervisor tracker
  similar to `MCPRegistryWatcher` that detects reviewer-state-ready +
  PR-pending mismatch and emits an alert. Sprint 63+ if delivery-miss
  pattern recurs frequently enough to warrant automation.

---

**Summary.** Verdict-delivery-miss is a distinct failure mode from
in-progress silence and stale-content false-positive. Detection
stacks 4 signals; recovery is a bypass-free `kind=query` chain (dev
→ lead → reviewer → dev). Recommended reviewer ack convention
(`[verdict-delivered] <verdict> at <head> + correlation_id <id>`)
enables delivery audits without tooling. Daemon-side automation is
a Sprint 63+ candidate.
