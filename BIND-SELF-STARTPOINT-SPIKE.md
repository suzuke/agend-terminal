# Spike: bind_self re-provision start-point data-loss (t-…83936-5)

> **Historical snapshot:** This spike records a point-in-time investigation and is
> not the current `bind_self` contract. Verify behavior against
> [`docs/FEATURE-worktree.md`](docs/FEATURE-worktree.md) and the current source.

Spike-first (lead vet BEFORE impl — data-loss surface, DUAL). All file:line in
`src/mcp/handlers/dispatch_hook/mod.rs` unless noted.

## Root cause (confirmed at file:line)
`ensure_branch_exists` (`mod.rs:693`) gates on the **LOCAL** ref only:
`mod.rs:749` `git rev-parse --verify refs/heads/<branch>`.
- **branch_exists = true** (`mod.rs:753-846`): fetches `origin/<branch>` and
  `update-ref`s the local ref to it (`:778-793`) → correct tip. `from_ref` is
  only used (ff-align) when `origin/<branch>` is ABSENT (`:794-844`).
- **branch_exists = false** (`mod.rs:847-882`): "Step 2: create from `from_ref`"
  → `git branch <branch> <from_ref>` (`:882`), `from_ref` hard-coded to
  `"origin/main"` by the bind caller (`:487`). **This arm NEVER fetches or
  consults `origin/<branch>`** (`:859-870` fetch only `from_ref`).

So: **local `refs/heads/<branch>` absent + `origin/<branch>` exists → new local
branch is based on origin/main; the branch's remote commits are orphaned the
moment the agent commits.** The two arms are INCONSISTENT: EXISTS gives
`origin/<branch>` precedence over `from_ref`; CREATE ignores `origin/<branch>`.

