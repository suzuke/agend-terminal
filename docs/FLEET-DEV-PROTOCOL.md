[繁體中文](FLEET-DEV-PROTOCOL.zh-TW.md)

# Fleet Development Protocol v1.2.1 (Safety Errata)

**Status:** ACTIVE — all fleet agents must follow this protocol.

## Protocol Structure & Maintenance

This document has two layers:

- **Normative layer (§0–§13)** — the rules. Everything you need to *act correctly right now*. No sprint numbers, PR/issue references, dates, or incident retellings belong here.
- **Appendix A — Rationale & Incident Log** — the *why* and the *when*. The empirical incidents, activation histories, and motivations behind the rules, keyed by section ID. A rule distilled from an incident carries a `↳ 緣由 A-§X` pointer; follow it only when questioning or revising the rule.
- **Appendix B — Section Number Map** — archaeology of past renumberings.

**Consolidation ritual (maintenance meta-rule).** New rules accrete; old ones rarely get retired. To keep the normative layer legible:

- A new rule MUST land in the normative layer as an *imperative* (what to do), with any incident narrative going to Appendix A — never inline.
- Every ~10 new rules OR every protocol-touching sprint, do a consolidation pass: merge overlapping rules, retire superseded ones (move the trail to Appendix A), and confirm the normative layer still reads in one sitting.
- If a normative section can no longer be understood without its appendix entry, the rule wording is incomplete — fix the wording, don't lean on the appendix.

**Operational precedence.** Within this protocol:

1. A daemon hard gate and the live MCP input schema define what the running system can execute.
2. A specific, explicitly scoped exception overrides a general rule only when it names the rule being relaxed and records the authorization.
3. Normative `MUST` / `NEVER` rules override examples, rationale, historical notes, and soft conventions.
4. If an example conflicts with the live schema or another normative rule, stop and report the mismatch; do not guess, bypass, or combine the two recipes.

## §0. KISS Principle

Every PR must answer: **"What real problem does this solve?"** and **"Would deletion break anyone?"** An evidenced absence of a concrete failure mode is `KISS-VIOLATION — REJECTED`; use `UNVERIFIED` only when the concern is claimed but cannot be proven (§3.3).

## §1. Task Board (Single Source of Truth)

Use daemon `task` tool, NOT per-agent local task lists.

**Primary lifecycle**: `create` (`open`) → `claim` (`claimed`) → `in_progress` → `in_review` / `verified` → `done`. Use `blocked` for a declared dependency and `cancelled` only when the task is intentionally abandoned.

**Rules**:
- Orchestrator creates dispatched tasks; an agent handling a direct operator request may create and claim its own task
- `depends_on` must be set when dependency exists
- `task({action:"done", id:"<task-id>", result:"<evidence-backed outcome>"})` must include a non-empty `result`

## §2. Decisions Panel

Use `decision({action: "post", ...})` to freeze scope definitions or ground truth changes.
- `tags` must include the track plus the most specific available artifact ID (`task`, `issue`, or `PR`); add the PR tag once a PR exists
- `scope: fleet` for cross-track; `scope: project` for track-specific
- `supersedes` links corrections to original decision

## §3. Review Protocol

### 3.1 Pre-implementation
Orchestrator posts scope decision + creates task.

### 3.2 Review Dispatch Contract (3 parts)
1. **Source of truth** — design doc or decision ID
2. **Scope boundary** — audit X, ignore Y
3. **Freshness boundary** — stale if changed after {sha}

