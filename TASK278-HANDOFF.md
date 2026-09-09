# Task 278 handoff

Branch: `fix/3582-runtime-ownership-278`

Code-change head: `28206e15990b77cb4233ec464ed41da3ebcc2bfb`

The latest main commit `07ca9298cfbbf7a0c5f46d4d9135e373141500b7` was integrated into the PR3582 base `5351eff81027bb11eb9c79b93d27221fa85c6300` by normal merge. The runtime fixtures now use pre-spawn guards, retained `Child` anchors, bounded exact-handle teardown, and explicit cleanup assertions. The stopped-leader test proves that a reaped leader plus a live sibling group member does not authorize runtime cleanup. The launched-worker test proves the retained worker remains live through the recovery-required assertion and exits only after explicit cleanup.

Validation is complete for the owned runtime scope, formatting, clippy, and diff checks. The full binary suite was run and is red with 7,491 passes and 9 unrelated fixture failures; see `TASK278-EVIDENCE.md` for the exact failure names and commands. Do not describe the full suite as green.

Next action: orchestrator/reviewer should inspect the two normal commits and decide whether the unrelated full-suite failures require separate follow-up. No publication or merge has been performed by this branch.
