# Spike: agend-git shim force-push gap (t-…78445-1, #2673 follow-up)

> **Historical snapshot:** This spike preserves the pre-fix investigation. It is
> not the current push-policy contract; see
> [`docs/GIT-BEHAVIOR.md`](docs/GIT-BEHAVIOR.md) and current source/tests.

Spike-first (no production code except part (a) comment softening — done). DUAL-class
(fleet-safety / data-loss-adjacent). All file:line in `src/bin/agend-git.rs` unless noted.

## The gap (confirmed at file:line)
`push_protected_violation` (`:2068-2103`, called from the push arm `:241-256`) treats
force as "HOW not WHAT" (doc `:2066`). `push_dest_refs` (`:2035-2045`) does
`a.strip_prefix('+')` and extracts only the DESTINATION ref; the gate denies only:
bulk (`--all`/`--mirror`, `:2116-2121`), wildcard dest (`*`), an explicit protected
dest, or a no-refspec push under `push.default=matching` (`:2133-2141`). A non-protected
dest like `feat/x` matches none → `None` → **allow**. Force never enters the decision:
`is_bulk_push_flag` explicitly excludes `--force`/`-f` (`:2116-2121`, test `:1013-1028`),
and `push_dest_refs_normalizes_refspec_targets_s3` (`tests.rs:904-924`) *asserts*
`push --force -u origin feat/x` is allowed. No `--force-with-lease` handling anywhere;
push audit fires ONLY on deny (`deny_trust_root :224-231`, `deny_protected_ref :247-254`)
— an allowed force-push emits no event.

**Impact**: any agent, any time, can `git push --force origin <feature>` and overwrite
existing commits on that origin branch (another agent's / another session's work, or a
wrong-based branch — e.g. #2673 state-3's residual edge). Same fleet-safety family as
#2662 (denylist/trust-root).

## Step 1 — does command-authority (t-777-1) cover this? NO.
The `t-777-1` proposal doc (`COMMAND-AUTHORITY-SPIKE*.md`, fixup-dev-2) is **not on disk**
(searched repo + `workspace/*` + `worktrees/`; fixup-dev-2's workspace was GC'd; 777-1
survives only in JSON logs). The accessible command-authority artifacts —
`workspace/gapfix-dev2/INSTANCE-ACTIONIZE-SPIKE.md` + `P0-CLASSIFIER-DESIGN.md` (task
14440-2) — are scoped to the 12 MCP **Instance-family tools** + `operator_gate` classify(),
with ZERO git/push/force content. Command-authority gates MCP *tool* calls; the git shim
is a *distinct enforcement layer* (intercepts the raw `git` subprocess). Force-push
authority is therefore out of command-authority scope regardless of the missing doc →
**proceed independently; nothing to defer to.**

## Threat / context model (proportionate)
Single-user single-machine fleet ([[feedback_security_single_user_single_machine]]).
Defending TWO failure modes: (a) benign agent footgun — a wrong-based / stale-based
`--force` overwrites real commits (this task's origin); (b) injected agent deliberately
destroying an origin branch. Force-push is a CAPABLE op, not inherently a footgun
([[feedback_restrict_only_footguns_not_capable_ops]]) — the footgun is specifically the
UNCONDITIONAL overwrite. So the fix should remove the footgun WITHOUT removing the
capability. Protected refs (main/master) are already hard-denied; this is only about
feature branches.

## Options (with cost / agent-workflow compat)
- **(i) Require `--force-with-lease`** (RECOMMENDED). Deny `--force`/`-f`/`+<dest>` to a
  non-protected feature dest UNLESS it carries `--force-with-lease[=…]` (or
  `--force-if-includes`); deny message teaches the safe form. Rationale: `--force-with-lease`
  refuses to overwrite if the remote moved since the pusher's last fetch — it kills the
  "clobber unseen commits" footgun while KEEPING the legitimate rebase-then-force workflow.
  · Compat: the common agent flow (rebase/amend → force-push) still works when the pusher
    has fetched (lease baseline = its remote-tracking ref); if someone else pushed, the
    lease FAILS loudly = exactly the footgun being caught. Friction = agents must switch
    from `--force` to `--force-with-lease` (the deny msg guides them). `+<dest>` refspec
    force has no lease form → deny it, steer to the flag form.
  · Cost: localized edit to `push_protected_violation` (add a force-detect + lease-detect
    arm) + tests (bare-force→deny, lease-force→allow, `+dest`→deny) + a `deny_force_no_lease`
    audit event. ~1 function + test file. Blast radius contained (§ below).
  · Risk: "擋太死逼 agent 繞 shim (AGEND_GIT_BYPASS)" — mitigated: lease is a 1-word change,
    not a capability loss; the deny message makes the fix obvious.
- **(ii) Warn + audit, allow.** Emit an `allowed_force_push` event + stderr warning on bare
  force-to-feature, but allow. Zero workflow friction + gives traceability, but does NOT
  prevent the data-loss. Good as a *complement* to (i) (audit the denies/allows) or a
  weaker standalone first step.
- **(iii) Status quo + docs.** Cheapest, zero safety. Not recommended given a merged PR
  (#2673) already documents relying on a backstop this gap defeats.

**Recommendation: (i) + the audit half of (ii)** — require `--force-with-lease` for
feature-branch force-pushes (footgun removed, capability kept), and emit a deny audit
event for observability. Proportionate to single-user context; mirrors the existing
protected-ref deny pattern.

## Blast radius (for the impl, if vetted)
- Decision: `push_protected_violation` `:2068-2103` (+ maybe `push_dest_refs` to surface the
  raw `+`/force flag instead of discarding it) — the force/lease detection needs the flag,
  which `:2035-2045` currently strips.
- Tests: `src/bin/agend-git/tests.rs:904-1028` (several ASSERT force-to-feature is allowed —
  they must be updated to the new contract; add the deny/allow-lease cases). Per lead's
  fixture guidance, adjust to the new semantics, don't weaken the rule.
- Audit: add `deny_force_no_lease` via the existing `write_git_event_typed` (~`:2413`).

## Part (a) — DONE
Softened the two overstated state-3 comments in
`src/mcp/handlers/dispatch_hook/branch_start_point.rs` (state table + inline): the non-ff
"second line of defense" now reads as a PARTIAL backstop that a bare `git push --force`
bypasses (cites t-…78445-1). Comment-only, no logic change.

## Open questions for lead / operator
1. Accept the `--force-with-lease` friction (hard deny, option i), or start with warn+audit
   (ii) only? I recommend (i) — data-loss surface, lease keeps the capability.
2. `--force-with-lease` baseline compat: confirm agents typically fetch before force-pushing
   (else bare-lease can misbehave). If not, the deny msg should say `--force-with-lease=<ref>:<sha>`.
3. Also cover `+<dest>` refspec force (I propose deny → steer to the flag form) — OK?
4. Scope: this touches the SHIM only (not MCP-tool authority). Confirm we're not waiting on
   the (missing) 777-1 doc.
