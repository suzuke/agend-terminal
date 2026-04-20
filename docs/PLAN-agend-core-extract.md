# Plan: extract `agend-core` crate to de-duplicate agend-terminal and agend-pty

> **Status: Not started 2026-04-20** — only the plan itself has been committed (`7953010`). `Cargo.toml` is still a single `[package]` (no `[workspace]`), and there is no `agend-core/` directory. Picking this up needs the prerequisites listed in the pickup memory (third consumer for BackgroundServices, etc.) plus a repo-merge decision with agend-pty.

Worktree: `/Users/suzuke/Documents/Hack/agend-refactor-core-extract`
Branch:   `refactor/agend-core-extract`

## 0. Context & evidence

Direct file-to-file comparison (grounding the plan):

| Module | agend-terminal | agend-pty | Observation |
|---|---|---|---|
| `inbox.rs` | 378 LOC, `InboxMessage{from,text,kind,timestamp:String}`, `NotifySource<'a>`, `deliver()` via `api::call`, INLINE_THRESHOLD=500 | 135 LOC, `InboxMessage{id:u64,sender,text,timestamp:u64}`, static `AtomicU64 NEXT_ID`, `InjectAction` enum, MAX_DIRECT_INJECT_LEN=500 | Same domain, incompatible on-disk schema. Only the 500-byte threshold matches. |
| `backend.rs` | 599 LOC, rich `BackendPreset` (instructions_path, spawn_flags, resume_mode, session_id capture, calibrated_version) | 279 LOC, thin preset (mcp_inject_flag, resume_flag as string), `build_full_command` helper | Terminal is a strict superset. Ready-patterns drift (`"bypass permissions\|❯"` vs `"Type your"`; Kiro has `--tui` only in pty). |
| `state.rs` | 594 LOC, 14 states + `StatePatterns` with compiled `Regex` + hysteresis timers | 708 LOC, 8 states + `ErrorKind` enum + event-driven `StateEvent` machine | Genuinely different models. Terminal = priority lattice; pty = event FSM. Not trivially mergeable. |
| `schedules.rs` vs `scheduler.rs` | Terminal: 287 LOC storage (`store.rs` load/mutate, timezone detection, `run_history`) + `daemon.rs::check_schedules` for exec | Pty: 195 LOC append-only JSONL + `AtomicU64 NEXT_ID`, embedded `check_due` | Storage formats incompatible. Schema drift total. |
| `telegram.rs` | 854 LOC, teloxide + tokio (async, rustls) | 377 LOC, isahc (sync HTTP) via `ChannelAdapter` trait | Runtime mismatch — blocker for naive merge. |
| `git.rs` vs `worktree.rs` | 401 LOC `worktree.rs`, uses `.git` exists check, branch validation, `WorktreeInfo` struct | 263 LOC, uses `paths::which("git")`, sanitizes branch differently, stores under `.agend/` | Similar intent, different conventions. |
| `mcp_config.rs` | 605 LOC | 256 LOC | agend-pty commit `d07cd65 "fix: 4 agend-terminal parity bugs"` hit a parity bug here — canonical evidence the duplicate-maintenance tax is real. |
| `instructions.rs` | 537 LOC, migrates stale `.claude/rules/agend.md`, sophisticated per-backend writer | 511 LOC, simpler `v1-agend-pty` marker, `AGEND_TEST_PASSPHRASE` E2E hook | Roughly similar structure; unique features either side. |

Runtime posture:
- agend-terminal: heavy tokio surface (teloxide, reqwest, crossbeam select).
- agend-pty: near-sync, isahc for HTTP, lib crate already exposed as `agend_pty_poc`.

Versions: terminal v0.3.0 (single-binary, no lib), pty v0.5.0 (4 bins + lib). `serde_yaml` vs `serde_yml` mismatch present.

## 1. Workspace topology decision