### 3.3 Verdict
`VERIFIED` / `REJECTED` / `UNVERIFIED` — **start the report with the verdict word** (the daemon's §3.3 evidence gate keys on the leading token).

Every review report must include: `scope_source`, `audit_mode`, `reviewed_head`, `commands`, `files`.

**Evidence block (#1666 Phase A — daemon-enforced).** A `VERIFIED` or `REJECTED` verdict MUST carry an `### Evidence` block proving the claim:
- `ran: <cmd> → <result>` — a command actually executed (e.g. `cargo test` / `clippy` / `gh pr checks <PR#>` / `grep`), with its outcome; and/or
- `cited: path:line — quote` — a source citation backing a finding.

`UNVERIFIED` is redefined as **"claimed but unproven"** — the evidence-exempt verdict. Use it when you assert a concern you could not run-or-cite (so the gate never forces fabricated evidence).

The daemon HARD-gates this at report time: a `VERIFIED`/`REJECTED` with **no recognizable evidence token** is rejected back to the reviewer. The canonical form is a structured `ran:` / `cited:` entry inside `### Evidence`; command tokens (`cargo`, `gh`, `clippy`, `grep`, `rg`) and a `path:line` cite are compatibility fallbacks. The gate is deliberately **lenient** — it checks evidence presence, not semantic sufficiency. Review depth remains lead/reviewer judgment under §3.21.

**Comments and prose are claims, not evidence.** Every factual assertion in a code comment, doc, or PR body is a claim to VERIFY against the code, never evidence in itself. Reachability / scope / "cannot happen" / "single chokepoint" claims must be proven from the actual guards and match arms in the source — author and reviewer alike. `↳ 緣由 A-§3.3`

### 3.3.1 CI Verification Gate (Sprint 61)
Before approving merge, orchestrator/reviewer MUST independently verify CI:

```
gh pr checks <PR#>
```

**Hard rules:**
- Exit code 0 (all checks pass) required before merge approval
- Do NOT rely on dev's self-reported CI status or ci_watch notifications alone
- Do NOT rely on partial check results (e.g., only LOC overrun passing)
- If any check is `pending`, wait and re-check
- If any check is `fail`, block merge and report to implementer

**Flake declarations require CI-log evidence.** Calling a CI failure a "flake" (to rerun instead of fix) MUST cite the real failing test name from the FAILING run's log:

```
gh run view <run-id> --log-failed
```

- The cited test must be a known-flake signature (timing/IO/ordering), not a deterministic failure wearing a flake label.
- NEVER extrapolate flakiness from a local or worktree `cargo`/`nextest` run. Local green ≠ CI green (platform, timing, parallelism, env differ); a local pass does NOT prove the CI failure was non-deterministic.
- No log evidence → treat the failure as REAL and fix it (a blanket "rerun + label flake" hides deterministic bugs).

`↳ 緣由 A-§3.3.1`

### 3.4 Re-review (r1+) Dispatch
Every re-review dispatch must enumerate all findings from the previous round with status: fixed / deferred / withdrawn. Missing or incomplete mapping → reviewer repeats the complete original scope (`full_review`) rather than reviewing only the claimed fixes.

### 3.5 Multi-reviewer
- Default: single primary reviewer
- Dual reviewer only when: high-risk shared behavior, repeated reject loop, primary requests, operator mandates
- **Adversarial review** maps to `review_class=dual` and additionally requires at least one reviewer to challenge authority boundaries, silent-failure paths, or runtime-state invariants rather than only reading the happy path
- Verdict severity: `REJECTED > UNVERIFIED > VERIFIED` — worst wins

**Merge-authority matrix:**
- Orchestrator merge: satisfy the task's `review_class` (`single` or `dual`) plus CI and verdict-mirror gates.
- Author/implementer self-merge: dual VERIFIED plus CI and verdict mirror.
- Operator docs-only self-merge: the explicit §3.6 exception; it does not relax CI.

### 3.6 LOW Docs-only Exception
All conditions must hold for single-reviewer or operator self-merge:
1. Only fleet-protocol / reviewer-contract docs, matching protocol regression tests, or the fleet-protocol template strings in `src/instructions.rs`
2. Diff ≤ 50 LOC
3. No runtime behavior change outside those template strings
4. No new or materially relaxed rule affecting normal/high-risk work

### 3.7 Cross-backend Claims
Must have per-backend test evidence, OR mark as `unverified cross-backend claim` + backlog task.

### 3.8 Cross-team Auth Chain
Cross-team reviewer borrowing or task delegation must cite operator auth chain (e.g. operator message ID). New agents must not assume cross-team access without explicit authorization.

### 3.9 External-fixture Validation
Three PR categories require external fixtures:
1. **Wire-format** — production capture / RFC fixture / cross-implementation reference
2. **Concurrent-state** — multi-threaded harness / loom / stress loop
3. **Persistence-replay** — write → restart → restore round-trip

Additional: wire-format invariant tests (pin shape); production-path-coupled (no helper mimics).

**Test through the REAL entry point (integration); don't inject input mid-pipeline.** A test that hand-feeds a helper's INPUT (e.g. passing `prs` straight to the classifier) skips — and therefore HIDES — the discovery/wiring path that produces that input in production. Drive the test from the real entry the production caller uses (the scanner / handler / dispatcher), so a discovery or wiring gap FAILS the test instead of being silently bypassed. Evidence: #1799 PR-3's unit test injected `prs` directly into the helper, hiding that discovery was seed-bound to pr-state; codex required an integration test through the real scanner to surface it. **Review checklist** — the reviewer MUST ask: *"does this test exercise the real entry point, or inject mid-pipeline?"* A mid-pipeline inject on a discovery/wiring-coupled path is an unverified-coverage gap → request a real-entry integration test.

### 3.10 Test-first
Feature/fix PRs must be test-first: failing test commit BEFORE impl commit.
- Every fix PR MUST include an empirical reproduction test case. Reviewers MUST verify the presence and validity of this test.
- Reviewer verifies RED and GREEN in daemon-managed named worktrees materialized from the test and implementation commits; never detach or checkout a SHA in-place (§3.19.1).
- Exemptions: docs-only, pure refactor, test-only, dep bump, EMERGENCY, pure deletion, empirical-revert

### 3.11 Deferred-defense
- (a) Known-issue recurs in production → auto-escalate to P0
- (b) Deferred backlog must have `due_at` (default: 2 sprints)
- (c) Same root cause deferred twice → mandatory dual reviewer + operator sign-off
- (d) Removing defensive code → 4-perspective counter-example challenge; 0 compelling = safe to delete

### 3.12 Verdict Externalization (was §3.5.13)
Fleet-internal verdict MUST mirror to the PR through the active SCM provider (`gh pr comment` on GitHub). Apply the §3.5 merge-authority matrix: author/implementer self-merge requires dual VERIFIED; orchestrator merge uses the task's review class; the §3.6 operator docs-only exception remains explicit. CI green + the required verdict mirror are always required.

**Canonical merge step: `repo({action:"merge", pr:<N>, repository:"<owner/repo>"})`** (the MCP `repo` tool → `handle_merge_repo`, `src/mcp/handlers/ci/mod.rs`). It issues the **byte-identical** merge a raw `gh` call would (`gh pr merge <N> --repo R --admin --squash --delete-branch`, pinned by `scm::tests::pr_merge_args_match_existing_gh_call`) but wraps it in three safety nets the raw command lacks:
1. **Safe repo-resolution (#1619)** — resolves the target `owner/repo` via `resolve_repo_or_error`; a detection miss ERRORS instead of silently merging against a hardcoded/maintainer repo.
2. **CI fail-closed gate** — runs `pr checks` (via `ScmProvider`) first; ANY non-`SUCCESS`/`SKIPPED` check OR an undeterminable result REFUSES the merge. Bypass only with `force=true` + a non-empty `force_reason` (audit-logged to `fleet_events.jsonl`).
3. **`verify_merge_landed` (#1467)** — `gh pr merge` exit-0 is necessary but NOT sufficient (merge-queue / eventual-consistency can exit-0 without landing); it re-`view`s the PR and reports `merged:false, pending:true` rather than a false success, so the caller re-queries instead of blindly re-merging.

It also routes through `ScmProvider` (platform-agnostic — not hardwired to `gh`).

⚠ **Scope of the gate:** `repo action=merge` gates **CI fail-closed**, NOT the review verdict. The dual-VERIFIED requirement above stays a **fleet convention** the orchestrator enforces (dispatch reviewer → await `VERIFIED` → THEN run `repo action=merge`) — this change does NOT make review a hard precondition of the merge primitive.

**Fallback (emergency / MCP unavailable):** raw `gh pr merge <N>` — the `--auto --squash --delete-branch` form (§3.12.1, server-side queue; needs strict branch protection) or the synchronous `--admin --squash --delete-branch` form (queue-contention recovery / admin-bypass, per #985/#988 deviation precedent). Prefer the MCP primitive; drop to raw `gh` only when the MCP path can't run.

#### 3.12.1 `gh pr merge --auto` adoption (Sprint 65, #973) — ACTIVE since 2026-05-20

> NOTE (t-protocol-merge-via-repo-action): §3.12 now makes **`repo action=merge`** the canonical merge step. This subsection governs the **raw-`gh` fallback** path — when you have to drop below the MCP primitive, `--auto` is the preferred raw form (it respects branch protection; the daemon's CI fail-closed gate is unavailable on this path).

When dropping to the raw-`gh` fallback, the preferred invocation is `gh pr merge <N> --auto --squash --delete-branch` (requires `gh` CLI >= 2.31.0). `--auto` moves merge submission to GitHub's server-side queue, eliminating the "Base branch was modified" race observed in #971 close-loop (2026-05-20).

**Prerequisites** (one-time per repo, enabled in #973):
- `allow_auto_merge: true` at repo level (`gh api repos/<owner>/<repo> -X PATCH -F allow_auto_merge=true`)
- Branch protection on `main` with `required_status_checks.strict=true` covering the full CI matrix (`Check (ubuntu-latest|macos-latest|windows-latest)`, `LOC overrun`, `audit`). Without `strict=true`, `--auto` invoked after all checks already reported merges IMMEDIATELY — silently skipping the §3.12 conjunction gate. With `strict=true`, GitHub re-checks against current main before merging.
- Admin-permission note: enabling these settings requires repo admin rights. Delegated maintainers should request via operator.

**Behavior**: `gh pr merge --auto` returns immediately (does NOT block on CI). Merge fires asynchronously when the protection-gate conjunction holds. Author MUST NOT poll manually; the `[pr-merged]` event (delivered by daemon PR-state aggregator #972 + gh-poll observation #986) is the close-loop confirmation source.

**Escape-hatch — stalled `--auto`**: if no `[pr-merged]` arrives within 30 min of CI green + verdict mirror posted, possible causes:
1. Required check never reports (CI infrastructure issue)
2. Branch protection mis-configuration (status-check context name drift)
3. Token / permission issue on the `--auto` arming agent

Recovery, in order:
- (a) Verify protection state: `gh api repos/<owner>/<repo>/branches/main/protection --jq '.required_status_checks'`
- (b) Verify PR is mergeable: `gh pr view <N> --json mergeable,statusCheckRollup`
- (c) Re-arm: `gh pr merge <N> --auto --squash --delete-branch` (idempotent — re-arming when already armed is a no-op)
- (d) Last resort — manual fallback: `gh pr merge <N> --squash --delete-branch` (synchronous; may hit base-modified race; retry 3s later if it does)
- Notify lead via `send({instance:"<lead>", request_kind:"update", message:"<case (a)/(b)/(c)/(d)>"})` if the escape hatch is invoked.

`↳ 緣由 / 活化史 A-§3.12.1`

### 3.13 Log-level Changes (was §3.5.14)
Must have inline rationale, otherwise `LEVEL-CHANGE-RATIONALE-ABSENT — UNVERIFIED`.

### 3.14 Observability PRs (was §3.5.15)
Must include e2e integration test exercising the production hook path.

### 3.15 Daemon-core Cushion Rule
PRs touching daemon core / channel / supervisor / state.rs must include stress test + lock-ordering analysis before dispatch. "不急 ship" principle — correctness over velocity for infrastructure changes.

### 3.16 Fleet / Max-Ceremony Discussion Discipline
This section applies when §3.21 selects FLEET or the high-risk override. In that path, a pre-impl source-code spike is mandatory. Lead's initial proposal MUST be challenged by dev's 5-10min source-code spike before Phase 2 dispatch. Spike outputs:
- Confirm or refute lead's initial site count
- Surface bonus emission sites lead missed
- Distinguish "near-bug" from "asserts-on-bug-signature" (issue body counts often conflate)
- Identify pre-existing helpers / deps that change scope estimate

**Three-party substantive consensus required**: reviewer must offer at least one design challenge AND dev must offer at least one impl concern before consensus is recorded. An evidenced triple ACK without substance is `RUBBER-STAMP — REJECTED`; use `UNVERIFIED` only when the concern cannot be proven.

**Issue body counts are estimates, not contracts.** When issue body says "N sites / N tests need updating," dev spike re-counts. Actual surface may be narrower OR wider than the initial estimate.

`↳ 緣由 A-§3.16`

### 3.17 Static-Review Limits + Runtime Validation Required
Static / structural review is INSUFFICIENT for the following surfaces:

- **CI workflow YAML** (cache layer interactions, runtime PATH/env)
- **Shell script** (variable interpolation, locale-dependent behavior)
- **Daemon refresh / lifecycle behavior** (in-memory state vs persisted state divergence)
- **Cross-platform binary semantics** (e.g., rustup-init `--version` exits 0 for any binary at the proxy path)

For these surfaces, a final `VERIFIED` verdict requires runtime evidence — typically the PR's own CI run on multiple platforms. The reviewer may begin static review immediately (§12.2), but must withhold final VERIFIED until the runtime evidence exists. Pure code-diff inspection does not suffice. Reviewer must explicitly note "runtime-validated via PR-CI run X" in their verdict report. If the PR's own CI doesn't exercise the affected path, request an empirical reproduction step.

**Generalizable invariant**: exit code 0 is not a strong identity contract for tool checks. Output shape is. `<tool> --version | grep -qE "^<tool> [0-9]"` is the correct content-validating idiom.

### 3.18 Reviewer Audit Conflict Resolution
When reviewer's claim contradicts dev's claim (e.g. reviewer "stale wording remains" vs dev "wording updated"), lead MUST do **independent verification** before accepting either side:
- `git show <SHA>:<file>` at the exact reviewed_head SHA
- `git diff <prev>..<reviewed_head>` for the disputed lines
- Run the relevant test or grep command independently

Lead replies to both with the empirical evidence. Reviewer/dev should self-correct rather than escalate to operator.

### 3.19 Reviewer Workspace Discipline
Reviewers MUST inspect PRs without mutating the canonical source repo. Read-only provider inspection needs no checkout; any full-tree inspection MUST use the reviewer's own daemon-bound worktree. Specifically:

- **Never `cd` into the canonical source repo** to inspect a PR. The canonical is the operator's working tree; reviewer activity must not leave detached HEAD or stale refs there.
- **Never create refs in canonical** (`git checkout -b tmp_pr_review`, `git checkout <sha>`, `git fetch origin pr/N/head:pr_head`, etc.). These leave `pr*_head` / `tmp*` / `review/*` branches behind that pollute `git branch --list` and confuse later operator commands.
- **Use `gh pr diff <N>` or `gh pr view <N> --json files`** to read PR contents without checkout. If a full tree inspection is needed, `repo({action:"checkout", repository_path:"<canonical>", branch:"<new-review-branch>", from_ref:"<full-PR-head>", expected_head:"<full-PR-head>", bind:true, task_id:"<task-id>", checkout_purpose:"disposable_review"})` provisions a daemon-managed named worktree with exact disposable-review provenance; `release_worktree({instance:"<self>"})` returns it without touching canonical.
- **If canonical state is observed dirty post-review** (detached HEAD, stale `tmp*` / `pr*_head` branches), pause the review and report an operational blocker. Do not turn unrelated workspace hygiene into a PR verdict. Operator cleanup uses a dry-run `repo({action:"cleanup_merged_branches", base:"main"})`, then applies selected candidate IDs with an audit reason.

Enforcement: L2 `agend-git` shim refuses `checkout -b` and `checkout <sha>` from agent callers when cwd=canonical (PR-B). L3 sweeper cleans the residue and auto-switches detached canonical HEAD back to main at daemon boot (PR-C).

### 3.19.1 Agent Git Anti-Patterns

§3.19 names what reviewers must not do. This section names two failure modes and the correct recovery path. Apply to every agent, not only reviewers. `↳ 緣由 A-§3.19.1`

**Anti-pattern 1 — `AGEND_GIT_BYPASS=1` to escape a shim deny.**

When the active git guard denies an agent action, the deny is a protocol signal, not a transient error. Re-running the same command with `AGEND_GIT_BYPASS=1` is forbidden.

- **WRONG**: shim denies → set `AGEND_GIT_BYPASS=1` → retry. The bypass succeeds at the git level but skips the protocol gate that the deny was enforcing; whatever the gate was protecting (canonical hygiene, lease invariants, reviewer workspace boundary) is now violated silently.
- **RIGHT**: abort the operation. Send `send({instance:"<lead>", request_kind:"query", message:"<denied command + reason>"})` and ask for the correct routing.

Reasoning:

- The legacy-compatible `AGEND_GIT_BYPASS=1` input exists for **daemon-internal helpers** (`canonical_hygiene`, `branch_sweep`, `conflict_notify`) that read worktree state from canonical-rooted paths and would otherwise self-deny. It is not an escape hatch for agents.
- The bypass typically surfaces hidden state on top of the original problem.
- "Ask, don't bypass" is the universal recovery: a deny means the daemon owns the routing answer, and asking is cheap.

**Anti-pattern 2 — `git checkout <sha>` to materialize a PR review.**

Even in the agent's own daemon-bound worktree, `git checkout <sha>` is the wrong primitive for PR review:

- Leaves detached-HEAD residue — the class of pollution that #852 (canonical hygiene) and #858 (shim deny matrix) exist to prevent.
- Conflicts with the daemon's branch lease on the worktree, producing later "branch already leased" errors that look unrelated.
- Bypasses §3.19's shim-enforced workspace boundary even when run from a non-canonical cwd, because the shim's lease/lifecycle invariants assume branch-rooted HEADs.

Right path, by inspection depth:

- **Full tree** (`cargo test` replay, runtime validation, multi-file inspection): `repo({action:"checkout", repository_path:"<canonical>", branch:"<new-review-branch>", from_ref:"<full-PR-head>", expected_head:"<full-PR-head>", bind:true, task_id:"<task-id>", checkout_purpose:"disposable_review"})`. The daemon requires a branch proven new locally and on `origin`, records the exact provisioned head in the initial signed binding, and permits guarded cleanup once the review task is terminal. `release_worktree({instance:"<self>"})` returns cleanly with no residue; dirty/diverged/ambiguous state is preserved fail-closed.
- **Read-only** (diff inspection, file listing): `gh pr diff <N>` or `gh pr view <N> --json files`. No working-tree mutation at all.

If `repo({action:"checkout", ...})` fails (lease already held, branch unknown, worktree quota exhausted) → **ask, don't bypass**. Send a `request_kind:"query"` message to lead with the failure mode; an authorized recovery may use `release_worktree({instance:"<target>", force:true, branch:"<branch>"})` or alternate provisioning. Falling back to `git checkout <sha>` after a `repo` failure recreates the exact class of pollution this section forbids.

**Relationship to §3.19.** §3.19 says *what reviewers must not do in canonical*. §3.19.1 says *what every agent must do when the protocol gate fires* — abort and ask, not bypass and retry.

### 3.19.2 Reviewer Base Workspace Branch Discipline

§3.19 covers the canonical source repo. This section covers the reviewer agent's OWN base workspace dir (e.g. `$AGEND_HOME/workspace/fixup-reviewer/`).

**Reviewers MUST NOT** do in-place `git checkout` of an impl branch into the agent's base workspace dir. The base workspace is daemon-bound to a specific branch (typically `main` or a long-lived review-housekeeping branch); checking out an impl branch in-place pollutes the base with stale-branch state that bleeds into future sessions.

Use one of:
- **(a) Dedicated review worktree**: resolve the subject PR's full head SHA, then provision a daemon-managed named worktree with `repo({action:"checkout", repository_path:"<canonical>", branch:"review/<N>-r0", from_ref:"<full-PR-head>", expected_head:"<full-PR-head>", bind:true, task_id:"<task-id>", checkout_purpose:"disposable_review"})`. The review branch must be new locally and remotely. Release it with `release_worktree({instance:"<self>"})` when done.
- **(b) GH-only review** (preferred for diff-only inspection): `gh pr diff <N>` + `gh pr view <N> --json files,reviews,statusCheckRollup`. No local checkout, no cleanup needed.

**NEVER** in-place `git checkout` of an impl branch in the agent's base workspace dir.

`↳ 緣由 A-§3.19.2`

**Relationship to §3.19.** §3.19 forbids checkout in CANONICAL. §3.19.2 forbids in-place checkout in the agent's BASE WORKSPACE. Both protect against stale-branch pollution at different boundaries.

### 3.19.3 Source-File Lookup — No Full-Disk Scan

Locating a source file (for example the active guard source under `vendor/agentic-git/`) with a full-disk `find / -name …` or `find ~ -name …` is **forbidden**. Run concurrently across the fleet, it spikes machine load fleet-wide (#2386: load 108 on a 16-core box).

Find source from a **fixed point**, not the filesystem root:
- Inside your bound worktree/repo: `git ls-files | rg <name>` or `rg --files | rg <name>` (index-scoped, fast).
- Need the repo root: `git rev-parse --show-toplevel` — never `find /` for a marker file.
- Path you don't know: read `binding_state({instance:"<self>"})` for your worktree path, or ask lead via `request_kind:"query"`. Never scan the whole disk.

Do not hardcode machine-specific absolute paths in shared artifacts (the protocol is cross-machine); resolve via `git`.

`↳ 緣由 A-§3.19.3`

### 3.20 Race-Condition PR Discipline

Race-class PRs ship with hidden timing dependencies that pass CI + reviewer VERIFIED yet break production. The lessons below apply to every spawn / async-coordination / multi-process-startup PR (the "race class"); same discipline framing as §3.19.1. `↳ 緣由 A-§3.20`

**SOP 1 — Pre-r0 race-condition question.**

Before dispatching r0 on a race-class PR, lead AND dev MUST answer in writing: *"Does this change have a race condition, and can I write a deterministic test that reproduces it without timing dependence?"* The answer goes in the spike report (or the dispatch message if no spike preceded).

Race class includes — but is not limited to — `tokio::spawn` / `thread::spawn` sites, multi-process startup ordering, `Drop`-vs-`enqueue` lifecycle, lock-ordering across modules, signal-handler-vs-main-loop coordination, daemon-vs-bridge handshake gates. If the answer is "no deterministic test possible," stop before implementation and record a waiver decision with the attempted deterministic designs, alternative empirical evidence, operator authorization, and mandatory SOP 2 smoke. Without that waiver, §3.10 and SOP 1 block merge.

**SOP 2 — Post-merge operator smoke sanity check (NOT a merge gate).**

Race-class PRs merge once SOP 1 (deterministic RED→GREEN tests) AND SOP 3 (reviewer RED-protocol) are both satisfied, unless the explicit no-deterministic-test waiver above replaces both with its recorded alternative evidence. SOP 2 is a **post-merge sanity check**, not a normal pre-merge gate; under a waiver it becomes mandatory immediately after merge.

**Post-merge smoke procedure**:

- Operator (or lead on operator's behalf) reproduces the race scenario on a **fresh, isolated `$AGEND_HOME`** — e.g. `/tmp/smoke` or `$TMPDIR/agend-smoke-$$`. **NEVER use the operator's daily `$AGEND_HOME`** (normally `~/.agend`, or the legacy fallback); smoke runs MUST be hermetic and disposable so a regression cannot leak into operator state.
- PR body MAY include a suggested smoke script enumerating the race scenario the fix targets (e.g. "start daemon cold + watch inbox for `bridge_connected` within 5s"). Optional, not required for merge approval.
- If post-merge smoke uncovers a regression: operator-driven revert (`git revert <merge-sha>`) — race regressions auto-escalate to P0 per §3.11(a) deferred-defense.

**Gating layers (the actual merge gates)**:

- **SOP 1** (deterministic RED→GREEN tests at unit/integration level) — the structural gate. Most race-class behaviour CAN be deterministically tested with proper mocking or DI; the bar is "is there a test that fails pre-fix and passes post-fix, on three back-to-back runs."
- **SOP 3** (reviewer RED-protocol execution on the test surface) — the audit gate. Reviewer must independently observe the RED→GREEN transition.
- SOP 2 post-merge smoke is supplementary empirical coverage, not a gate.

"No deterministic test possible" is rare — usually a deterministic design exists with `tokio::test` + paused time, channel-based synchronization, or trait-injected clocks. A waiver must define what SOP 3 executes instead (for example a bounded stress harness plus production-entry trace) and must never claim a RED→GREEN observation that did not occur.

**SOP 3 — Reviewer RED-protocol for race-class PRs.**

For race-class PRs, the reviewer MUST execute the RED→GREEN protocol (not skim it):

1. Materialize the pre-fix commit as a daemon-managed named worktree on a new branch (for example `review/<N>-red`) via `repo({action:"checkout", from_ref:"<full-pre-fix-SHA>", expected_head:"<full-pre-fix-SHA>", bind:true, task_id:"<task-id>", checkout_purpose:"disposable_review", ...})`; never checkout the SHA in-place.
2. Confirm RED: the new tests compile-fail, fail at runtime, or fail with the expected error signature.
3. Release the RED worktree, then inspect the fix in a separate daemon-managed named worktree/branch (for example `review/<N>-green`).
4. Confirm GREEN without flakiness on three back-to-back runs.

The verdict body MUST explicitly state both immutable SHAs, named worktrees, commands, the RED signature, and the GREEN 3/3 result.

Reviewers who skip the protocol on a race-class PR get `RUBBER-STAMP — REJECTED` with evidence per §3.3. The PR returns to dev for explicit reviewer RED-protocol execution before re-dispatch.

**Relationship to §3.19.1.** §3.19.1 says *what every agent must do when a protocol gate fires*. §3.20 says *what lead, dev, and reviewer must do BEFORE the gate could fire* on race-class PRs — a sanctioned discipline addition, not a replacement for any existing rule. Race-class triage at r0 dispatch is cheaper than the ship-then-revert cycle empirically observed on #881.

### 3.21 Proportional Ceremony — right-size process to task risk

Match fleet ceremony to where a task's risk actually lives. Decided by **lead judgment**, NOT a daemon classifier — a rubric nobody follows is compliance-theater (false confidence, blame-shift). #1656 shipped review-tiering as pure judgment and it caught real defects. Record each dispatch's ceremony call via `decision({action: "post", ...})` — the decision log IS the classifier (zero new code). `↳ 緣由 #1656/#1659/#1660 dialectic`

**Three INDEPENDENT axes — decide separately, never collapse into one "trivial/non-trivial" flag.** A task can be single-agent + light-review + spike-REQUIRED (e.g. #1658).

- **A. Fleet vs Single.** FLEET iff *"wrong = expensive"* AND *"only an adversary-who-tries-to-break-it catches the flaw — a test you could write would not"* (the #1654 authority bypass, #1635 evasion forms, #1629 sibling deadlocks). Else SINGLE — small / fail-safe-default / proven-pattern / author-verifiable by a test or empirical run (#1625, #1657's diff). Lead's 5-second question: *"If this is subtly wrong — how bad, and would my own test catch it or only an attacker?"*
- **B. Spike vs Skip.** Default = spike (its value is PREMISE-CHECK, not size-gating). Skip → straight to impl **only if ALL FIVE hold**: 1 single named fix site (no "investigate/where" verb) 2 structural not behavioral root cause — a fact you can SEE (dup/typo/missing-arm), not "because <runtime behavior>" 3 fix self-evident from the consumer you already read 4 no test/lint-enforced construct is MOVED (the #1642 silent de-enforcement trap) 5 no premise-inversion risk — any "already exists / doesn't exist" assumption verified by reading code FIRST (the #1658 trap: assumed gate absent, one existed). Mnemonic **Site · Structural · Self-evident · Stationary · Verified**. Skip is REVERSIBLE: if impl reveals the premise was shakier (site fans out, unrelated test breaks, grep surfaces more call sites), abort to spike immediately. The skip-checklist is applied by the same agent who'd write the impl → guard against confirmation-biased "yeah, obvious".
- **C. Review tier (#1656).** normal/single → dual → adversarial, by blast radius. See §3.5.

**High-risk OVERRIDE (the safety floor).** Regardless of apparent size, ANY of these forces MAX ceremony on all three axes (fleet + spike + adversarial review): authority/security surface · silent-failure mechanism (wrong key/glob, dead config field, approval/prompt routing) · empirically-unverifiable integration claim ("works on the installed version/schema/tool") · invariant / forcing-function change · blast-radius that depends on runtime state, not diff size. The cost of waving through ONE disguised-high-risk change (shipped authority bypass / silent deadlock) dwarfs the ceremony saved on many genuinely-low-risk ones.

**Two cross-cutting principles.**
- **Match ceremony TYPE to risk location.** When the risk is in the *diagnosis/RCA* (not the diff), the load-bearing check is **empirical verification** (treatment/control run), not more reviewers — #1657's risk was the schema key (caught by an A/B run), not the 1-line diff that dual-review scrutinized.
- **Asymmetric bias — when unsure on ANY axis, escalate.** False-positive ceremony costs minutes; false-negative ships an authority bypass / silent deadlock. Default: when in doubt, more ceremony.

`↳ 緣由: 2026-06-02 4-agent dialectic (dev/dev-2/codex/reviewer-2), /tmp/ceremony-spike-*.md. Resolves #1659 + #1660 as policy (no code).`

### 3.22 Spike-First Planning Gate

When §3.21-B selects a spike (premise-risk) **OR** the work carries an operator-decision fork (a choice only the operator/lead may settle), the spike and the impl MUST be **separate dispatches** — never one combined task.

- **Spike is ANALYSIS-only** (no production code). It delivers a **decision-manifest**: each premise check stated as confirmed-or-refuted with code evidence, and each operator-decision fork teed up with concrete options + a recommendation.
- **Impl is dispatched only AFTER the forks are resolved**, with `depends_on` containing the spike task ID. Decision IDs belong in the implementation task's scope source / description, not in the task-dependency list. Impl scope is derived from the manifest, not assumed up front.
- **No batch approval.** Do NOT pre-approve spike + impl as one unit: the impl's real scope is unknown until the spike resolves the premise and the forks, so approving impl in advance approves an unknown.

**Reinforcement-only** (lead judgment, like §3.21) — enforced at dispatch, NOT a daemon hard-gate. The mechanizable candidate is chokepoint=dispatch / signal="does this impl have a resolved decision-manifest?", but per KISS this stays a convention until a real gate is justified (restrict footguns, not capable ops).

`↳ 緣由 A-§3.22`

## §4. Daemon Enforcement Gates

### 4.1 Push-time Semantic Gate (Sprint 44)
Daemon validates dev's push claim matches actual diff. Recognized grammar:
- `"no other changes"` / `"byte-equal verified"` / `"scope follows dispatch spec X"` / `"only formatting"` / `"deps unchanged"`

Unknown grammar → hard reject. No pass-through.

### 4.2 Reviewer SHA-staleness Gate (Sprint 44)
Daemon compares `reviewed_head` against PR HEAD at verdict time. Mismatch → reject verdict. Reviewer must `git fetch` and re-review. Fail-closed (fetch failure = reject).

### 4.3 Hallucinated-fn Check (Sprint 44)
When push claim references a function name, daemon verifies existence via syn-lite + rg fallback. Not found → reject push.

### 4.4 Reserved Name Warning (Sprint 46)
Instance names with routing semantics (`general`, `lead`, `dev`, `reviewer`) emit warning on create. Not a hard reject.

### 4.5 Cross-team ACK Absorption Exception (Sprint 61, #612)
For one-shot Codex backends, same-team `update` and `report` messages are persisted to the inbox without waking the receiver when the receiver is not an orchestrator and the message is not a correlated response to a blocker the receiver already drained. Cross-team messages, messages to an orchestrator, and correlated blocker responses still inject into the PTY. ACK absorption suppresses an unnecessary wake; it never drops the message.

## §5. Async Pipeline

Impl pushes PR then immediately starts next task. Reviewer issues verdict then immediately takes next review. dev-lead maintains the pending list; the task's required `review_class` must be satisfied by VERIFIED verdicts and CI must be green (independently verified via `gh pr checks`) before an authorized merge.

**Key rules**:
- Impl push must include scope statement (follows spec / deviated because)
- Orchestrator pre-dispatch verification: cross-check dev's claim against actual artifact before forwarding to reviewer
- dev-lead may use a one-shot `schedule({action: "create", ...})` as a 30-minute check-in fallback; it must not become a repeated polling loop
- **Review class before branch dispatch**: create every PR-producing branch task with `task({action: "create", ..., review_class: "single" | "dual"})`. The existing task's `metadata.review_class` is authoritative; adding `review_class` only to a later `send` cannot repair an unspecified task and the daemon fails closed.
- **Post-dispatch verification (Sprint 62)**: dispatch with `send({instance: "<receiver>", request_kind: "task", task_id: "<task-id>", branch: "<branch>", message: "<brief>"})`. A successful task-dispatch response confirms enqueue/routing, but it does not expose a stable message-level receipt. `delivery_mode` is optional routing metadata: ordinary send or fallback paths may expose it, while the primary task-dispatch wrapper may omit it. Treat its presence as routing metadata and its absence as normal. No message ID is returned, and successful routing is not proof that the receiver read, understood, or acknowledged the task. If the receiver does not reply within ~5 min, combine liveness from `list_instances({instance: "<receiver>"})`, visible activity from `pane_snapshot({instance: "<receiver>"})`, branch state from `binding_state({instance: "<receiver>"})`, and the eventual report. None of these signals alone proves that the task was understood.
  - If the signals show no progress, diagnose the lease or dispatch path before re-dispatching. The receiver or its authorized team orchestrator may use `release_worktree({instance: "<receiver>"})`; forced release additionally requires the known branch: `release_worktree({instance: "<receiver>", force: true, branch: "<branch>"})`.
- **Pane-claim is not delivery**: agent writing a response in its own pane is NOT a `send`. Every reply / verdict / report must be triggered via the MCP `send` tool. Receivers do not see pane content. Verify via §6 channel discipline.
- **Post-PR-merge close-loop reporting**: each PR `request_kind: "report"` MUST include a "lessons learned" section noting process wins, scope shifts, unexpected discoveries. Captures process-maturity signals for protocol evolution.
- Takeover requires 4 criteria independently verified (heartbeat stale ≥1h, last_input frozen, idle state, zero activity)
- Worktree release and branch deletion are separate state transitions: a clean, pushed/handed-off worktree may be released before merge so the agent can take another task; the daemon preserves the unmerged branch and cleanup intent. Delete the branch only after §10's preservation proof. A pending/queued merge is not deletion proof.
- Post-merge: orchestrator verifies main CI green before reporting task completion upstream. Failed main CI = immediate P0 (revert or hotfix).
- Orchestrator owns `ci({action: "watch", repository: "<owner/repo>", branch: "<branch>", task_id: "<task-id>"})` for own-orchestrated branches
- Stuck-agent timeout: see §9 timeout staircase

## §6. Communication

Use `send` for all inter-agent messaging:

| `request_kind` | Use | Expects reply? |
|---|---|---|
| `task` | delegation | yes |
| `report` | result/verdict | depends |
| `update` | FYI | no |
| `query` | question | yes |

**Routing**: `instance` (single) or `instances` / `team` / `tags` (broadcast)

**Dispatch milestone updates** — for PR-producing implementation work, send `request_kind: "update"` to the dispatcher at each of these milestones without being asked:

1. **r0 ready** — PR opened (or work artifact handed off), with verbatim links / heads.
2. **CI all-green** — every CI gate the PR runs has reported success. The `[ci-pass]` watch broadcast does NOT substitute — confirm via your own update so the dispatcher's loop closer fires regardless of their channel state.
3. **Reviewer verdict received** — VERIFIED / REJECTED / UNVERIFIED, with the reviewer's identity and key finding summary.

Re-review cycles (r1, r2, …) repeat the same three milestones. The dispatcher relies on these as the loop closer; missing any forces them to poll, which is anti-pattern (see §7).

For analysis, spike, review, or operational tasks that do not produce a PR, report the requested artifact/result and mark the PR-specific milestones not applicable; do not invent a PR lifecycle.

- Pure ack → do not reply (ACK absorption §4 handles this automatically)
- Response channel must match source channel
- **Response-channel discipline**: `[user:NAME via telegram]` → `reply`; `[from:AGENT_NAME]` → `send`; no prefix (operator typed directly in the TUI) → direct text. Do not assume direct text is universally mirrored.
- **Inbox vs PTY delivery (Sprint 62)**: messages are durably enqueued; eligible messages may also inject into the active PTY. An empty pending-inbox drain is not delivery proof because the message may already have been drained or injected. Use the dispatch result, later task/report state, `list_instances({instance: "<receiver>"})`, and `pane_snapshot({instance: "<receiver>"})` as complementary signals; see §4.5 for absorption exceptions.
- **Daemon auto-inject marker `[AGEND-AUTO]` (#1769)**: the daemon resumes a stuck agent by injecting a keystroke (e.g. `continue`) straight into the PTY, which otherwise looks identical to the operator typing it — a bare injected `continue` was once mistaken by an orchestrator for an operator command and a task dispatched from it. Such nudges now carry an `[AGEND-AUTO kind=...]` prefix (sibling of `[AGEND-MSG]`). **Rule:** treat an `[AGEND-AUTO]` line as a low-priority RESUME signal — continue in-progress work — and **never** as an operator command or a basis to dispatch a task / make a decision. Inbox/operator-relay messages keep their own `[AGEND-MSG]`/`[from:]` headers and are unaffected.

## §7. CI

Use `ci({action: "watch", repository: "<owner/repo>", branch: "<branch>", task_id: "<task-id>"})` for ongoing monitoring, not manual polling. Exception: merge-gate final verification requires one-shot `gh pr checks <PR#>` per §3.3.1. A clean, pushed/handed-off worktree may be released earlier; branch deletion still requires §10 preservation proof.

**No manual orchestrator polling**. Orchestrators (lead, general,
operator-in-the-loop) MUST NOT manually poll PR / CI state via
`gh pr view`, `gh run list`, repeated `cargo test`, or equivalent.
Rely on:

1. The dispatchee's `request_kind: "update"` milestones (§6) — r0 ready, CI
   all-green, reviewer verdict.
2. `ci({action: "watch", ...})` fan-out — `[ci-pass]` / `[ci-fail]` /
   `[ci-watch-stalled]` arrive automatically.

Repeated polling loops mask broken dispatch communication and burn cache /
rate-limit budget unnecessarily. If a milestone is missing past a
reasonable window, the correct response is to message the dispatchee
asking why, not to start a polling loop. Explicit one-shot checks required
by a merge gate or exact-head post-merge verification are allowed. Polling is also a smell that the dispatch
brief itself didn't enumerate the expected milestones — fix the
dispatch, not the symptom.

**PR open semantics (Sprint 54)**. Implementers MUST open feature PRs as
**ready** for review by default. The `--draft` flag is reserved for
exactly three scenarios:

1. **Smoke / verification PRs** that will not be merged (e.g. CI
   notification path tests). Title prefixes `[smoke]` / `chore: smoke`.
2. **Explicit work-in-progress** where the implementer needs to push
   midway and is not yet asking for review. Move to ready before
   pinging lead/reviewer.
3. **External-PR patches** where lead is augmenting a community
   contribution before the upstream PR is merged.

A draft PR is hidden from GitHub's default UI filters, so operator and
reviewer miss it without explicit checks. Default-ready keeps the
review pipeline visible.

**Setup-warning surfacing (Sprint 54 P0-4)**. CI-related MCP responses
may include a top-level `setup_warning` string when no GitHub token is
reachable (env unset AND `gh` unavailable/unauthed). The daemon polls
unauthenticated in that state and exhausts the 60 req/hr cap quickly.
Agents MUST surface `setup_warning` verbatim to the user the first time
it appears in a session — it is operator-actionable guidance, not a
log line. Suggested phrasing: "CI watch responded: <setup_warning>".
Subsequent occurrences within the same session may be deduplicated.

**Health surface (Sprint 54 P0-5)**. The `ci({action: "watch", ...})` response
and the `ci({action: "status"})` aggregator both carry `rate_limit_active`,
`rate_limit_until`, and `next_poll_eta` so agents can tell whether CI
polling is healthy without reading watch files. The daemon also
fans out two inbox event kinds when polling stalls behind a rate-limit
window: `ci-watch-stalled` after 3 consecutive missed polls (exactly
once per stall window) and `ci-watch-resumed` on the first successful
poll afterward. Both events go to every subscriber via the P0-1 fan-out
contract — no last-write-wins. Surface stalled events promptly; resumed
events confirm recovery and may be acknowledged silently.

### 7.1 CI Tool Identity & Cache Hygiene (Sprint 62)

**Tool identity check via output shape, not exit code.** When a CI step verifies a binary's identity (e.g. `cargo`, `rustc`, `rustfmt`):

```yaml
# WRONG — rustup-init binary at proxy path also exits 0 for --version
cargo --version

# RIGHT — content-validating grep ensures shape matches
cargo --version | grep -qE "^cargo [0-9]"
```

Stale `rustup-init` binaries can masquerade as `cargo` / `rustc` / `rustfmt` when cache restores them to the proxy path. Exit-0 alone does NOT prove identity.

`↳ 緣由 A-§7.1`

**Cache pollution requires prevention OR validated cleanup.** Detection alone is insufficient if recovery surface is harder than prevention. KISS: prefer "don't cache the polluted directory" (`Swatinem/rust-cache@v2 with cache-bin: false`) over "detect stale state and rm + reset". Recovery code itself becomes maintenance burden + new failure surface.

### 7.2 Cross-Platform Test Idioms

Cross-platform test failures observed multiple times in 2026-05-13/14 sessions. Mandatory idioms:

- **Time arithmetic**: never use unchecked `Instant::now() - Duration` or `Instant + Duration` with untrusted durations; either can panic when the result is outside the representable range. Use `checked_sub` / `checked_add`, or inject `now: Instant` for tests.
- **Regex hot-path**: never per-call `Regex::new` in a hot loop. Use `LazyLock<Vec<Regex>>` (or `OnceLock`). Performance ratio: ~100× speedup, prevents Windows runner test timeout from cumulative `min_hold` budget.
- **PTY EOF semantics**: never assume EOF behavior matches across cmd.exe/bash/ConPTY. Shell-backend tests need `#[cfg_attr(windows, ignore = "tracking #N")]` if EOF semantic divergence is the bug not the SUT.
- **Path mangling**: sanitize both `/` (Unix path) AND `\` + `:` (Windows drive letter) when constructing worktree paths from source paths.

### 7.3 Wedged-Run Recovery

When a CI workflow run **wedges** — a job stays `in_progress` past 2× typical platform completion time and `gh run cancel <run-id>` returns success but the job status doesn't transition — push an **empty commit** to the PR branch to trigger a fresh workflow run. This is a sanctioned recovery technique, not a workaround.

```
repo({action: "checkout", repository_path: "<canonical>", branch: "<PR-branch>", bind: true, task_id: "<task-id>"})
cd <bound-worktree>
git commit --allow-empty -m "ci: nudge wedged runner (PR #N wedged Nhr)"
git push origin <PR-branch>
```

The fresh CI run fires on the new HEAD; the old wedged run becomes irrelevant (it eventually GH-Actions-times-out at 6 hours without affecting merge). The prior verdict is stale because HEAD changed. Before re-stamping, prove content identity with equal tree OIDs (`git rev-parse <old-head>^{tree}` and `git rev-parse <new-head>^{tree}`) or an empty `git diff <old-head>..<new-head>`; then send a new verdict for the new `reviewed_head`. Merge gates on the fresh CI result.

**When to apply** — all three conditions must hold:

- CI job in_progress for >2× typical platform completion time (e.g. macOS jobs typically finish ~10–15 min; >30 min is wedge territory).
- `gh run cancel <run-id>` reports success but the wedged job's status doesn't change within ~2 minutes.
- Other platforms' jobs already completed (proves the issue is platform-specific, not a workflow / coordinator regression).

**What this is NOT**:

- **NOT force-push.** The empty commit advances HEAD via fast-forward; branch history is preserved. Squash-merge folds the nudge commit + the real work into the single PR commit on main, so the nudge leaves no trace in the merged history.
- **NOT a workaround for legitimate test failures.** A failing test means a real bug. The nudge only addresses runners that genuinely wedged with no progress — same configuration that was working minutes ago would re-pass on a fresh runner.
- **NOT for "tests are slow".** Slow-but-progressing CI is a different problem (cache miss, fixture cost). Wait for normal completion; if the slowness is systemic, file a separate issue.

`↳ 緣由 A-§7.3`

This recovery technique parallels [§3.19.1](#3191-agent-git-anti-patterns)'s framing for protocol-gate recovery: a deny / wedge is a signal, not a transient error. Document the sanctioned response so future operators don't reach for force-push or `gh run rerun --failed` (which re-runs the same wedged platform on the same SHA, often re-wedging on the same runner-pool resource).

## §8. Progress Visibility

Task state changes emit to Telegram. Instance lifecycle events (non-fleet.yaml origin) broadcast with `origin` field. `create_instance` defaults to isolated workspace (`$AGEND_HOME/workspace/<name>`).

## §9. Waiting & Timeout

- `set_waiting_on` to declare blockers (auto-clears after 120s inactivity)
- Use `schedule({action: "create", ...})` for check-ins (cross-backend)

**Timeout staircase** (single source of truth):

| Elapsed since dispatch | Action |
|---|---|
| < 20 min | Normal. `list_instances({instance: "<agent>"})` — fresh heartbeat means the process is active, not that the task is complete. |
| 20 min, heartbeat fresh | Agent working. Extend wait. |
| 20 min, heartbeat stale (>120s) | Ping via `send` with direct question. |
| 25 min, no response after the ping | Inspect task, pane, binding, and dirty state. A fresh restart of the same agent is allowed only after durable handoff state is current and no uncommitted work would be lost. |
| ≥ 1 h | Reassignment/takeover requires all four independent criteria: stale heartbeat, frozen last input, idle/error state, and zero task activity. |

**Backend modifiers**:
- kiro-cli: 1-2h longer wait (context compaction self-heals); escalate to operator rather than `interrupt`
- Other backends (claude/codex/opencode/agy/grok): use staircase above as-is

### Supervisor Notify
Daemon detects agent entering error state (UsageLimit/RateLimit/Hang/Crashed/AuthError/PermissionPrompt) → notifies orchestrator. 60s debounce per agent.

### 9.1 Context-Full Self-Restart

When an agent (especially a lead/orchestrator) detects its own context approaching full (~80-85%; the pane footer shows `N% context used`), it restarts **itself** — the daemon performs the kill+respawn; no second agent is required to trigger it.

- **`mode="fresh"`, never `resume`** — `resume` reloads the prior context. Only `restart_instance({instance: "<self>", mode: "fresh", reason: "context-full self-restart"})` starts clean.
- **Procedure**:
  1. Land all live state on durable stores **before** restarting — update `SESSION-HANDOFF.md` to current (handoff entry point, in-flight PRs, merge procedure, member status, pending dispatches, decisions), post any open `decision`s, ensure work is on the `task` board. Nothing may depend on in-memory context.
  2. Ensure the bound worktree is clean or the in-progress changes are committed. The daemon may refuse a dirty-worktree restart; do not force it without operator authorization.
  3. Pick a lull — never mid-merge or mid-step of an irreversible action.
  4. Call the fresh restart. The daemon emits one `[AGEND-RESUME]` bootstrap trigger after respawn; do not create a redundant scheduled kick.
  5. On `[AGEND-RESUME]`, rebuild state from the authoritative task board and `list_instances`, drain the inbox, then use `SESSION-HANDOFF.md` as a stale-tolerant hint and continue pending work.
- The restart call may not return because the calling process is replaced. A peer may perform a liveness check, but is not required to trigger the restart.

## §10. Git Workflow

- Never commit directly to main; always use worktree + branch
- Use a descriptive conventional prefix such as `feat/`, `fix/`, `docs/`, `refactor/`, `test/`, `review/`, or `chore/`
- Release a clean, pushed/handed-off worktree when changing tasks; delete its branch only after preservation proof below
- **Worktree lifecycle is daemon-owned.** For dispatched work, `send({instance: "<assignee>", request_kind: "task", task_id: "<task-id>", branch: "<branch>", message: "<brief>"})` binds the assignee—not the dispatcher—to a daemon-managed worktree. Find it with `binding_state({instance: "<self>"})`, `cd` into it, and use normal git there. Do not run raw `git worktree add`, do not switch the canonical repo, and do not use a bypass to escape a shim deny.
- **Provision/re-bind**: prefer `repo({action: "checkout", repository_path: "<canonical>", branch: "<branch>", from_ref: "<base>", bind: true, task_id: "<task-id>"})` for a fresh task. Use `bind_self({repository_path: "<canonical>", branch: "<branch>", task_id: "<task-id>"})` only to re-bind a recovered worktree, resolve the source repo from fleet metadata, or reclaim the same branch after release. Protected branches are rejected and cross-agent conflicts must be resolved through the owner/lead. Pair the binding with `release_worktree({instance: "<self>"})`.
- Normal bound pushes participate in daemon lifecycle/CI integration. If an operator-authorized exceptional push bypasses that integration, explicitly arm `ci({action: "watch", repository: "<owner/repo>", branch: "<branch>", task_id: "<task-id>"})`; see §13.

### release_worktree branch-cleanup scope

Releasing a daemon-managed worktree and deleting its local branch are distinct:

1. A clean worktree whose commits are pushed or durably handed off may be released before merge. The daemon retains an unmerged branch and records cleanup intent.
2. Local branch deletion requires one of: the branch is an ancestor of main; the provider proves that a PR for the matching head was merged; or structural squash proof passes together with the 24-hour age floor.
3. A missing remote tracking ref alone is never deletion proof: local-only commits may still need preservation.
4. Protected refs (`main`/`master`) are never touched.

Automatic lifecycle cleanup applies only to daemon-managed worktrees with a verified `.agend-managed` marker. User/operator-created worktrees and any unverifiable marker are preserved.

### release_worktree parameter form

Use `release_worktree({instance: "<self>"})`. Forced recovery additionally requires the known branch: `release_worktree({instance: "<self>", force: true, branch: "<branch>"})`. A missing required `instance` hard-rejects; extra unknown keys may warn and be ignored, so never treat them as cleanup. Verify success with `binding_state({instance: "<self>"})` returning `bound: false`.

### 10.6 Dispatch Binding Ownership

A task send with `branch` auto-binds the **assignee**. It does not bind or move the dispatcher. Therefore the dispatcher must not release its own worktree as generic pre-dispatch hygiene.

If the assignee is already bound to a different branch, resolve that binding before dispatch: ask the assignee to commit/hand off and call `release_worktree({instance: "<assignee>"})`, or have an authorized orchestrator use the forced form with the exact branch. Never release another agent's worktree speculatively; an active dirty binding may contain unreported work.

### 10.7 Empty `init` Commits on a Worktree Branch

Backend CLIs, including Claude Code, Codex, and Kiro CLI, may create empty
`init` session-checkpoint commits in a bound worktree. A scratch-test leak was
also a historical source, but repository tests now guard mutating scratch-repo
git commands; a `t <t@t>` committer is not a timeless or exclusive RCA. Do not
infer the producer from the subject or committer alone.

The live git interception and pre-push cleanup now live in vendored
`agentic-git` (`cleanup_init_pile_pre_push`). The in-tree `agend-git` binary is
kill-family-only and no longer handles git, so its old line references and
behavior are not an operational source of truth. On a normal guarded push,
eligible `init` / `initial` commits are removed only after the guard proves the
subject, body, and file diff are safe. The daemon also exposes
`repo({action:"cleanup_init_commits", instance:"<agent>"})` for an explicit
cleanup request.

**Agent guidance:** never hand-clean these commits with reset, rebase, amend, or
force-push. Push normally and let the guard perform its bounded cleanup. If an
unexpected commit is non-empty, carries a meaningful body, or survives the normal
cleanup, preserve it and report the exact branch/SHA to the lead; do not classify
it as harmless from the word `init` alone. Reviewers still verify immutable RED
and GREEN refs in daemon-managed worktrees (§3.10/§3.20).

### 10.8 Backend TUI Render Duplication (#1464)

A backend pane occasionally shows the **same line rendered twice** in a row,
even though the source content had it once. This is a **backend-renderer
artifact, not an agend bug** — and the same root-cause class as the #1401
residual-text investigation: the inner backend's TUI does a partial redraw /
reflow that re-emits (or fails to clear) a line.

agend's own layers are proven faithful and are **not** where this originates:
- `VTerm::process` is **pure alacritty** (`processor.advance`) — zero custom
  grid/scroll/line manipulation; the grid reflects exactly the bytes the
  backend emits.
- `render_to_buffer` is a **monotonic 1:1** grid→buffer copy (each viewport row
  maps to exactly one grid line) — it cannot duplicate a line.

It is **cosmetic** and self-heals on the next full repaint (a resize or any
event that triggers `terminal.clear()`). **Do NOT hunt for a fix in agend's
`vterm` / `render` layers — both are clean.** If mitigation is ever pursued it
belongs at the inject-timing / forced-redraw layer, not the render path.

### 10.9 GitHub CLI Authorship Signature (soft convention)

All fleet instances share the operator's GitHub account, so `gh`-authored issues, PRs, and comments carry no instance/model attribution — unlike git commits, which get auto-stamped `Agend-Agent` trailers via the `prepare-commit-msg` hook (§10.7). There is **no** `gh` shim; this is a soft convention, not enforced interception.

When you author a body-bearing `gh` action (`gh issue create`, `gh pr create`, `gh pr comment`, `gh pr review`), append a one-line signature to the body when attribution matters — multi-agent threads, cross-team handoffs, anything an operator may later need to trace to a specific instance:

```
---
*<instance-name>* · <backend/model>
```

Use `$AGEND_INSTANCE_NAME` for the instance and your resolved backend/model. Skip it for trivial passthrough comments where authorship is obvious. Soft requirement — omission is not a gate failure.

↳ 緣由 A-§10.9

## §11. Tool Quick Reference

| Need | Use | NOT this |
|---|---|---|
| Track work | `task({action: "create" / "claim" / "update" / "done", ...})` | local task lists |
| Record decisions | `decision({action: "post", ...})` | Markdown-only decisions |
| Assign work | create task, then `send({request_kind: "task", task_id: "...", ...})` | only one |
| Report results | `send({request_kind: "report", parent_id: "...", correlation_id: "...", ...})` | pane text |
| CI monitoring | `ci({action: "watch", repository: "...", branch: "...", task_id: "..."})` | manual polling loops |
| CI merge gate | `gh pr checks <PR#>` | trusting dev self-report |
| Wait state | `set_waiting_on({condition: "..."})` | prose |
| Instance health | `list_instances({instance: "..."})` | guessing |
| Clear blocked health | `health({action: "clear", instance: "..."})` | stale local notes |
| Schedule | `schedule({action: "create", ...})` | backend-specific tools |
| Timeout | §9 staircase, then `restart_instance({instance: "...", mode: "fresh", ...})` | immediate destructive restart |

**Daemon-unreachable behavior.** The agent-facing MCP bridge is a daemon proxy;
do not plan any tool workflow around a local/offline fallback. When the daemon
connection is unavailable, a tool call returns an actionable connection error
instead of proving that a mutation or delivery occurred. Surface that error,
restore the daemon/socket, then retry the original operation with the same
correlation identifiers. Internal handler fallbacks used by tests or recovery
code are not an agent-facing availability contract.

### 11.1 State Persistence Across Daemon Refresh (Sprint 62)

Daemon binary refresh (recompile + restart, or hot-reload via `mcp_registry_watcher`) invalidates several in-memory state stores. **After every `mcp_registry_watcher` notification fired**, the following state should be re-verified:

- **CI watch state** — fixed by #786, but pre-#786 watches may be missing
- **Instance registry vs team metadata sync** — fixed by #785 (better-error surfaces desync); team membership outlives instance restart, may reference wiped instances
- **Source_repo on team** — historically wiped by `teams.json` migration on refresh (was #781 root cause); persisted as of #781 but verify with `grep source_repo fleet.yaml` if behavior unexpected
- **Active bindings** — in-memory `bind_in_flight` flag may be lost; check `binding_state({instance: "<agent>"})`. If a binding is proven dangling, use normal `release_worktree({instance: "<agent>"})`; guarded force recovery requires `force: true` plus the exact `branch`.

**Operator workflow**: `mcp_registry_watcher` notification = restart-needed signal. Run `agend-terminal stop && cargo build --release && agend-terminal start` to pick up new binary. Subsequent agent dispatches benefit from fresh code.

**Agent workflow**: do NOT assume state survives daemon refresh. Re-verify via `team({action: "list"})`, `ci({action: "status"})`, and `binding_state({instance: "<self>"})` after any refresh notification.

## §12. Workflow Efficiency

### 12.1 Pipeline Dispatch
Push PR then immediately start next task. Depth ≤ 2. Must branch from main (no stacking on pending PR).

### 12.2 Reviewer Does Not Wait for CI
Start review on PR push. `reviewed_head` is a snapshot; subsequent commits reset verdict.

### 12.3 Task Close
`in_progress` → `verified` (reviewer) → merge (CI green per §3.3.1) → post-merge main CI green → `done`.

**Post-merge verification**: After squash-merge, capture the immutable merge SHA and have the target team orchestrator/operator register an exact-head protected-branch watch:

```
ci({action: "watch", repository: "<owner/repo>", branch: "main", head_sha: "<full-merge-sha>", task_id: "<task-id>", next_after_ci: "<orchestrator>"})
```

Only the matching exact-head success closes the task; a newer unrelated main run is not evidence for this merge. Protected exact-head watches are currently GitHub-only; on another provider, obtain provider-native evidence pinned to the merge SHA or report the close gate UNVERIFIED. If that exact SHA fails, immediately investigate and fix (revert if necessary).

### 12.4 Worktree Mandatory
Impl/reviewer must work in a worktree, never the canonical working tree. An ordinary branch-carrying task dispatch with binding enabled (a non-empty `branch`, without `bind:false`) auto-binds the assignee; branchless and `bind:false` dispatches do not. Confirm with `binding_state({instance: "<self>"})`, `cd` into the reported worktree, and use normal git. Do not self-provision with raw git; provision deliberately through `repo({action: "checkout", repository_path: "<canonical>", branch: "<branch>", from_ref: "<base>", bind: true, task_id: "<task-id>"})` or the recovery-oriented `bind_self` form in §10. Agents never turn a shim deny into permission to bypass. Full rule + exceptions: §12.4 and §13.

### 12.5 Spawn Site Rationale
Every spawn must have `// fire-and-forget: <reason>` OR store JoinHandle. Test-only exempt.

### 12.6 Multi-PR Wave Sequential Merge
When multiple PRs ship in the same wave (same dispatch/task_id):
1. Merge sequentially: A → rebase B on new main → re-verify CI → merge B → ...
2. Never parallel merge — later PRs have stale base
3. After each merge, remaining PRs must rebase and re-run CI before merge. The rebase changes `reviewed_head`, so the prior verdict is stale: re-review the new head, or, if the tree is byte-identical, publish a new re-stamp citing equal tree OIDs/empty diff.

This constraint is communicated in the dispatch message text (there is no daemon-enforced param — the removed `send.sequencing` passthrough had no consumer). Recipients MUST merge one at a time and verify CI between each merge.

### 12.7 Linked-Issue Close Convention

A PR that resolves a tracked issue MUST carry a closing keyword (`Closes #N` / `Fixes #N` / `Resolves #N`) in its **PR body**, so the platform auto-closes the issue on merge-to-default-branch.

- A bare `#N` reference does NOT auto-close, and is ambiguous — a mention/cross-reference is not a fix (e.g. a cluster-sibling issue still open). Use the keyword only when the PR actually resolves the issue.
- **No daemon auto-close.** A daemon-side `Closes #N` parser would be redundant with native platform behavior, inert until this convention is adopted, and `gh issue close` is GitHub-only (conflicts with the multi-platform `ScmProvider` direction).
- **Reinforcement-only.** This is a convention, not a gate; lead manually closes any straggler at merge time.

`↳ 緣由 A-§12.7`

## §13. `AGEND_GIT_BYPASS=1` Usage

**TL;DR:** agents use normal git inside their daemon-managed worktree and never bypass a shim denial. Bypass is reserved for daemon internals and explicitly operator-authorized repair/bootstrapping exceptions.

### 13.1 When you should NOT use bypass

Inside your bound worktree, all routine git ops pass through the shim cleanly. Run them bare:

```bash
git status / diff / log / show
git add / commit / fetch
git push origin <your-branch>     # any branch except main
```

Do not preemptively prefix `AGEND_GIT_BYPASS=1`. If the shim denies an action, stop and follow the daemon-managed remediation or ask the lead/operator; the denial is not permission to retry beneath the guard.

### 13.2 Authorized bypass scopes

The allowed scopes are narrow:

- Daemon-internal git helpers set bypass to avoid recursion through their own shim.
- An operator may authorize a one-command repair after daemon-managed release/recovery routes have been exhausted. Record the command, reason, affected repo/branch, and result.
- §13.5 permits a fix for a bug that blocks its own normal delivery, but only with explicit operator authorization and the PR disclosure defined there.
- A repository-owned test wrapper may set bypass internally for a tool whose own nested git probes would otherwise recurse (for example a configured `nextest` wrapper). Agents do not add the prefix ad hoc.

Raw worktree lifecycle, switching protected branches, and pushing to main are not agent bypass scopes. Use `repo`, `bind_self`, `release_worktree`, and the PR/merge workflow.

### 13.3 Why bypass is costly

Skipping the shim skips the safety net:

- **Phase 1 trailer skipped** — commit lacks `Agend-Agent: <name>` provenance, breaks audit trail
- **Deny matrix skipped** — risky ops (force-push to protected refs, etc.) run unguarded
- **Git registry can drift** — `git worktree add` outside the daemon's pool leaves untracked entries; subsequent leases may collide
- **Phase 5 hotspot warning skipped** — concurrent edits to flagged files don't surface on the dispatch path

Any one of these can invalidate review or strand operator state; treat bypass as an audited exception, not convenience.

### 13.4 Default workflow

1. Run bare `git <command>`.
2. If the shim denies, read the deny message — it names the specific reason and suggests a remediation.
3. Follow a daemon-managed remediation (`repo`, `bind_self`, `release_worktree`) when offered.
4. If the only proposed remediation is bypass, an agent pauses and requests lead/operator direction. Only the operator or an explicitly authorized procedure may approve the exact one-command scope.

`AGEND_GIT_BYPASS_UNTIL=<epoch>` is for audited, time-bounded operator interventions; it is not an agent convenience flag.

### 13.5 Bug-Blocks-Its-Own-Fix Exception (Sprint 62)

When fixing a daemon binding bug (or another bug whose existence prevents the bypass-free workflow itself), the fix PR may use a one-command bypass only after normal daemon-managed recovery is shown unable to deliver the fix and the operator authorizes the exact scope.

**Acceptance criteria for this exception**:
1. PR body MUST include a `## Bypass scope rationale` section explicitly framing the loop:
   - The bug being fixed
   - Why fix removes the future need for bypass
   - One-shot scope limited to this single PR
2. Record operator authorization and each bypassed command/result in the task or decision log
3. Bypass commits remain reviewable in branch history; squash-merge may condense final main
4. After the PR merges and the daemon updates, all subsequent work returns to the zero-bypass workflow
5. Worktree manipulation and protected-branch mutation remain forbidden; this exception cannot authorize them implicitly

`↳ 緣由 A-§13.5`

---

## Appendix A — Rationale & Incident Log

The *why* and *when* behind normative rules. Incident narratives, activation histories, and empirical motivations relocated here from the rule text; referenced from the normative layer via `↳ 緣由 A-§X`. Reading this is optional unless you are questioning or revising a rule.

### A-§3.3 — Evidence Is External to the Claim
Several reviews accepted comments or PR prose as proof of reachability and scope, then source inspection showed dead paths, bypassing call sites, or missing events. The rule requires evidence from executable behavior or cited source rather than restating the author's claim.

### A-§3.3.1 — CI Verification Gate
Sprint 61 incident — ci_watch emitted false [ci-pass] on partial completion, leading to merge of failing code.

**Flake-evidence rule:** a blanket "rerun + label flake" reflex repeatedly masked deterministic failures — a CI Coverage run went red on mostly REAL failures mislabeled as flakes, churning reruns instead of fixing. The recurring trap is extrapolating "it's flaky" from a local/worktree pass: local green ≠ CI green (platform / timing / parallelism / env), so a local pass is not evidence the CI failure was non-deterministic. Requiring the `gh run view <id> --log-failed` failing-test name forces the claim to name a real, known-flake signature before a rerun is justified; absent that, the default is "real failure, fix it."

### A-§3.12.1 — `gh pr merge --auto` adoption
**Activation status**: ACTIVE as of 2026-05-20 after #986 gh-poll integration shipped (PR #990, merge commit 4242c24). Prior to this date the canonical form was the legacy synchronous `gh pr merge <N> --squash --delete-branch` because `--auto`'s async-return discarded the synchronous merge confirmation. With #972 PR-state aggregator + #986 gh-poll integration both live, the `[pr-merged]` event now fires from real GitHub observation post-merge, restoring async-flow visibility and unlocking this default switch.

**Async confirmation pipeline (#972 + #986)**: `--auto` returns immediately, so the synchronous "PR merged at SHA" terminal feedback is gone. The daemon PR-state aggregator (#972, merged be23875) + gh-poll integration (#986, merged 4242c24) together emit `[pr-merged]` events to the PR author's inbox after observing the GitHub-side merge. Author waits for the event rather than polling.

**Activation history**: §3.12.1 was introduced in #973 (this rule's home PR) but kept INACTIVE until #972 + #986 both shipped. Activation switch landed 2026-05-20 as a docs-only follow-up (flips canonical form from legacy `gh pr merge ... --squash --delete-branch` to `gh pr merge ... --auto --squash --delete-branch`).

### A-§3.16 — Phase 1 Discussion Discipline
Rationale: lead's "from-code-structure" inference consistently misses scope holes (8/12 PRs in 2026-05-14 retrospective).

### A-§3.19.1 — Agent Git Anti-Patterns
Both failure modes surfaced empirically by the #863 reviewer incident. The bypass typically surfaces hidden state on top of the original problem: in the #863 incident, bypassing a checkout deny materialized a phantom `.gitignore` conflict that did not exist on the target branch, leaving the reviewer stuck on a fabricated merge conflict.

### A-§3.19.2 — Reviewer Base Workspace Branch Discipline
Incident 2026-05-20 — fixup-reviewer base dir was found stuck on `fix/900-spawn-env-propagation` with 492 deletion markers from a 2026-05-18 in-place checkout that was never reverted. Recovery cost session backend state (`.codex/.claude/.gemini/.kiro/.opencode/AGENTS.md` removed by `git clean -fd` because those dirs weren't in the fix/900-era `.gitignore`). The reflog showed the original Sprint discipline used `review/NNN-r0` per-PR worktrees correctly (2026-05-16 entries), then drifted to in-place checkouts.

### A-§3.19.3 — Source-File Lookup: No Full-Disk Scan
#2386 (2026-06-23, operator-confirmed): a `de2eb8` code-review workflow's agents/subagents each ran an ad-hoc full-disk `find / -name agend-git.rs` to locate the git-shim source. Run concurrently across the fleet, this spiked machine load to **108 on a 16-core box** — a one-shot blowup that recurs whenever an agent doesn't know a path and reaches for `find /`. Not an `agend` scan (the daemon never does this); the fix is preventive guidance (§3.19.3), not a code gate: an index-scoped lookup (`git ls-files` / `rg`) inside the bound worktree is both correct and cheap.

### A-§3.20 — Race-Condition PR Discipline
Empirical motivation: #881 ("app mode never owns the daemon") shipped CI green + reviewer VERIFIED on 2026-05-17, then surfaced a spawn-and-attach race on the first cold-start with a slow filesystem flush. Operator reverted at 470c251; #882 reopen fix shipped at 0fd89e8 with a probe_api gate + `--foreground` mode + actionable error path.

Why SOP 2 is post-merge, not a gate: a pre-merge smoke gate creates a chicken-and-egg problem. The operator's daily binary runs from main; to smoke an unmerged race-class PR they must (a) build from the branch manually and (b) point `$AGEND_HOME` away from their daily setup. The gate then blocks merge until smoke confirms — but smoke can't run without operator side-work that breaks the merge flow. PR #908 (2026-05-18 #896 fix) stalled exactly on this loop; operator directive at the time: "smoke gate會造成 chicken-and-egg的問題，要拿掉".

The bar for SOP 3 — fixup-reviewer's #882 verdict, verbatim: "Checked out pre-fix revert base 470c251 in this worktree. Verified target helper/tests are absent there by source grep."

### A-§3.22 — Spike-First Planning Gate
Distilled from the 2026-06-18 governance batch (operator D1–D5). Recurring failure mode: a combined spike+impl dispatch pre-commits to an impl scope the spike then refutes — e.g. #2325's "copy key is broken" framing and D1's "parse `Closes #N`" approach both inverted once the spike actually read the code / checked native platform behavior, so any impl approved up front would have built the wrong thing. Separating the dispatches and gating impl on a decision-manifest makes the premise-check load-bearing rather than decorative, and stops batch-approval from blessing an unknown scope.

### A-§7.1 — CI Tool Identity & Cache Hygiene
Pattern caught 2026-05-14 PR #772 v1 → v3 evolution; v1's `cargo --version` exit check missed pollution; v2 detection-recover failed; v3 `cache-bin: false` prevention shipped.

### A-§7.3 — Wedged-Run Recovery
PR #863 (#852 residual PR-A) hit a `windows-latest` wedge on 2026-05-16. The job stayed `in_progress` for 9+ hours starting at 15:19 UTC; `gh run cancel` was accepted but the job never transitioned. Empty-commit nudge dispatched at 16:14 UTC → fresh CI fired → green within ~10 min → merge proceeded. Operator-authorized, ~5 min total recovery vs. the alternative (wait 6 hours for GH-Actions timeout + manual re-trigger).

### A-§10.9 — GitHub CLI Authorship Signature
#2109 proposed an `agend-gh` shim (analog to `agend-git`) to auto-inject instance+model into gh bodies. Operator ruled against the shim (2026-06-14): the `agend-git` shim is a behavioral-correctness necessity (worktree redirect, #821/#1463) without which the daemon breaks, whereas gh authorship is observability cosmetics — a second PATH-hijack shim binary for it is over-engineering. Downgraded to the §10.9 soft convention; #2109 closed as a note.

### A-§12.7 — Linked-Issue Close Convention
Operator decision 2026-06-18 (governance D1). The D1 spike found: (1) recent fleet PRs used bare `#N` (0/12), which never auto-closes and is ambiguous (a reference ≠ a fix — e.g. #2158 was referenced bare by merged PRs yet legitimately stayed open); (2) a daemon-side `Closes #N` parser would be redundant with GitHub/GitLab native auto-close AND inert until PRs adopt the keyword; (3) `gh issue close` is GitHub-only, conflicting with the multi-platform `ScmProvider` direction. So the fix is the convention (use the keyword → native close), not daemon code. First real use: #2325 / PR #2328 auto-closed #2325 via its `Closes` keyword; straggler fallback is lead closing manually at merge (e.g. #2327).

### A-§13.5 — Bug-Blocks-Its-Own-Fix Exception
Reference: PR #779 (Sprint 61) Option 1 + Option 3 daemon binding fix shipped under this exception. PR #781 + #800 followed standard ZERO BYPASS workflow.

---

## Appendix B — Section Number Map (old → new)

| Old (v1 full) | New (condensed) |
|---|---|
| §3.5.5 | §3.6 |
| §3.5.9 | §3.7 |
| §3.5.10 | §3.9 |
| §3.5.11 | §3.10 |
| §3.5.12 | §3.11 |
| §3.5.13 | §3.12 |
| §3.5.14 | §3.13 |
| §3.5.15 | §3.14 |
| §3.6 | §5 |
| §10.1-10.5 | §12.1-12.5 |
