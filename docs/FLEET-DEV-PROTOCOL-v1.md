# Fleet Development Protocol v1.2

**Status:** ACTIVE — all fleet agents must follow this protocol.
**Version history:** v1.0 (2026-04-22), v1.1 (2026-04-23), v1.2 (2026-04-26).
**Informed by:** implementer feedback, reviewer feedback, operator observations, 4-perspective challenge round.

## 0. Foundational KISS principle (Sprint 29 amendment, operator m-41 #9)

Every PR, amendment, or sprint proposal must answer two questions in the description or commit message: **"What real problem does this solve?"** and **"Would deletion break anyone?"** Reviewers MUST flag any change that lacks a concrete problem or whose deletion would be invisible to operators / agents / production users.

Violation = `KISS-VIOLATION — UNVERIFIED unless the PR provides a concrete failure mode that the change prevents`. The reviewer's verdict body must cite the protected failure mode.

**Why this rule exists**: Sprint 29 over-engineering audit (`docs/audit-over-engineering-2026-04-28.md`) found 9 paranoid-defense items totaling ~5400 LOC accumulated across Sprint 21-28 — each individually justified at write-time, none defending against a real threat in the localhost-single-user model. The 4-perspective challenge round on item #1 (RBAC removal) found 0 of 9 attempted counter-examples held; PR #285 deleted 858 LOC. The pattern: code that nobody can articulate the threat for is dead weight, not defense.

**How to apply**: when writing a PR, lead the description with the concrete failure mode; when reviewing, refuse to accept "defense in depth" or "future-proofing" as standalone justification. Either there's a real failure mode being prevented, or the change is over-engineering.

**Cross-references**: operator m-41 #9 ("把 KISS 寫入到所有人的遵守規則裡"); Sprint 29 audit doc; PR #285 RBAC removal; §3.5.12 deferred-defense gate.

## 1. Shared task board as single source of truth

**Use daemon `task` tool, NOT per-agent local TaskCreate.**

All work items visible to all agents via `task list`.

### Lifecycle

```
task create (orchestrator)
  → task claim (implementer)
    → task update --status in_progress (implementer, on PR push)
    → task update --status blocked (if waiting)
    → task update --status verified (reviewer, on VERIFIED verdict)
    → task done --result "PR #N merged" (dev-lead, on merge)
```

**Three-state completion model (v1.2):** `in_progress` → `verified` → `done`.
See §10.3 for full rules and edge cases.

### When to create tasks

| Event | Action |
|---|---|
| New PR planned | `task create --title "PR-1: set_waiting_on" --priority high --assignee at-dev-2 --depends_on []` |
| Review finding (REJECTED) | `task create --title "PR59-F2: anonymous caller gate" --priority high --assignee at-dev-2` |
| Follow-up identified | `task create --title "Followup: set_display_name anon gap" --priority low` |
| Design decision needed | Use `decision(action: post)` instead (see §2) |

### Querying

```
task list                              # all open tasks
task list --filter_assignee at-dev-2   # my tasks
task list --filter_status blocked      # what's stuck
```

### Rules

- Orchestrator creates tasks. Implementer/reviewer update status.
- `depends_on` must be set when dependency exists (enables blocking graph).
- `task done` must include `--result` with PR number or summary.
- Never use Claude Code's internal TaskCreate for fleet-visible work.

## 2. Decisions panel for scope + corrections

**Use `decision(action: post)` to freeze anything that defines scope or changes ground truth.**

### When to post decisions

| Event | Example |
|---|---|
| PR scope defined | `post_decision --title "Track1-PR2 scope" --tags track-1,pr-2 --content "§4.3 gate + §4.4 stale decay + §7 PR-2 tests"` |
| Scope intentionally narrowed | `post_decision --title "Stale decay deferred to PR-3" --tags track-1,pr-3 --content "..."` |
| Reviewer correction | `post_decision --title "PR59-F1 withdrawn" --tags track-1,pr-59 --content "inherited baseline, not PR-authored diff" --supersedes d-xxx` |
| Design choice with trade-offs | `post_decision --scope fleet --title "Heartbeat threshold 120s" --tags track-1 --content "rationale: ..."` |

### Rules

- `tags` must include track name + PR number for filterability.
- `scope: fleet` for cross-track decisions; `scope: project` for track-specific.
- `ttl_days: 30` default (decisions auto-archive; can be refreshed).
- Reviewer should trust latest `decision(action: post)` over reconstructing intent from multiple artifacts.
- `supersedes` field links corrections to original decision.

## 3. Review cycle protocol (Reviewer Contract v1.1)

Extends Reviewer Contract v0.1 with structured tooling.

### Pre-implementation

1. Orchestrator posts **scope decision** per PR:
   ```
   post_decision --title "Track1-PR1 scope"
     --tags track-1,pr-1,scope
     --content "§4.1 MCP tool + §4.2 heartbeat + §7 PR-1 tests. Drive-by: atomic save_metadata."
   ```
2. Orchestrator creates **task** for the PR:
   ```
   task create --title "PR-1: set_waiting_on + heartbeat"
     --assignee at-dev-2 --priority high
     --description "Scope decision: d-xxx"
   ```

### On review dispatch

Every review/re-review handoff includes a **3-part contract**:
1. **Source of truth:** design doc sections OR decision ID
2. **Scope boundary:** "audit X, ignore Y"
3. **Freshness boundary:** "stale if files/commits change after {sha}"

### On rejection

1. Orchestrator creates **one task per finding**:
   ```
   task create --title "PR59-F2: anonymous caller gate"
     --assignee at-dev-2 --priority high
     --description "set_waiting_on accepts empty instance_name → metadata/.json"
   ```
2. Re-review dispatch references **task IDs**, not prose:
   ```
   "Audit task T-5 only. Scope: gate + negative pin. Do not broaden."
   ```

### On reviewer correction/withdrawal

Post a decision:
```
post_decision --title "PR59-F1 withdrawn"
  --tags track-1,pr-59,review-correction
  --content "inject_provenance is inherited baseline (origin/main), not PR-authored."
  --supersedes d-xxx
```

### Verdict wording (unchanged from v0.1)

`VERIFIED` / `REJECTED` / `UNVERIFIED`

### Metadata fields (v1.1 addition, extended v1.2)

Add to every review report:
- `scope_source`: decision ID or design doc section that defined scope
- `audit_mode`: `full_review` | `finding_reaudit` | `scope_conformance`
- `reviewed_head`: git SHA at time of review (v1.2: snapshot, not contract — any subsequent commit resets verdict state)
- `commands`: verification commands run (e.g. `cargo test --features tray`)
- `files`: files audited

**VERIFIED is an audit trail, not a quality guarantee.** The verdict records what was checked at `reviewed_head`; it does not promise the code is bug-free. This framing prevents retroactive blame when post-merge issues surface.

### Re-review dispatch template (v1.2)

When dispatching r2 (re-review after REJECTED), the dispatch must enumerate r1 findings with status:

```
r1 findings:
- F1: <description> → fixed (commit abc1234)
- F2: <description> → deferred (tracked as task t-xxx)
- F3: <description> → withdrawn (decision d-xxx)
```

If r1 findings status is missing, reviewer falls back to `audit_mode: full_review`.


### 3.5 Multi-reviewer dispatch

Multi-reviewer support exists to reduce reviewer bottlenecks without weakening Reviewer Contract v1.1. The default remains **one accountable reviewer per PR**.

#### Allocation

The orchestrator assigns a review to one primary reviewer using load-based routing first, with round-robin fallback.

Load-based routing checks, in order:

1. Open or claimed review tasks on the shared task board.
2. The reviewer instance `waiting_on` state.
3. Recent inbox task/query obligations that require a reply.
4. Explicit busy responses from the reviewer.

If reviewer availability is equivalent or unknown, use round-robin among eligible reviewers.

A reviewer who cannot start promptly should reply with a structured busy response:

```text
BUSY
current: <task id or message id>
unblock: <condition or estimate>
can_take_after: <time or "unknown">
```

The orchestrator may then reassign the review instead of waiting.

**Implementation note:** Review delegates are tracked in `dispatch_tracking.json` alongside impl delegates. The 15/30min stuck timeout (`sweep_stuck`) applies to review dispatches equally.

#### Active review lifecycle

A reviewer may have at most one active review task unless an incoming task is explicitly marked `interrupt=true` with a reason.

A review task becomes active when the reviewer reads or accepts the delegate_task, and remains active until `send(request_kind: report)` is sent.

New review dispatches to an active reviewer are queued, not implicitly preemptive. The reviewer must finish and report the active task before starting another, unless:

1. The new task is an explicit continuation of the same PR/thread, or
2. The dispatch includes `interrupt=true` with a reason and the orchestrator records why.