### Option A — Single Cargo workspace, both binaries in one repo
Pros: one PR compiles both; rust-analyzer sees everything; cheapest refactor; single CI; `cargo test -p agend-core` covers shared code once.
Cons: merging two repos' histories is political (who owns main?); loses independent release cadence; CI pipelines diverge; OSS presence under agend-pty changes.

### Option B — Two repos, `agend-core` as a separately-published crate
Pros: preserves both repos' release lines; `agend-core` publishable (crates.io or git dep); each consumer tracks versions independently.
Cons: highest coordination cost — every `agend-core` change wants a release + two version bumps; integration bugs caught late; path-dep dev hides semver violations until release.

### Option C — `agend-core` inside agend-terminal, agend-pty consumes via pinned git dep
Pros: reuses terminal's stronger CI; terminal owns canonical impl (more tests, superset types); minimal repo reorg.
Cons: asymmetric — agend-pty contributors need push rights to agend-terminal for shared fixes; second-class feel; git-dep rev pinning is brittle across forks.

### Recommendation: **A (single workspace) with a staged migration**

Reasoning: drift tax is already material (d07cd65 proves it); the two binaries serve overlapping users on overlapping roadmaps; release-cadence independence is lower value than bug parity. Blocker for A is merging agend-pty's v0.5.0 history into agend-terminal. So **A-via-detour**: start with B/C during extraction (temporary git-dep on agend-core), then physically move agend-pty into the workspace once the shared surface is stable. Mergeable PRs now without committing to repo-merge on day one.

Target layout after full migration:
```
agend-terminal/                 # workspace root
├── Cargo.toml                  # workspace = ["agend-terminal", "agend-core", ...]
├── agend-core/                 # new
│   ├── Cargo.toml
│   └── src/lib.rs
├── agend-terminal/             # moved from current src/
│   └── src/
└── (phase 5) agend-pty/        # absorbed
```

During PRs 1-4, agend-pty keeps its own repo and consumes `agend-core` via git-dep (path-dep during local dev).

## 2. Extraction order ranking

Ranked by (a) divergence cost paid, (b) public-API stability, (c) dependency-graph depth.

| Rank | Module | Divergence cost | API stability | Deps | Verdict |
|---|---|---|---|---|---|
| 1 | **`backend::Backend` + `from_command`** | High (ready-patterns already drifted) | Very stable (5 variants, serde names fixed) | Zero external | **First PR.** Smallest unit, biggest parity signal. |
| 2 | `inbox::InboxMessage` + enqueue/drain JSONL primitives | Medium (schemas diverged) | Moderate — needs shared schema choice | serde only | Second. Needs schema decision (adopt terminal's + pty's `id`). |
| 3 | `mcp_config.rs` common helpers | **Very high — d07cd65 lived here** | Low (still churning) | serde_json | Third. Extract AFTER backend enum stabilized. |
| 4 | `instructions.rs` | Medium (both have migration quirks) | Moderate | std::fs | Fourth. Depends on Backend. |
| 5 | `scheduler` | High (storage incompatible) | Low | cron, chrono, serde | Fifth. Needs schema unification. |
| 6 | `state.rs` | Very high (different models) | Unstable | regex | Sixth or never — may need shared trait only. |
| 7 | `telegram.rs` | Critical runtime mismatch | Unstable | tokio vs isahc | **Last, probably not at all** until pty adopts tokio. Extract only the trait. |
| 8 | `git/worktree` | Medium | Moderate | `std::process::Command` | Mid-priority. |

**Smallest first merge: PR#1 — Backend enum + preset struct.**
- Already-proven drift (Claude ready_pattern disagreement).
- Stable enum variants.
- Zero dep on other shared modules.
- Trivial parity test: run both `from_command` impls over 20 command strings and assert identical outputs.

## 3. Step-by-step PR sequence (each ≤1 day)

### PR 0 — Scaffold (≤2 hrs)
Files: `Cargo.toml` → `[workspace]`, new `agend-core/Cargo.toml`, `agend-core/src/lib.rs` (empty), `src/` moves to `agend-terminal/src/`.
Validate: `cargo build --all` green, `cargo test --all` green, `agend-terminal --version` works.
Keep `agend-pty` on path-dep locally; git dep as fallback for contributors (document in README).

