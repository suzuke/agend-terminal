# Sprint 55 P0-B — Unified Bind Dynamic Binding (FINAL)

**Status**: Phase 3 lead-synthesized design — pending operator review per m-3637 design-first directive
**Date**: 2026-05-08
**Phase 1 input**: dev RCA `docs/internal/DESIGN-sprint55-p0b-unified-bind-v1-dev-rca.md`
**Phase 2 input**: reviewer challenge `docs/internal/CHALLENGE-sprint55-p0b-unified-bind-reviewer.md`
**Authors**: dev (primary RCA) + reviewer (challenge) + lead (synthesis)

## §1 Executive summary

**Problem**: Current `bind_self` MCP tool requires explicit `repo: "owner/name"` GitHub-format argument. `ci(action: watch)` likewise requires explicit `repo` arg. Operators must compute and pass GitHub identifier even when the local source_repo path's `git remote get-url origin` would yield the same answer. Plus: post Sprint 54 PR #519 fleet.yaml `source_repo:` field, the binding chain has 3-tier resolution but no caller-facing dynamic API uses it cleanly.

**Final design** (per dev RCA + reviewer Phase 2 challenge):
- `bind_self(source_repo: PathBuf, branch: &str)` — daemon resolves owner/repo via existing `derive_repo_from_remote` + `parse_github_owner_repo` parsers
- `ci(action: watch)` accepts no `repo` arg → reads sender's `binding.json::source_repo` → derives owner/repo
- Backward-compat: dual-arg acceptance (legacy `repo: "owner/name"` still works) for **two-sprint deprecation cycle** (Sprint 55 → 56 warn → Sprint 57 remove)
- Three-tier source_repo resolution: explicit arg → fleet.yaml `source_repo:` → working_directory fallback (preserved per Sprint 54 PR #519)
- **EC10 SPLIT**: auto-bind dispatch trigger scope filter → separate **P0-C** doc (see `DESIGN-sprint55-p0c-auto-bind-task-class-filter.md`)
- **EC4 non-GitHub remote**: add `repo: Option<String>` companion field to `InstanceConfig` for explicit override

**Premise check**: Code surface intact. All hooks for proposed unified binding exist on `26e7331`:
- `parse_github_owner_repo` handles HTTPS/HTTP/SSH/`ssh://git@` (`dispatch_hook/mod.rs:95-111`)
- `derive_repo_from_remote` runs `git remote get-url origin` with bypass (`mod.rs:118-130`)
- `dispatch_auto_bind_lease` consumes `source_repo` via fleet.yaml resolution (PR #519)
- `bind_full` persists `source_repo` into `binding.json`
- `handle_watch_ci` idempotent + append-aware (Sprint 54 P0-1)

## §2 Chosen design: dual-arg bind_self with derivation pipeline

```rust
// New unified shape
bind_self(source_repo: PathBuf, branch: &str) -> Result<{worktree_path, branch, owner_repo}>

// Backward-compat (Sprint 55 only, warn-log)
bind_self(repo: "owner/name", branch: ...) -> existing path with deprecation warning

// Both args present
bind_self(repo: ..., source_repo: ...) -> reject with code: "ambiguous_args"

// ci(watch) auto-derive
ci(action: "watch")  // no repo arg
  → reads caller's binding.json source_repo
  → derives owner/repo via existing parsers
  → invokes existing watch logic
```

### Three-tier source_repo resolution chain (PR #519 preserved + observability per reviewer m-25)

1. **Explicit arg** in `bind_self(source_repo=...)` — wins
2. **Fleet.yaml `source_repo:`** — second-tier fallback (already wired in `dispatch_auto_bind_lease:36-43`)
3. **`working_directory`** — third-tier fallback (PR #519 chain preserves)
4. **`home/workspace/<agent>` stub** — fourth-tier (deprecation candidate; emit warning when hit)

**Per reviewer challenge**: emit info log naming which tier was hit; emit WARN when tier 4 is hit (silent stub-source produces wrong provenance). Optional strict-mode env flag to reject tier 4 in production.

## §3 Rejected alternatives

- **Auto-detect upstream remote when origin is fork** — rejected (magical/surprising; honor explicit `repo` arg instead)
- **Multi-remote priority parser (origin/upstream/etc)** — rejected for P0-B core; `remote: "upstream"` arg is future extension if needed
- **Hard-cutover deprecation (one-sprint)** — rejected per reviewer; **two-sprint window** chosen for caller migration runway
- **Auto-clone fork per agent** — rejected (complexity explosion; multi-agent shared source_repo already supported per Sprint 53 P0-1.5)
- **EC10 task-class filter bundled in P0-B core** — rejected per reviewer; **split as P0-C** (separate doc)
- **EC10 in minimal `bind: false` opt-out form** within P0-B — acknowledged as alternative if leadership insists on bundling (defer to operator decision)

## §4 Edge case adjudication (10 + 5 reviewer-added = 15 total)

### EC1 — `ci(watch)` invoked without prior `bind_self`
**Recommendation**: Surface explicit error `{"error": "ci(watch) needs explicit 'repo' arg OR active binding (call bind_self first)", "code": "no_binding_no_repo"}`. No silent cwd-based derivation.

### EC2 — Fork vs upstream
**Recommendation**: Honor explicit `repo` arg over derivation. Default derivation reflects local origin (fork or upstream, whichever it is). Operator workflow choice via fleet.yaml `repo:` companion field (NEW per EC4 reviewer challenge).

### EC3 — Multi-remote (origin + upstream + others)
**Recommendation**: Inherit current behavior — parse `origin` only. Future extension: `remote: "upstream"` arg if needed.

### EC4 — SSH vs HTTPS URL normalization
**Recommendation**: No new code. `parse_github_owner_repo` already handles 4 URL forms (Sprint 53 P0-2). 

**Reviewer-added**: For non-GitHub remotes (parser returns None) → add `repo: Option<String>` companion field to `InstanceConfig`. Derivation order:
1. Explicit `bind_self(repo=...)` arg
2. `binding.repo` / fleet.yaml `repo:` override
3. Derive from source_repo origin (GitHub parser)

If all three fail → structured error `code: "non_github_remote_no_override"`.

### EC5 — Detached HEAD
**Recommendation**: No impact. `branch` is explicit caller arg; daemon doesn't read local HEAD.

### EC6 — Migration (fleet.yaml `source_repo:` agents from PR #519)
**Recommendation**: Three-tier resolution chain (per §2 above) + observability:
- INFO log naming which tier was used per resolution
- WARN log when tier 3 (working_directory) hit
- WARN log when tier 4 (workspace stub) hit
- Optional strict-mode env flag rejects tier 4 in production

### EC7 — Stale ci-watch on `release_worktree`
**Recommendation**: `release_worktree(agent)` should unsubscribe ci-watches matching released `repo + branch` exactly; leave unrelated watches untouched.

**Reviewer-added challenge**: This audit is **Phase 3 IMPL prerequisite owned by dev**. Reviewer demands explicit proof in PR (test + trace) that release_worktree cleans only matching watch scope.

### EC8 — Multi-agent shared source_repo
**Recommendation**: Inherit Sprint 53 P0-1.5 cross-agent registry check + Sprint 54 P0-1 idempotent append. Multiple agents on different branches share source_repo cleanly.

### EC9 — Argument deprecation
**Final**: **Two-sprint deprecation window** (per reviewer m-25):
- **Sprint 55**: dual-arg support + warning telemetry (`repo` arg → warn-log; `source_repo` arg → preferred)
- **Sprint 56**: tighten warnings to explicit migration notices (each `repo` use logs ERROR-level + caller name)
- **Sprint 57**: remove legacy `repo` arg path; hard-fail on `repo`-only callers

If both args present → reject with `code: "ambiguous_args"` (immediate rejection, not deprecation-window-soft).

### EC10 — **SPLIT to P0-C** (auto-bind dispatch trigger scope)
Reviewer's verdict: distinct semantic from P0-B's HOW-to-bind contract. Split as P0-C separate dispatch.

See `docs/DESIGN-sprint55-p0c-auto-bind-task-class-filter.md` for full P0-C design.

### EC11 (reviewer-added) — Concurrent `bind_self` same agent, different branches
**Recommendation**: Per-agent in-flight guard (mutex or atomic counter) to prevent interleaved binding state writes. Bind operations serialized per-agent.

### EC12 (reviewer-added) — No remote configured in source_repo
**Recommendation**: Explicit error `code: "no_remote_configured"` with remediation hint: "configure origin remote OR pass explicit `repo` arg".

### EC13 (reviewer-added) — Branch absent on origin
**Recommendation**: Reject by default with `code: "branch_not_found"`. Do NOT auto-create remote branch silently.

If operator wants auto-create, add explicit `auto_create: bool` flag (Sprint 56+ extension).

### EC14 (reviewer-added) — Source_repo path exists but is not a git repo
**Recommendation**: Explicit validation via `git -C <path> rev-parse --git-dir` before deriving anything. Error `code: "not_a_git_repo"`.

### EC15 (reviewer-added) — Stale binding source_repo path deleted after bind
**Recommendation**: `ci(watch)` fallback validates source_repo path exists before reading. If gone → fail deterministically with `code: "source_repo_path_deleted"`. Do NOT silently fall back to cwd.

## §5 Cross-P0 integration testing (per reviewer m-25)

When P0-A + P0-B both land, integration test matrices:

### Matrix 1: Unified bind + reply guard interoperability
- Channel-reply attribution behavior remains correct while binding state mutates
- Bind agent → telegram input → mid-bind branch switch → verify reply lands on correct channel

### Matrix 2: Recovery test
- Restart daemon during bound session + ci-watch active + reply action
- Verify both P0-A reply attribution + P0-B binding restoration behave correctly post-restart

### Matrix 3: Negative-path
- non-git repo, no origin remote, non-GitHub remote, missing branch, stale path

### Matrix 4: Concurrency
- Simultaneous `bind_self` from same agent on different branches
- Two agents sharing same source_repo on different branches

## §6 LOC + Tier estimate

| Component | LOC est | Notes |
|---|---|---|
| `handle_bind_self` arg shape change (`worktree.rs:37-102`) | ~40-60 | Accept `source_repo: PathBuf`, dual-arg backward-compat with warn-log, invoke `derive_repo_from_remote` |
| `handle_watch_ci` auto-binding lookup (`ci/mod.rs:159-285`) | ~30-50 | Read sender's `binding.json::source_repo` when `repo` arg absent |
| Backward-compat shim (legacy `repo` arg + warn-log) | ~10-20 | Conditional handler entry; deprecation path |
| `release_full` ci-watch unsubscribe (EC7) | ~15-25 | Audit current behavior first; add unsubscribe loop matching `repo + branch` if absent |
| `repo: Option<String>` companion field (EC4) | ~10-15 | Additive `InstanceConfig` field + serde + resolver chain plumbing |
| Per-agent bind in-flight guard (EC11) | ~10-20 | Atomic flag or mutex per-agent in heartbeat_pair |
| 3-tier resolution chain observability (EC6) | ~15-25 | INFO + WARN logs per tier hit |
| Tests | ~80-120 | 15 edge case unit tests + integration matrices |
| **Total** | **~210-335** | Slightly above general's 200-300 nominal due to reviewer-added edge cases |

**Tier**: Tier-1 single primary review (codex) baseline. **Escalate Tier-2** if RCA during IMPL surfaces cross-cutting changes beyond enumerated sites OR test infrastructure churn (e.g. mock binding.json shape changes ripple into unrelated tests).

**LOC ceiling**: 350 nominal (above general's 300, justified by reviewer-added edge cases adding 5 cases × ~10-15 LOC each = ~50-75 additional). 400 hard escalate.

## §7 Out of scope (deferred)

- **GitLab/Bitbucket forge support** — future extension via additional parser
- **`remote: "upstream"` arg** — future extension if multi-remote workflow surfaces
- **Auto-create remote branch** — explicit `auto_create: bool` flag (Sprint 56+)
- **EC10 task-class filter** — split as P0-C (separate Sprint 55 design)
- **`worktree_pool::lease` semantics redesign** — production-proven, untouchable

## §8 Implementation order (per reviewer + lead consensus)

**Sequential preferred**: P0-A first (smaller user-facing correctness guard) → P0-B core (binding refactor) → P0-C (task-class filter) if approved.

**Parallel possible** if PR scopes are strictly disjoint:
- P0-A touches `src/mcp/handlers/channel.rs` + `src/channel/mod.rs`
- P0-B core touches `src/mcp/handlers/worktree.rs` + `src/mcp/handlers/dispatch_hook/mod.rs` + `src/mcp/handlers/ci/mod.rs` + `src/fleet.rs`
- P0-C touches `src/task` + `src/mcp/handlers/dispatch_hook/mod.rs` (overlap with P0-B)

Recommendation: **Sequential** — channel discipline first to lock routing invariants, then binding refactor with confidence in routing layer。Reduces debugging ambiguity if regressions surface during rollout。

## §9 Risks

| Risk | Severity | Mitigation |
|---|---|---|
| EC9 deprecation period creates dual-arg ambiguity bugs | LOW-MED | Reject when both args present; warn-log when `repo` arg used; two-sprint runway |
| EC10 split increases Sprint 55 P0 count from 2 to 3 | LOW | Smaller individual PRs, easier review/test |
| Non-GitHub remote silent fail without `repo:` companion | LOW | EC4 add `repo: Option<String>` field |
| Multi-remote workflows surprise | LOW | Honor explicit arg; future extension explicit |
| `release_worktree` auto-unsubscribe breaks intentional multi-watch agents | LOW | Match `repo + branch` exactly per EC7; unrelated watches unaffected |
| Backward-compat shim weakens migration "stick" | LOW | Two-sprint deprecation cycle; tier-by-tier escalation of warning severity |
| LOC creeps past 350 ceiling due to reviewer-added EC11-EC15 | LOW-MED | Some edges (EC13/EC14/EC15) may turn out to be ~5 LOC each (validation guards); total may stay ≤300 |

## §10 Status / next steps

- **Phase 1 dev RCA**: Complete
- **Phase 2 reviewer challenge**: Complete
- **Phase 3 lead synthesis**: This document
- **Operator review**: Pending m-3637 directive
- **Phase 4 IMPL** (post-approval): dev primary, reviewer Tier-1, ~210-335 LOC, ~3-4hr cycle

**Disagreement summary** (resolved per Phase 3 synthesis):
- EC10 bundling: dev "Phase 2 picks" → reviewer "split as P0-C" → **lead concurs split**
- EC9 deprecation cycle: dev "1 sprint" → reviewer "2 sprints" → **lead concurs 2 sprints**
- EC4 non-GitHub remote: dev "explicit error" → reviewer "add `repo: Option<String>` override field" → **lead concurs add field**
- EC6 fallback chain observability: dev "info log" → reviewer "info+warn+strict-mode" → **lead concurs full observability**
- EC7 ownership: dev "audit during P0-B IMPL" → reviewer "explicit IMPL prerequisite + test proof" → **lead concurs IMPL prerequisite**

Sprint 53/54 prior-art covers most concurrency + lock-ordering risks. P0-B core is bounded refactor + edge-case hardening.

## §11 P0-C reference

EC10 originally bundled with P0-B; reviewer Phase 2 challenge split it. See:

`docs/DESIGN-sprint55-p0c-auto-bind-task-class-filter.md`

Standalone scope: ~30-50 LOC for minimal opt-out form (`bind: false` task field), ~80-150 LOC for full task-class taxonomy. Operator decides which form post-P0-B.