This encodes the anti-interrupt rule from the post-Sprint 7 review: silent preemption by newer dispatch was the Sprint 8 PR-H failure mode. The addendum prevents it by protocol rather than reviewer discipline alone.

#### Primary reviewer accountability

Every PR review has exactly one **primary reviewer**. The primary reviewer owns the final review report and must cover the full Reviewer Contract v1.1:

1. Source of truth.
2. Scope boundary.
3. Freshness boundary.

The primary report must include the standard metadata fields:

```yaml
scope_source: <decision id or design doc section>
audit_mode: full_review | finding_reaudit | scope_conformance
freshness_boundary: <commit sha, file version, or explicit stale condition>
```

#### Second reviewer exception

Do not assign two reviewers to the same PR by default.

A second reviewer may be assigned only when the dispatch explicitly includes `second_reviewer: true` or equivalent prose, and one of these conditions applies:

1. High-risk shared behavior, protocol, CI, or merge-gate change.
2. Repeated reject/re-review loop where a fresh eye is needed.
3. Primary reviewer requests a second opinion on a bounded question.
4. Operator or orchestrator records a scope decision requiring dual review.

The second reviewer must receive a bounded contract:

```text
Second reviewer scope: <specific files, findings, or question>
Must pin freshness boundary: <same commit/file boundary as primary>
Expected output: VERIFIED | REJECTED | UNVERIFIED with evidence
```

A second reviewer may narrow the audit surface, but must not omit the freshness boundary. A secondary `VERIFIED` does not replace the primary review.

#### LOW docs-only single-reviewer exception

Sprint 22 P3 amendment. Mid-scope and code-touching protocol PRs continue to require dual reviewer per the rules above. The exception below narrows the dual-review trigger so that genuinely docs-only protocol edits — the kind operators land routinely as protocol housekeeping — do not bottleneck on two reviewers.

**A protocol PR qualifies for the single-reviewer exception when ALL of the following hold:**