### PR 1 — Extract `agend-core::backend`
- `agend-core/src/backend.rs` (new) — canonical `Backend` + `BackendPreset` (superset of terminal's).
- `agend-core/src/lib.rs` — `pub mod backend;`
- `agend-terminal/src/backend.rs` — `pub use agend_core::backend::*;` + thin wrapper for terminal-only `spawn_flags`.
- `agend-pty/src/backend.rs` — same pattern; `build_full_command` stays in pty (it uses `config::resolve_backend_binary`).

From terminal: canonical enum, `ResumeMode`, `preset()`, `from_command`, `calibrated_version`, `all()`, `name()`, `format_model_arg`, `read_session_id`, `save_session_id`.
From pty: nothing — its preset is a subset; unused fields `#[allow(dead_code)]` until pty wires them.

Parity validation:
- `agend-core/tests/backend_parity.rs` — 25 command strings × 5 backends; assert `from_command` deterministic.
- Snapshot test: `preset()` output matches a JSON golden per backend.
- Both bins' existing tests pass unchanged.

### PR 2 — Extract `agend-core::inbox::{InboxMessage, enqueue, drain}`
Schema decision: **adopt terminal's struct + pty's `id: u64`** → `{id, from, text, kind, timestamp: RFC3339}`. Add migration shim per consumer.
- `agend-core/src/inbox.rs` — primitives only (no `deliver` — depends on each repo's API call).
- Terminal's `inbox::deliver` / `notify_agent` / `NotifySource` stay in terminal.
- Pty's `InboxStore::store_or_inject` stays in pty.

Parity: property test — enqueue N messages, drain returns N in order, correct under terminal's existing thread-contention fixture.

### PR 3 — Extract `agend-core::mcp_config` common helpers
Pull out: env-block builders, path-templating per backend's MCP config file. CLI shell-outs stay in each bin.
Why now: fixes d07cd65 class of bug at the source. Hard PR — schemas have drifted, so refactor-and-unify, not lift-and-shift.
Validation: replay the 4 bugs from d07cd65 as regression tests in `agend-core/tests/mcp_config_regression.rs`.

### PR 4 — Extract `agend-core::instructions` writer primitives
Pull out: marker-merge helper (`write_with_marker`), per-backend generate fns where content is identical. Keep backend-specific quirks (Claude statusline, Kiro injection) in each bin.
Validation: golden-file tests — generate into tempdir per backend, diff bytes against checked-in files.

### PR 5 — Extract `agend-core::git` (worktree + branch sanitize)
Unify terminal's `worktree.rs::validate_branch` with pty's `sanitize_branch` into `branch::sanitize` + `branch::validate`. Extract `is_git_repo`, `has_commits`, worktree create/prune.
Validation: tempdir integration test, full lifecycle from both consumers.

### PR 6 — Extract `agend-core::scheduler::Schedule` (data type + cron + due-check)
Schema unification is the hard part. Adopt terminal's full schema (`timezone`, `run_history`, `label`, `created_by`, RFC-3339). Pty's AtomicU64 counter becomes a helper on the store. Add migrator from pty's legacy JSONL.
Validation: load pty-format fixture, run migration, round-trip through terminal CRUD.

### PR 7 — Define `agend-core::channel::ChannelAdapter` trait, do NOT yet port telegram
Extract the abstract trait already in pty's `channel.rs`. Terminal implements over teloxide; pty keeps isahc impl. This is the **firewall** between async/sync halves.
Validation: both bins compile, existing channel-integration tests pass.

### PR 8 — State: extract shared `ErrorKind` classifier + `StatePatterns` source of truth
Keep two different `AgentState` enums but unify: (i) error-pattern regex table, (ii) hysteresis constants, (iii) `is_permanent` predicate. Expose via `agend_core::state::{detect_error, HYST_ERROR_MS, HYST_ACTIVE_MS}`.
Validation: replay both repos' state-transition fixtures against shared classifier; identical classifications required.

### PR 9 — Physically absorb agend-pty into the workspace
At this point `agend-core` has: backend, inbox, mcp_config, instructions, git, scheduler, channel trait, state primitives. PR 9 moves agend-pty's `src/` into the workspace, drops git-dep, lets rust-analyzer see everything. Use subtree merge to preserve history.

### PR 10+ (optional) — Async convergence
ONLY after PR 9: port agend-pty's telegram to tokio+teloxide, delete isahc, share `agend-core::telegram`. Behavior change, not refactor — gated on its own decision.

## 4. Risk list

| Risk | Detection / mitigation |
|---|---|
| **Async/sync runtime mismatch.** Tokio code in `agend-core` would force agend-pty to depend on tokio. | `agend-core/Cargo.toml` forbids tokio/teloxide/reqwest/isahc until PR 10. CI lint. `ChannelAdapter` trait is the boundary. |
| **Schema drift silently invalidates on-disk data** (e.g. `.sender` vs `.from`). | Every schema-changing PR ships a migrator + test that loads old-format fixture. Add magic-version bytes to JSONL headers going forward. |
| **Divergent semantics hidden in duplicates** — e.g. `from_command("codex-cli-rs")` — terminal `starts_with("codex-")` vs pty `starts_with("codex")`. | Fuzz test 100 generated command strings; diff old-terminal vs old-pty vs new-agend-core. Any three-way mismatch = incorrect extraction. |
| **Regex compilation cost regression** in state.rs — terminal pre-compiles, pty uses `contains`. | Benchmark state detection per frame before/after. |
| **CI clone-size balloon** post-merge. | Shallow clone in CI; document `git clone --filter=blob:none`. |
| **Feature-flag creep** — "pty-only" / "terminal-only" features fragment `agend-core`. | Constitution: agend-core is platform-agnostic, async-agnostic, IO-trait-abstracted. Feature flags may not gate public API — only dep-opt-in. PR reviewer checklist. |
| **Parity-fix-in-one-consumer-only regressions**. | For every module in `agend-core`, both consumers must `pub use agend_core::…::*` rather than redefine types. Clippy lint forbids duplicate `Backend` enum. |
| **Version-pinning drift** PR 0 → PR 9 when pty uses git-dep. | Pin by commit hash; bump in lockstep PRs; automated script. |
| **Test cross-pollination** — teloxide-using tests leak into `agend-core`. | `agend-core` `[dev-dependencies]` whitelist; CI runs `cargo test -p agend-core --no-default-features`. |

## 5. Test strategy

Three layers:

**(1) `agend-core` carries its own unit tests.** Each extracted module ports tests from whichever consumer had better coverage. Target: >80% line coverage in `agend-core` per PR.

**(2) `agend-core` carries cross-crate parity tests** in `agend-core/tests/parity_*.rs`:
- `parity_backend.rs` — golden JSON per backend preset.
- `parity_inbox.rs` — load fixture JSONL, enqueue/drain, byte-for-byte equality.
- `parity_mcp_config.rs` — replay d07cd65's 4 bug scenarios.
- `parity_state.rs` — recorded PTY capture per backend, assert transition sequence.

**(3) Consumer bins keep E2E tests unchanged.** Before each PR merges: `cargo test -p agend-terminal` and `cargo test -p agend-pty` (via path-dep in dev). Either failing = not mergeable.

**Before/after equivalence proof per module:**
- a. Record fixture outputs from OLD impl against test vector.
- b. Extract.
- c. Re-run tests; fixture must not change.
- d. If fixture must change (legitimate unification), PR description carries a "behavior change" checklist.

**CI gates per PR:**
1. `cargo fmt --check`
2. `cargo clippy --workspace -- -D warnings`
3. `cargo test --workspace`
4. `cargo doc --workspace --no-deps`
5. `scripts/parity_check.sh` — runs parity fixtures against both bins built from the PR's SHA.
