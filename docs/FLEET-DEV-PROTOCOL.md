# Fleet Development Protocol v1.2 (Condensed)

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

## §0. KISS Principle

Every PR must answer: **"What real problem does this solve?"** and **"Would deletion break anyone?"** Changes lacking a concrete failure mode = `KISS-VIOLATION — UNVERIFIED`.

## §1. Task Board (Single Source of Truth)

Use daemon `task` tool, NOT per-agent local task lists.

**Lifecycle**: `create` → `claim` → `in_progress` → `verified` → `done`

**Rules**:
- Orchestrator creates tasks; Implementer/Reviewer update status
- `depends_on` must be set when dependency exists
- `task done` must include `--result`

## §2. Decisions Panel

Use `decision(action: post)` to freeze scope definitions or ground truth changes.
- `tags` must include track + PR number
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
`VERIFIED` / `REJECTED` / `UNVERIFIED` — **start the report with the verdict word** (§3.12 convention; the daemon keys on it).

Every review report must include: `scope_source`, `audit_mode`, `reviewed_head`, `commands`, `files`.

**Evidence block (#1666 Phase A — daemon-enforced).** A `VERIFIED` or `REJECTED` verdict MUST carry an `### Evidence` block proving the claim:
- `ran: <cmd> → <result>` — a command actually executed (e.g. `cargo test` / `clippy` / `gh pr checks <PR#>` / `grep`), with its outcome; and/or
- `cited: path:line — quote` — a source citation backing a finding.

`UNVERIFIED` is redefined as **"claimed but unproven"** — the evidence-exempt verdict. Use it when you assert a concern you could not run-or-cite (so the gate never forces fabricated evidence).

The daemon HARD-gates this at report time: a `VERIFIED`/`REJECTED` with **no recognizable evidence token** (a `cargo`/`gh`/`clippy`/`grep` command line, or a `path:line` cite) is rejected back to the reviewer. The gate is deliberately **lenient** — it accepts any one recognized token and rejects only on total absence; it does NOT enforce a fixed format. (The §3.21 risk-tier review DEPTH remains lead/reviewer judgment — not daemon-enforced.)

**Comments and prose are claims, not evidence (#2018).** Every factual assertion in a code comment, doc, or PR body is a claim to VERIFY against the code, never evidence in itself. Reachability / scope / "cannot happen" / "single chokepoint" claims must be proven from the actual guards and match arms in the source — author and reviewer alike. (2026-06-11 surfaced four in one day: a compaction-loss rationale that named dead code, a "single chokepoint" that several call sites bypassed, an "agy is a hook backend" that emitted zero events, and an "EXDEV" fallback on a same-directory rename.)

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

### 3.4 Re-review (r2) Dispatch
Must enumerate r1 findings with status: fixed / deferred / withdrawn. Missing → reviewer falls back to `full_review`.

### 3.5 Multi-reviewer
- Default: single primary reviewer
- Dual reviewer only when: high-risk shared behavior, repeated reject loop, primary requests, operator mandates
- Verdict severity: `REJECTED > UNVERIFIED > VERIFIED` — worst wins

### 3.6 LOW Docs-only Exception
All conditions must hold for single-reviewer or operator self-merge:
1. Only `docs/FLEET-DEV-PROTOCOL-*.md` or `REVIEWER-CONTRACT-*.md` edits
2. Diff ≤ 50 LOC
3. No `src/` behavior change (`src/instructions.rs` template strings exempt)
4. No new rule affecting mid-scope+ PRs

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
- Reviewer verifies: `git checkout <test-sha>` fails → `HEAD` passes
- Exemptions: docs-only, pure refactor, test-only, dep bump, EMERGENCY, pure deletion, empirical-revert

### 3.11 Deferred-defense
- (a) Known-issue recurs in production → auto-escalate to P0
- (b) Deferred backlog must have `due_at` (default: 2 sprints)
- (c) Same root cause deferred twice → mandatory dual reviewer + operator sign-off
- (d) Removing defensive code → 4-perspective counter-example challenge; 0 compelling = safe to delete

### 3.12 Verdict Externalization (was §3.5.13)
Fleet-internal verdict MUST mirror to GH PR comment (`gh pr comment`). Self-merge gate: dual VERIFIED + CI green + verdict mirror posted — all three required before merge.

**Canonical merge step: `repo action=merge pr=<N>`** (the MCP `repo` tool → `handle_merge_repo`, `src/mcp/handlers/ci/mod.rs`). It issues the **byte-identical** merge a raw `gh` call would (`gh pr merge <N> --repo R --admin --squash --delete-branch`, pinned by `scm::tests::pr_merge_args_match_existing_gh_call`) but wraps it in three safety nets the raw command lacks:
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
- Notify lead via `send(kind=update)` if escape-hatch invoked, with case (a)/(b)/(c)/(d) identifier.

`↳ 緣由 / 活化史 A-§3.12.1`

### 3.13 Log-level Changes (was §3.5.14)
Must have inline rationale, otherwise `LEVEL-CHANGE-RATIONALE-ABSENT — UNVERIFIED`.

### 3.14 Observability PRs (was §3.5.15)
Must include e2e integration test exercising the production hook path.

### 3.15 Daemon-core Cushion Rule
PRs touching daemon core / channel / supervisor / state.rs must include stress test + lock-ordering analysis before dispatch. "不急 ship" principle — correctness over velocity for infrastructure changes.

### 3.16 Phase 1 Discussion Discipline (Sprint 62)
**Pre-impl source-code spike is mandatory.** Lead's initial proposal MUST be challenged by dev's 5-10min source-code spike before Phase 2 dispatch. Spike outputs:
- Confirm or refute lead's initial site count
- Surface bonus emission sites lead missed
- Distinguish "near-bug" from "asserts-on-bug-signature" (issue body counts often conflate)
- Identify pre-existing helpers / deps that change scope estimate

**Three-party substantive consensus required**: reviewer must offer at least one design challenge AND dev must offer at least one impl concern before consensus is recorded. Triple ACK without substance = rubber-stamp = `RUBBER-STAMP — UNVERIFIED`.

**Issue body counts are estimates, not contracts.** When issue body says "N sites / N tests need updating," dev spike re-counts. Actual surface may be narrower OR wider than the initial estimate.

### 3.17 Static-Review Limits + Runtime Validation Required
Static / structural review is INSUFFICIENT for the following surfaces:

- **CI workflow YAML** (cache layer interactions, runtime PATH/env)
- **Shell script** (variable interpolation, locale-dependent behavior)
- **Daemon refresh / lifecycle behavior** (in-memory state vs persisted state divergence)
- **Cross-platform binary semantics** (e.g., rustup-init `--version` exits 0 for any binary at the proxy path)

For these surfaces, a `VERIFIED` verdict requires runtime evidence — typically the PR's own CI run on multiple platforms. Pure code-diff inspection does not suffice. Reviewer must explicitly note "runtime-validated via PR-CI run X" in their verdict report. If the PR's own CI doesn't exercise the affected path, request an empirical reproduction step.

**Generalizable invariant**: exit code 0 is not a strong identity contract for tool checks. Output shape is. `<tool> --version | grep -qE "^<tool> [0-9]"` is the correct content-validating idiom.

### 3.18 Reviewer Audit Conflict Resolution
When reviewer's claim contradicts dev's claim (e.g. reviewer "stale wording remains" vs dev "wording updated"), lead MUST do **independent verification** before accepting either side:
- `git show <SHA>:<file>` at the exact reviewed_head SHA
- `git diff <prev>..<reviewed_head>` for the disputed lines
- Run the relevant test or grep command independently

Lead replies to both with the empirical evidence. Reviewer/dev should self-correct rather than escalate to operator.

### 3.19 Reviewer Workspace Discipline
Reviewers MUST inspect PRs from their own daemon-bound worktree. Specifically:

- **Never `cd` into the canonical source repo** to inspect a PR. The canonical is the operator's working tree; reviewer activity must not leave detached HEAD or stale refs there.
- **Never create refs in canonical** (`git checkout -b tmp_pr_review`, `git checkout <sha>`, `git fetch origin pr/N/head:pr_head`, etc.). These leave `pr*_head` / `tmp*` / `review/*` branches behind that pollute `git branch --list` and confuse later operator commands.
- **Use `gh pr diff <N>` or `gh pr view <N> --json files`** to read PR contents without checkout. If a full tree inspection is needed, `repo action=checkout` MCP tool provisions a fresh daemon-managed worktree at the PR's HEAD; releasing it (`release_worktree`) does not touch canonical.
- **If canonical state is observed dirty post-review** (detached HEAD, stale `tmp*` / `pr*_head` branches), the reviewer's verdict is REJECTED until the canonical is cleaned (operator action OR `repo action=cleanup_merged_branches` with the `reviewer_checkout` category once L3 lands).

Enforcement: L2 `agend-git` shim refuses `checkout -b` and `checkout <sha>` from agent callers when cwd=canonical (PR-B). L3 sweeper cleans the residue and auto-switches detached canonical HEAD back to main at daemon boot (PR-C).

### 3.19.1 Agent Git Anti-Patterns

§3.19 names what reviewers must not do. This section names two failure modes and the correct recovery path. Apply to every agent, not only reviewers. `↳ 緣由 A-§3.19.1`

**Anti-pattern 1 — `AGEND_GIT_BYPASS=1` to escape a shim deny.**

When the `agend-git` shim denies an agent action, the deny is a protocol signal, not a transient error. Re-running the same command with `AGEND_GIT_BYPASS=1` is forbidden.

- **WRONG**: shim denies → set `AGEND_GIT_BYPASS=1` → retry. The bypass succeeds at the git level but skips the protocol gate that the deny was enforcing; whatever the gate was protecting (canonical hygiene, lease invariants, reviewer workspace boundary) is now violated silently.
- **RIGHT**: abort the operation. Send `kind=query` to lead/orchestrator naming the denied command + the shim's reason string, and ask for the correct routing.

Reasoning:

- `AGEND_GIT_BYPASS=1` exists for **daemon-internal helpers** (`canonical_hygiene`, `branch_sweep`, `conflict_notify`) that read worktree state from canonical-rooted paths and would otherwise self-deny. It is not an escape hatch for agents.
- The bypass typically surfaces hidden state on top of the original problem.
- "Ask, don't bypass" is the universal recovery: a deny means the daemon owns the routing answer, and asking is cheap.

**Anti-pattern 2 — `git checkout <sha>` to materialize a PR review.**

Even in the agent's own daemon-bound worktree, `git checkout <sha>` is the wrong primitive for PR review:

- Leaves detached-HEAD residue — the class of pollution that #852 (canonical hygiene) and #858 (shim deny matrix) exist to prevent.
- Conflicts with the daemon's branch lease on the worktree, producing later "branch already leased" errors that look unrelated.
- Bypasses §3.19's shim-enforced workspace boundary even when run from a non-canonical cwd, because the shim's lease/lifecycle invariants assume branch-rooted HEADs.

Right path, by inspection depth:

- **Full tree** (`cargo test` replay, runtime validation, multi-file inspection): `repo action=checkout source=<canonical> branch=<PR-branch> bind=true`. The daemon provisions a fresh worktree at the named branch, binds it to the caller, and `release_worktree` returns cleanly with no residue.
- **Read-only** (diff inspection, file listing): `gh pr diff <N>` or `gh pr view <N> --json files`. No working-tree mutation at all.

If `repo action=checkout` fails (lease already held, branch unknown, worktree quota exhausted) → **ask, don't bypass**. Send `kind=query` to lead with the failure mode; lead routes via `force_release_worktree` or alternate provisioning. Falling back to `git checkout <sha>` after a `repo` failure recreates the exact class of pollution this section forbids.

**Relationship to §3.19.** §3.19 says *what reviewers must not do in canonical*. §3.19.1 says *what every agent must do when the protocol gate fires* — abort and ask, not bypass and retry.

### 3.19.2 Reviewer Base Workspace Branch Discipline

§3.19 covers the canonical source repo. This section covers the reviewer agent's OWN base workspace dir (e.g. `~/.agend-terminal/workspace/fixup-reviewer/`).

**Reviewers MUST NOT** do in-place `git checkout` of an impl branch into the agent's base workspace dir. The base workspace is daemon-bound to a specific branch (typically `main` or a long-lived review-housekeeping branch); checking out an impl branch in-place pollutes the base with stale-branch state that bleeds into future sessions.

Use one of:
- **(a) Dedicated review worktree**: `git worktree add -b review/<N>-r0 <path> origin/<impl-branch>` — read-only review in a fresh worktree separate from the agent base. Release with `release_worktree` when done.
- **(b) GH-only review** (preferred for diff-only inspection): `gh pr diff <N>` + `gh pr view <N> --json files,reviews,statusCheckRollup`. No local checkout, no cleanup needed.

**NEVER** in-place `git checkout` of an impl branch in the agent's base workspace dir.

`↳ 緣由 A-§3.19.2`

**Relationship to §3.19.** §3.19 forbids checkout in CANONICAL. §3.19.2 forbids in-place checkout in the agent's BASE WORKSPACE. Both protect against stale-branch pollution at different boundaries.

### 3.20 Race-Condition PR Discipline

Race-class PRs ship with hidden timing dependencies that pass CI + reviewer VERIFIED yet break production. The lessons below apply to every spawn / async-coordination / multi-process-startup PR (the "race class"); same discipline framing as §3.19.1. `↳ 緣由 A-§3.20`

**SOP 1 — Pre-r0 race-condition question.**

Before dispatching r0 on a race-class PR, lead AND dev MUST answer in writing: *"Does this change have a race condition, and can I write a deterministic test that reproduces it without timing dependence?"* The answer goes in the spike report (or the dispatch message if no spike preceded).

Race class includes — but is not limited to — `tokio::spawn` / `thread::spawn` sites, multi-process startup ordering, `Drop`-vs-`enqueue` lifecycle, lock-ordering across modules, signal-handler-vs-main-loop coordination, daemon-vs-bridge handshake gates. If the answer is "no deterministic test possible," the PR escalates reviewer RED-protocol scrutiny per SOP 3 and SOP 2 post-merge smoke becomes the primary empirical signal.

**SOP 2 — Post-merge operator smoke sanity check (NOT a merge gate).**

Race-class PRs merge once SOP 1 (deterministic RED→GREEN tests) AND SOP 3 (reviewer RED-protocol) are both satisfied. SOP 2 is a **post-merge sanity check**, not a pre-merge gate.

**Post-merge smoke procedure**:

- Operator (or lead on operator's behalf) reproduces the race scenario on a **fresh, isolated `$AGEND_HOME`** — e.g. `/tmp/smoke` or `$TMPDIR/agend-smoke-$$`. **NEVER use the operator's daily `~/.agend-terminal`**; smoke runs MUST be hermetic and disposable so a regression cannot leak into operator state.
- PR body MAY include a suggested smoke script enumerating the race scenario the fix targets (e.g. "start daemon cold + watch inbox for `bridge_connected` within 5s"). Optional, not required for merge approval.
- If post-merge smoke uncovers a regression: operator-driven revert (`git revert <merge-sha>`) — race regressions auto-escalate to P0 per §3.11(a) deferred-defense.

**Gating layers (the actual merge gates)**:

- **SOP 1** (deterministic RED→GREEN tests at unit/integration level) — the structural gate. Most race-class behaviour CAN be deterministically tested with proper mocking or DI; the bar is "is there a test that fails pre-fix and passes post-fix, on three back-to-back runs."
- **SOP 3** (reviewer RED-protocol execution on the test surface) — the audit gate. Reviewer must independently observe the RED→GREEN transition.
- SOP 2 post-merge smoke is supplementary empirical coverage, not a gate.

If SOP 1 honestly says "no deterministic test possible" (rare — usually achievable with `tokio::test` + paused time, channel-based synchronization, or trait-injected clocks), SOP 3 still applies and merge proceeds. SOP 2 post-merge smoke then carries proportionally more weight as the only remaining empirical signal — the PR description should call this out so the operator runs the smoke promptly post-merge.

**SOP 3 — Reviewer RED-protocol for race-class PRs.**

For race-class PRs, the reviewer MUST execute the RED→GREEN protocol (not skim it):

```
# Reviewer's bound worktree (NOT canonical — §3.19):
git checkout <pre-fix-base>     # revert commit OR last known good
# Confirm RED: the new tests compile-fail, fail at runtime,
# or fail with the expected error signature.
git checkout <fix-head>
# Confirm GREEN: tests pass without flakiness on three back-to-back runs.
```

The verdict body MUST explicitly state the protocol execution: "Checked out pre-fix base `<sha>` in this worktree. Verified target tests absent/failing there [by source grep / by cargo test exit code]. Reapplied fix HEAD; tests pass 3/3 runs."

Reviewers who skip the protocol on a race-class PR get `RUBBER-STAMP — UNVERIFIED` per §3.16 substantive-consensus requirement. The PR returns to dev for explicit reviewer RED-protocol execution before re-dispatch.

**Relationship to §3.19.1.** §3.19.1 says *what every agent must do when a protocol gate fires*. §3.20 says *what lead, dev, and reviewer must do BEFORE the gate could fire* on race-class PRs — a sanctioned discipline addition, not a replacement for any existing rule. Race-class triage at r0 dispatch is cheaper than the ship-then-revert cycle empirically observed on #881.

### 3.21 Proportional Ceremony — right-size process to task risk

Match fleet ceremony to where a task's risk actually lives. Decided by **lead judgment**, NOT a daemon classifier — a rubric nobody follows is compliance-theater (false confidence, blame-shift). #1656 shipped review-tiering as pure judgment and it caught real defects. Record each dispatch's ceremony call via `decision(action: post)` — the decision log IS the classifier (zero new code). `↳ 緣由 #1656/#1659/#1660 dialectic`

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
- **Impl is dispatched only AFTER the forks are resolved**, and `depends_on` the spike (and the decision that settled each fork). Impl scope is derived from the manifest, not assumed up front.
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
One-shot backends (Codex) skip PTY injection for `kind=update` and `kind=report` messages to avoid wasting turns. However, **cross-team messages are NEVER silently absorbed** — they always inject to PTY regardless of backend or message kind. Team membership is checked at delivery time; agents not in any team are treated as cross-team (safe default). Absorbed messages are audit-logged as `ack_absorbed` events.

## §5. Async Pipeline

Impl pushes PR then immediately starts next task. Reviewer issues verdict then immediately takes next review. dev-lead maintains pending list; dual-VERIFIED + CI green (independently verified via `gh pr checks`) → self-merge.

**Key rules**:
- Impl push must include scope statement (follows spec / deviated because)
- Orchestrator pre-dispatch verification: cross-check dev's claim against actual artifact before forwarding to reviewer
- dev-lead uses `schedule(action: create)` for auto-poll (30min fallback)
- **Post-dispatch verification (Sprint 62)**: after `send(kind: task)` returns success (no error), if receiver does not reply within ~5min, dispatcher MUST verify via fallback path:
  - `inbox(instance: <receiver>)` — confirms unread queued message (offline agents only; active agents receive PTY direct injection and inbox stays empty)
  - `describe_instance(name: <receiver>)` — confirms agent_state active (PTY delivery already arrived)
  - `binding_state(agent: <receiver>)` — confirms task lifecycle started (binding metadata present)
  - If all three show no progress, suspect lease conflict / stale binding / dispatch path block — investigate (`force_release_worktree` if needed) before re-dispatching
- **Pane-claim is not delivery**: agent writing a response in its own pane is NOT a `send`. Every reply / verdict / report must be triggered via the MCP `send` tool. Receivers do not see pane content. Verify via §6 channel discipline.
- **Post-PR-merge close-loop reporting**: each PR `kind=report` MUST include a "lessons learned" section noting process wins, scope shifts, unexpected discoveries. Captures process-maturity signals for protocol evolution.
- Takeover requires 4 criteria independently verified (heartbeat stale ≥1h, last_input frozen, idle state, zero activity)
- Merge must atomically include `git worktree remove` + `git branch -D`
- Post-merge: orchestrator verifies main CI green before reporting task completion upstream. Failed main CI = immediate P0 (revert or hotfix).
- Orchestrator owns `ci(action: watch)` for own-orchestrated branches
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

**Dispatch milestone updates** — when you accept a `task` dispatch, send `kind=update` to the dispatcher at each of these milestones without being asked:

1. **r0 ready** — PR opened (or work artifact handed off), with verbatim links / heads.
2. **CI all-green** — every CI gate the PR runs has reported success. The `[ci-pass]` watch broadcast does NOT substitute — confirm via your own update so the dispatcher's loop closer fires regardless of their channel state.
3. **Reviewer verdict received** — VERIFIED / REJECTED / UNVERIFIED, with the reviewer's identity and key finding summary.

Re-review cycles (r1, r2, …) repeat the same three milestones. The dispatcher relies on these as the loop closer; missing any forces them to poll, which is anti-pattern (see §7).

- Pure ack → do not reply (ACK absorption §4 handles this automatically)
- Response channel must match source channel
- **Router-layer channel discipline (Sprint 52)**: daemon auto-mirrors agent direct text to the corresponding channel. Agent does not need to force `reply` tool — infrastructure handles routing.
- **Inbox vs PTY delivery (Sprint 62)**: active agents receive messages via PTY direct injection (not queued in `inbox`). `inbox(instance: X)` returning empty does NOT mean X received nothing — only means X has no unread queue. Verify delivery via `describe_instance` (active state = PTY received) or `pane_snapshot` rather than inbox alone. Inbox queue only fills for offline agents or undeliverable messages.
- **Daemon auto-inject marker `[AGEND-AUTO]` (#1769)**: the daemon resumes a stuck agent by injecting a keystroke (e.g. `continue`) straight into the PTY, which otherwise looks identical to the operator typing it — a bare injected `continue` was once mistaken by an orchestrator for an operator command and a task dispatched from it. Such nudges now carry an `[AGEND-AUTO kind=...]` prefix (sibling of `[AGEND-MSG]`). **Rule:** treat an `[AGEND-AUTO]` line as a low-priority RESUME signal — continue in-progress work — and **never** as an operator command or a basis to dispatch a task / make a decision. Inbox/operator-relay messages keep their own `[AGEND-MSG]`/`[from:]` headers and are unaffected.

## §7. CI

Use `ci(action: watch)` for ongoing monitoring, not manual polling. Exception: merge-gate final verification requires one-shot `gh pr checks <PR#>` per §3.3.1. Clean up worktree + branch after merge.

**No manual orchestrator polling**. Orchestrators (lead, general,
operator-in-the-loop) MUST NOT manually poll PR / CI state via
`gh pr view`, `gh run list`, repeated `cargo test`, or equivalent.
Rely on:

1. The dispatchee's `kind=update` milestones (§6) — r0 ready, CI
   all-green, reviewer verdict.
2. `ci(action: watch)` fan-out — `[ci-pass]` / `[ci-fail]` /
   `[ci-watch-stalled]` arrive automatically.

Manual polling masks broken dispatch communication and burns cache /
rate-limit budget unnecessarily. If a milestone is missing past a
reasonable window, the correct response is to message the dispatchee
asking why, not to poll. Polling is also a smell that the dispatch
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

**Health surface (Sprint 54 P0-5)**. The `ci(action: watch)` response
and the new `ci(action: status)` aggregator both carry `rate_limit_active`,
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

- **Time arithmetic**: never `Instant::now() - Duration` (Windows monotonic clock anchors to system uptime → underflow on fresh VM). Use `Instant::add` (saturating) or DI inject `now: Instant` for tests.
- **Regex hot-path**: never per-call `Regex::new` in fed loop. Use `LazyLock<Vec<Regex>>` (or `OnceLock`). Performance ratio: ~100× speedup, prevents Windows runner test timeout from cumulative `min_hold` budget.
- **PTY EOF semantics**: never assume EOF behavior matches across cmd.exe/bash/ConPTY. Shell-backend tests need `#[cfg_attr(windows, ignore = "tracking #N")]` if EOF semantic divergence is the bug not the SUT.
- **Path mangling**: sanitize both `/` (Unix path) AND `\` + `:` (Windows drive letter) when constructing worktree paths from source paths.

### 7.3 Wedged-Run Recovery

When a CI workflow run **wedges** — a job stays `in_progress` past 2× typical platform completion time and `gh run cancel <run-id>` returns success but the job status doesn't transition — push an **empty commit** to the PR branch to trigger a fresh workflow run. This is a sanctioned recovery technique, not a workaround.

```
repo action=checkout source=<canonical> branch=<PR-branch> bind=true
cd <bound-worktree>
git commit --allow-empty -m "ci: nudge wedged runner (PR #N wedged Nhr)"
git push origin <PR-branch>
```

The fresh CI run fires on the new HEAD; the old wedged run becomes irrelevant (it eventually GH-Actions-times-out at 6 hours without affecting merge). Reviewer's prior VERIFIED verdict still applies — the SHA advance is byte-identical content, so `reviewed_head` SHA-staleness gate (§4.2) accepts a re-stamp once CI completes; merge gates on the fresh CI result.

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

Task state changes emit to Telegram. Instance lifecycle events (non-fleet.yaml origin) broadcast with `origin` field. `create_instance` defaults to isolated workspace (`~/.agend-terminal/workspace/<name>`).

## §9. Waiting & Timeout

- `set_waiting_on` to declare blockers (auto-clears after 120s inactivity)
- Use `schedule(action: create)` for check-ins (cross-backend)

**Timeout staircase** (single source of truth):

| Elapsed since dispatch | Action |
|---|---|
| < 20 min | Normal. `describe_instance` — fresh heartbeat = agent active. |
| 20 min, heartbeat fresh | Agent working. Extend wait. |
| 20 min, heartbeat stale (>120s) | Ping via `send` with direct question. |
| 20 min, no response to ping | `replace_instance` and re-dispatch. |

**Backend modifiers**:
- kiro-cli: 1-2h longer wait (context compaction self-heals); escalate to operator rather than `interrupt`
- Other backends (claude/codex/gemini/opencode): use staircase above as-is

### Supervisor Notify
Daemon detects agent entering error state (UsageLimit/RateLimit/Hang/Crashed/AuthError/PermissionPrompt) → notifies orchestrator. 60s debounce per agent.

### 9.1 Context-Full Self-Restart (Sprint 63, empirically validated 2026-06-15)

When an agent (especially a lead/orchestrator) detects its own context approaching full (~80-85%; the pane footer shows `N% context used`), it restarts **itself** — the daemon performs the kill+respawn; no second agent is required to trigger it.

- **`mode="fresh"`, never `resume`** — `resume` reloads the full prior context, giving zero relief. Only `restart_instance(instance=<self>, mode="fresh")` starts clean.
- **Procedure**:
  1. Land all live state on durable stores **before** restarting — update `SESSION-HANDOFF.md` to current (handoff entry point, in-flight PRs, merge procedure, member status, pending dispatches, decisions), post any open `decision`s, ensure work is on the `task` board. Nothing may depend on in-memory context.
  2. **Self-schedule a kick** so the fresh instance auto-resumes (a fresh boot otherwise sits idle indefinitely — nothing starts it). Before restarting:
     `schedule(action="create", instance=<self>, run_at=<now + ~90s, ISO 8601>, message="resume: read SESSION-HANDOFF.md + MEMORY.md and continue", label="self-restart-kick")`.
     The schedule is daemon-side and keyed by instance name, so it survives the restart; the one-shot fires ~90s after respawn and wakes the fresh self to continue. (Delete it after resuming, or let the one-shot auto-complete.)
  3. Pick a lull — never mid-merge or mid-step of an irreversible action.
  4. Call `restart_instance(instance=<self>, mode="fresh", reason="context-full self-restart")`.
  5. The daemon cleanly kills the process (`delete: child exited cleanly`) and respawns it fresh (Ctx 0.0%). On boot the SessionStart hook auto-loads `MEMORY.md`; the fresh self reads `SESSION-HANDOFF.md` and continues.
- **The one caveat (and its mitigation)**: the self-restart call's response never returns (the caller's process is gone), so no external party confirms the fresh instance booted. Mitigation: handoff + `MEMORY.md` auto-load make the fresh self self-sufficient. A peer (e.g. `general`) is **optional** for a post-restart liveness confirm but is **no longer required to trigger** the restart — this supersedes the earlier "ask general to fresh-restart you" convention (delegation is now confirm-only, not the trigger path).

**Empirical basis**: validated 2026-06-15 — an agent called `restart_instance(self, mode="fresh")`; daemon logs showed the tool call → `delete: child exited cleanly` → fresh respawn at Ctx 0.0% with no prior-context memory. A second agent is not part of the mechanism. Also validated 2026-06-15 — the fresh instance sat idle ~80s doing nothing, then the self-scheduled one-shot kick fired (`schedule_trigger`) and it resumed, with zero peer involvement.

## §10. Git Workflow

- Never commit directly to main; always use worktree + branch
- Branch naming: `feat/`, `fix/`, `docs/`
- Clean up immediately after merge
- **Never** `git worktree add <path> main` — locks main, breaks operator builds. Always use `-b <new-branch>`. Recovery: `cd <worktree> && git switch -c <dedicated-branch>`
- **Dispatched work: the daemon already auto-binds your worktree — `cd` in, do NOT self-provision.** Every `send(kind: task, branch: X)` auto-binds the assignee to a daemon-managed worktree at dispatch (find it via `binding_state(agent: <self>)` → `worktree`). Do NOT run your own `git worktree add` / `AGEND_GIT_BYPASS=1 git worktree add` for dispatched tasks — that double-provisions and the stray op detaches the operator's canonical HEAD (#2234). **Enforced (#2234 fix B):** the `agend-git` shim DENIES an agent's `AGEND_GIT_BYPASS` `worktree add` / positional `checkout|switch <ref>` when cwd is canonical-rooted (the source repo or any worktree of it). Use normal git inside the bound worktree (the shim routes it correctly — no bypass needed). Genuinely need to provision yourself? Use `bind_self {repository_path, branch}` (daemon-tracked), or for a one-shot escape set `AGEND_GIT_ALLOW_CANONICAL_MUTATE=1`. A shim deny is a protocol signal — ask, don't escalate to bypass (§3.19.1).
- **Generic `bind_self` (Sprint 54 P1-7)**: any agent (lead, dev, reviewer, …) may proactively claim a worktree via `bind_self {repository_path, branch}` without going through the dispatch hook. Inherits every dispatch invariant — Phase 1 trailers, P0-1.5 cross-agent registry, P0-1.6 actual-HEAD verification, P0-X release_worktree as sole exit, source_repo persistence, auto watch_ci. Use case: lead orchestrator escalating to Path A IMPL on a hot branch. Pair with `release_worktree` to unbind. `main`/`master` rejected with E4.5; cross-agent branch conflicts return `code: cross_agent_conflict`.
- **`AGEND_GIT_BYPASS` pushes do NOT auto-watch CI (known limitation).** A bypass push goes straight through the `agend-git` shim (execs real git) and never reaches the dispatch_hook that arms `watch_ci` — so **no `[ci-ready]` fires for that branch**, even when CI is green. After a bypass push that opens or updates a PR, arm the watch manually with `ci(action: watch)` (or rely on lead's manual `gh pr checks` as a fallback). Normal daemon-bound pushes auto-watch per the `bind_self`/dispatch invariants above. (#1750 A1 made a *failed* arm on the normal path visible; the deeper fix — re-enabling the push hook so bypass is rarely needed — is gated on the #1751 broad shim-footgun fix.)

### release_worktree branch-cleanup scope

`release_worktree` auto-cleanup ONLY operates on branches that satisfy ALL of:
1. The worktree was daemon-managed (`.agend-managed` marker verified)
2. The branch is confirmed merged into main OR remote tracking ref is gone (squash-merge)
3. Protected refs (main/master) are NEVER touched

User-checkout branches, operator-created worktrees without `.agend-managed` marker, and any branch where the marker cannot be verified are NEVER deleted.

### release_worktree parameter form

Use `release_worktree(agent: <self>)`. The `path: ...` form is NOT a recognized schema — daemon silently no-ops on unknown params. Verify cleanup with `binding_state(agent: <self>)` returning `bound: false`.

### 10.6 Lead Pre-Dispatch Release (Sprint 62)

Every `send(kind: task, branch: <X>)` triggers daemon auto-bind for the **dispatcher** (lead) to branch X, then dispatches the task to the assignee. If lead is already bound to a previous dispatch branch, the new dispatch may fail with `lease_failed` OR succeed but leave the previous binding stale.

Normalize: lead MUST `release_worktree(agent: <self>)` BEFORE every `send(kind: task)`. This:
1. Clears stale lead bindings from prior dispatches
2. Ensures the new auto-bind doesn't conflict
3. Prevents dev's subsequent claim from hitting "branch already checked out" errors

Lead role does not need a worktree for orchestration work — release immediately after each dispatch is correct hygiene.

### 10.7 Bare `init` Heartbeat Commits — Source & Handling (#1462)

Empty `init` commits sometimes pile up on a bound branch. **Correcting the
prior (inaccurate) wording in this section**: the daemon does NOT write them.

**Who produces them**: the **backend CLI process** (Claude Code / Codex / etc.)
emits `git commit --allow-empty -m "init"` itself, as a session-checkpoint
side effect. These pass through the `agend-git` shim's `ChdirPass` like any
other agent git call, so the committer is the operator's **global git
identity** (user.name/user.email), not the daemon. See the matching note on the
`Action::CleanupAndChdirPushPass` variant in `src/bin/agend-git.rs`.

**What the daemon actually does** (it never creates these): only
(a) stamps `Agend-Agent` / `Agend-Task` / `Agend-Branch` / `Agend-Issued-At`
trailers onto whatever commit happens, via the `prepare-commit-msg` hook, and
(b) **cleans** them — the 5 git hooks plus the pre-push `agend-git` shim
(`cleanup_init_pile_pre_push`) soft-reset the empty `init` pile out of the
push range.

**Cadence is episodic, not periodic**: they arrive in **bursts** (several in
the same second), separated by **hours to days**, triggered by backend session
events — NOT on a fixed ~90s timer. Idle agents do not steadily accumulate them.

**Distinct from agend's own init commit**: in a fresh/empty repo agend creates
a **one-time** `init (agend-terminal)` commit (committer `agend@localhost`,
under `AGEND_GIT_BYPASS`). That is a different, legitimate, single commit — not
the bare-`init` backend heartbeat described here.

**Agent guidance — do NOT hand-clean the pile**:
- **Never** manually `git reset --soft` + force-push to scrub the `init` pile.
  The pre-push shim + the cleanup hooks already strip them from the pushed
  range, and squash-merge collapses anything that slips through. The local
  pile is **cosmetic** — it does not reach `main`. (A dev in an earlier session
  wasted effort force-pushing to remove these; don't repeat that.)
- **Forensics** (PR #1468, merged): after a rebuild + daemon restart, the shim
  records the process ancestry the next time a bare `init` fires — this will
  eventually pin down exactly which backend emits them. Until then, the
  source is "the backend", not the daemon.

**Acceptance / §3.10 verifiability**: these commits are NOT to be
amend-rewritten or force-pushed away (per §10 hard rule). Reviewers look past
them via `git log --no-merges --grep`. An anchor RED commit may sit between
`init` commits and the impl GREEN commit — verify §3.10 by
`git checkout <anchor-sha>` (cargo test fails) → `git checkout <impl-sha>`
(cargo test passes), ignoring the intervening `init` noise.

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
| Track work | `task(action: create/list/claim/done)` | local task lists |
| Record decisions | `decision(action: post)` | Markdown files |
| Assign work | `send(kind: task)` + `task(action: create)` | only one |
| Report results | `send(kind: report)` | free-text |
| CI monitoring | `ci(action: watch)` | manual `gh run list` loops |
| CI merge gate | `gh pr checks <PR#>` | trusting dev self-report |
| Wait state | `set_waiting_on` | prose |
| Health check | `describe_instance` | guessing |
| Schedule | `schedule(action: create)` | backend-specific tools |
| Timeout | `replace_instance` | waiting forever |

**Daemon-state error format (Sprint 54 #488 hotfix)**. Tools that
depend on daemon-resident state — `reply`,
`download_attachment` — never silently fall back to a local handler
when the daemon is unreachable. They return a structured error of the
form `tool '<NAME>' requires daemon API; not reachable: <CAUSE>`.
Agents seeing this prefix should surface the message as-is to the
user (it's operator-actionable: restart daemon / check socket) rather
than retry blindly. Stateless tools (`inbox`, `task`, `send`, etc.)
still fall back gracefully for offline workflows.

### 11.1 State Persistence Across Daemon Refresh (Sprint 62)

Daemon binary refresh (recompile + restart, or hot-reload via `mcp_registry_watcher`) invalidates several in-memory state stores. **After every `mcp_registry_watcher` notification fired**, the following state should be re-verified:

- **CI watch state** — fixed by #786, but pre-#786 watches may be missing
- **Instance registry vs team metadata sync** — fixed by #785 (better-error surfaces desync); team membership outlives instance restart, may reference wiped instances
- **Source_repo on team** — historically wiped by `teams.json` migration on refresh (was #781 root cause); persisted as of #781 but verify with `grep source_repo fleet.yaml` if behavior unexpected
- **Active bindings** — in-memory `bind_in_flight` flag may be lost; check `binding_state(agent)` and `force_release_worktree` if dangling

**Operator workflow**: `mcp_registry_watcher` notification = restart-needed signal. Run `agend-terminal stop && cargo build --release && agend-terminal start` to pick up new binary. Subsequent agent dispatches benefit from fresh code.

**Agent workflow**: do NOT assume state survives daemon refresh. Re-verify via `team list`, `ci action=status`, `binding_state` after any refresh notification.

## §12. Workflow Efficiency

### 12.1 Pipeline Dispatch
Push PR then immediately start next task. Depth ≤ 2. Must branch from main (no stacking on pending PR).

### 12.2 Reviewer Does Not Wait for CI
Start review on PR push. `reviewed_head` is a snapshot; subsequent commits reset verdict.

### 12.3 Task Close
`in_progress` → `verified` (reviewer) → merge (CI green per §3.3.1) → post-merge main CI green → `done`.

**Post-merge verification**: After squash-merge, orchestrator MUST verify main branch CI passes:
```
gh run list -b main --limit 1
```
or wait for ci_watch [ci-pass] on main. Only declare task `done` after main CI is confirmed green. If main CI fails post-merge, immediately investigate and fix (revert if necessary).

### 12.4 Worktree Mandatory
Impl/reviewer must use worktrees. `git worktree add -b <branch> <path> origin/main`. **Never** `git worktree add <path> main`.

### 12.5 Spawn Site Rationale
Every spawn must have `// fire-and-forget: <reason>` OR store JoinHandle. Test-only exempt.

### 12.6 Multi-PR Wave Sequential Merge
When multiple PRs ship in the same wave (same dispatch/task_id):
1. Merge sequentially: A → rebase B on new main → re-verify CI → merge B → ...
2. Never parallel merge — later PRs have stale base
3. After each merge, remaining PRs must rebase and re-run CI before merge

Daemon enforcement: `send(sequencing: "sequential-merge-only")` signals this constraint to downstream agents. Recipients MUST merge one at a time and verify CI between each merge.

### 12.7 Linked-Issue Close Convention

A PR that resolves a tracked issue MUST carry a closing keyword (`Closes #N` / `Fixes #N` / `Resolves #N`) in its **PR body**, so the platform auto-closes the issue on merge-to-default-branch.

- A bare `#N` reference does NOT auto-close, and is ambiguous — a mention/cross-reference is not a fix (e.g. a cluster-sibling issue still open). Use the keyword only when the PR actually resolves the issue.
- **No daemon auto-close.** A daemon-side `Closes #N` parser would be redundant with native platform behavior, inert until this convention is adopted, and `gh issue close` is GitHub-only (conflicts with the multi-platform `ScmProvider` direction).
- **Reinforcement-only.** This is a convention, not a gate; lead manually closes any straggler at merge time.

`↳ 緣由 A-§12.7`

## §13. `AGEND_GIT_BYPASS=1` Usage

**TL;DR:** emergency override only. Default is bare `git`. Bypass when shim explicitly denies AND the operation is on the required-bypass list below.

### 13.1 When you should NOT use bypass

Inside your bound worktree, all routine git ops pass through the shim cleanly. Run them bare:

```bash
git status / diff / log / show
git add / commit / fetch
git push origin <your-branch>     # any branch except main
git checkout <existing-branch>    # within current repo
git reset --hard <ref>            # within your worktree
```

Don't preemptively prefix `AGEND_GIT_BYPASS=1`. Try bare git, read the deny message if it fires, then decide.

### 13.2 When bypass is required

Operations on the lifecycle/safety surface the daemon manages directly:

- `git worktree add` / `remove` / `move` — worktree pool is daemon-owned (Phase 3 lease, P0-X release)
- `git checkout main` from an agent worktree — cross-branch deny in the shim matrix
- Operator manual cleanup of orphan worktree or orphan binding (no MCP tool yet for some edge cases)
- Daemon's own internal git command — bypass is set by the daemon to prevent self-recursion through its own shim

If your op isn't on this list and you reach for bypass, you're probably solving the wrong problem.

Note: `git push origin main` is **workflow-prohibited** (PR + CI gates required), but the current shim matrix does not deny it directly. The protection comes from review process, not from the shim. Don't push to main even though `git push origin main` would not trip a shim deny today.

### 13.3 Why bypass is costly

Skipping the shim skips the safety net:

- **Phase 1 trailer skipped** — commit lacks `Agend-Agent: <name>` provenance, breaks audit trail
- **Deny matrix skipped** — risky ops (force-push to protected refs, etc.) run unguarded
- **Git registry can drift** — `git worktree add` outside the daemon's pool leaves untracked entries; subsequent leases may collide
- **Phase 5 hotspot warning skipped** — concurrent edits to flagged files don't surface on the dispatch path

These are not catastrophic individually. They erode the invariants the shim was built to maintain.

### 13.4 Default workflow

1. Run bare `git <command>`.
2. If the shim denies, read the deny message — it names the specific reason and suggests a remediation.
3. If the remediation is "use bypass," set `AGEND_GIT_BYPASS=1 AGEND_GIT_BYPASS_AGENT=<your-name>` for that one command.
4. If the remediation is something else (e.g., "use the task board to get a worktree assignment"), follow it.

`AGEND_GIT_BYPASS_UNTIL=<epoch>` exists for time-windowed bypass during multi-step operator interventions; per-command env is preferred for normal use.

### 13.5 Bug-Blocks-Its-Own-Fix Exception (Sprint 62)

When fixing a daemon binding bug (or any bug whose existence prevents the bypass-free workflow itself), the fix PR may legitimately require one-shot `AGEND_GIT_BYPASS=1` for `git add` / `git commit` / `git push` of THIS PR — because the very bug being fixed blocks the bypass-free path.

**Acceptance criteria for this exception**:
1. PR body MUST include a `## Bypass scope rationale` section explicitly framing the loop:
   - The bug being fixed
   - Why fix removes the future need for bypass
   - One-shot scope limited to this single PR
2. Bypass commits land in branch history (per §10.7); squash-merge cleans final main
3. After this PR merges + daemon binary updates, all subsequent PRs revert to ZERO BYPASS workflow
4. Operator authorization required if scope expands beyond commit/push (e.g. into worktree manipulation)

`↳ 緣由 A-§13.5`

---

## Appendix A — Rationale & Incident Log

The *why* and *when* behind normative rules. Incident narratives, activation histories, and empirical motivations relocated here from the rule text; referenced from the normative layer via `↳ 緣由 A-§X`. Reading this is optional unless you are questioning or revising a rule.

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
