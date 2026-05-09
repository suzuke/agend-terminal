# Lead closeout synth — claim-state discipline

**Sprint 59 Wave 1 PR-3 — process doc + invariant pins.**
Captures the Sprint 58 Wave 3 PR-1 dispatch protocol gap (where lead's
closeout synth asserted "claimed by dev" before any actual claim
event landed) and pins the cross-check procedure that prevents
recurrence. Pairs with the Wave 4 PR-1 #566 structural enforcement —
the schema gate ensures `task_id` correlation; this doc ensures the
operator-facing closeout narrative reflects ground truth.

---

## 1. Principle

When emitting a sprint / wave / PR closeout synth, **never assert a
claim-state without independent verification**. "Dispatched" is not
"claimed". "Task board entry created" is not "agent has acknowledged
the task". "Reviewer queued" is not "review in progress". Every
claim-state line in a closeout must be provable from observable
fleet state at the moment of writing.

The closeout synth is a contract with the operator + downstream
agents — they treat claim-state lines as factual reporting. A
synth that fabricates a claim-state (even unintentionally, by
defaulting to "happy path" assumptions) propagates a false ground
truth that downstream coordination depends on. Sprint 58 Wave 3
PR-1 burned ~2hr of dev wall time to exactly this failure mode.

---

## 2. Sprint 58 Wave 3 PR-1 incident — what happened

**Timeline** (per dispatch references `m-20260508195729...` /
`m-20260509031109840434-59`):

1. Lead dispatched Wave 3 PR-1 #12 cross-platform clippy gate via
   informal narrative (no `task action=create` on the task board,
   no explicit `task_id` reference in the dispatch payload).
2. Lead's closeout synth wrote "**dispatched**, dev claiming now"
   and proceeded with subsequent wave planning under that
   assumption.
3. dev never received a structured task to claim — the task board
   had no entry for #12, the dispatch message had no `task_id`
   binding. dev defaulted to passive idle-poll waiting for an
   "imminent" task board entry that never materialized.
4. ~100 minutes later, general/operator queried dev's progress and
   discovered the gap. dev's prior broadcast had said "**will
   claim** upon receipt of task board ID" — a conditional, not a
   claim. Lead's synth had elided the conditional into a present-
   tense assertion.

