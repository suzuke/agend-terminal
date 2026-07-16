# Spike: canonical heartbeat + binding path-resolve (t-…83936-4)

> **Historical snapshot:** This is point-in-time design evidence, not a current
> runtime contract. Use [`docs/FEATURE-worktree.md`](docs/FEATURE-worktree.md)
> and the current source for authoritative behavior.

Spike-first design for lead vet BEFORE impl (touches daemon lifecycle core). DUAL.

## Root cause (confirmed at file:line)
`binding_state.rs:116-117`:
```rust
let worktree_valid = worktree_exists_on_disk
    && (wt_path.join(".git").exists() || wt_path.join(".agend-managed").exists());
```
A linked worktree's `.git` is a **pointer file** (`gitdir: <canonical>/.git/worktrees/<name>`).
`wt_path.join(".git").exists()` is true for the *pointer file itself* even after the
canonical it points to is deleted → `worktree_valid=true` while the worktree is
unusable. That is exactly tonight's 40-min-silent incident AND the dev3
dangling-`.git` hazard (t-…83936-2) — same class, fold both in.

## Protection ① — binding_state path-resolve liveness (binding_state.rs)
Add, in the bound branch, real resolution (all paths are ABSOLUTE from
binding.json → cwd-independent):
```rust
let source_repo = b["source_repo"].as_str().unwrap_or("");
let canonical_present = !source_repo.is_empty() && std::fs::metadata(source_repo).is_ok();
let gitdir_resolves = git_ok(wt_path, &["rev-parse","--git-dir"]); // resolves the .git POINTER
let worktree_valid = worktree_exists_on_disk
    && (wt_path.join(".git").exists() || wt_path.join(".agend-managed").exists())
    && gitdir_resolves;                       // NEW: pointer must RESOLVE, not just exist
let invalid_reason =                          // NEW explicit field
    if !worktree_exists_on_disk { Some("worktree_missing") }
    else if !canonical_present   { Some("canonical_missing") }
    else if !gitdir_resolves     { Some("gitdir_dangling") }
    else { None };
```
- `git_ok(dir, args)` = `git -C <dir> …` exit 0 (fail-closed on any error), a
  read-only subprocess — fine for an on-demand diagnostic tool.
- **Consumer impact (vet point):** `worktree_valid` becomes correctly *false* when
  the gitdir dangles (today it's falsely true). Consumers use it for the rebuild
  decision, so tightening is the desired behavior (agent rebuilds instead of
  committing into a dead worktree). New `invalid_reason` is additive.

## Protection ② — canonical heartbeat (NEW per-tick handler)
New `src/daemon/per_tick/canonical_heartbeat.rs`, registered in
`per_tick/mod.rs:322` `vec![ … Box::new(CanonicalHeartbeatHandler::new(60)) ]`.
Mirrors `OfflineUnreadAlertHandler` (offline_unread_alert.rs) 1:1:
- cadence 60 ticks (~10 min at the 10 s tick) — vs 40 min silent tonight; tunable
  via env. Cheap: 1-few repos, a stat + optional `git rev-parse` each.
- each fire: `binding::bound_source_repos(home)` → distinct ABSOLUTE source_repo
  paths. For each: **fresh `std::fs::metadata(abs)` + `abs/.git` present** (cheap
  deletion detect); optionally `git -C abs rev-parse --git-dir` (corruption).
- missing → `crate::channel::notify_all_escalation_channels(...)` (loud operator
  page) + event-log `kind=canonical_repo_missing`.
- per-repo dedup latch (count/state-keyed, like offline_unread_alert): page ONCE,
  re-page on a new repo missing, reset when it returns → no per-tick spam.

### The cwd-orphaned-inode trap (explicit)
Tonight the daemon's own cwd WAS the deleted canonical (held inode → looked
alive). The heartbeat must therefore resolve by ABSOLUTE PATH, never cwd-relative:
`bound_source_repos` yields absolute paths; `std::fs::metadata(abs)` and
`git -C abs` both do fresh path lookups independent of the process cwd. Invariant:
**never `.`/relative in this handler.** Pin with a test that runs the check with
the process cwd set to a since-deleted dir and asserts it still flags the missing
absolute repo.

## Tests (RED-first)
- ①: worktree with a `.git` pointer to a deleted canonical → `worktree_valid=false`,
  `invalid_reason="gitdir_dangling"` (RED against current code = true). Plus
  `canonical_missing` (source_repo path gone) and the happy path.
- ②: pure `decide()` dedup latch (page-once / re-page / reset), and a
  cwd-independence test (cwd = deleted dir, abs repo missing → still fires).

## Ask
Vet: (1) tightening `worktree_valid` (consumer impact) OK, or add a NEW field
`worktree_resolves` and leave `worktree_valid` as-is? (2) cadence 60t OK? (3)
git-subprocess vs stat-only for the heartbeat's per-repo check? Then I impl → DUAL.