## Why it's conditional (explains why my own binds were fine)
The trigger is a MISSING local ref: fresh canonical clone / pruned-after-release
re-bind (= canonical-incident recovery, dev2's case). When the local ref
survives across dispatch cycles, the EXISTS path runs and picks the right tip.

## Blast radius (ALL reach `mod.rs:487`→`ensure_branch_exists`)
- `bind_self`: `worktree.rs:33`→`:111`→lease→`:487`.
- dispatch auto-bind (`send kind=task`): `comms.rs:269`→lease→`:487`.
- `repo checkout bind:true`: `ci/checkout.rs` → `ensure_branch_exists(from_ref)`.
- `bind_self rebase_mode` recovery + release/re-claim: safe-repair then same lease.
- NOT affected: reuse-live-worktree short-circuit (`mod.rs:483-488`, `reused` skips
  ensure_branch_exists). → A single fix in ensure_branch_exists covers all.

## Fix (mirror the EXISTS-path precedence in the CREATE path)
Insert between the branch_exists block (`:846`) and "Step 2" (`:847`): when the
LOCAL ref is absent, fetch `+refs/heads/<branch>:refs/remotes/origin/<branch>`
(bounded, best-effort), and if `origin/<branch>` exists →
`git branch <branch> refs/remotes/origin/<branch>` and return; else fall through
to the existing from_ref create. Safe: there is NO local ref here, so nothing to
clobber (no ff-check needed — unlike the EXISTS path). Centralized = fixes every
entry point at once. `origin` is the working-branch push remote (#2047), correct
regardless of a fork `from_ref`.

## Return value (n_branch / auto_created)
Purely observability (surfaced as `n_branch` in the response JSON via
`ci/checkout.rs`; no destructive gate). For create-from-`origin/<branch>` return
**(false, fetched)** = "pre-existing on the remote, materialized locally" —
consistent with the EXISTS path's `(false, …)` and accurate (branch has history,
is not brand-new).

## Test plan (RED-first)
Bare-remote fixture mirroring `tests.rs:108-131` (`git init --bare`, push):
1. **RED regression**: origin/main @A; push `origin/<branch>` @B (B≠A, B not an
   ancestor of A); ensure LOCAL `refs/heads/<branch>` ABSENT →
   `ensure_branch_exists(branch, "origin/main")` → assert `refs/heads/<branch>` ==
   **B** (origin/<branch>), NOT A. Fails RED on current code (lands on A).
2. remote has NO `origin/<branch>` → still creates from from_ref (no regression).
3. Full `dispatch_hook` suite green — esp. `..._refreshes_stale_origin_before_
   create_1755` (`tests.rs:85`, its branch is not on origin → unaffected) and the
   EXISTS-path `..._syncs_stale_local_to_origin` (`tests.rs:2655`).

## Vet questions
Q1. Return `n_branch=false` for create-from-origin (my rec, consistent+accurate) OK?
Q2. Centralize in `ensure_branch_exists` (fixes all entry points) — confirm scope
    vs bind_self-only. I rec centralized (all entries want the same safety).
Q3. Extra bounded fetch of `origin/<branch>` only on the local-ref-absent
    (provision/recovery) path — acceptable? (steady-state dispatch of an existing
    local ref is untouched).

## POST-VET IMPL FINDING (2026-07-07) — fail-closed blast radius + design fork
Lead vetted GO with a fail-closed refinement (fetch-fail → Err). Implementing it
(strict) surfaced two facts sent to lead as an A/B query (correlation t-…83936-5):

1. **Blast radius (strict)**: ~25 of 66 dispatch_hook tests fail — all because
   `setup_test_repo` (used 33×) configures a DELIBERATELY UNREACHABLE origin
   (`file:///dev/null`) + staged `refs/remotes/origin/main`; strict `git fetch
   origin` always fails there → every "create new branch" test hits the
   fail-closed Err. Making the fixture reachable also flips ~5 `!fetch_attempted`
   assertions. = core test-infra migration.
2. **Premise partly invalid (verified in /tmp)**: `git clone` populates
   `refs/remotes/origin/*` for ALL remote branches with NO extra fetch
   (`rev-parse refs/remotes/origin/spike/xyz` → exit 0 on a fresh clone). Recovery
   (fresh clone; prune-after-release removes only local `refs/heads`, never
   `refs/remotes/origin/<branch>`) ⇒ the remote-tracking ref is ALREADY present.
   So a **no-fetch check of `refs/remotes/origin/<branch>` catches dev2's case with
   0 test breakage**; strict fetch+Err only adds cover for "stale clone + branch
   pushed after last fetch + offline now", a non-incident edge, and blocks ALL
   new-branch dispatch during any origin outage.

**A/B to lead**:
- A (strict, as vetted): fetch-fail→Err + migrate setup_test_repo reachable + fix
  ~5 assertions + 3 bespoke. Most conservative, large diff, outage blocks all provision.
- B (hybrid, RECOMMENDED): (1) refs/remotes/origin/<branch> present → create from it
  (no fetch, 0 breakage); (2) absent → bounded best-effort fetch + recheck; (3) still
  absent → create from from_ref if any refs/remotes/origin/* view exists, else Err.
  Catches incident + online-stale, fixture-compatible, fail-closes only when totally
  blind. Narrow uncovered edge: offline + stale-clone + branch pushed since last fetch.

Current code = A (strict, uncommitted). Awaiting lead A/B before finalizing + tests.

## RESOLUTION (2026-07-07) — lead chose B (hybrid); IMPLEMENTED
Lead ruled B (evidence corrected the fetch premise). Implemented in
ensure_branch_exists, extracted to `branch_start_point::create_new_branch` (the whole
create path — Step 1.5 guard + #1755 from_ref create) to keep mod.rs under its 1575-LOC
grandfathered ceiling (was 1574; pure move). 4 lead-required items done:
(1) state-3 fail-open tradeoff + non-ff backstop commented at the code; (2) 4 state tests
pin all states (state 1 RED-verified); (3) layer-1 lag-vs-EXISTS-arm asymmetry noted in
the code comment (pre-fetch advances the view to tip when online); (4) "view non-empty"
includes origin/HEAD, commented. B breaks 0 existing tests except 2 fixtures with no
origin view (#2010 fork, p780 broken-origin) → each given a staged refs/remotes/origin/main
(a real clone always has one). Full gate green.