**Why it happened**: lead's synth was authored from intended state
("dispatch sent → dev should claim") rather than observed state
("does the task board show a claimed entry? does dev's last
broadcast confirm receipt?"). The narrative form invited filling
in plausible defaults; the observable state would have surfaced
the gap immediately.

**Companion structural fix**: Sprint 58 Wave 4 PR-1 #566 made
`send kind=task` reject without explicit `task_id`, eliminating
the dispatch-without-correlation failure mode at the schema layer.
This doc covers the **narrative** layer — even when the schema
gate passes, the closeout synth must still cross-check ground
truth before claiming a state.

---

## 3. Cross-check procedure

Before writing any claim-state assertion in a closeout synth, run
the following checklist. Quote the verifying observation in the
synth itself when the assertion is non-trivial (e.g. multi-PR
chains, dependent dispatches).

### 3.1 Task board state
- [ ] `task action=list filter_assignee=<dev>` shows the entry in
      `claimed` or `in_progress` state (NOT `open`).
- [ ] The entry's `id` matches the `task_id` the dispatch message
      carried.

### 3.2 Git / branch state
- [ ] `gh pr list --state open` includes the expected PR for the
      claimed task (when the claim is past the r0 milestone).
- [ ] OR: the dispatch is recent enough (< 30 min) that pre-r0 work
      is plausible — in which case write "**dispatched, awaiting r0
      push milestone confirmation**" rather than claim-state.

### 3.3 Dev's broadcast trail
- [ ] dev's most recent broadcast (`inbox` query OR explicit recall)
      contains a positive acknowledgement: "claiming now" /
      "in_progress" / "§6 milestone 1 r0 + push" / similar.
- [ ] **Conditional language ("will claim", "ready to start", "upon
      receipt") is NOT a claim**. Treat as `dispatched` only until
      a positive acknowledgement lands.

### 3.4 Cross-vantage idle watchdog signal
- [ ] If the dispatch is > 60 min old and no broadcast acknowledgement
      has landed, the Sprint 59 Wave 1 PR-2 dev watchdog (if
      enabled) will already have pinged lead. Treat the absence of
      such a ping as one positive signal — but not a substitute for
      §3.3.

### 3.5 Wave 4 PR-1 task_id integration
- [ ] The dispatch message included `task_id=t-...` (Wave 4 PR-1
      protocol). If absent, the task can't be programmatically
      correlated — the §3.1–§3.3 checks are the only ground truth.

---

## 4. Anti-pattern examples

### 4.1 Don't write

> "Wave 3 PR-1 #12 cross-platform clippy gate **dispatched, dev
> claiming now**. Wave 3 PR-2 dispatch in progress."

This conflates dispatch with claim. The "dev claiming now" assertion
needs a verifying observation; absent one, downgrade to
"dispatched, awaiting acknowledgement."

### 4.2 Do write

> "Wave 3 PR-1 #12 cross-platform clippy gate **dispatched** (task
> board: `t-20260509031123330954-8`). Awaiting dev's §6 milestone 1
> r0 + push confirmation before chain-dispatching PR-2."

This separates the verified state (dispatch sent + task board
entry) from the unverified pending state (dev's acknowledgement).
Downstream readers know exactly what's real and what's expected.

### 4.3 Don't write

> "Wave 1 PR-2 #568 r0 pushed, **CI green**, reviewer dispatched."

The "CI green" assertion needs a verifying observation. CI may
still be running; assuming green skips the §6 milestone 2 ping
that confirms.

### 4.4 Do write

> "Wave 1 PR-2 #568 r0 pushed (`869303b`). CI watch armed. Reviewer
> dispatch will follow §6 milestone 2 (CI green) confirmation per
> single-primary serialization."

This pins the CI-green check as a future trigger, not a present
fact.

### 4.5 Don't write

> "Sprint 59 Wave 1 closeout 達成: 5 PRs merged."

Without a `gh pr list --state merged --base main` cross-check
against the expected PR set, this can be off-by-one (an in-flight
PR mistaken for merged) or off-by-many (multiple chain-dispatched
PRs assumed complete).

### 4.6 Do write

> "Sprint 59 Wave 1 closeout 達成 — verified merged via
> `gh pr list --state merged`: #567 #568 #569 #570 #571 (5 PRs,
> all on main). Cumulative ledger: NN PRs / NN ledger-clean streak."

Quote the verifying command + the observed PR list. The narrative
becomes self-auditing.

---

## 5. Integration with Wave 4 PR-1 #566 task_id discipline

Wave 4 PR-1 made `send kind=task` reject without `task_id`,
ensuring every dispatch carries a programmatically-correlatable
identifier. This doc complements that gate at the narrative layer:

- **Wave 4 PR-1 (schema)**: dispatch can't physically lack a
  task_id correlation handle.
- **Wave 1 PR-3 (this doc)**: closeout synth can't fabricate a
  claim-state on top of that handle without verifying the
  handle's downstream state.

`task_id` alone is **not sufficient signal** of claim. A `task_id`
on a dispatch message proves the lead intended the task; it does
NOT prove the dev acknowledged or began work. The cross-check
procedure (§3) is the gap-closing surface.

When in doubt: downgrade the synth's claim-state language to the
weakest verifiable state. "Dispatched, awaiting acknowledgement"
is always safer than "dispatched, dev claimed". The dev's positive
acknowledgement is the upgrade signal — emit only when observed.

---

## History

- **Sprint 58 Wave 3 PR-1 incident** (2026-05-09): lead's closeout
  synth asserted "claimed by dev" before any claim event landed;
  dev idle-waited ~100min before general/operator surfaced the gap.
- **Sprint 58 Wave 4 PR-1 #566** (2026-05-09): structural fix —
  `send kind=task` requires explicit `task_id`. PR merged at
  `e8fc699`.
- **Sprint 59 Wave 1 PR-1 #567 (#9 task stall watchdog)**: ETA-
  based per-task stall detection — closes "task running over"
  vantage.
- **Sprint 59 Wave 1 PR-2 #568 (#10+#12 idle watchdog cluster)**:
  per-agent + fleet-wide idle detection — closes "no-task-id
  dispatch silently stalls" vantage.
- **Sprint 59 Wave 1 PR-3 (this doc)**: narrative-layer claim-
  state discipline — closes "synth fabricates a claim-state"
  vantage.

The structural + watchdog + narrative layers together prevent the
recurrence of the Sprint 58 Wave 3 PR-1 failure mode along all
known dimensions.