1. **LOW risk classification** — `docs/FLEET-DEV-PROTOCOL-v1.md` (or sibling protocol files) edit only.
2. **Diff size ≤ 50 LOC** measured against `git diff origin/main...HEAD` (3-dot diff so the reviewer's freshness-boundary read is unaffected).
3. **No production code change beyond `src/instructions.rs` template strings** — i.e., the only `src/` touch allowed is the agent-instructions template that mirrors the protocol prose. Any change to `src/agent.rs`, `src/daemon/`, `src/channel/`, `src/mcp/`, `src/api/`, `src/app/`, or `src/tasks.rs` (etc.) **disqualifies** the exception even if the docs portion is small.
4. **No new rule that mid-scope+ PRs would inherit** — e.g., adding a new `§3.5.x` subsection that introduces an enforcement obligation for non-protocol PRs is mid-scope, not LOW. Pure clarifications, typo fixes, and example additions to existing rules qualify.

**When the exception applies, the dispatch may select either:**

- **Single reviewer** (default for the LOW-docs-only path) — primary reviewer alone covers the full Reviewer Contract v1.1 against the existing freshness boundary.
- **Operator self-merge** — operator may merge directly without dispatching a reviewer when the operator authored or proof-read the diff. This matches the existing operator-author lifecycle for HOTFIX paths.

The dispatch document or PR description must state which arm of the exception is invoked, e.g. `LOW docs-only protocol PR — single reviewer per §3.5.5` or `LOW docs-only protocol PR — operator self-merge per §3.5.5`. Reviewers and the orchestrator can audit the qualification by reading the diff against the four conditions above.

**Mid-scope+ unchanged**: the rules in [Second reviewer exception](#second-reviewer-exception) above still trigger dual review for protocol or merge-gate changes outside this exception (high-risk shared behavior, repeated reject loops, primary-requested second opinion, operator-mandated dual review). The exception narrows the *default* dual-review obligation for docs-only edits; it does not change when the dispatch may explicitly opt in to dual review.

**Amendment recursion** (this PR's own gate): the amendment PR that introduces this exception is itself dispatched under the *pre-amendment* §3.5.4 mandatory-dual-review rule (i.e., it walks dual reviewer). The exception only applies to PRs landing **after** this amendment merges. This avoids the bootstrap paradox of the amendment PR using its own exception to short-circuit its review.

**Case study evidence** (PR #226 — Sprint 21 Phase 5a Q6 protocol amendment, `§10.5 Rule 5 spawn rationale`): operator self-merged at 2026-04-27 03:35 UTC after a single-reviewer VERIFIED + 3-platform CI green. The PR was 2 files (`docs/FLEET-DEV-PROTOCOL-v1.md` +44, `src/instructions.rs` +1), +45/-0 lines total — exemplifying *exactly* the LOW docs-only + `instructions.rs` template touch pattern that this exception's conditions #2-#3 carve out. Under the pre-amendment rule this technically required dual review; the operator's self-merge worked out (no regression, dev-reviewer Tier-1 verdict robust) but encoded the gap this exception now formalises. Future docs-only protocol PRs should cite this exception in the dispatch rather than relying on operator override.

**Edge-case tightening** (Sprint 22 P3 r2 cross-vantage findings — anti-game protections):

- **Sibling protocol files (condition #1) defined explicitly, not glob**: `docs/FLEET-DEV-PROTOCOL-*.md` and `docs/REVIEWER-CONTRACT-*.md` (currently `FLEET-DEV-PROTOCOL-v1.md` + `REVIEWER-CONTRACT-v0.1.md`). Adding a new sibling protocol file that did not exist when this exception merged disqualifies the exception until the protocol file list is explicitly extended in a later amendment.
- **`src/instructions.rs` template-strings carve-out (condition #3) requires reviewer attestation**: the reviewer (or operator on self-merge) must explicitly verify that the change is to a literal prompt-content string with no logic side-effect — no new `format!`/template placeholders, no new conditional branches gated on the new content, no new agent-instruction clauses changing tool-use guidance. A template change that introduces new behavior (even one new conditional emit) disqualifies the exception.
- **Stacked-PR aggregation guard against 50-LOC bypass**: if the same author lands more than one LOW docs-only protocol PR within a single sprint, every PR after the first reverts to dual review for the remainder of that sprint. Prevents a 200-LOC change being split into four ≤50-LOC PRs to dodge dual review.
- **Test-file edits qualify only if pure-cosmetic**: edits to `tests/*.rs` qualify under condition #3 ONLY if the change is whitespace, comment rewording, or doc-comment alignment with no `assert!` / `expect_*` / fixture-data / behaviour-affecting change. Semantic test edits (new assertion, mutated fixture, removed gate) disqualify the exception even when LOC ≤ 50.

**Cross-team coordination**: if `docs/FLEET-TS-PROTOCOL-*.md` or sibling cross-team protocol files mirror §3.5.4–§3.5.6, the `ts-lead` should be notified post-merge to evaluate a parallel amendment that keeps the cross-team review contract aligned (otherwise LOW docs-only protocol PRs in one team's protocol file may diverge from the other team's review obligation).

#### Contract splitting

Do not split Reviewer Contract v1.1 so that no reviewer owns the whole contract.

Allowed:

- Primary covers source of truth, scope boundary, and freshness boundary.
- Secondary audits a bounded risk area against the same freshness boundary.
- Secondary performs grep-based confirmation for a named finding or regression risk.

Not allowed:

- Primary checks only source/freshness while secondary checks only scope.
- Two partial reviews are combined into one implied full review.
- A secondary review uses a different commit or file state without declaring the primary review stale.

#### Verdict merge rules

For merge gating, verdict severity is ordered:

```text
REJECTED > UNVERIFIED > VERIFIED
```

Worst verdict wins for the PR gate.

A `REJECTED` verdict must include finding-level evidence and the violated source/scope reference. The orchestrator creates one task per accepted finding.

An `UNVERIFIED` verdict must state the missing audit surface or stale boundary that prevented verification.

A `VERIFIED` verdict must state the freshness boundary reviewed.

If reviewers disagree on verdict or evidence:

1. The PR remains unmerged.
2. The orchestrator compares findings against the dispatch contract and freshness boundary.
3. If the conflict is factual or scope-related, the orchestrator posts a correction or clarification decision.
4. If the conflict cannot be resolved by the dispatch contract, escalate to the operator.

#### Re-review routing

Finding re-audits should normally return to the reviewer who issued the finding.

Reassign only when:

1. The original reviewer is unavailable or busy.
2. The re-audit is purely mechanical and the source/freshness boundary is explicit.
3. The orchestrator records that a fresh reviewer is intentional.

A reassigned re-audit must reference the finding task ID and must not broaden scope unless the dispatch explicitly says so.

#### 3.5.9 Cross-backend behavior claims

Any PR body claim of cross-backend behavior—e.g. "All CLI backends do X", "supports Y across kiro/Codex/Claude/Gemini", "behavior consistent on Linux/macOS/Windows"—**must** be either:

1. **Backed by per-backend test evidence** (preferred). Either:
   - Real backend spawn test (e.g. `#[ignore]` or cargo feature gated)
   - Capability matrix entry referenced in PR body with `verified: true` per backend

2. **Marked explicitly as `unverified claim`** with backlog task reference for verification:
   - PR body must contain phrase `unverified cross-backend claim` plus task ID
   - Backlog task must describe how/when verification will run

##### Reviewer enforcement

When reviewing a PR with cross-backend claims:
- Check PR body for both evidence (option 1) and unverified mark (option 2)
- If neither present, output `REJECTED` with finding "unverified cross-backend claim — must add per-backend test evidence or mark as unverified with backlog reference"

##### Rationale

Sprint 9 PR #159 (`interrupt` MCP tool) merged with PR body claim "All CLI backends treat ESC as stop generation". No per-backend test verified this; the claim was inferred from documentation. Operator caught the gap post-merge. Sprint 10 PR-X (backend harness) added transport verification but explicitly left semantics `Unverified`.

This rule prevents the pattern where reviewer-merged PRs ship documentation claims that were never tested. Either prove with evidence or transparently flag as future work.

#### 3.5.10 External-fixture validation

Sprint 25 P0 amendment. Three classes of bugs persistently slip past internal-only tests because the test harness shares the impl's blind spots. Each class requires a category-specific external fixture; PRs in each class must ship at least one such test. The classes:

1. **Wire-format** (cross-process IPC / stdio framing / network protocol)
2. **Concurrent-state** (shared mutable state / locks / cross-thread races)
3. **Persistence-replay** (state written to disk and restored across daemon restart)

Internal mock pairs that frame both sides identically — or that exercise a single-threaded narrative — or that hold state in memory only — are insufficient for these bug classes. Each class has a concrete production failure that motivated this rule.

##### Wire-format fixtures

PRs touching `src/bin/`, `src/api/`, `src/channel/`, `src/mcp/`, `src/daemon/lifecycle.rs`, `src/daemon/task_sweep.rs`, etc. **must** include a test exercising a payload captured from — or bit-for-bit replicating — a real-world client/server interaction.

**Internal mock pairs that frame both sides identically are insufficient.** They are vulnerable to "tests-testing-tests" failure modes where both halves share the same bug and parity holds while correctness fails. Sprint 25 P0 PR #250 + PR #253 dual-VERIFIED missed a Content-Length framing bug for exactly this reason; the bug shipped to production and caused a 30-minute MCP outage before PR #255 fixed it.

Acceptable wire-format fixtures:

1. **Production capture** — log file or wire trace from a real client (e.g. `~/.agend-terminal/bridge-trace-<pid>.log` from a Claude Code session).
2. **RFC/spec fixture** — byte-exact payload from a published protocol specification (MCP spec, JSON-RPC 2.0 spec, telegram-bot-api response samples).
3. **Cross-implementation reference** — payload generated by an INDEPENDENT implementation of the same protocol (e.g. TS/JS MCP SDK as reference for our Rust impl).

##### Concurrent-state fixtures

PRs touching shared mutable state, locks, or cross-thread coordination (`src/daemon/heartbeat_pair.rs`, `src/daemon/supervisor.rs`, vterm grid resize / render paths, channel sink registry, agent registry, file-locked stores under `src/store.rs`, etc.) **must** include a multi-threaded test that exercises producer/consumer or writer/reader race windows.

**Single-threaded fixtures cannot reproduce TOCTOU, cross-thread races, or resize-mid-render bugs.** A test that drives the impl synchronously from one thread cannot expose the temporal gap between two threads' observations of the same state. Today's vterm:167 panic (cap-vs-access temporal gap; same race class as PR #194 / PR #225 deferred items) shipped despite the existing test suite for exactly this reason — every test ran on one thread.

Acceptable concurrent-state fixtures:

1. **Multi-threaded harness** — at least two threads exercising distinct roles (one mutates state, the other reads it) with explicit synchronization points and assertion of the consistent-snapshot invariant. Example: vterm grid resize on thread A while thread B walks cells for render.
2. **Loom / shuttle interleaving** — formal model-checked exhaustive interleaving for small concurrent units, when applicable to the lock structure under test.
3. **Stress loop** — bounded-iteration loop with PRNG-driven schedule perturbation to surface intermittent races (paired with `--test-threads=1` flag suppression so the test isn't masked).
4. **Deterministic race-symptom simulation** (when type is `!Send`) — if the type under test cannot cross thread boundaries (e.g., third-party API constraints like alacritty `Term: !Send`), explicit acknowledgment + outcome-state simulation (direct field manipulation reproducing the failure mode without literal concurrent execution) is acceptable in lieu of multi-threaded harness, provided the mechanism is verifiable by code review. Cite PR #259 F1 `concurrent_resize_render_frame_integrity` as canonical example. (Sprint 25 P3 r2 amendment per PR #259 reviewer M1 finding.)

##### Persistence-replay fixtures

PRs touching state persisted to disk and restored across daemon restart (`src/decisions.rs`, `src/tasks.rs`, `src/task_events.rs`, `src/inbox.rs`, vterm scrollback, fleet config load, etc.) **must** include a round-trip test that writes state, simulates daemon restart, restores from disk, and re-parses without panic or silent corruption.

**In-memory tests cannot reproduce state-poison-survives-restart cascades.** A panic in the restore path triggers OS-level supervisor restart, which restores the same poisoned state, which re-triggers the panic — a death loop. Today's vterm cascading-restart pathology (5 consecutive panics before lucky 6th-restart partial-write truncation) is the canonical example. The bug had no validation gate between PTY write side and scrollback read side.

Acceptable persistence-replay fixtures:

1. **Round-trip test** — `write_state()` → `simulate_daemon_restart()` → `restore_state()` → assert no-panic AND state matches expected (or fails with operator-actionable error, not panic).
2. **Poison-fixture replay** — known-bad input written through the persistence layer, then replayed from disk. Asserts the restore path either sanitizes/rejects gracefully OR validates input pre-write so poison never reaches disk.
3. **Migration coverage** — for schema-versioned state, fixtures for v(N-1) data restored by v(N) reader; asserts forward-compat fail-closed (per existing `task_events::SCHEMA_VERSION` pattern, Sprint 24 P0 PR1).
4. **Logical-lifecycle replay** (when persistence is in-memory cross-boundary) — if state persists across a logical lifecycle boundary (daemon restart / reconnect / new session) but doesn't touch disk (e.g., PTY scrollback in alacritty `Term`), dump→fresh-instance→re-process round-trip is acceptable in lieu of write→disk→restore→replay. Cite PR #259 F2 `persistence_replay_poison_no_panic` as canonical example. (Sprint 25 P3 r2 amendment per PR #259 reviewer M2 finding.)

##### Reviewer enforcement

When reviewing a PR with scope-listed file changes, reviewer **must** identify which of the 3 categories apply (the PR's nature dictates: wire-protocol → wire-format; shared-state/locks/threads → concurrent-state; persistence/restore/replay → persistence-replay; multi-class PRs require multiple category fixtures) and verify presence of at least one external-fixture test per applicable category. Absent → flag as `EXTERNAL-FIXTURE-ABSENT (<category>) — UNVERIFIED unless explicitly waived with rationale`. Operator self-merge requires explicit fixture justification per applicable category in the commit message.

##### Exemption

Refactor PRs that demonstrably preserve byte-output / lock structure / persistence schema (e.g. moving framing code between modules without changing wire format; refactoring lock acquisition order without changing concurrency semantics; renaming persistence fields without changing on-disk shape) may waive external-fixture if they include a regression test asserting equivalence against the pre-refactor output (byte-equivalence for wire-format; race-free invariant for concurrent-state; round-trip-equivalence for persistence-replay).

##### Sanctioned-tool decline policy (Sprint 27 amendment)

When `Cargo.toml` (or sibling design doc) flags a tool as the **sanctioned approach** for a category — typical comment shape `# <design-doc> §X names \`<crate>\` as the sanctioned approach for <purpose>` — declining that tool in favor of a rolled-own implementation requires (a) explicit declaration in the PR description naming the sanctioned tool being declined, AND (b) narrow-scope architectural rationale (capability gap, scope-filter limitation, etc.) that distinguishes the use case from the design-doc's assumed use case. Without (a)+(b) the decline is silent and reviewer-unverifiable; future maintainers see only the rolled-own and cannot judge whether to migrate. Canonical example: PR #273 r4 declined `tracing-test = "0.2"` because `tracing_test::logs_contain` is scope-filtered and doesn't capture custom `target = "behavioral_shadow"`; rolled-own `tracing_subscriber::fmt().with_writer(parking_lot::Mutex<Vec<u8>>)` captures all targets unconditionally. r1-r6 silent decline cycle (6 cycles) caught by reviewer-2 m-236 NIT 2 + dev-reviewer m-237 strict reading. Parallel pattern to §3.5.11 r3 empirical-revert (architectural rationale required to bypass the default rule).

#### 3.5.11 Test-first verification for feature/fix PRs

Sprint 25 P0 amendment. Feature PRs and bug-fix PRs **must** be authored test-first: the failing test commit MUST land in the PR's commit history BEFORE the implementation commit that makes it pass.

**Reviewer-enforceable verification** (the test must be runnable at the test-only commit and observably fail, then runnable at HEAD and observably pass):

```bash
# Reviewer runs against the PR's branch:
git checkout <test-commit-sha>
cargo test <test-name>          # MUST observe failure
git checkout HEAD                # back to PR tip
cargo test <test-name>          # MUST observe pass
```

**Violation = UNVERIFIED** unless the PR matches one of the explicit exemptions below. Reviewer attestation in the verdict body is required: `test-first verified: <test-commit-sha> failed; HEAD passes` (or explicit exemption citation).

##### Explicit exemptions (no test-first required)

- **Documentation-only PRs** (matches §3.5.5 LOW docs-only single-reviewer exception scope: `docs/*.md`, source doc-comment-only changes, no `src/` behavior change).
- **Pure refactor PRs** with byte-preserving guarantee — existing tests pass at both pre-refactor and post-refactor commits, the PR description states "pure refactor, no behavior change", and the reviewer must confirm.
- **Test-only PRs** (the test IS the deliverable; no separate impl).
- **Dependency bumps** with no source-tree changes beyond `Cargo.toml` / lockfile.
- **EMERGENCY hotfix** — production-down / data-loss-risk / security-breach scenarios may fast-track without test-first IF (a) the PR title carries an `[EMERGENCY]` tag with rationale, AND (b) a backfill PR adding the regression test merges within 24 hours. The orchestrator (typically dev-lead) approves the emergency tag at dispatch time; the reviewer notes the deferral in the verdict.
- **Pure deletion PRs** — implementation removal + corresponding test rename/assertion-flip in the same commit; no new behavior asserted. Qualifies when (a) test changes ARE the only test deliverable (rename existing tests to assert new behavior, not new failing assertions), AND (b) deletion is grep-verifiable at code-review time (reviewer can confirm the removed code path has 0 hits in production). Reviewer attestation: `pure deletion verified: <grep-command> → 0 hits in production code path`. Canonical example: PR #262 `a3cfa09` KILL Content-Length fallback — tests renamed to assert NDJSON-only skip behavior, impl deleted CL parse path, `grep -rn "Content-Length" src/bin/agend-mcp-bridge.rs` → 0 hits.
- **Empirical-revert exemption** (Sprint 26 amendment) — when the test is **architecturally un-runnable at a separate RED commit** because the test depends on an impl-provided fixture, type, function, or env-var configuration that lives in the same PR's impl. A literal RED commit would either fail-to-compile or have nothing to assert against. Reviewer-enforceable substitute: **empirical revert at HEAD** — reviewer reverts the impl portion (typically `git revert <impl-sha> --no-commit` or a single-step checkout that removes the impl), observes the test fails or fails-to-compile, restores impl, observes test passes. Conditions: (a) PR description must explicitly declare the architectural impossibility (e.g., "test depends on impl-provided fixture; no separate RED state possible"), AND (b) the revert sequence must be documented step-by-step in the PR description so reviewer can reproduce, AND (c) impl removal must be reproducible in a single revert/checkout step (no hand-edit gymnastics). Reviewer attestation: `empirical-revert verified: revert <impl-sha> → test failed/uncompilable; restore → test passes`. Canonical example: PR #267 r3 `d8ce15a` slow-loris timeout — test depended on the env-var-override impl present in HEAD; a separate RED commit would have nothing to assert against. Distinct from EMERGENCY hotfix exemption (this is architectural, not time-pressure).

##### Rationale

Per operator philosophy override 2026-04-27: "強制 (違反 = UNVERIFIED)". Post-hoc tests are confirmation-biased toward what the impl already does, not toward what the spec requires. The red→green ordering forces the test to embody the intended behavior independently of the impl's actual behavior.

##### Complementary with §3.5.10

External-fixture (§3.5.10) defines **what** a test must exercise (real-world payload, not internal mock pair); test-first (§3.5.11) defines **when** the test must be authored (before impl, not after). For wire-protocol PRs, both rules apply: an external-fixture test must be the first commit, then impl makes it pass.

##### Self-amendment qualification

This amendment ships as docs-only and qualifies under §3.5.5 LOW docs-only single-reviewer exception (single-reviewer, no `src/` behavior change). The amendment does not apply to itself recursively — under §3.5.10 it is exempt because no protocol-layer files are touched, and under §3.5.11 it is exempt as a documentation-only PR per §3.5.5 scope.

#### 3.5.12 Deferred-defense process (production-panic recurrence prevention)

Sprint 25 P3 amendment. When a production bug is deferred to a backlog item instead of fixed in the current PR, three enforcement gates prevent the "defer → forget → recur" pattern.

**Incident chain that motivated this rule**: PR #194 (2026-04-21, saturating cap — deferred root-cause resize race to backlog `t-20260426150432078733-1`) → PR #225 (Q7 sweep doc-only — same root cause still deferred) → 2026-04-27 vterm L167 panic recurrence (5 consecutive daemon crashes from the same unfixed race). Two successive "defer" decisions with no enforcement gate allowed a known production panic to recur 6 days later.

##### (a) Known-issue P0 trigger

When a production panic or data-loss incident occurs AND the panic signature (file:line, error message substring) matches an existing deferred backlog item's description, the backlog item is **automatically escalated to P0 hotfix priority**. The orchestrator must dispatch the fix within the current sprint — no further deferral permitted.

**Reviewer enforcement**: when reviewing a PR that defers a bug to backlog, the reviewer must verify the backlog item exists and has a `due_at` set (see rule (b)). If the deferred bug later recurs in production, the original deferral PR's reviewer shares accountability for the gap.

##### (b) Deferred backlog SLA

Every deferred backlog item created from a PR review finding or a known production bug **must** carry a `due_at` deadline (default: 2 sprints from creation). The task board's `due_at` field is the enforcement mechanism.

When `due_at` expires without resolution, the orchestrator must either:
1. Escalate to P0 and dispatch immediately, OR
2. Extend `due_at` with explicit operator approval and a `decision(action: post)` recording the extension rationale.

Backlog items without `due_at` that match deferred-from-PR patterns are flagged by the orchestrator during sprint planning.

##### (c) Dual-reviewer escalation on repeated deferral

When the **same root cause** is deferred for the **second time** (i.e., a PR defers a bug that was already deferred in a prior PR), the second deferral requires:
1. **Mandatory dual reviewer** (even if the PR would otherwise qualify for single reviewer), AND
2. **Operator sign-off** via `decision(action: post)` with explicit acknowledgment of the repeated deferral.

Without both, the reviewer must issue **UNVERIFIED** on the deferral. The implementation fix itself may still be VERIFIED — only the deferral decision is gated.

**Detection heuristic**: grep the deferred backlog item's description for the same file:line or error signature as the current PR's deferred finding. Match → repeated deferral → dual + operator gate.

##### Explicit exemptions

- **EMERGENCY hotfix** PRs (per §3.5.11 emergency exemption) may defer root-cause fixes with a 24-hour backfill SLA instead of the 2-sprint default, but the backfill item must still carry `due_at`.
- **Cross-platform issues** where the root cause requires platform-specific investigation (e.g., Windows-only race) may extend `due_at` to 4 sprints with orchestrator approval.

##### (d) Counter-example construction rule for over-engineering removal (Sprint 29 amendment, operator m-41 #8 + m-102)

When a removal PR proposes deleting a defensive mechanism (RBAC layer, policy gate, paranoid validation, or any "defense in depth" code), the 4-perspective challenge round MUST include explicit attempts to construct counter-examples — concrete attacker-capability scenarios where the mechanism's removal would enable a real failure that the mechanism was preventing.

**Rule**: if all 4 perspectives independently fail to construct any compelling counter-example, the absence of counter-examples is itself the §3.5.12 deferred-defense gate satisfaction. "找不到 counter-example = 證 X 真可砍" (operator m-41 #8 wording).

**Reviewer attestation on removal PRs**: `counter-example construction attempted: <N> scenarios tried; <M> compelling cases found; verdict <GO/NO-GO>`. If M > 0, the removal must explicitly address the failure mode identified.

**Canonical example**: PR #285 RBAC removal (Sprint 29). 4-perspective challenge round attempted 9 scenarios (prompt-injected agent floods telegram, less-trusted agent added to fleet, defense-in-depth-if-cookie-leaks, operator hardening, compliance audit logging, multi-user shared machine, untrusted CI environment, misconfiguration prevention, fine-grained per-agent permissions). 8 failed outright via attacker-capability reasoning (attacker pivots to non-RBAC-gated channels — `send`, subprocess spawn, file I/O, process exit); 1 weakest case (#9 reviewer-only-reply convention enforcement) had lighter alternatives via instruction-prompt + PR review. 0 compelling counter-examples → §3.5.12 gate satisfied → 858 LOC deletion VERIFIED.

**Cross-reference precedent**: §3.5.11 #6 pure-deletion exemption (PR #265 r2, PR #267 r3 d8ce15a, PR #285 RBAC removal canonical) — the deletion exemption mechanically allows a single-commit removal; this rule (d) substantively gates whether the removal SHOULD happen via counter-example failure analysis.

##### Self-amendment qualification

This amendment ships as docs-only and qualifies under §3.5.5 LOW docs-only single-reviewer exception. Cross-references the real incident chain (PR #194 → #225 → vterm L167 recurrence) as the motivating evidence; rule (d) extends with Sprint 29 RBAC removal canonical (PR #285).

#### 3.5.13 Verdict externalization — fleet-internal verdict MUST mirror to GH PR

Sprint 25 closeout amendment. Every fleet-internal review verdict (VERIFIED / REJECTED / UNVERIFIED) delivered via inbox `kind=report` **must** be mirrored as a GitHub PR comment via `gh pr comment <N> --body "..."` so the operator's cron view + manual GH-UI review can see the verdict without inbox access.

##### Rule

After every Tier-1 / Tier-2 fleet review verdict (PRIMARY or cross-vantage) delivered via inbox `kind=report`, the reviewer (or dev-lead synthesizing) **must** post a GH PR comment summarizing:

- **Reviewer name + tier** (e.g. `dev-reviewer-2 cross-vantage Tier-2` / `dev-reviewer Tier-1 PRIMARY` / `single-reviewer LOW per §3.5.5`)
- **Verdict** (VERIFIED / REJECTED / UNVERIFIED)
- **`reviewed_head` SHA**
- **Main findings**, categorized as BLOCKING / NIT / RECOMMEND
- **Pipeline next-step** (r2 dispatch / self-merge / dual VERIFY collected / etc.)

##### Self-merge gate

**dev-lead self-merge MUST be preceded by the mirror comment posted.** If self-merge happens before mirror, the merge is treated as **UNVERIFIED** until the mirror comment is retroactively posted (and the reviewer flags the missing mirror in the next dispatch).

##### Why this rule exists

Inbox-only verdicts are a fleet-internal channel — only fleet members + operator-via-`inbox` MCP tool see them. Operator's cron view, manual GH-UI PR review, and external collaborators all rely on the GH PR comment thread. **Un-mirrored verdict = operator sees nothing on GitHub = enforcement leaves no public trace = effectively undone.**

The fix is purely process-level: the verdict already exists in the inbox; mirror reproduces it on GH. No additional code, no new MCP tool, no new infrastructure — just one `gh pr comment` per verdict.

##### Format example

```bash
gh pr comment <N> --body "$(cat <<'EOF'
## Tier-2 cross-vantage review — VERIFIED

- **Reviewer**: dev-reviewer-2 cross-vantage
- **reviewed_head**: <SHA>
- **Verdict**: VERIFIED with N findings

### Findings

- **BLOCKING**: (none)
- **NIT**: (list)
- **RECOMMEND**: (list)

### Pipeline next-step

dev-reviewer Tier-2 PRIMARY pending. Both VERIFIED → dev-lead self-merge.

EOF
)"
```

##### Cross-references — incident chain

This rule was operator-mandated permanently per **operator m-84** (telegram 2026-04-27) after observing fleet-internal verdicts not surfacing on GitHub during Sprint 25 P3 review wave:

- **PR #262** (MCP framing KILL Content-Length) — r1 UNVERIFIED + r2 VERIFIED; both verdicts initially inbox-only; mirror backfilled retroactively after operator m-84 directive.
- **PR #263** (active peer PID watch) — VERIFIED verdict mirrored after backfill.
- **PR #264** (slow-loris timeout, Sprint 25 P3 closeout) — VERIFIED verdict mirrored after backfill.

The 3 backfilled mirror comments serve as canonical format examples for future reviewers. Operator's directive is permanent — applies to all PRs going forward, not just Sprint 25 P3 wave.

##### Explicit exemptions

- **§3.5.5 LOW docs-only single-reviewer exception PRs**: still subject to mirror requirement — single-reviewer verdict mirrors as `single-reviewer LOW per §3.5.5` reviewer-tier label. The exception narrows reviewer count, not verdict-externalization scope.
- **Operator self-merge of operator-authored PRs**: operator's own PRs may be merged without fleet-reviewer verdict mirror IF the PR description itself documents the rationale (operator's own statement is the public trace). Fleet-reviewer-issued verdicts on operator PRs still require mirror.
- **kind=update notifications** (status pings, queue announcements): NOT subject to mirror — only `kind=report` verdicts.

##### Anti-game coverage

- **"I'll mirror after merge"** → §3.5.13 self-merge gate explicitly forbids; pre-merge mirror required.
- **"My inbox verdict was clear"** → still required to mirror; inbox is fleet-internal channel, not public trace.
- **"Mirror is just duplication"** → operator visibility / external collaborator visibility / cron-view visibility all rely on GH; fleet-internal-only is invisible to those audiences.

##### Self-amendment qualification

This amendment ships as docs-only and qualifies under §3.5.5 LOW docs-only single-reviewer exception. The amendment **applies to itself recursively**: the verdict on this PR (issued by dev-reviewer per Path A single-reviewer authority) must mirror to the GH PR comment thread before dev-lead self-merge. This is intentional dogfood — the rule's first application is the amendment that introduces it.

#### 3.5.14 UX regression prevention (telemetry log-level changes)

Sprint 27 amendment. Telemetry log-level changes (`tracing::debug!` ↔ `tracing::info!` ↔ `tracing::warn!` etc.) directly affect operator default-visible noise: most operators run with default-INFO subscribers, so a `debug!` → `info!` bump silently converts opt-in observability into opt-out spam. Reviewer **must** flag any unexplained level change as `LEVEL-CHANGE-RATIONALE-ABSENT — UNVERIFIED` unless the PR carries (a) inline code comment at the call site stating the new level's rationale, AND (b) reviewer attestation that the change matches operator-visibility intent (opt-in stays at DEBUG; default-visible alarm moves to WARN). Canonical example: PR #273 r4 bumped shadow telemetry `debug!` → `info!` with no rationale — would have spammed all default-INFO operators with shadow noise; reviewer-2 m-236 NIT 3 caught + r5 reverted to `debug!` (correct opt-in semantics).

#### 3.5.15 Observability PR e2e requirement

Sprint 27 amendment. Shadow-mode and observability PRs (telemetry, metrics, divergence dashboards, anything where the deliverable is a *signal-emit* the operator later reads) **must** include at least one end-to-end integration test that exercises the production hook path before VERIFIED. Unit-level tests that call the emit function directly, OR fixture-replay tests that assert state-machine transitions without observing the emitted signal, are insufficient — they pass while the production wire-up is dead code. Reviewer attestation: `e2e-through-production-hook verified: <test-name> exercises <production-path>`. Canonical example: PR #273 r5 silently shipped a timing bug where `last_output.elapsed()` was measured AFTER `last_output = Instant::now()` in `state.rs::feed()`, so behavioral telemetry always saw silence ≈ 0 and never emitted — the entire feature was dead code in production. Detected only when r6 added an e2e test that fed a fixture, slept past the silence threshold, fed again, and asserted the captured tracing output contained `silence_thinking`. r1-r5 (5 cycles) of unit-level + isolated-fixture tests passed without exposing the bug. reviewer-2 m-250 self-correction memorialized the heuristic: structural test compliance ≠ functional correctness. Defends against the same isolation-masks-integration trap class regardless of feature.

### 3.6 Async pipeline — push-and-immediately-continue

Sprint 26 amendment (operator m-177). To eliminate idle wait time across the impl/reviewer/dev-lead pipeline, the protocol adopts an **asynchronous-push model**: impl agents and reviewers push their work and **immediately move to the next task** without waiting for CI / dual-VERIFIED / merge confirmation. dev-lead persists the PR pending list, watches CI, and self-merges on dual-VERIFIED + green.

#### 3.6.1 Impl push semantics

After pushing a PR (whether r1 or r2), impl-agent:

1. Posts a `kind=update` notification to dev-lead with PR URL + HEAD SHA + diff stat.
2. **Immediately** picks up the next dispatched task from inbox.
3. Does NOT poll CI status; does NOT wait for reviewer verdict; does NOT block on dev-lead merge.

If the PR is later REJECTED or has CI failure, impl receives a return-to-fix dispatch via inbox (queue-priority message). Impl decides when to take it based on current task queue state.

#### 3.6.2 Reviewer push semantics

After issuing a verdict (`kind=report` inbox + §3.5.13 GH PR mirror), reviewer:

1. **Immediately** picks up the next dispatched review task.
2. Does NOT wait for the OTHER reviewer (cross-vantage or PRIMARY counterpart) to issue their verdict.
3. Does NOT wait for dev-lead self-merge confirmation.

If a r2 dispatch arrives later (impl pushed fix, dev-lead asks for re-review), reviewer takes it from inbox in priority order.

#### 3.6.3 dev-lead orchestration

dev-lead maintains the PR pending list (active reviews, pending CI, pending merge) as durable state. On each tick:

1. Watch CI for in-flight PRs.
2. Check inbox for verdicts.
3. Self-merge when dual-VERIFIED + CI green.

If both reviewers VERIFIED but dev-lead misses the convergence (e.g., bottleneck or context-shift), the **periodic active poll fallback** must detect within N minutes. Default N = 30 min; operator-tunable. Canonical evidence: PR #269 self-merge gap incident — dev-lead missed dual-VERIFIED convergence by ~1 hour, operator caught manually. The active-poll fallback prevents this category of stall.

#### 3.6.4 Edge cases

- **CI flaky** → manual `gh run rerun <run-id>` is the operator-authorized retry path; no auto-retry policy at this stage. dev-lead surfaces flake to operator if same test fails 2+ times across reruns.
- **dev-lead bottleneck or context-exhaustion** → operator may direct-dispatch self-merge if dev-lead context-exhausted (per operator m-181 takeover protocol). Adjacent process pattern: see PR #268 r2 dev-lead takeover — async pipeline addresses normal flow; m-181 protocol addresses exception flow.
- **GH PR view = ground truth** — when dev-lead memory and inbox state diverge (mid-merge crash, multi-day PRs, etc.), the GH PR comment thread + commit history are authoritative. §3.5.13 mirror enables this — operator/external viewers see verdicts on GH without inbox access.

#### 3.6.5 Cross-references

- **Operator m-177**: original directive for async pipeline adoption (eliminate idle wait time).
- **PR #269 self-merge gap incident**: motivating evidence for the periodic active-poll fallback (§3.6.3).
- **PR #268 r2 dev-lead takeover (operator m-181 protocol)**: adjacent process pattern when context-exhaustion blocks pipeline progress.
- **§3.5.13 verdict externalization**: enables §3.6.4 "GH PR view = ground truth" — verdicts mirrored on GH allow recovery from dev-lead memory drift.

#### 3.6.6 Self-amendment qualification (this PR — first §3.6 dogfood)

This amendment ships as docs-only and qualifies under §3.5.5 LOW docs-only single-reviewer exception. The amendment **applies to itself as the first canonical §3.6 test**: after I push this PR, dev-reviewer immediately picks up the review without waiting for CI; on VERIFIED, dev-lead self-merges; I do not wait for any of these and immediately pick up the next dispatch (Sprint 27 cost/benefit challenge). The async pipeline working end-to-end on this very PR is the first dogfood.

#### 3.6.7 ScheduleWakeup auto-poll (Sprint 27 amendment)

The §3.6.3 periodic active-poll fallback ("default N = 30 min") is a *requirement*, not a *mechanism*. dev-lead's backend (Claude Code) provides the `ScheduleWakeup` tool; **every PR entering dev-lead's pending list MUST be paired with a `ScheduleWakeup` call at CI ETA + N min slack** (default N = 5 min). Wakeup fires re-poll PR list mergeable + dual-VERIFIED → auto-merge per §3.6.3. Do **not** depend on "remembering to poll" in working memory — operator m-5 directive cites three recurring instances where dev-lead missed dual-VERIFIED convergence (PR #269 self-merge gap, PR-A r3-r7 chain, PR-B). Memory-rule reinforcement failed three times; the structural fix is mechanical scheduling. Reviewer attestation on dev-lead-orchestration PRs: `ScheduleWakeup-paired with pending-list entry: verified at <PR>`.

#### 3.6.8 Takeover 4-criteria independent verify (operator m-7)

The dev-lead takeover protocol (m-181 — when impl context-exhausted, dev-lead may complete the PR) requires **all four criteria independently verified by dev-lead** before invocation:

1. `last_heartbeat` stale ≥ 1 hour
2. `last_input` frozen multiple hours
3. `agent_state` idle non-tool_use
4. Zero git / task / description activity for the same window

**"Impl self-reports context near limit" is NOT a trigger** — dev-lead must independently verify the 4 criteria using `describe_instance` / git log / task board. Self-reported context-pressure is unreliable: the canonical evidence is PR-B m-5 takeover plan (initially GO based on silent-freeze assumption) where impl-2 self-flagged context-near-limit but `describe_instance` later showed 49% remaining — NOT near-limit, takeover would have been wrong. Reviewer attestation on takeover-invocation PRs: `4-criteria independently verified: heartbeat=<ts>, last_input=<ts>, state=<state>, activity=0 events in <window>`. Adjacent to §3.6.4 dev-lead bottleneck edge case — formalizes the gating.

#### 3.6.9 Git auto-cleanup on merge (operator m-2)

dev-lead (or operator) self-merging a PR **must** treat the cleanup pair as part of the same atomic step:

```bash
git worktree remove --force <wt-path>   # the worktree paired with the PR branch
git branch -D <pr-branch>                # the local branch paired with the PR
```

Skipping cleanup accumulates branch + worktree sprawl. Canonical incident: 2026-04-28 14:00Z operator manually cleaned **94 local branches + 74 worktrees** that had accumulated across Sprint 25-27 (5/5 manually pruned). The atomic-step formulation prevents recurrence — self-merge without cleanup is incomplete, not "deferrable". Recommended implementations (any one suffices):

- `scripts/git-cleanup-merged.sh` — fetch + list merged-PR-branches + corresponding worktrees + prune
- Git `post-merge` hook auto-trigger
- §3.6.3 self-merge orchestration treats cleanup as part of the same step (no separate "remember later")

Enforcement: a Sprint-end review that finds N+ stale merged-PR branches or worktrees flags `branch-sprawl regression — UNVERIFIED until cleaned`. Cross-reference: §3.6.3 (self-merge gate) — the cleanup pair is a post-condition of self-merge, not an independent task.

#### 3.6.10 Orchestrator owns watch_ci for own-orchestrated PR branches (Sprint 29 amendment, operator m-91)

`watch_ci` notification is injected into the **caller's** inbox. The orchestrator (typically `dev-lead`) MUST own `watch_ci` calls for every PR branch they orchestrated. `general` and the operator MUST NOT call `watch_ci` on dev-orchestrated branches except during emergency takeover (per operator m-181 protocol).

For cross-team branches, the orchestrator of that team owns the call.

**Why**: when `general` or the operator manually invokes `watch_ci`, the green/red notification arrives in the caller's inbox — `general` then has to relay it to the orchestrator, adding hops, latency, and operator confusion. Routing the call to the right owner from the start eliminates the relay path.

**Reviewer attestation on dev-lead-orchestration PRs**: `watch_ci-ownership verified: dev-lead invoked watch_ci at <PR>; no operator/general manual call`.

Cross-references: operator m-91 directive; §3.6.7 ScheduleWakeup auto-poll (paired discipline — both are dev-lead orchestration tools); §3.6.3 self-merge gate (watch_ci notification feeds the gate).

## 4. Communication rules

### Hop reduction

Target: implementer → orchestrator → reviewer → orchestrator → implementer (4 hops)
→ reduce where possible.

**Auto-merge on VERIFIED:**
When orchestrator dispatches review, include: "If VERIFIED → I will auto-merge. No need to wait for my ack."

This eliminates 1 hop (reviewer → orchestrator → merge → notify).

### Ack absorption

- `requires_reply: false` on status updates and notifications.
- Pure ack messages ("收到", "OK", "👍") → do NOT reply. Break chain.
- Only reply when there's new information to add.

### Message semantics

| `request_kind` | When | Expects reply? |
|---|---|---|
| `task` | delegation, review dispatch | yes |
| `report` | result, verdict, status update | depends on content |
| `update` | FYI, notification | no |
| `query` | question, discussion | yes |

### Response channel matches source channel

Every agent must reply via the same channel the input arrived on:

| Source signal | Reply mechanism |
|---|---|
| `(Reply using the reply tool, NOT direct text)` system hint | `reply` MCP tool (telegram) |
| `[from:OTHER_AGENT_NAME]` prefix | `send` MCP tool |
| **Neither of the above** (operator typed in TUI) | **direct text** — do not use any tool |

**Why**: the daemon does not intercept TUI stdin, so there is no hint. If the agent uses `reply` (telegram) when the operator typed in TUI, the response appears in telegram instead of the terminal — the operator waits forever in TUI. The reverse (direct text when input came from telegram) is equally broken.

## 5. CI integration

### Use `watch_ci` instead of manual polling

```
watch_ci --repo suzuke/agend-terminal --branch feat/my-branch
```

Daemon auto-injects CI failure logs. No manual `gh pr checks --watch`.

### After PR merge

Implementer cleans up:
```bash
git worktree remove /path/to/worktree
git branch -D feat/branch-name
git fetch origin main && git checkout main && git pull --ff-only
```

(Future: daemon auto-cleanup on merge detect — tracked as enhancement.)

## 6. Progress visibility for operator

### What gets emitted to Telegram fleet binding

Level **(a) task state changes** (per at-dev-2/at-dev-4 consensus):

| Event | Example notification |
|---|---|
| Task created | `[task] #5 created: "PR-1 set_waiting_on" → at-dev-2` |
| Task claimed | `[task] #5 claimed by at-dev-2` |
| Task blocked | `[task] #5 blocked: waiting on PR-1 review` |
| Task done | `[task] #5 done: PR #59 merged` |
| Review verdict | `[review] PR #59: VERIFIED by at-dev-4` |
| Decision posted | `[decision] "Track1-PR2 scope" posted` |

### Operator queries (via Telegram or TUI)

- "進度？" → orchestrator runs `task list` + `list_decisions --tags current-track` and summarizes.
- Active task count + blocked count + done count gives instant pulse.

### 6.1 Instance lifecycle event broadcast (Sprint 29 amendment, operator m-41 #10)

When the daemon detects an instance creation OR spawn that did NOT originate from the loaded `fleet.yaml` — manual `create_instance` MCP call, template deployment, scheduled-routine instantiation — it MUST broadcast an `instance-created` `<fleet-update>` event to:

1. The chat-proxy instance (`general`)
2. The team orchestrator (typically `dev-lead`) for the new instance's team

The broadcast payload MUST include an `origin` field disambiguating the creation path:

| `origin` value | Meaning |
|---|---|
| `manual` | Operator invoked `create_instance` MCP tool directly |
| `fleet.yaml` | Instance defined in fleet.yaml at daemon bootstrap |
| `template` | Instance materialized from a `deploy_template` call |
| `schedule` | Instance spawned by a `create_schedule` cron tick |

**Why**: prior to this rule, instances appearing outside fleet.yaml (e.g., `kiro-cli-ae9898` materializing during Sprint 27) had no traceable origin. Operator + orchestrator had to guess whether the agent was operator-deliberate, a routine misfire, or an externally-injected leak. Including `origin` in the broadcast eliminates the guesswork.

**Format example**:
```json
<fleet-update>
{"kind": "instance-created", "name": "kiro-cli-ae9898", "team": "dev", "origin": "template", "created_at": "2026-04-29T03:21:14Z"}
</fleet-update>
```

**Cross-references**: operator m-41 #10; §3.6.4 (GH PR view = ground truth — same anti-mystery principle applied to GH artifacts); §6 fleet update emission (extends the existing telegram emission contract to cover instance lifecycle events).

## 7. Waiting and timeout

### Declaring wait state

When blocked on another agent, CI, or external event:

```
set_waiting_on --condition "review from at-dev-4 on PR #63"
```

- Automatically cleared after 120s of no MCP activity (stale decay).
- Visible via `describe_instance` and `list_instances` to all agents and operator.
- Orchestrator can query `list_instances` to see who's waiting on what.

### Scheduling check-ins (cross-backend)

**Do NOT rely on backend-specific mechanisms** (e.g., Claude Code `ScheduleWakeup`).
Use daemon-level scheduling, which works for all backends:

```
create_schedule --target general
  --message "Check inbox: at-dev-2 should have finished PR-1 by now"
  --run_at "2026-04-22T21:00:00+08:00"
  --label "pr1-check"
```

One-shot schedules auto-disable after firing. Use for:
- Timeout checks on delegated tasks
- Periodic progress polling during long operations
- Reminder to follow up on review verdicts

### Timeout policy

| Elapsed since dispatch | Action |
|---|---|
| < 20 min | Normal. Check `describe_instance` — `last_heartbeat` fresh = agent active. |
| 20 min, agent `last_heartbeat` fresh | Agent is working. Extend wait. |
| 20 min, agent `last_heartbeat` stale (> 120s) | **Ping to verify liveness.** `send` with a direct question. |
| 20 min, no response to ping | **Escalate.** `replace_instance` and re-dispatch task. |
| Agent state `permission` + heartbeat fresh | Heartbeat gate suppresses false positive (A5 fix). Trust heartbeat. |
| Agent state `permission` + heartbeat stale | May be genuinely stuck. Ping first, then escalate. |

### Liveness check procedure

```
# Step 1: check heartbeat
describe_instance --name at-dev-2
# → last_heartbeat: "2026-04-22T12:55:00Z" (< 120s ago = fresh)

# Step 2: if stale, ping
send_to_instance --instance_name at-dev-2
  --message "Status check: are you still working on task t-xxx?"
  --requires_reply true

# Step 3: if no reply within 5 min → replace
replace_instance --name at-dev-2 --reason "unresponsive after timeout"
```

### After task completion

1. Implementer: `send(request_kind: report)` → `task done --result "PR #N merged"`
2. Orchestrator: picks up from inbox (or scheduled check-in fires)
3. Clean up: `delete_schedule --id <check-in-schedule-id>` if one was set

## 8. Git workflow

### Worktree rules (from CLAUDE.md, reinforced)

- **Never** commit directly to main. Always use worktree + branch.
- `docs-skip-PR` means skip review ceremony, NOT skip branch isolation.
- Branch naming: `feat/`, `fix/`, `docs/` prefix per change type.

### After PR merge

Clean up immediately. Don't accumulate stale worktrees.

## 9. Tool usage quick reference

| Need | Tool | NOT this |
|---|---|---|
| Track work items | `task create/list/claim/done` | Claude Code TaskCreate |
| Record decisions | `decision(action: post)` | Markdown files |
| Assign work | `send(request_kind: task)` (rich context) + `task create` (persistent) | Only one of them |
| Report results | `send(request_kind: report)` | Free-text send_to_instance |
| Watch CI | `watch_ci` | Manual `gh pr checks` |
| Declare wait state | `set_waiting_on` | Prose in messages |
| Check agent health | `describe_instance` (has `last_heartbeat`) | Guessing from pane |
| Schedule check-in | `create_schedule` (one-shot `--run_at`) | Backend-specific ScheduleWakeup |
| Timeout escalation | `replace_instance` (after ping fails) | Silently waiting forever |


## 10. Workflow efficiency rules (v1.2)

Three rules to eliminate idle time. Operator-authorized 2026-04-26.

### 10.1 Pipeline dispatch

**Rule:** Implementer pushes PR, then immediately starts the next task. Do not wait for review or merge.

- PR rejected → implementer interrupts current task to rework the rejected PR.
- PR merged → current task unaffected, continue.

**Edge case policies:**

| ID | Policy |
|---|---|
| E1.1 | **Strict on-top-of-main.** Pipeline tasks must branch from main. If next task depends on a pending PR, do not pipeline — wait for merge. |
| E1.2 | **Pipeline depth ≤ 2.** Maximum: 1 PR in review + 1 task in progress. Three-deep cascades are unmanageable. |
| E1.3 | **Context-switch threshold on reject.** If next task is ≤30% done, switch back immediately. If ≥70% done, dev-lead may allow finishing before rework. Between 30-70%, dev-lead decides. |
| E1.4 | **Backend-aware capacity.** Claude agents: 3-4 concurrent review items. Kiro-cli agents: 1-2. Same caps apply to implementers. |

### 10.2 Reviewer does not wait for CI

**Rule:** Reviewer starts code review as soon as PR is pushed. CI green → send verdict immediately. CI red → handle by failure type.

**Edge case policies:**

| ID | Policy |
|---|---|
| E2.1 | **CI fail classification by job.** `fmt`/`clippy` red → lint issue, impl fixes, verdict still valid. `build`/`test` red → logic error, requires one more review round. Snapshot-only diff (no generator logic change) → impl updates snapshot, verdict valid. |
| E2.2 | **CI green is necessary, not sufficient.** Reviewer verdict is authoritative regardless of CI color. CI green does not auto-approve; CI red does not auto-reject. |
| E2.3 | **Force-push during review invalidates verdict.** Default: any push after review starts resets verdict. Exception: reviewer can verify commit-level patch hash matches via stack-base diff — but default invalidation is safer. |
| E2.4 | **`reviewed_head` is a snapshot, not a contract.** VERIFIED applies to the exact SHA in `reviewed_head`. Any subsequent commit resets verdict state. Aligns with GitHub "dismiss stale review on push". |
| E2.5 | **Dual reviewer (§3.5.4) not short-circuited.** Rule 2 does not override §3.5.4 mandatory dual review. Dev-lead must not auto-merge on single VERIFIED + CI green when dual review is required. (Sprint 22 P3 inserted §3.5.5 LOW docs-only exception; the §3.5.4 dual-review rule still governs all non-exception cases.) |
| E2.6 | **Reviewer pipeline cap by backend.** Reviewers also pipeline. Claude: 3-4 concurrent reviews. Kiro-cli: 1-2. |
| E2.7 | **Scope-creep priority over CI red.** REJECT primary reason is always scope violation. CI failure is secondary detail. |
| E2.8 | **r2 dispatch must enumerate r1 findings.** Re-review dispatch template must list each r1 finding as fixed/deferred/withdrawn. Missing enumeration → reviewer falls back to `full_review`. |

### 10.3 Task close on completion

**Rule:** Task state tracks PR lifecycle through three states: `in_progress` → `verified` → `done`.

| State | Who sets | When |
|---|---|---|
| `claimed` | Implementer | Task accepted |
| `in_progress` | Implementer | PR pushed |
| `verified` | Reviewer | VERIFIED verdict sent |
| `done` | Dev-lead | PR merged |

**Ownership:** Impl-claimed tasks are closed by the impl owner (or dev-lead on merge). Reviewers close only tasks assigned to them (review dispatch tasks). This avoids permission conflicts where reviewer cannot update impl-claimed tasks.

**Edge case policies:**

| ID | Policy |
|---|---|
| E3.1 | **Impl owns close for impl tasks.** Reviewer sets `verified`; dev-lead sets `done` on merge. Reviewer does not attempt `task done` on impl-claimed tasks (daemon rejects non-owner close). |
| E3.2 | **Three-state model.** `in_progress` (impl working) → `verified` (reviewer approved) → `done` (merged). No skipping states. |
| E3.3 | **Merge fail handling.** If merge fails (conflict) after `verified`, task drops back to `in_progress`. Impl resolves conflict, re-pushes, reviewer re-verifies. |
| E3.4 | **Multi-round review cycle.** REJECTED → task stays `in_progress` → rework → push → re-review → `verified`. Task never enters `done` until merge. |
| E3.5 | **Dev-lead merge gate.** Dev-lead verifies task state before merge. If reviewer/impl forgot to update, dev-lead updates as safety net. Protocol-level mitigation; daemon auto-close on PR merge is a future enhancement. |
| E3.6 | **Idempotent close.** Closing an already-done task is a no-op (daemon should not error). |
| E3.7 | **Done-but-superseded.** If scope changes after task is done, post a decision and create a new task with `depends_on`. Do not reopen the original. |
| E3.8 | **Verdict evidence chain.** Every verdict report must include: `reviewed_head`, `scope_source`, `audit_mode`, `commands`, `files`. See §3 metadata fields. |

### 10.4 Worktree mandatory for impl and reviewer (v1.2 amendment)

**Rule:** All implementers and reviewers must work in a git worktree, not the main repository working tree. This prevents checkout races when multiple agents pipeline work concurrently (v1.2 Rule 1).

**Evidence:** git reflog showed 12+ checkout operations in 1 hour on the main repo — two implementers racing `git checkout` on the same working tree during pipeline dispatch.

**Worktree naming convention:**
```
git worktree add ../agend-terminal.worktrees/<branch-name> <branch-name>
```
Or for reviewers:
```
git worktree add /tmp/agend-prNNN-review <branch-name>
```

**Exceptions:**
- **dev-lead**: merge operations + read-only queries may use the main repo (atomic merge is safe).
- **general**: operator interface agent, not bound by impl/reviewer workflow.

**Edge case policies:**

| ID | Policy |
|---|---|
| E4.1 | **Worktree per branch.** Each PR branch gets its own worktree. Do not reuse worktrees across branches. |
| E4.2 | **Cleanup after merge.** `git worktree remove <path>` + `git branch -d <branch>` after PR merge. Stale worktrees waste disk and confuse `git worktree list`. |
| E4.3 | **Consistent with CLAUDE.md.** This rule formalizes the existing CLAUDE.md global rule "never commit directly to main; always use worktree + branch" into the fleet protocol. |
| E4.4 | **Pipeline + worktree.** Rule 1 pipeline dispatch naturally requires worktrees — you cannot have two branches checked out in one working tree. Worktree mandatory is the mechanical prerequisite for pipeline to work safely. |

### 10.5 Spawn site rationale (v1.2 amendment)

**Rule:** Every `tokio::spawn` / `thread::spawn` / `std::thread::Builder::new().spawn(...)` site **MUST** carry a `// fire-and-forget: <reason>` comment at the call site OR explicitly store the `JoinHandle` for graceful join on shutdown. The choice between the two is design-conscious — fire-and-forget acknowledges the thread/task outlives the daemon shutdown path; stored-JoinHandle commits to a join order in the shutdown sequence.

**Evidence:** Sprint 20 Track B (`docs/codebase-review-2026-04-27/DAEMON.md` JoinHandle inventory) found 11 spawn sites in daemon scope, **only 1** with explicit rationale (`supervisor.rs` module-doc). Sprint 20.5 Track 7 B↔C cross-validation extended this to **13–15+ fleet-wide spawn sites with 0 graceful-join handling** — `app/telegram_hooks.rs:56,76` added two more unnamed sites in Track C scope, confirming the pattern is systemic, not daemon-internal.

This rule formalizes the v1.2 baseline so Sprint 22+ hardening (real graceful-join refactor) has an explicit pre-condition: every new spawn site explains itself, every refactor that adds a `JoinHandle` has a documented reason.

**Naming convention** (rule applies to thread name + comment):

```rust
// fire-and-forget: <reason>; shutdown semantics: <how this thread exits>
std::thread::Builder::new()
    .name("agent_pty_read".into())
    .spawn(move || pty_read_loop(...))
    .ok();
```

OR (stored JoinHandle):

```rust
// joined-on-shutdown: <where the join happens>
let handle = std::thread::Builder::new()
    .name("daemon_tick".into())
    .spawn(move || tick_loop(...))?;
// ... join during graceful shutdown:
let _ = handle.join();
```

**Exceptions:**
- **`#[cfg(test)]` modules** (E5.2): test-only spawns inside `#[cfg(test)] mod tests { ... }` are exempt — test fixtures often spawn ephemeral threads that the test framework reaps automatically.
- **Trait-method spawn that wraps a caller-provided closure** (E5.3): if the spawn is delegated by a trait method and the caller is responsible for the rationale (e.g. `Channel::send` internally spawns to send-and-forget per-channel), the trait-method site inherits the caller-site rationale. Document at the caller site, not at the trait-method site.

**Edge case policies:**

| ID | Policy |
|---|---|
| E5.1 | **Short-lived (<1s) spawns** can use a 1-line rationale. Full design rationale (pointing at architecture doc / module-level explanation) is only required when the thread/task lives for the full daemon process or longer than 1s. Example: `// fire-and-forget: short-lived (~300ms) cleanup of orphan binding; thread dies on completion` is acceptable for `app/telegram_hooks.rs:56`. |
| E5.2 | **Test-only spawns exempt.** `#[cfg(test)] mod tests { ... }` spawn sites do not require this comment. Production-quality `tracing::warn!` etc. are still recommended for tests that exercise real spawn paths but not enforced. |
| E5.3 | **Trait-method spawn inherits caller rationale.** When a trait method (e.g. `Channel::send`) spawns internally on behalf of a caller, the trait-method site carries a short pointer (`// see caller site for fire-and-forget rationale`) and the caller site carries the full rationale. This avoids stale duplication if multiple callers share a trait method. |
| E5.4 | **Spawn allowlist invariant test (Phase 5b enforce).** Sprint 21 Phase 5b (impl-2) introduces a `cargo test` invariant that `rg "thread::spawn|tokio::spawn"` against an allowlist of explicitly-rationalised sites — test fails if a new spawn site lands without `// fire-and-forget: ` or `// joined-on-shutdown: ` comment in the same file. Forward-reference to the test (when it lands): `tests/spawn_rationale_audit.rs`. |

**See also:** Sprint 20 SYNTHESIS.md JoinHandle inventory (counts + per-site doc status); Sprint 20.5 SYNTHESIS v2 fleet-wide extension; per-backend agent files (`.agend/<backend>.md`) for backend-specific spawn instructions; §10.4 Rule 4 (worktree mandatory) — sibling rule covering a different audit surface (worktree race vs spawn race).
