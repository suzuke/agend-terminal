# Architecture-14 convergence ledger

This is the authoritative progress ledger for the Architecture-14 convergence
program. It records architecture outcomes, not PR throughput: a merged PR is
evidence for an item, but does not complete the item by itself.

## Snapshot and authority

- Snapshot date: 2026-07-16
- `agend-terminal` baseline: `31130f9324d42d3e699a7c560ffc0b07d5dc3776`
  on `main`
- vendored `agentic-git` baseline: `5c02b1421beda6590e8ebdfd137df9b49ee0bd02`
- GitHub state at the snapshot: PR #2818 is merged and included in this source
  baseline; issue #2782 is closed after its exact-revoke acceptance scope landed
- Program state: **0 done, 9 in progress, 5 pending, 0 blocked**

Evidence is ranked in this order:

1. Current-baseline source or a reproducing test.
2. A merged commit plus its exact-head and protected-main verification record.
3. A current issue, task, or decision record.
4. Historical reports, which are leads only until reconciled with current source.

When these disagree, the higher-ranked evidence wins. In particular, issue
titles and old source counts are not treated as current facts.

## Status and completion rules

| Status | Meaning |
|---|---|
| `done` | Every required slice is merged, the whole invariant is demonstrated, exact protected-main CI is green, and required runtime/deployment smoke tests pass. |
| `in progress` | At least one durable foundation or bounded slice is merged, but part of the target invariant remains open. |
| `pending` | Design or prerequisites may exist, but implementation has not yet established a material part of the target invariant. |
| `blocked` | No safe progress can be made until an external condition changes. A dependency alone normally means `pending`, not `blocked`. |

For every item, completion requires all of the following:

1. Failure-first tests cover the real production entry points and restart/replay
   boundaries, not only helpers.
2. Review is bound to the exact subject head and any stale verdict is rejected.
3. Branch CI passes, the slice merges, and CI passes again for the exact merge
   SHA on protected `main`.
4. Runtime-affecting work passes the relevant daemon restart, deployment, and
   cross-platform smoke matrix.
5. Rollback is either a tested exact-merge revert or a documented forward repair
   for durable state that cannot safely be downgraded.

Rollback must preserve the invariant: never recover by deleting evidence,
disabling an admission check, or restoring a known fail-open path. Before a
durable schema or authority cutover, a slice may use a tested exact-merge revert.
After cutover, it needs a rehearsed downgrade or forward-repair procedure that
preserves WIP, generations, journals, and unsettled obligations. An item cannot
be `done` until its applicable rollback path has been exercised.

The risk-biased execution order is:

`1 → 4 → 10 → 3 → 5 → 2 → 6 → 8 → 9 → 7 → 11 → 12 → 14 → 13`

This is not a total dependency order. Independent safety slices may proceed in
parallel, but the order must be re-evaluated after every exact-main close loop.

## Summary

| # | Architecture outcome | Status | Current remaining invariant |
|---:|---|---|---|
| 1 | Exact authority identity | in progress | Generation-scoped admission must cover crash recovery and every mutation/review/release entry point. |
| 2 | Unified durable workflow | pending | One replayable workflow episode must own dispatch through exact-main completion. |
| 3 | Ledger/outbox authority | in progress | Reporter- and CI-scoped settlements are bounded slices; separate durable rows must still converge on one action authority. |
| 4 | Strict task-board routing and owner normalization | in progress | Routing and typed blank-owner normalization are merged; membership-change settlement is not canonical end to end. |
| 5 | Usage-limit takeover | in progress | Operator-capability ingress is enforced; a generation-fenced replacement transaction and exact-once resume remain. |
| 6 | Review provenance | in progress | Exact revoke and pre-CI assignment are merged; reviewer transfer/restart/reassignment remains incomplete. |
| 7 | Ordered merge train | pending | No durable restart-resumable merge queue owns all gates. |
| 8 | Notification routing and obligation settlement | in progress | All actionable notifications must share correlated delivery and settlement semantics. |
| 9 | Session continuity | pending | Coherent checkpoint proof and exactly-once fresh-session resume are not implemented. |
| 10 | Transactional worktree lifecycle | in progress | Managed release and branch retirement now fail closed; all destructive paths still need one permit/journal, and shared-directory deletion plus submodule writes remain open. |
| 11 | Shared `RuntimeCore` | in progress | App and headless modes still have duplicate ownership and local API loopbacks. |
| 12 | Typed backend capability contract | pending | Partial model/resume types must become one complete per-backend capability matrix. |
| 13 | Typed invariant migration | in progress | String/source guards still need a systematic proof-type replacement audit. |
| 14 | Windows process reliability | pending | Native Windows process-tree, ConPTY, handle, timeout, and restart proofs are absent. |

## 1. Exact authority identity — in progress

**Target invariant.** No production mutation, review, restart, or release path
may act on a bare instance name or stale generation. Competing leases and an
A-to-B identity replacement must fail closed and remain correct after replay.

**Current evidence.** [PR #2777](https://github.com/suzuke/agend-terminal/pull/2777)
(`e13a3f30`) merged the fail-closed workspace identity guard. Its merge train
also completed exact-main verification. Current source still creates app-mode
agents with `crash_tx: None` in
[`pane_factory.rs`](../../src/app/pane_factory.rs#L340), while the watchdog
documents that the normal crash-respawn machinery is inert in this mode in
[`respawn_watchdog.rs`](../../src/daemon/per_tick/respawn_watchdog.rs#L20).
That keeps [issue #2765](https://github.com/suzuke/agend-terminal/issues/2765)
on the critical path.

**Remaining invariant.** Introduce generation/incarnation-scoped crash admission
and audit every production authority entry point so a name lookup cannot regain
authority after validation.

**Done/verification.** Deterministic same-owner restart, foreign-owner refusal,
stale-generation, competing-lease, crash-before-admission, and daemon-replay
tests must pass through real app and headless entry points. Runtime smoke must
show that a stale exit cannot publish `Restarting` for a replacement instance.

## 2. Unified durable workflow — pending

**Target invariant.** Dispatch, delivery, branch linkage, review, CI, merge,
protected-main verification, and terminal settlement are phases of one durable,
replayable aggregate. Premature done, merge, head move, and restart fail closed.

**Current evidence.** Foundations are distributed across
[PR #2763](https://github.com/suzuke/agend-terminal/pull/2763) (`7a9c12b1`,
freshness-aware PR state),
[PR #2789](https://github.com/suzuke/agend-terminal/pull/2789) (`f935ef95`,
dispatch prevalidation),
[PR #2797](https://github.com/suzuke/agend-terminal/pull/2797) (`a306be9c`,
plan-ack authority), and
[PR #2798](https://github.com/suzuke/agend-terminal/pull/2798) (`af24226f`,
durable protected-CI handoff). They do not yet form one aggregate.
[Issue #2454](https://github.com/suzuke/agend-terminal/issues/2454) is also a
boundary symptom: 18 production `crate::api::call` / `call_at` references
remain under `src/mcp/handlers` at this baseline (excluding test/repro modules
and doc comments), rather than the 35 in the historical title.

**Remaining invariant.** A single `DispatchSaga`-equivalent authority must own
phase transitions, exact subject identity, receipts, recovery, and settlement;
individual watcher or inbox rows must not become competing workflow truth.

**Done/verification.** Replay every legal transition and inject crashes between
each durable write and side effect. Premature done/merge, stale head, duplicate
delivery, daemon restart, and protected-main CI failure must all preserve one
recoverable non-terminal episode.

## 3. Ledger/outbox authority — in progress

**Target invariant.** A dropped/full/disconnected wake or daemon restart must
never lose an action or execute it twice. Terminal or discarded rows must never
become actionable again.

**Current evidence.** [PR #2766](https://github.com/suzuke/agend-terminal/pull/2766)
(`7e3277cd`) added durable reviewer-assignment authority and a pending outbox;
[PR #2788](https://github.com/suzuke/agend-terminal/pull/2788) (`029fa3a7`)
made obsolete-assignment retirement durable; and
[PR #2798](https://github.com/suzuke/agend-terminal/pull/2798) (`af24226f`)
added a durable protected-CI handoff episode. Subsequent bounded slices added
CAS-checked terminal feature-watch removal in
[PR #2808](https://github.com/suzuke/agend-terminal/pull/2808) (`76d9ab33`)
and reporter-scoped dispatch settlement in
[PR #2813](https://github.com/suzuke/agend-terminal/pull/2813) (`17df827f`).
These remain independent ledgers with related but non-identical lifecycle
rules; the new slices narrow replay ambiguity but do not establish one outbox
authority.

**Remaining invariant.** Converge action authority on row-first persistence,
stable correlation identity, monotone states, CAS claim/settlement, explicit
supersession, and restart reconciliation shared by workflow and notification
consumers.

**Done/verification.** Crash-inject before/after enqueue, delivery, ack,
supersede, and settlement; replay from every durable prefix. Prove at-least-once
delivery with idempotent action, no terminal re-fire, and no silent discard.

## 4. Strict task-board routing and owner normalization — in progress

**Target invariant.** A task ID resolves to one opaque board route; ambiguous or
unreadable routing fails closed. Empty/whitespace owners normalize to unassigned,
and ACL decisions use one canonical owner identity across membership changes.

**Current evidence.** [PR #2769](https://github.com/suzuke/agend-terminal/pull/2769)
(`64f6953e`) merged strict routing and closed
[issue #2760](https://github.com/suzuke/agend-terminal/issues/2760).
[PR #2797](https://github.com/suzuke/agend-terminal/pull/2797) (`a306be9c`)
validates plan-ack authority under the transition lock, and
[PR #2799](https://github.com/suzuke/agend-terminal/pull/2799) (`be1b3546`)
fixed cross-board, cross-lease auto-release.
[PR #2809](https://github.com/suzuke/agend-terminal/pull/2809) (`ba4dc043`)
then introduced typed `AssigneePatch` handling: omitted owner preserves the
current value, explicit null clears it, and blank/whitespace strings normalize
to unassigned at the task write boundary (`src/tasks/handler.rs`).

**Remaining invariant.** Add an explicit administrative settlement/migration
path for tasks orphaned by team or project membership changes, and prove every
secondary ACL/serialization path consumes the normalized owner representation.

**Done/verification.** The routing/ACL matrix must cover default and project
boards, unreadable/duplicate IDs, blank owners, team assignees, membership
changes, replay, and concurrent owner transitions. No actor may gain or lose
authority solely because two paths normalized the same identity differently.

## 5. Usage-limit takeover — in progress

**Target invariant.** One usage-limit episode creates one checkpoint,
notification, and takeover action. Replacement is generation-fenced; dirty or
unproven state refuses; resume is exactly once.

**Current evidence.** [PR #2759](https://github.com/suzuke/agend-terminal/pull/2759)
(`864bd5db`) merged durable UsageLimit control-plane episodes. This is Slice 1:
it establishes episode identity and durability, not transactional replacement.
[PR #2814](https://github.com/suzuke/agend-terminal/pull/2814) (`e4fd2d20`)
added the operator-invoked `usage_limit_takeover` MCP action and enforces its
operator capability at API ingress, with the real handler and ingress paths
covered together. That closes the agent-self-invocation authority gap, but the
handler remains a bounded takeover slice rather than the full replacement saga.

**Remaining invariant.** Implement the mandatory operator-invoked, idempotent
replacement transaction with generation fencing, coherent checkpoint proof,
recovery from every partial state, and typed backend refusal. Automatic takeover
remains optional until telemetry justifies it; it must not be required to close
the safety invariant.

**Done/verification.** Repeated signals, concurrent operator requests, dirty
worktrees, unsupported backends, crash during replacement, and old/new backend
output races must converge on one replacement and one resume claim.

## 6. Review provenance — in progress

**Target invariant.** Merge authority comes only from a receipt bound to exact
repository, branch, head SHA, assignment generation, reviewer incarnation, and
review class. Head/reviewer changes invalidate old authority; GitHub mirrors are
reconciled after restart.

**Current evidence.** Foundations include
[PR #2766](https://github.com/suzuke/agend-terminal/pull/2766) (`7e3277cd`),
[PR #2772](https://github.com/suzuke/agend-terminal/pull/2772) (`bce3cb39`),
[PR #2783](https://github.com/suzuke/agend-terminal/pull/2783) (`cf83697f`),
and [PR #2788](https://github.com/suzuke/agend-terminal/pull/2788)
(`029fa3a7`).
[PR #2805](https://github.com/suzuke/agend-terminal/pull/2805) (`02b7dd67`)
added an orchestrator-authorized exact-target revoke surface (slice 1 of
[#2782](https://github.com/suzuke/agend-terminal/issues/2782));
[PR #2806](https://github.com/suzuke/agend-terminal/pull/2806) (`742ecc1a`)
creates an exact-subject pending PR-state record before terminal CI and closed
[#2800](https://github.com/suzuke/agend-terminal/issues/2800); and
[PR #2807](https://github.com/suzuke/agend-terminal/pull/2807) (`afbf9c84`)
made review-worktree deletion authority-proven with durable cleanup intents.
[PR #2818](https://github.com/suzuke/agend-terminal/pull/2818) (`31130f93`)
added typed daemon-provisioned disposable-review provenance in the initial
signed binding, with exact-head and new-branch admission plus fail-closed release
gates independent of assignment authority.

**Remaining invariant.** Issue #2782's orchestrator exact-revoke scope is closed,
but the broader Architecture-14 lifecycle still requires reviewer restart/swap
and orchestrator transfer/reassignment to release or move the exact assignment
without a delete-only workaround. Reconcile receipts after restart and preserve
the exact-head merge gate across each transition.

**Done/verification.** Reviewer restart/swap, stale generation, head move,
assignment before CI completion, duplicate verdict, GitHub mirror restart, and
revoke interruption must all preserve correct merge authority.

## 7. Ordered merge train — pending

**Target invariant.** Concurrent PRs serialize against one exact base/head
identity. A rebase invalidates prior gates, restart preserves queue position,
and only the exact reviewed and CI-verified head may merge.

**Current evidence.** [PR #2763](https://github.com/suzuke/agend-terminal/pull/2763)
provides freshness-aware PR state and
[PR #2796](https://github.com/suzuke/agend-terminal/pull/2796) (`528b0c28`)
exposes exact target SHA for CI watches. The recent merge train was coordinated
successfully, but by orchestration policy rather than a durable queue. PR #2806
closed #2800, so review can now be assigned before terminal CI while exact-head
CI remains the merge gate; that prerequisite does not itself serialize a train.

**Remaining invariant.** Build a restart-resumable queue owned by the unified
workflow aggregate, with explicit invalidation and successor activation.

**Done/verification.** A three-PR clean chain and a conflicting chain must
serialize deterministically. Inject rebase, force-push, merge failure, daemon
restart, branch deletion, and newer-main commits; no stale gate may survive.

## 8. Notification routing and obligation settlement — in progress

**Target invariant.** Every actionable notification has one intended recipient,
stable correlation identity, durable delivery state, and exact obligation
settlement. Delayed duplicates and reminder residue are harmless.

**Current evidence.** [PR #2771](https://github.com/suzuke/agend-terminal/pull/2771)
(`d983dbf5`) removed a blocked-state projection split brain;
[PR #2788](https://github.com/suzuke/agend-terminal/pull/2788) (`029fa3a7`)
durably supersedes obsolete review assignments; and
[PR #2798](https://github.com/suzuke/agend-terminal/pull/2798) (`af24226f`)
settles protected-CI handoffs by durable episode. A live audit also found that
team/project membership transitions can leave old tasks with no actor able to
settle them, linking this item to item 4.

**Remaining invariant.** Replace notification-specific settlement rules with
the item-3 ledger contract, including recipient incarnation, parent/correlation
identity, supersession, timeout, discharge, and terminal-task reconciliation.

**Done/verification.** Wrong-recipient, reconnect duplicate, restart between
delivery and ack, supersede persistence failure, terminal linked task, project
membership change, and poll-reminder replay tests must settle exactly the
intended obligation and no other row.

## 9. Session continuity — pending

**Target invariant.** At the configured context threshold (80% in the program
acceptance scenario), the system records an immutable coherent checkpoint and a
separate restart action journal. A fresh session claims the exact next step once;
dirty or mid-transaction state cannot restart automatically.

**Current evidence.** Runtime-wide threshold persistence and the item-5 usage
episode foundation exist, but the full checkpoint/action protocol does not.
Current [issue #2765](https://github.com/suzuke/agend-terminal/issues/2765)
demonstrates that app-mode crash recovery is not yet generation-admitted.
Per-instance threshold configuration in
[issue #2779](https://github.com/suzuke/agend-terminal/issues/2779) is explicitly
not required for this correctness outcome.

**Remaining invariant.** Persist checkpoint proof
`Requested → Ready | Refused | Superseded` separately from the exclusive action
journal `Prepared → OldSessionStopped → NewSessionStarted → ResumeClaimed →
Consumed | NeedsOperator`, using task/binding/head/clean-state and incarnation
proofs.

**Done/verification.** Threshold repetition, dirty state, partial checkpoint,
stop/start crash, duplicate resume delivery, stale incarnation, and unhandled
query/task obligations must replay to one safe next action or an explicit
operator stop.

## 10. Transactional worktree lifecycle — in progress

**Target invariant.** Create, reuse, rebase, force reclaim, release, deletion,
janitor, retention, and GC share a normalized path lock, typed lifecycle permit,
durable journal, CAS recovery, recursive-submodule handling, and Windows-safe
rollback.

**Current evidence.** Merged foundations include
[PR #2768](https://github.com/suzuke/agend-terminal/pull/2768) (`35d5e664`),
[PR #2778](https://github.com/suzuke/agend-terminal/pull/2778) (`6a584839`),
[PR #2780](https://github.com/suzuke/agend-terminal/pull/2780) (`b2d61b81`),
[PR #2786](https://github.com/suzuke/agend-terminal/pull/2786) (`f7e717f2`),
[PR #2787](https://github.com/suzuke/agend-terminal/pull/2787) (`4c7d814d`),
[PR #2790](https://github.com/suzuke/agend-terminal/pull/2790) (`b4d2be1f`),
and [PR #2799](https://github.com/suzuke/agend-terminal/pull/2799)
(`be1b3546`). The post-ledger slices materially narrow destructive lifecycle
paths: [PR #2810](https://github.com/suzuke/agend-terminal/pull/2810)
(`0b127f2a`) delegates daemon-managed `repo release` to the canonical guarded
release using an exact binding fingerprint, marker identity validation, and
WIP preservation;
[PR #2815](https://github.com/suzuke/agend-terminal/pull/2815) (`06efae12`)
enforces branch-retirement disposition and occupancy gates; and
[PR #2816](https://github.com/suzuke/agend-terminal/pull/2816) (`1d83b423`)
makes checkout-recovery sweeps respect active path locks; and
[PR #2818](https://github.com/suzuke/agend-terminal/pull/2818) (`31130f93`)
adds typed `disposable_review` checkout provenance, exact provisioned-head CAS,
new-branch proof, and terminal-task/occupancy/PR cleanup gates for self-provisioned
review worktrees. These are foundations, not yet one durable lifecycle transaction.
[Issue #2764](https://github.com/suzuke/agend-terminal/issues/2764)
remains: deletion captures fleet state before removal, but
[`cleanup_working_dir`](../../src/agent_ops.rs#L504) receives only home/name/path
and trusts on-disk identity artifacts, so another live instance sharing the
canonical directory is not always rejected. In vendored `agentic-git`,
[issue #34](https://github.com/suzuke/agentic-git/issues/34) remains because
`submodule` writes are not classified as mutating.

**Remaining invariant.** Migrate all mutation roots and leaves to the same typed
permit/capability and durable journal, including janitor/retention/GC and branch
creation; fix shared live-owner deletion and the upstream submodule
classification before updating the gitlink.

**Done/verification.** Race create/reuse/rebase/release/delete under aliases,
symlinks, corrupt bindings, nested submodules, process death, and concurrent new
leases. Recovery must preserve WIP and converge without an unjournaled destructive
operation on macOS, Linux, and Windows.

## 11. Shared `RuntimeCore` — in progress

**Target invariant.** Owned TUI mode and headless `run_core` share one service
ownership model for registry, tick, recovery, API, and shutdown. Attached mode
is an explicit non-owner. No worker is duplicated and restart order is defined.

**Current evidence.** Extraction and ordering foundations include
[PR #2770](https://github.com/suzuke/agend-terminal/pull/2770) (`054125d3`)
and [PR #2775](https://github.com/suzuke/agend-terminal/pull/2775)
(`a1d82f47`). The remaining scale is visible in source:
[`run_app`](../../src/app/mod.rs#L678) still spans about 1,055 lines to its end
at line 1732, with no `AppState` struct, and 15 production local-API loopbacks
remain under `src/mcp/handlers`. These keep
[issue #2453](https://github.com/suzuke/agend-terminal/issues/2453) and
[issue #2454](https://github.com/suzuke/agend-terminal/issues/2454) current.
The app-mode crash gap in issue #2765 is also an ownership symptom.

**Remaining invariant.** Extract one owned runtime/service graph and direct
in-process command boundary, then make TUI a client of that core rather than a
second daemon implementation.

**Done/verification.** A mode matrix must prove exactly one registry/tick/API/
recovery owner, causal startup and reverse shutdown order, restart convergence,
and attached-mode non-ownership. The production MCP paths must not loop through
the local API merely to reach same-process state.

## 12. Typed backend capability contract — pending

**Target invariant.** Every registered CLI/custom/raw backend explicitly declares
model, restart/resume, state signal, usage-limit, checkpoint, native delegation,
and nested-execution capabilities. Unsupported operations return typed errors;
callers never infer support from terminal text or flags.

**Current evidence.** [PR #2757](https://github.com/suzuke/agend-terminal/pull/2757)
(`5aee597e`) added capability-gated explicit model intent and typed `set_model`.
Source also has partial types such as
[`ResumeMode`](../../src/backend.rs#L258) and
[`model_capability`](../../src/backend.rs#L799). The residual of
[issue #2744](https://github.com/suzuke/agend-terminal/issues/2744) is only
automatic observation/capture of an in-session model change; the explicit path
is already implemented. [agentic-git issue #26](https://github.com/suzuke/agentic-git/issues/26)
tracks the missing machine-verifiable embedder/delegation contract.

**Remaining invariant.** Consolidate the partial fields into one typed contract
and force every control-plane operation to branch on it. Native child execution
must be deny-first until writer identity, binding, event, and quiescence proofs
are available.

**Done/verification.** A generated matrix must cover every registered backend
and each operation with supported, unsupported, and degraded cases. Custom/raw
backends and unknown versions must fail with stable typed errors, never guessed
behavior.

## 13. Typed invariant migration — in progress

**Target invariant.** Every retired source-string or grep guard is replaced by a
stronger proof type, private API, or runtime admission check plus a real-entry
and replay test. Any retained scan has a written threat-model rationale.

**Current evidence.** [PR #2773](https://github.com/suzuke/agend-terminal/pull/2773)
(`e4841350`) added a fixture-only real-git provenance seam and
[PR #2774](https://github.com/suzuke/agend-terminal/pull/2774) (`1fe1461d`)
exercised the production nonblocking file lock. These are bounded examples, not
a systematic retirement. Upstream
[agentic-git issue #26](https://github.com/suzuke/agentic-git/issues/26) lacks a
machine-verifiable embedder contract, and
[issue #34](https://github.com/suzuke/agentic-git/issues/34) exposes a concrete
classification gap.

**Remaining invariant.** Inventory all load-bearing scans after items 1–12 and
14 stabilize, classify each by threat model, and replace it only when the new
typed boundary is strictly stronger. Supporting module split
[agentic-git issue #30](https://github.com/suzuke/agentic-git/issues/30) is not a
completion prerequisite.

**Done/verification.** For every removal, demonstrate the old alias/re-export/
rename bypass as RED and the new production entry point as GREEN, including
restart/replay. Audit output must enumerate and justify every retained scan.

## 14. Windows process reliability — pending

**Target invariant.** Windows Job Object process-tree kill, ConPTY lifecycle,
handle closure, file-lock behavior, path/identity handling, timeout diagnostics,
log retention, restart, and shutdown are deterministic with no long silent hang.

**Current evidence.** General tick-stall diagnostics exist, but no complete
Windows reliability program or native runtime proof has landed. macOS/Linux
success and cross-compilation are not evidence for Windows process semantics.

**Remaining invariant.** Establish native Windows process ownership and
diagnostic contracts, then close each process/PTY/handle failure mode under a
real Windows runner.

**Done/verification.** Native Windows CI and runtime smoke must deterministically
cover child/grandchild tree kill, ConPTY open/close, leaked handles, lock
contention, Unicode/long paths, timeout with retained logs, daemon restart, and
shutdown. A bounded watchdog must always produce actionable diagnostics rather
than a silent hang.

## Source-validated issue intake

This matrix decides whether an issue changes Architecture-14 scope. “Excluded”
does not mean invalid; it means the issue is not required to close these 14
correctness outcomes.

| Issue | Snapshot classification | Architecture-14 mapping | Disposition/evidence |
|---|---|---:|---|
| [agend-terminal #2453](https://github.com/suzuke/agend-terminal/issues/2453) | Confirmed architecture debt | 11 | `run_app` is about 1,055 lines and no `AppState` exists. |
| [agend-terminal #2454](https://github.com/suzuke/agend-terminal/issues/2454) | Confirmed architecture debt | 2, 11 | 15 production MCP-to-local-API loopbacks remain; the title's count of 35 is stale. |
| [agend-terminal #2744](https://github.com/suzuke/agend-terminal/issues/2744) | Partial residual | 12 | Explicit typed model intent merged in #2757; only automatic in-session observation/capture remains. |
| [agend-terminal #2760](https://github.com/suzuke/agend-terminal/issues/2760) | Fixed on main; closed | — | Strict routed lookup merged in #2769; do not keep it on the critical path. |
| [agend-terminal #2762](https://github.com/suzuke/agend-terminal/issues/2762) | **Excluded optional feature** | — | CLI fallback stays experimental unless measured MCP discovery failures establish a correctness need. |
| [agend-terminal #2764](https://github.com/suzuke/agend-terminal/issues/2764) | Confirmed current safety bug | 10 | Cleanup does not use the pre-delete fleet snapshot to reject every other live owner of a shared canonical directory. |
| [agend-terminal #2765](https://github.com/suzuke/agend-terminal/issues/2765) | Confirmed current bug | 1, 9, 11 | App-created agents still use `crash_tx: None`; restart publication is not generation-admitted. |
| [agend-terminal #2779](https://github.com/suzuke/agend-terminal/issues/2779) | **Excluded optional feature** | — | Per-instance thresholds add configurability, not a missing correctness invariant. |
| [agend-terminal #2781](https://github.com/suzuke/agend-terminal/issues/2781) | **Excluded separate P3 bug** | — | Decimal percentage formatting/Kiro regex inconsistency is real but not an Architecture-14 dependency. |
| [agend-terminal #2782](https://github.com/suzuke/agend-terminal/issues/2782) | Acceptance scope fixed; closed | 6 | #2805 added orchestrator exact revoke and closed the issue; restart/reviewer-swap transfer and complete reassignment settlement remain broader Architecture-14 hardening. |
| [agend-terminal #2800](https://github.com/suzuke/agend-terminal/issues/2800) | Fixed on main; closed | — | #2806 creates exact-subject pending PR state before terminal CI; do not keep it on the critical path. |
| [agentic-git #26](https://github.com/suzuke/agentic-git/issues/26) | Confirmed contract gap | 12, 13 | No machine-verifiable embedder/binding/event contract exists. |
| [agentic-git #30](https://github.com/suzuke/agentic-git/issues/30) | Supporting refactor only | — | The library remains large, but a module split does not itself close an invariant. |
| [agentic-git #34](https://github.com/suzuke/agentic-git/issues/34) | Confirmed current safety gap | 10, 13 | `submodule` and `submodule--helper` are absent from mutating-command classification. |

Dependency-only updates, formatting cleanup, optional CLI fallback (#2762),
per-instance threshold configuration (#2779), the decimal-context P3 (#2781),
and the agentic-git module split (#30) must not be counted as Architecture-14
progress or used to delay a correctness close loop.

## Maintaining this ledger

Every update must:

1. Advance both source baselines to exact immutable SHAs.
2. Re-query linked issue and PR state and re-check cited source lines.
3. Record merged slices as evidence without changing item status unless the
   whole remaining invariant and completion gate are proven.
4. Add newly discovered correctness gaps to the issue matrix before mapping
   them to an item; optional features remain explicitly excluded.
5. Preserve item numbering and names. Add detail within an item instead of
   renumbering the program.
6. Record the exact protected-main CI and runtime/deployment evidence before
   changing an item to `done`.
