//! agend-git — transparent git shim for fleet-managed worktrees.
//!
//! Intercepts git commands via PATH shadowing. Reads binding.json to
//! determine the active worktree, then either:
//! - passthrough (unbound read-only commands)
//! - chdir + pass (bound commands routed to worktree)
//! - silent-exempt (gh post-merge cleanup checkout — Sprint 57 Wave 2 Track D)
//! - deny (forbidden operations with LLM-friendly error)
//!
//! Bypass: AGEND_GIT_BYPASS=1 | AGEND_GIT_BYPASS_AGENT=<name> | AGEND_GIT_BYPASS_UNTIL=<epoch>
//!
//! Cross-platform: Unix uses exec() for process replacement; Windows uses
//! status() + exit(code) for equivalent behavior.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

// #1651: share the EXACT HMAC verifier with the daemon by source (the shim is a
// separate binary that cannot link the lib). `config_integrity` declares the
// same file as `mod integrity_core`; this `#[path]` include guarantees no
// signer/verifier algorithm drift.
#[path = "../integrity_core.rs"]
mod integrity_core;

/// #1504 L3: max times the shim may re-enter before hard-failing. Healthy
/// operation never exceeds 1 (real git ≠ shim → no re-entry), so a small cap
/// has zero false-trip risk while containing a self-resolution spawn storm.
const MAX_SHIM_DEPTH: u32 = 3;

/// Current shim recursion depth, read from the propagated sentinel env.
fn shim_depth() -> u32 {
    env::var("AGEND_GIT_SHIM_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    // #1504 L3: recursion guard. If git ever resolves back to THIS shim (e.g.
    // AGEND_REAL_GIT unset + self-exclusion miss), each real-git spawn re-enters
    // here; on Windows `exec_real_git` uses `status()` (spawn), so it's an
    // unbounded process storm, not a single exec-replace. Cap the depth and
    // hard-fail with an actionable message. Checked BEFORE `should_bypass`
    // because the bypass path also execs real git.
    let depth = shim_depth();
    if depth >= MAX_SHIM_DEPTH {
        eprintln!(
            "agend-git: FATAL recursion guard tripped (AGEND_GIT_SHIM_DEPTH={depth}) — the \
             shim resolved git to itself. AGEND_REAL_GIT is unset/unresolvable and \
             $AGEND_HOME/bin was not excluded from PATH. Set AGEND_REAL_GIT to the real \
             git binary. (#1504)"
        );
        std::process::exit(70); // EX_SOFTWARE
    }

    // Bypass checks (3-layer per §7).
    if should_bypass() {
        // #2158: audit a SUB-AGENT's own `AGEND_GIT_BYPASS=1 git <mutating>` op —
        // the stray-worktree / drift vector the daemon-side bypass audit
        // (git_helpers.rs, #2242 PR2(iii)) cannot see. The shim is a SEPARATE
        // binary on the AGENT PATH; the daemon PATH is shim-free, so the two audits
        // cover DISJOINT callers (no double-log). Top-level ops only
        // (`shim_depth()==0` skips git re-invoked by a hook); commit/add excluded
        // (Option B — agents bypass-commit into their OWN worktree constantly, so
        // logging those floods fleet_events for ~zero forensic value). Best-effort,
        // never blocks: the `exec_real_git` below is unchanged.
        if shim_depth() == 0 {
            let subcommand = args.first().map(|s| s.as_str()).unwrap_or("");
            if bypass_op_is_audited(subcommand, &args) {
                let home = env::var("AGEND_HOME").unwrap_or_default();
                if !home.is_empty() {
                    let agent = env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
                    // instrument-only: D3 #2158 — bypass audit emit, control-flow-
                    // inert; the `exec_real_git` below is unchanged.
                    log_bypass_mutating_op(&home, &agent, &args);
                }
            }
            // #2234 fix B: DENY (not just log) an AGENT's bypass-provisioning op
            // in a canonical-rooted repo. The daemon already auto-binds a worktree
            // at dispatch; a stray bypass `worktree add` / `checkout|switch <ref>`
            // here detaches the operator's canonical HEAD (or strays a worktree).
            // Daemon internals never reach this shim (daemon PATH is shim-free) and
            // carry no AGEND_INSTANCE_NAME, so this only bites true agents. Exits
            // non-zero before the passthrough exec when the deny fires.
            enforce_agent_canonical_bypass_deny(&args);
        }
        exec_real_git(&args, None);
    }

    let agent = env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
    let home = env::var("AGEND_HOME").unwrap_or_default();

    if agent.is_empty() || home.is_empty() {
        // #2234 defect#2: a NON-agent caller (no AGEND_INSTANCE_NAME) early-exits
        // here to the TERMINAL `exec_real_git` BEFORE reaching `classify` — so a
        // canonical-HEAD-touching `git checkout|switch <branch>` (e.g.
        // `git checkout origin/main`, which detaches canonical HEAD) passed
        // through with zero attribution. Record one fleet_events line with
        // process ancestry first (only when we have a home to write to — the
        // daemon-correlated culprit inherits AGEND_HOME but dropped
        // AGEND_INSTANCE_NAME). Instrument-only: the passthrough exec is unchanged.
        if !home.is_empty() {
            // instrument-only: D3 #2234 — non-agent canonical-checkout audit,
            // control-flow-inert; the `exec_real_git` below is unchanged.
            log_nonagent_canonical_checkout(&home, &agent, &args);
        }
        exec_real_git(&args, None);
    }

    // Read binding.
    let binding = read_binding(&home, &agent);
    let subcommand = args.first().map(|s| s.as_str()).unwrap_or("");

    // #2234: loud WARN for the cwd↔bound-worktree drift. When a bound agent's cwd
    // is its `<home>/workspace/<agent>` clone — a SEPARATE git object store from its
    // bound worktree — the shim routes git to the worktree (ChdirPass) while the
    // agent's file edits / cargo / `AGEND_GIT_BYPASS=1 git` act on the clone, so git
    // reads look "fake" (clean / already-merged) and the agent silently works
    // against a stale tree. Surface it with an ACTIONABLE recovery hint (stderr +
    // fleet_events) instead of corrupting the agent's git cognition. operator
    // ruling (d-20260616134854749703-2): warn-only — NEVER block (a fail-closed
    // block would deny 100% of bound agents, whose cwd is always this clone; the
    // harm is rare and recoverable). Purely additive — does NOT change any routing
    // decision below. Latched per `(cwd, is_mutating)` (see
    // `warn_workspace_drift_once`): ≤2 warns per cwd (first read + first mutating
    // op) so the agent sees the hint before its first write without per-op spam.
    maybe_warn_workspace_drift(&home, &agent, &binding, subcommand);

    // #1463: non-destructive forensic capture of backend `init` heartbeat
    // commits. The RCA established these are produced by a backend process
    // (committer = user's global git identity, because this shim's
    // `commit → ChdirPass` does NOT inject `-c user.*`; agend's own init
    // bypasses via AGEND_GIT_BYPASS and never reaches here). Desk research
    // could not isolate WHICH backend / process; this hook records the full
    // process ancestry + invocation context the instant the shim sees the
    // heartbeat-shaped commit, so the next live occurrence is pinned. Pure
    // logging — the commit still passes through unchanged.
    if subcommand == "commit" && commit_is_init_heartbeat_argv(&args) {
        log_init_heartbeat_forensics(&home, &agent, &args);
    }

    // Sprint 57 Wave 2 Track D: resolve parent-process-is-gh signal once.
    // Used by `classify` to recognize gh-driven post-merge cleanup
    // checkouts and silently exempt them from the E4.5 cross-branch
    // fence. See `invocation_is_gh_post_merge` for the rationale.
    let parent_is_gh = invocation_is_gh_post_merge();

    // #778 Option 3 + #852 residual PR-A: resolve cwd-is-canonical-
    // rooted once, pass into classify as a pure bool so the leniency
    // rule is unit-testable without a real filesystem fixture. Detects
    // BOTH daemon-provisioned worktrees AND the canonical source repo
    // (post-#852-residual; pre-fix only matched worktrees).
    let canonical_cwd = cwd_is_canonical_rooted();

    // #852: resolve agent-vs-operator caller identity once. Agents are
    // daemon-spawned subprocesses (AGEND_INSTANCE_NAME set); operators
    // are interactive shells with no such env. Used by classify's
    // canonical-checkout gate to prevent reviewer-style PR-inspection
    // from polluting canonical worktrees with stale refs.
    let is_agent_caller = env::var_os("AGEND_INSTANCE_NAME").is_some();

    // #1463 (A): a bound agent's mutating command whose cwd is a FOREIGN git
    // repo (separate object store — e.g. a test scratch repo) should operate on
    // THAT repo, not be redirected into the worktree. Post-process the classify
    // result so the (unchanged, unit-tested) `classify` stays cwd-agnostic.
    let action = apply_foreign_repo_passthrough(
        classify(
            subcommand,
            &args,
            &binding,
            parent_is_gh,
            canonical_cwd,
            is_agent_caller,
        ),
        subcommand,
        &args,
        cwd_is_foreign_repo(&binding),
    );

    // #1463 (B): on every ChdirPass, strip the caller's leading global target
    // overrides (`-C` / `--git-dir` / `--work-tree`) so the shim's own
    // `-C <worktree>` is authoritative. Passthrough is left verbatim (the
    // command runs against the cwd it was actually aimed at).
    match action {
        Action::Passthrough => exec_real_git(&args, None),
        Action::ChdirPass(worktree) if is_conflict_capable(subcommand) => {
            exec_with_conflict_guidance(
                &strip_target_overrides(&args),
                &worktree,
                &home,
                &agent,
                subcommand,
            );
        }
        Action::ChdirPass(worktree) => {
            exec_real_git(&strip_target_overrides(&args), Some(&worktree))
        }
        Action::CleanupAndChdirPushPass(worktree) => {
            // #2379 ③ denylist-core: refuse a push whose range carries a
            // trust-root file BEFORE the (never-blocking) init-pile cleanup.
            // ⚠ CONTRACT CHANGE: this is the FIRST blocking deny on the push
            // path — until now `CleanupAndChdirPushPass` always passed through
            // to real `git push`. `cleanup_init_pile_pre_push` below KEEPS its
            // "NEVER blocks" contract (this is a separate, independent check
            // that runs first); the arm as a whole is now always-pass → MAY
            // exit(1). Fail-closed (see `push_trust_root_denylist_violation`).
            if let Some(reason) = push_trust_root_denylist_violation(&worktree) {
                emit_deny_error(subcommand, &reason, &agent, Some(&binding));
                write_git_event_typed(
                    &home,
                    &agent,
                    subcommand,
                    "deny_trust_root",
                    None,
                    Some(&reason),
                );
                std::process::exit(1);
            }
            // #2379 S3: deny a push that could write a protected ref (hardcode main|master ∪
            // the signed policy.toml override) — across the WHOLE push surface: explicit
            // refspec dest, `--all`/`--mirror` (push all heads), wildcard dest, and a
            // no-refspec push under `push.default=matching`. See `push_protected_violation`.
            // Shim-layer defense-in-depth (the remote's branch protection is the primary
            // gate); fail-closed (`load_protected_refs`). A normal push of the agent's own
            // branch is untouched.
            if let Some(reason) = push_protected_violation(
                &args,
                &load_protected_refs(&home),
                push_default_is_matching(&worktree),
            ) {
                emit_deny_error(subcommand, &reason, &agent, Some(&binding));
                write_git_event_typed(
                    &home,
                    &agent,
                    subcommand,
                    "deny_protected_ref",
                    None,
                    Some(&reason),
                );
                std::process::exit(1);
            }
            // #883: best-effort pre-push cleanup of empty `init`
            // heartbeat commits. Failure is logged + we still pass
            // through to real `git push` — cleanup MUST NOT block
            // real work from landing.
            cleanup_init_pile_pre_push(&worktree);
            exec_real_git(&strip_target_overrides(&args), Some(&worktree))
        }
        Action::SilentExempt {
            target_branch,
            reason,
        } => {
            // Sprint 57 Wave 2 Track D: gh-driven post-merge cleanup
            // checkout. Already-merged PR + already-deleted remote
            // branch — the local checkout is purely cosmetic from
            // gh's perspective. Skip the actual git invocation
            // (preserves E4.5: no real checkout to main happens),
            // log the exemption for security review, exit 0 so gh
            // continues its post-merge cleanup quietly.
            write_git_event_typed(
                &home,
                &agent,
                subcommand,
                "post_merge_cleanup_exempt",
                Some(&target_branch),
                Some(&reason),
            );
            std::process::exit(0);
        }
        Action::Deny(reason) => {
            emit_deny_error(subcommand, &reason, &agent, Some(&binding));
            write_git_event_typed(&home, &agent, subcommand, "deny", None, Some(&reason));
            std::process::exit(1);
        }
    }
}

// ── Bypass ──────────────────────────────────────────────────────────────

fn should_bypass() -> bool {
    if env::var("AGEND_GIT_BYPASS").is_ok() {
        return true;
    }
    if let Ok(agent_bypass) = env::var("AGEND_GIT_BYPASS_AGENT") {
        if let Ok(current) = env::var("AGEND_INSTANCE_NAME") {
            if agent_bypass == current {
                return true;
            }
        }
    }
    if let Ok(until_str) = env::var("AGEND_GIT_BYPASS_UNTIL") {
        if let Ok(until) = until_str.parse::<u64>() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if now < until {
                return true;
            }
        }
    }
    false
}

// ── Binding ─────────────────────────────────────────────────────────────

#[derive(Default)]
struct Binding {
    task_id: Option<String>,
    branch: Option<String>,
    worktree: Option<String>,
}

fn read_binding(home: &str, agent: &str) -> Binding {
    let dir = PathBuf::from(home).join("runtime").join(agent);
    let path = dir.join("binding.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Binding::default(),
    };
    // #1651: verify the HMAC sidecar BEFORE trusting the binding (esp. its
    // `branch` push-authority). A blind self-authorization rewrite — an injected
    // agent editing its own `binding.json` without the key — fails verify, so we
    // treat the agent as UNBOUND (fail-closed → `deny_unbound_else_chdir` denies
    // mutating ops, the SAME path a parse failure already takes). Missing sidecar
    // / missing key / mismatch all fail closed. Defense-in-depth against injection
    // blind-write, NOT a security boundary: a same-uid agent could read the key
    // and re-sign (accepted); true sealing needs OS-isolation (parked #1653).
    let tag = std::fs::read_to_string(dir.join("binding.json.sig")).unwrap_or_default();
    if !integrity_core::verify(Path::new(home), content.as_bytes(), &tag) {
        return Binding::default();
    }
    let v: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Binding::default(), // parse failure = unbound (fail-safe)
    };
    let b = Binding {
        task_id: v["task_id"].as_str().map(String::from),
        branch: v["branch"].as_str().map(String::from),
        worktree: v["worktree"].as_str().map(String::from),
    };
    // P0-1.6: orphan binding defense.
    // If binding points to a worktree path that no longer exists (e.g. operator
    // ran `git worktree remove` after the daemon wrote the binding, or a stale
    // binding survived a daemon restart), treat the agent as unbound rather
    // than letting chdir fatal at exec time. Daemon-side reconcile will
    // eventually clean the stale file; this guard is only a fail-safe.
    if let Some(ref wt) = b.worktree {
        if !std::path::Path::new(wt).exists() {
            return Binding::default();
        }
    }
    b
}

fn is_bound(binding: &Binding) -> bool {
    binding.task_id.is_some()
}

// ── Classification ──────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Passthrough,
    ChdirPass(String),
    /// #883: chdir into the bound worktree and run a pre-push
    /// init-commit-pile cleanup BEFORE `exec`-ing real `git push`.
    /// Backend session-checkpoint heartbeats (Claude / Codex / Kiro
    /// etc.) periodically `commit --allow-empty -m "init"` inside the
    /// bound worktree; without a pre-push gate these accumulate on
    /// the PR branch and leak to origin (operator visible as "一堆
    /// init" on the PR's mobile UI for #882). The cleanup is a
    /// local-only soft-reset; it does NOT force-push or otherwise
    /// rewrite remote history. On cleanup failure we log + still
    /// chdir-pass to the real push (cleanup MUST NOT block real
    /// work from landing).
    CleanupAndChdirPushPass(String),
    /// Sprint 57 Wave 2 Track D: gh post-merge cleanup checkout
    /// recognized — exit 0 without invoking real git. E4.5 protection
    /// is preserved (no actual checkout to main happens) and the
    /// `gh pr merge --delete-branch` cleanup proceeds silently.
    SilentExempt {
        target_branch: String,
        reason: String,
    },
    Deny(String),
}

/// Local mirror of `agent_ops::is_protected_ref`. The wrapper binary
/// is intentionally self-contained (no `crate::*` imports) so it
/// builds standalone without the full library surface. Sprint 57
/// Wave 2 Track B introduced the lib-side helper; the literal here
/// MUST stay in sync.
///
/// CR-2026-06-14: matched **case-insensitively** (mirrors the lib-side fix).
/// A case-insensitive FS folds `Main`→`main`, so a case-sensitive guard would
/// let `branch="Main"` slip past gh-push protection here.
fn is_protected_ref(branch: &str) -> bool {
    branch.eq_ignore_ascii_case("main") || branch.eq_ignore_ascii_case("master")
}

/// #778 Option 3 (originally) + #852 residual PR-A: detect that cwd
/// is rooted inside a canonical-origin git repo — either a
/// daemon-provisioned worktree (`.git` file with `gitdir:` pointer)
/// OR the canonical source repo itself (`.git` directory with
/// `[remote "origin"]` in config). The `origin` remote is what
/// distinguishes a canonical-rooted cwd from an orphan workspace-
/// placeholder repo (daemon startup creates these before fleet
/// config resolves; they have no remote and no project files).
///
/// **#852 residual fix**: the pre-PR-A logic required `.git` to be a
/// FILE (worktree marker shape only), which returned FALSE when the
/// caller was inside the canonical SOURCE REPO (`.git` is a
/// directory there). That gap let reviewer agents who `cd
/// canonical && git checkout <sha>` slip past the `is_agent_caller
/// && canonical_cwd` deny at line ~297-303, producing the
/// detached-HEAD pollution operator observed at 21:46 + 22:24 today
/// (`checkout: moving from main to <sha>` reflog entries post-21:23
/// daemon restart). The broadened detection covers BOTH shapes.
///
/// Renamed from `cwd_is_canonical_worktree` to `cwd_is_canonical_rooted`
/// — the previous name's "worktree" suffix was misleading after
/// broadening since the canonical source repo isn't a worktree.
fn cwd_is_canonical_rooted() -> bool {
    match env::current_dir() {
        Ok(cwd) => path_is_canonical_rooted(&cwd),
        Err(_) => false,
    }
}

/// #2234 Patch A: the canonical-rooted test for an ARBITRARY directory, so a
/// `git -C <path>` op can be judged against the dir git WOULD operate in rather
/// than the shim's process cwd. Extracted verbatim from `cwd_is_canonical_rooted`
/// (which now delegates with the process cwd — its behavior is byte-identical).
/// `<dir>/.git` is a DIR (canonical source repo) or a FILE (linked worktree)
/// whose origin config carries `[remote "origin"]`.
fn path_is_canonical_rooted(dir: &Path) -> bool {
    let dot_git = dir.join(".git");
    let meta = match std::fs::metadata(&dot_git) {
        Ok(m) => m,
        Err(_) => return false,
    };

    if meta.is_file() {
        // Worktree case (pre-#852-residual logic, unchanged).
        // `.git` file carries a `gitdir:` pointer to
        // `<source>/.git/worktrees/<entry>`; grandparent is
        // `<source>/.git` which carries the source repo's config.
        let content = match std::fs::read_to_string(&dot_git) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let gitdir_str = match content
            .lines()
            .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))
        {
            Some(s) => s,
            None => return false,
        };
        let gitdir = PathBuf::from(gitdir_str);
        let source_git_dir = match gitdir.parent().and_then(|p| p.parent()) {
            Some(p) => p.to_path_buf(),
            None => return false,
        };
        let config = match std::fs::read_to_string(source_git_dir.join("config")) {
            Ok(c) => c,
            Err(_) => return false,
        };
        config.contains("[remote \"origin\"]")
    } else if meta.is_dir() {
        // Canonical source repo case (#852 residual broadening).
        // `.git` is a directory; read `.git/config` directly to check
        // for `[remote "origin"]`. Same defense against orphan
        // workspace-placeholder repos that have no remote.
        let config = match std::fs::read_to_string(dot_git.join("config")) {
            Ok(c) => c,
            Err(_) => return false,
        };
        config.contains("[remote \"origin\"]")
    } else {
        // Unknown `.git` shape (symlink to neither file nor dir,
        // etc.) — fail closed.
        false
    }
}

/// #1463: resolve the `.git` directory for `start` the way git does — walk up
/// to the first `.git`, following a `.git` FILE's `gitdir:` pointer (linked
/// worktree) but NOT a `.git` symlink (fail-closed against pointer-craft).
/// Returns the canonicalized gitdir, or `None` on any IO/parse ambiguity.
/// Kept separate from `cwd_is_canonical_rooted` (which keys off `[remote
/// "origin"]` presence) per the #1463 ruling — same `.git`-resolve spirit,
/// different semantics (object-store identity vs canonical-origin membership).
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let start = std::fs::canonicalize(start).ok()?;
    let mut dir: &Path = &start;
    loop {
        let dot_git = dir.join(".git");
        match std::fs::symlink_metadata(&dot_git) {
            Ok(meta) if meta.is_dir() => return std::fs::canonicalize(&dot_git).ok(),
            Ok(meta) if meta.is_file() => {
                let content = std::fs::read_to_string(&dot_git).ok()?;
                let ptr = content
                    .lines()
                    .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))?;
                let p = PathBuf::from(ptr);
                let gitdir = if p.is_absolute() { p } else { dir.join(p) };
                return std::fs::canonicalize(&gitdir).ok();
            }
            // `.git` is a symlink or other irregular shape → fail-closed.
            Ok(_) => return None,
            // No `.git` here → walk up.
            Err(_) => dir = dir.parent()?,
        }
    }
}

/// #1463: resolve the COMMON git dir (shared object store + refs) for `start`,
/// mirroring git's `commondir` resolution — a linked worktree's gitdir carries
/// a `commondir` file pointing at the shared `<source>/.git`. Two paths that
/// resolve to the SAME commondir share one object store. Canonicalized; `None`
/// on any ambiguity. Defeats `.git`-pointer craft: a `.git` file pointing
/// `gitdir: <canonical>/.git` resolves to canonical's commondir (no `commondir`
/// file in the main gitdir → gitdir IS the common dir), so it is NOT seen as
/// foreign.
fn resolve_commondir(start: &Path) -> Option<PathBuf> {
    let gitdir = find_git_dir(start)?;
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(s) => {
            let rel = s.trim();
            if rel.is_empty() {
                gitdir.clone()
            } else {
                let p = PathBuf::from(rel);
                if p.is_absolute() {
                    p
                } else {
                    gitdir.join(p)
                }
            }
        }
        Err(_) => gitdir.clone(),
    };
    std::fs::canonicalize(&common).ok()
}

/// #1463 (A): is the current working directory a git repo whose object store is
/// SEPARATE from the bound worktree's (a foreign / scratch repo, e.g. a test
/// incubator)? When true a mutating command was aimed at THAT repo, not the
/// worktree — passing it through avoids hijacking it into the worktree (the
/// init-pile pollution). Fail-closed: a retargeting env var, an unresolved
/// commondir, or a missing worktree → `false` (→ ChdirPass keeps the existing
/// protection). SAFETY: canonical and EVERY sibling worktree resolve to
/// canonical's commondir, so a `true` here can ONLY mean a genuinely-separate
/// store the agent cannot use to reach canonical / shared refs / a sibling.
fn cwd_is_foreign_repo(binding: &Binding) -> bool {
    // GIT_DIR / GIT_COMMON_DIR / GIT_WORK_TREE retarget git independently of the
    // cwd `.git` discovery this check relies on → fail-closed.
    if env::var_os("GIT_DIR").is_some()
        || env::var_os("GIT_COMMON_DIR").is_some()
        || env::var_os("GIT_WORK_TREE").is_some()
    {
        return false;
    }
    let wt = match binding.worktree {
        Some(ref w) => w,
        None => return false,
    };
    let cwd = match env::current_dir() {
        Ok(c) => c,
        Err(_) => return false,
    };
    paths_are_foreign(&cwd, Path::new(wt))
}

/// #1463 (A): the pure object-store-identity comparison behind
/// `cwd_is_foreign_repo` — `true` iff both paths resolve to a commondir AND the
/// two commondirs differ. Fail-closed (`false`) if either side is unresolvable.
/// Split out so the adversarial matrix is hermetically testable without
/// touching the process cwd.
fn paths_are_foreign(cwd: &Path, worktree: &Path) -> bool {
    match (resolve_commondir(cwd), resolve_commondir(worktree)) {
        (Some(c), Some(w)) => c != w,
        _ => false,
    }
}

/// #2234 (C): pure decision — is `cwd` the agent's stale WORKSPACE clone rather
/// than its bound worktree? True iff `cwd` is rooted in the agent's configured
/// workspace dir (`<home>/workspace/<agent>`) AND that dir is a git object store
/// SEPARATE from the bound `worktree`. The workspace-dir gate is what
/// distinguishes the harmful drift (fleet.yaml `working_directory` is a separate
/// stale clone) from a LEGITIMATE foreign scratch repo a bound agent
/// intentionally `cd`'d into (#1463 test incubators) — those live elsewhere and
/// must NOT warn. Fail-closed (`false`) on any unresolvable path. Pure (only
/// reads the filesystem for the passed paths) so the matrix is hermetically
/// testable without touching the process cwd.
fn is_workspace_clone_drift(home: &str, agent: &str, cwd: &Path, worktree: &Path) -> bool {
    let ws = PathBuf::from(home).join("workspace").join(agent);
    let (Ok(ws_c), Ok(cwd_c)) = (std::fs::canonicalize(&ws), std::fs::canonicalize(cwd)) else {
        return false;
    };
    cwd_c.starts_with(&ws_c) && paths_are_foreign(&cwd_c, worktree)
}

/// #2234: emit a drift warning (stderr + a `cwd_worktree_drift` fleet_events line)
/// when `cwd` is the agent's stale workspace clone. Latched on the `(cwd,
/// is_mutating)` pair via two per-class markers
/// (`<home>/runtime/<agent>/cwd_drift_warned.{read,mut}`): a standing drift warns
/// at most ONCE per class — the first read-class op AND the first mutating-class
/// op each warn, so the agent is guaranteed to see the hint before its first
/// dangerous write, while no op-class spams (≤2 warns per cwd). The cheap marker
/// read short-circuits the commondir walk on every subsequent op of that class; a
/// NEW drifted cwd re-warns (new info). `is_mutating` (from `is_mutating_local` at
/// the call site) selects the class. Best-effort — every step swallows errors so
/// it can NEVER block or alter the git command (warn-only). Returns whether it
/// warned (for tests). Takes `cwd` explicitly so the latch + emit are testable
/// without mutating the process cwd.
fn warn_workspace_drift_once(
    home: &str,
    agent: &str,
    cwd: &Path,
    worktree: &Path,
    is_mutating: bool,
) -> bool {
    let cwd_c = match std::fs::canonicalize(cwd) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let cwd_s = cwd_c.to_string_lossy().to_string();
    // Per-class latch: `.mut` for mutating ops, `.read` for everything else, so
    // each class warns at most once per cwd (≤2 total) without spamming.
    let marker = PathBuf::from(home)
        .join("runtime")
        .join(agent)
        .join(if is_mutating {
            "cwd_drift_warned.mut"
        } else {
            "cwd_drift_warned.read"
        });
    // Cheap latch check FIRST — already warned for this exact cwd in this class →
    // skip the (filesystem-walking) drift detection entirely.
    if std::fs::read_to_string(&marker)
        .map(|s| s.trim() == cwd_s)
        .unwrap_or(false)
    {
        return false;
    }
    if !is_workspace_clone_drift(home, agent, &cwd_c, worktree) {
        return false;
    }
    eprintln!("{}", drift_warning_message(&cwd_c, worktree));
    write_git_event_typed(
        home,
        agent,
        subcmd_for_drift_event(),
        "cwd_worktree_drift",
        Some(&worktree.to_string_lossy()),
        Some(&cwd_s),
    );
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&marker, cwd_s.as_bytes());
    true
}

/// Stable `subcommand` field value for the `cwd_worktree_drift` event — the
/// drift is a per-invocation environment property, not tied to a git subcommand.
fn subcmd_for_drift_event() -> &'static str {
    "(cwd-drift)"
}

/// #2234: the loud drift WARN's actionable recovery message — single source of
/// truth for both the `eprintln!` emit and the contract test (#1493: route the
/// consumer through the producer, never hand-copy the shape). operator ruling
/// (d-20260616134854749703-2): the shim does NOT run `git status` or maintain a
/// backend-config exclusion set itself (keep it simple) — it tells the agent the
/// concrete commands to run. Names the cwd-clone vs worktree, how to CHECK what
/// mislanded, and how to RECOVER (absolute-path re-edit / `cp`), with r2's
/// correction that `cd` alone is insufficient (git keys on the binding, not cwd;
/// Edit/Write use absolute paths unaffected by cd).
fn drift_warning_message(cwd: &Path, worktree: &Path) -> String {
    let cwd_d = cwd.display();
    let wt_d = worktree.display();
    format!(
        "agend-git: \u{26a0} #2234 cwd/worktree drift — your cwd '{cwd_d}' is a \
         SEPARATE git repo from your bound worktree '{wt_d}'. git (via this shim) \
         runs against the worktree, but file edits made with a cwd-relative path \
         land in THIS cwd clone where git can't see them — so reads look \
         stale/'fake' and commits can come up empty.\n  \
         CHECK what mislanded:  git -C '{cwd_d}' status --short   \
         (any real source files listed there were written to the wrong repo)\n  \
         RECOVER: re-make those edits using ABSOLUTE paths under '{wt_d}', or copy \
         them across (`cp '{cwd_d}/<file>' '{wt_d}/<file>'`) — `cd` alone does NOT \
         move edits already written by absolute path. git already routes to the \
         worktree; verify with  AGEND_GIT_BYPASS=1 git -C '{wt_d}' status"
    )
}

/// #2234: thin env wrapper wiring `warn_workspace_drift_once` to the live process
/// cwd + binding. No-op when unbound (no worktree to compare against) or the cwd is
/// unreadable. Called once per shim invocation from `main`. `subcmd` selects the
/// per-class latch via `is_mutating_local` so the read- and mutating-class warnings
/// latch independently (≤2 per cwd).
fn maybe_warn_workspace_drift(home: &str, agent: &str, binding: &Binding, subcmd: &str) {
    let Some(ref wt) = binding.worktree else {
        return;
    };
    let Ok(cwd) = env::current_dir() else {
        return;
    };
    let _ = warn_workspace_drift_once(home, agent, &cwd, Path::new(wt), is_mutating_local(subcmd));
}

/// #1463 (A): the LOCAL mutating subcommands eligible for foreign-repo
/// passthrough — exactly the porcelain/plumbing mutating arm's token set.
/// `push` and `checkout`/`switch` are deliberately excluded (kept maximally
/// protected); read-only and unbound paths never reach the conversion.
fn is_mutating_local(subcmd: &str) -> bool {
    matches!(
        subcmd,
        "commit"
            | "pull"
            | "reset"
            | "revert"
            | "cherry-pick"
            | "stash"
            | "merge"
            | "rebase"
            | "am"
            | "add"
            | "rm"
            | "mv"
            | "read-tree"
            | "update-index"
            | "apply"
    )
}

/// #2027: a `git branch` / `git tag` invocation that NAMES a ref — it creates,
/// deletes, moves, or inspects a SPECIFIC ref (a positional ref name, or a
/// `-d`/`-D`/`-m`/`-M`/`-c`/`-C`/`--delete`/`--move`/`--copy` flag) — as opposed to
/// the bare `git branch` LIST form. `classify` groups `branch`/`tag` with the
/// read-only commands (they DEFAULT to listing), so a bound agent's
/// `git branch <name>` is `ChdirPass`'d into the worktree. In a FOREIGN repo that
/// is the #2027 success-lie: `git branch <new>` runs against the worktree, so the
/// foreign repo silently gets nothing yet exits 0; a name the worktree already
/// holds reports `fatal: already exists` from the wrong repo. A ref-naming
/// branch/tag in a foreign repo must run against THAT repo (passthrough).
///
/// Conservative by design: over-classifying (e.g. `git branch --list <pattern>`)
/// only adds the CORRECT foreign-passthrough (the pattern list should run on the
/// foreign repo too); under-classifying is the bug. Only `args[0]` (the
/// subcommand) is skipped — `args[1..]` are scanned for a ref-mutating flag or a
/// positional ref name.
fn branch_tag_names_ref(subcmd: &str, args: &[String]) -> bool {
    if !matches!(subcmd, "branch" | "tag") {
        return false;
    }
    args.iter().skip(1).any(|a| {
        let s = a.as_str();
        // Create / delete / move / copy a NAMED ref.
        matches!(
            s,
            "-d" | "-D" | "--delete" | "-m" | "-M" | "--move" | "-c" | "-C" | "--copy"
        )
        // CURRENT-branch mutators that need NO positional (the git-branch upstream /
        // description ops) — dash-prefixed, so the positional check below misses
        // them. `--set-upstream-to=<up>` takes the `=` form; `--set-upstream-to <up>`
        // takes a following value (caught as a positional, but matched here so the
        // no-value-yet form is covered too).
        || matches!(
            s,
            "--set-upstream-to" | "--set-upstream" | "--unset-upstream" | "--edit-description"
        )
        || s.starts_with("--set-upstream-to=")
        // `-u <up>` (separate, value caught as positional) AND the GLUED short form
        // `-u<up>` (e.g. `-uorigin/main`, value attached in one dash-prefixed token —
        // missed by both the exact `== "-u"` and the positional check). codex r2.
        || s == "-u"
        || (s.starts_with("-u") && s.len() > 2)
        // A positional ref name / pattern / flag value.
        || !s.starts_with('-')
    })
}

/// #2158: which `AGEND_GIT_BYPASS=1 git <subcmd>` ops are worth auditing — Option B
/// (lead-chosen), the stray-worktree / drift / stray-tree-push vector. EXCLUDES
/// `commit`/`add`: agents bypass-commit into their OWN worktree constantly, so
/// logging those would flood fleet_events for ~zero forensic value (a bypass commit
/// lands in the agent's own tree, not a stray one). Read-only ops never match.
/// `branch` is audited only in its ref-MUTATING form (create/delete/move — reuse
/// `branch_tag_names_ref`), not the bare list; `tag` is excluded (not a worktree
/// vector). Pure → unit-testable.
fn bypass_op_is_audited(subcmd: &str, args: &[String]) -> bool {
    matches!(
        subcmd,
        "worktree" | "checkout" | "switch" | "reset" | "clean" | "push"
    ) || (subcmd == "branch" && branch_tag_names_ref(subcmd, args))
}

/// #2158: which bypass layer authorized this op — forensics that distinguishes a
/// blanket env grant from a scoped/time-boxed one. Mirrors `should_bypass`'s
/// precedence order; `"unknown"` only if the env changed between the two checks.
fn active_bypass_layer() -> &'static str {
    if env::var("AGEND_GIT_BYPASS").is_ok() {
        "env"
    } else if env::var("AGEND_GIT_BYPASS_AGENT").is_ok() {
        "agent"
    } else if env::var("AGEND_GIT_BYPASS_UNTIL").is_ok() {
        "until"
    } else {
        "unknown"
    }
}

/// #1463 (A) + #2027: convert a bound-agent `ChdirPass` into `Passthrough` when
/// cwd is a foreign repo AND the command is a local mutating one (#1463) or a
/// ref-naming `branch`/`tag` (#2027). Pure so the matrix is unit-testable;
/// everything else (push/checkout ChdirPass, Deny, already-Passthrough) is
/// returned unchanged.
fn apply_foreign_repo_passthrough(
    action: Action,
    subcmd: &str,
    args: &[String],
    cwd_foreign: bool,
) -> Action {
    match action {
        Action::ChdirPass(_)
            if cwd_foreign && (is_mutating_local(subcmd) || branch_tag_names_ref(subcmd, args)) =>
        {
            Action::Passthrough
        }
        other => other,
    }
}

/// #1463: index of the real subcommand in `args` — the first non-option token,
/// skipping leading git global options and CONSUMING the value of value-taking
/// ones (so a `-C <path>` value is not mistaken for the subcommand). `None` if
/// there is no subcommand (globals only). Mirrors the subset of git's
/// global-option grammar that takes a separate value.
fn subcommand_index(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if !a.starts_with('-') {
            return Some(i);
        }
        // Separated-value globals consume the next token; glued / `=` forms and
        // no-value globals are a single token.
        if matches!(
            a,
            "-C" | "-c"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--super-prefix"
                | "--config-env"
                | "--exec-path"
        ) {
            i += 2;
        } else {
            i += 1;
        }
    }
    None
}

/// #2234 Patch A: the directory git WOULD operate in after applying the leading
/// global `-C <path>` option(s). git chdir's for each `-C`, and multiple are
/// cumulative — a non-absolute `-C` is relative to the path accumulated so far.
/// Only the global region `[0, sub_idx)` is walked (a post-subcommand `-C`, e.g.
/// `git commit -C <commit>` = reuse-message, is an unrelated option). Other
/// value-taking globals are skipped WITH their value (mirrors `subcommand_index`)
/// so their argument is never mistaken for a `-C` target. Starts from the process
/// cwd; returns it unchanged when there is no leading `-C`. Residual:
/// `--git-dir`/`--work-tree` are deliberately NOT resolved (they don't move cwd;
/// a `--git-dir` pointing at canonical is a separate, narrower vector left to a
/// follow-up). Pure modulo the one `current_dir()` read, so the `-C` math is
/// unit-testable from a known cwd.
fn effective_cwd_through_globals(args: &[String], sub_idx: usize) -> PathBuf {
    let mut cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut i = 0;
    while i < sub_idx {
        match args[i].as_str() {
            "-C" => {
                if let Some(p) = args.get(i + 1) {
                    let p = PathBuf::from(p);
                    cwd = if p.is_absolute() { p } else { cwd.join(p) };
                }
                i += 2;
            }
            // Other value-taking globals consume their value (same set as
            // `subcommand_index`); no-value globals advance one token.
            "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix"
            | "--config-env" | "--exec-path" => {
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }
    cwd
}

/// #1463 (B): on a ChdirPass for a MUTATING-local command, strip the caller's
/// LEADING global target overrides (`-C`, `--git-dir`, `--work-tree`, incl.
/// glued / `=` / separated-value forms) so the shim's own `-C <worktree>` is the
/// SOLE authority — a caller's trailing `-C <elsewhere>` would otherwise win the
/// left-to-right `-C` race and mutate `<elsewhere>` (e.g. canonical) despite the
/// redirect. GATED to mutating-local subcommands: a NON-mutating `git -C <dir>
/// rev-parse / log / config / init` keeps honoring `-C` (those neither pollute
/// nor mutate history, and stripping them would break legit helpers such as
/// `ensure_project_root`). Only the GLOBAL (pre-subcommand) region is touched,
/// so a post-subcommand `-C` (`git commit -C <commit>` = reuse-message) is
/// preserved. Non-mutating commands and arg lists without a leading override are
/// returned unchanged. Also closes the same `-C` blind spot in the pre-existing
/// canonical protection.
fn strip_target_overrides(args: &[String]) -> Vec<String> {
    let sub_idx = match subcommand_index(args) {
        Some(i) if is_mutating_local(args[i].as_str()) => i,
        // No subcommand, or a non-mutating one → leave `-C` etc. intact.
        _ => return args.to_vec(),
    };
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    // Walk ONLY the leading global region [0, sub_idx).
    while i < sub_idx {
        let a = args[i].as_str();
        // Separated-value target overrides → drop the option AND its value.
        if a == "-C" || a == "--git-dir" || a == "--work-tree" {
            i += 2;
            continue;
        }
        // Glued / `=` target overrides → drop the single token.
        if a.starts_with("-C") || a.starts_with("--git-dir=") || a.starts_with("--work-tree=") {
            i += 1;
            continue;
        }
        // Other value-taking globals: KEEP, with their value.
        if matches!(
            a,
            "-c" | "--namespace" | "--super-prefix" | "--config-env" | "--exec-path"
        ) {
            out.push(args[i].clone());
            if i + 1 < sub_idx {
                out.push(args[i + 1].clone());
            }
            i += 2;
            continue;
        }
        // No-value global → keep the single token.
        out.push(args[i].clone());
        i += 1;
    }
    out.extend_from_slice(&args[sub_idx..]);
    out
}

/// #1511 follow-up: a MUTATING-form action — deny when unbound, else route to
/// the caller's PRIVATE bound worktree. Mirrors the porcelain mutating arm body
/// so the flag-discriminated plumbing arms (`restore --staged`, `update-ref`,
/// `symbolic-ref` write) share one contract.
fn deny_unbound_else_chdir(bound: bool, binding: &Binding) -> Action {
    if !bound {
        return Action::Deny("unbound — no active task assignment".into());
    }
    match binding.worktree {
        Some(ref wt) => Action::ChdirPass(wt.clone()),
        None => Action::Deny("bound but no worktree path".into()),
    }
}

/// #1511 follow-up: a READ / working-tree-form action — passthrough when
/// unbound, else chdir to the bound worktree. Mirrors the `_` default arm, used
/// for the NON-mutating forms (`restore` working-tree, `symbolic-ref` read) so
/// they aren't over-denied.
fn pass_unbound_else_chdir(bound: bool, binding: &Binding) -> Action {
    if bound {
        if let Some(ref wt) = binding.worktree {
            return Action::ChdirPass(wt.clone());
        }
    }
    Action::Passthrough
}

fn classify(
    subcmd: &str,
    args: &[String],
    binding: &Binding,
    parent_is_gh: bool,
    canonical_cwd: bool,
    is_agent_caller: bool,
) -> Action {
    let bound = is_bound(binding);

    match subcmd {
        // Read-only commands: passthrough when unbound, chdir when bound.
        "status" | "log" | "diff" | "show" | "blame" | "ls-files" | "ls-tree" | "rev-parse"
        | "fetch" | "remote" | "branch" | "tag" | "describe" | "shortlog" | "reflog" => {
            if bound {
                if let Some(ref wt) = binding.worktree {
                    return Action::ChdirPass(wt.clone());
                }
            }
            Action::Passthrough
        }
        // Config/help: always passthrough.
        "config" | "help" | "version" | "init" | "clone" => Action::Passthrough,
        // #883: `push` factored out into its own arm so the pre-push
        // init-commit-pile cleanup gets wired between bind check and
        // the real `git push` exec. Other mutating commands keep the
        // plain `ChdirPass`.
        "push" => {
            if !bound {
                return Action::Deny("unbound — no active task assignment".into());
            }
            if let Some(ref wt) = binding.worktree {
                Action::CleanupAndChdirPushPass(wt.clone())
            } else {
                Action::Deny("bound but no worktree path".into())
            }
        }
        // Mutating commands: deny when unbound, else route to the bound
        // worktree.
        //
        // #1511: `read-tree`, `update-index`, and `apply` are index-mutating
        // plumbing that previously fell to the `_` default arm — which is
        // `unbound → Passthrough`, so an unbound agent (cwd = canonical) could
        // `git read-tree -m …` straight into the SHARED source-repo index
        // (UU/DU markers leaking to every worktree). Folding them into this
        // mutating arm gives them the safe contract the porcelain mutators
        // already have: `unbound → Deny` (closes the hole) and
        // `bound → ChdirPass` (routes to the agent's PRIVATE worktree index,
        // ignoring cwd — so a bound agent is safe even standing in canonical).
        // No canonical_cwd gate needed: ChdirPass already redirects away from
        // cwd. Daemon-internal callers set `AGEND_GIT_BYPASS=1` and never reach
        // `classify`. Exact-match tokens — `read-tree` does NOT catch the
        // read-only `merge-tree`. `reset` stays here (it always required a
        // binding); `git reset --hard` remains the agent's self-recovery tool.
        // Deferred to follow-up (ref/porcelain nuance, not index plumbing):
        // `restore --staged`, `update-ref`, `symbolic-ref`.
        "commit" | "pull" | "reset" | "revert" | "cherry-pick" | "stash" | "merge" | "rebase"
        | "am" | "add" | "rm" | "mv" | "read-tree" | "update-index" | "apply" => {
            if !bound {
                return Action::Deny("unbound — no active task assignment".into());
            }
            if let Some(ref wt) = binding.worktree {
                Action::ChdirPass(wt.clone())
            } else {
                Action::Deny("bound but no worktree path".into())
            }
        }
        // Checkout/switch: deny unbound, deny cross-branch.
        "checkout" | "switch" => {
            let target_branch = args.get(1).map(|s| s.as_str()).unwrap_or("");
            if !bound {
                // #852: agent callers must NOT use the #778 Option-3
                // leniency below. The leniency was designed for the
                // operator-typed validation-canary flow (operator runs
                // `repo action=checkout` to provision a worktree in
                // detached-HEAD, then `git switch <branch>` to land on
                // the branch; that follow-up needs to pass without a
                // BYPASS). But the gate wasn't agent-aware, so
                // reviewer agents whose binding lookup failed for the
                // canonical-rooted cwd fell through to the same
                // leniency — and the resulting `git checkout <sha>` /
                // `git checkout -b tmp_review` calls polluted
                // canonical's branch list with stale `pr*_head` /
                // `tmp*` / `review/*` refs. Operator surfaced the
                // recurrence on PR #805 morning + PR #850 afternoon.
                // Fix: route agents to either `repo action=checkout
                // bind=true` (gives them a properly-bound worktree)
                // or `gh pr diff/view` (read-only). Operator path
                // unchanged.
                if is_agent_caller && canonical_cwd {
                    return Action::Deny(
                        "agent callers must not checkout in canonical \
                         (use `repo action=checkout` for PR inspection or \
                         `gh pr diff/view` for read-only). #852."
                            .into(),
                    );
                }
                // #778 Option 3: shim leniency for canonical-rooted
                // unbound worktrees. When cwd is inside a worktree whose
                // `.git` pointer resolves to a source repo carrying a
                // `[remote "origin"]` config entry (i.e. a canonical
                // repo, not the orphan workspace-placeholder daemon
                // startup leaves), allow `git checkout`/`git switch
                // <branch>` as a Passthrough. Closes the chicken-and-egg
                // surfaced by validation canary 2026-05-14:
                // `repo action=checkout` provisions the worktree in
                // detached-HEAD but doesn't bind, so the natural
                // follow-up `git switch <branch>` would otherwise need
                // a BYPASS. Narrow by design — `target_branch` must be
                // a positional argument (not a flag) and the worktree
                // must be daemon-provisioned canonical-rooted, so the
                // surface is limited to navigation within an already-
                // materialized worktree.
                if !target_branch.is_empty() && !target_branch.starts_with('-') && canonical_cwd {
                    return Action::Passthrough;
                }
                return Action::Deny("unbound — no active task assignment".into());
            }
            // Check for cross-branch attempt.
            if let Some(ref assigned) = binding.branch {
                if !target_branch.is_empty()
                    && target_branch != assigned
                    && !target_branch.starts_with('-')
                {
                    // Sprint 57 Wave 2 Track D: gh post-merge cleanup
                    // exemption. Trigger requires ALL of:
                    //   - target is a protected ref (main / master)
                    //   - parent process is `gh` (signal that this
                    //     invocation is from `gh pr merge --delete-branch`
                    //     post-merge local-state cleanup)
                    //   - we're in the agent-invoked path (AGEND_INSTANCE_NAME
                    //     was set; bound binding is the consequence of that)
                    // Heuristic robustness: a non-gh parent (interactive
                    // shell, script, IDE) reaches the cross-branch deny
                    // unchanged, preserving E4.5 protection for the
                    // operator-typed case the rule was originally built
                    // for.
                    if is_protected_ref(target_branch) && parent_is_gh {
                        return Action::SilentExempt {
                            target_branch: target_branch.to_string(),
                            reason: format!(
                                "gh post-merge cleanup checkout to '{target_branch}' \
                                 from binding-branch '{assigned}' \
                                 (parent process detected as `gh`); \
                                 PR merge already succeeded — silent exit avoids \
                                 noisy false-positive deny on the operator's terminal"
                            ),
                        };
                    }
                    return Action::Deny(format!(
                        "cross-branch — assigned to '{assigned}', cannot switch to '{target_branch}'"
                    ));
                }
            }
            if let Some(ref wt) = binding.worktree {
                Action::ChdirPass(wt.clone())
            } else {
                Action::Deny("bound but no worktree path".into())
            }
        }
        // Worktree management: always deny (fleet-managed).
        "worktree" => Action::Deny("fleet-managed — use agend-terminal worktree tools".into()),
        // #1511 follow-up: index/ref-mutating plumbing that also fell to the
        // `_` default arm (unbound → Passthrough, so an unbound agent in
        // canonical could mutate the shared store). Unlike read-tree/
        // update-index/apply (#1511), these need FLAG/ARG discrimination so the
        // read-only / working-tree forms aren't over-denied.
        //
        // `restore`: only `--staged`/`-S` touches the INDEX (per-worktree →
        // ChdirPass isolates a bound agent). A bare or `--worktree` restore
        // touches the working tree only — left as a non-mutating passthrough
        // (#1511fu scope; matches the operator's "don't block bare restore").
        "restore" => {
            let touches_index = args.iter().any(|a| a == "--staged" || a == "-S");
            if touches_index {
                deny_unbound_else_chdir(bound, binding)
            } else {
                pass_unbound_else_chdir(bound, binding)
            }
        }
        // `update-ref` always writes or deletes a ref (no read form) → mutating.
        //
        // SHARED-REF CAVEAT: refs live in the COMMON `.git` store and are shared
        // across worktrees (unlike the per-worktree index), so `ChdirPass` does
        // NOT isolate a bound agent's ref write from canonical. We still adopt
        // #1511's contract — `unbound → Deny` closes the documented
        // canonical-write hole; bound agents are trusted (active task) and
        // already fenced by the E4.5 cross-branch / worktree-policy porcelain
        // guards. Raw `update-ref` is not part of any agent flow. (Policy A.)
        "update-ref" => deny_unbound_else_chdir(bound, binding),
        // `symbolic-ref <name>` (no value) READS the ref; `<name> <ref>` or
        // `-d/--delete` WRITES it. Only the write form mutates — the read form
        // must pass so an unbound `git symbolic-ref HEAD` isn't wrongly denied.
        // Same shared-ref caveat as `update-ref` for the write form.
        "symbolic-ref" => {
            let deletes = args.iter().any(|a| a == "-d" || a == "--delete");
            // Non-flag args after the subcommand: 1 = read, ≥2 = write.
            let value_args = args.iter().skip(1).filter(|a| !a.starts_with('-')).count();
            if deletes || value_args >= 2 {
                deny_unbound_else_chdir(bound, binding)
            } else {
                pass_unbound_else_chdir(bound, binding)
            }
        }
        // Default: passthrough when unbound, chdir when bound.
        _ => {
            if bound {
                if let Some(ref wt) = binding.worktree {
                    return Action::ChdirPass(wt.clone());
                }
            }
            Action::Passthrough
        }
    }
}

// ── Parent-process detection (gh post-merge cleanup heuristic) ──────────

/// Sprint 57 Wave 2 Track D: detect that this `agend-git` invocation
/// is a child of `gh`. Returns `true` only when AGEND_INSTANCE_NAME is
/// set (i.e. we're inside the agent-invoked path the cross-branch
/// fence guards) AND the parent process name is `gh`. Conservative
/// by design: any platform-specific lookup failure returns `false`,
/// letting the fence fire as it would have pre-Track-D rather than
/// silently weakening E4.5.
fn invocation_is_gh_post_merge() -> bool {
    // Operator-shell invocations don't have AGEND_INSTANCE_NAME set;
    // those already hit the early passthrough at the top of `main()`,
    // so the cross-branch fence never fires for them. Restricting the
    // exemption to AGEND_INSTANCE_NAME-set invocations keeps the
    // surface tight.
    if env::var("AGEND_INSTANCE_NAME")
        .ok()
        .is_none_or(|s| s.is_empty())
    {
        return false;
    }
    parent_process_name()
        .map(|n| process_basename_is_gh(&n))
        .unwrap_or(false)
}

/// Pure helper for testability — accepts any process-name string and
/// returns whether it looks like the `gh` binary (basename match).
/// Handles common platform formats: "gh", "/usr/local/bin/gh",
/// "C:\\Program Files\\GitHub CLI\\gh.exe".
fn process_basename_is_gh(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Strip any trailing newline/whitespace and split off the basename.
    let last = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed).trim();
    // Match either `gh` or `gh.exe` (case-insensitive on Windows
    // semantics, but case-sensitive paths are universal — the gh CLI
    // ships its binary lower-case).
    last == "gh" || last.eq_ignore_ascii_case("gh.exe")
}

#[cfg(target_os = "linux")]
fn parent_process_name() -> Option<String> {
    let ppid = unsafe { libc::getppid() };
    let path = format!("/proc/{ppid}/comm");
    std::fs::read_to_string(&path).ok().map(|s| {
        s.trim_end_matches(['\n', '\r', '\0', ' '])
            .trim()
            .to_string()
    })
}

#[cfg(target_os = "macos")]
fn parent_process_name() -> Option<String> {
    let ppid = unsafe { libc::getppid() };
    let output = std::process::Command::new("ps")
        .args(["-p", &ppid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(target_os = "windows")]
fn parent_process_name() -> Option<String> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let pid = sysinfo::Pid::from_u32(std::process::id());
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let parent_pid = sys.process(pid)?.parent()?;
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[parent_pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(parent_pid)
        .map(|p| p.name().to_string_lossy().to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn parent_process_name() -> Option<String> {
    None
}

// ── #1463: init-heartbeat forensic capture ──────────────────────────────

/// Extract the `-m` / `--message` value from a `commit` argv. Supports
/// `-m x`, `-mx`, `--message x`, `--message=x`. Returns the first message.
fn extract_commit_message(args: &[String]) -> Option<&str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-m" || a == "--message" {
            return it.next().map(|s| s.as_str());
        }
        if let Some(v) = a.strip_prefix("--message=") {
            return Some(v);
        }
        if a.len() > 2 && a.starts_with("-m") && !a.starts_with("--") {
            return Some(&a[2..]);
        }
    }
    None
}

/// True if argv is a `commit` whose message is a heartbeat subject
/// (`init` / `initial`) — the bare-init shape the pre-push cleanup targets.
/// Detected from argv (the commit hasn't run yet, so the empty-diff check
/// used by `commit_is_empty_heartbeat` isn't available here); the subject is
/// the heartbeat signature at invocation time. Deliberately does NOT require
/// `--allow-empty` so a heartbeat that omits it is still captured (forensics
/// errs toward catching; whether `--allow-empty` was present is logged).
fn commit_is_init_heartbeat_argv(args: &[String]) -> bool {
    if args.first().map(|s| s.as_str()) != Some("commit") {
        return false;
    }
    extract_commit_message(args)
        .map(is_heartbeat_subject_shim)
        .unwrap_or(false)
}

/// This shim process's parent PID (the process that invoked `git`). Unix
/// via `libc::getppid`; -1 elsewhere (Windows `libc` has no `getppid` — the
/// process-tree forensics are unix-only, matching `parent_process_name`).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parent_pid() -> i32 {
    unsafe { libc::getppid() }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_pid() -> i32 {
    -1
}

/// Walk the process ancestry from this shim's parent up to `max` levels.
/// Each entry is `pid ppid comm args` (one ancestor). The immediate parent
/// is the process that invoked `git` — i.e. the backend CLI we want to pin.
/// Best-effort; empty on unsupported platforms or `ps` failure.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_ancestry(max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut pid = parent_pid();
    for _ in 0..max {
        if pid <= 1 {
            break;
        }
        let line = std::process::Command::new("ps")
            .args(["-o", "pid=,ppid=,comm=,args=", "-p", &pid.to_string()])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if line.is_empty() {
            break;
        }
        // 2nd whitespace field is ppid — parse it to keep walking up.
        let ppid: i32 = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        out.push(line);
        if ppid <= 1 {
            break;
        }
        pid = ppid;
    }
    out
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_ancestry(_max: usize) -> Vec<String> {
    Vec::new()
}

/// #2234 defect#2: is this argv a POSITIONAL-branch `checkout`/`switch` — the
/// canonical-HEAD-touching shape (`git checkout <branch>`, e.g. `origin/main`)?
/// A flag-only / empty target (`git checkout -b …` is still positional after the
/// flag, but a bare `-`/`--detach` lead arg is not a branch nav) is excluded.
/// Pure (no cwd / IO) so the gate is unit-testable.
fn is_positional_branch_checkout(args: &[String]) -> bool {
    matches!(
        args.first().map(|s| s.as_str()),
        Some("checkout") | Some("switch")
    ) && args
        .get(1)
        .is_some_and(|t| !t.is_empty() && !t.starts_with('-'))
}

/// #2234 fix B (pure decision — unit-testable without env/cwd/git). Should an
/// agent's `AGEND_GIT_BYPASS` op be DENIED for canonical-repo safety?
///
/// Deny iff ALL hold:
/// - `agent_present` — `AGEND_INSTANCE_NAME` set (a real fleet agent; daemon
///   internals never reach this shim and carry no instance name);
/// - NOT `escape` — the one-shot `AGEND_GIT_ALLOW_CANONICAL_MUTATE` override is
///   absent (deliberately a SEPARATE env from `AGEND_GIT_BYPASS`, which is what
///   put us on the bypass path in the first place);
/// - `canonical` — cwd is canonical-rooted (the source repo OR a worktree of it,
///   per `cwd_is_canonical_rooted`);
/// - the op is **provisioning / HEAD-detaching**: `worktree add` (the stray /
///   detach vector) or a positional `checkout|switch <ref>` (NOT
///   `checkout -- <pathspec>` / flag forms — excluded by
///   `is_positional_branch_checkout`).
///
/// Deliberately NOT denied: other `worktree` subcommands (`list` is read-only;
/// `remove`/`prune`/`repair`/`move` don't detach or stray); `reset` (agent
/// self-help, moves a branch ref without detaching; `worktree add`'s internal
/// reset is moot once add itself is denied); `push`/`commit`/`add`/`clean`/
/// `branch` (agents legitimately bypass those in their own worktree).
///
/// `args` is the SUBCOMMAND-ROOTED slice — the caller strips leading git globals
/// via `subcommand_index`, so `args.first()` is the real subcommand even for
/// `git -C <path> worktree add` (#2234 Patch A r4).
fn deny_agent_canonical_bypass(
    agent_present: bool,
    escape: bool,
    canonical: bool,
    args: &[String],
) -> bool {
    if !agent_present || escape || !canonical {
        return false;
    }
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    // Only `worktree ADD` is the stray/detach vector. Other worktree subcommands
    // (list/remove/prune/repair/move) neither detach the canonical HEAD nor
    // create a stray worktree, so they pass — `worktree list` in particular is
    // read-only (r4 #2316: blocking all of `worktree` over-blocked beyond the
    // documented threat).
    let is_worktree_add = sub == "worktree" && args.get(1).map(String::as_str) == Some("add");
    is_worktree_add || is_positional_branch_checkout(args)
}

/// #2234 fix B: read the live env + cwd, and if [`deny_agent_canonical_bypass`]
/// fires, print an actionable message and exit non-zero (refusing the bypass
/// passthrough). No-op otherwise. Cross-platform — only env/args/cwd inspection
/// (`cwd_is_canonical_rooted` already has macOS/Linux/Windows impls).
fn enforce_agent_canonical_bypass_deny(args: &[String]) {
    let agent = env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
    let escape = env::var("AGEND_GIT_ALLOW_CANONICAL_MUTATE").as_deref() == Ok("1");
    // #2234 Patch A (r4): resolve the REAL subcommand through leading git globals
    // (`-C`, `-c`, `--git-dir`, …) so `git -C <canonical> worktree add` is judged
    // as `worktree add` — not the `-C` token (the `args.first()` blind spot) — AND
    // against the dir `-C` points at, not the shim's process cwd. No subcommand
    // (globals only) → nothing to deny.
    let Some(sub_idx) = subcommand_index(args) else {
        return;
    };
    let sub_args = &args[sub_idx..];
    let canonical = path_is_canonical_rooted(&effective_cwd_through_globals(args, sub_idx));
    if !deny_agent_canonical_bypass(!agent.is_empty(), escape, canonical, sub_args) {
        return;
    }
    let sub = sub_args.first().map(|s| s.as_str()).unwrap_or("");
    // #2379 ② (r6): the unique header + canonical-specific bypass live in a
    // testable formatter (so the no-"security"-wording meta-test covers THIS prose
    // too, not just the Action::Deny path) — it reuses the shared `deny_remedy_lines`.
    for line in format_canonical_bypass_deny(&agent, sub) {
        eprintln!("{line}");
    }
    std::process::exit(1);
}

/// #2234 defect#2: record a NON-agent (no `AGEND_INSTANCE_NAME`) canonical-cwd
/// `checkout`/`switch <branch>` that the shim is about to pass through via the
/// early-exit `exec_real_git` (it never reaches `classify`). These callers have
/// no agent identity, so attribution relies entirely on PROCESS ANCESTRY — this
/// is the blind spot that left `git checkout origin/main` (canonical-HEAD detach)
/// unattributed. Mirrors `log_init_heartbeat_forensics`: best-effort append to
/// the daemon-observable `fleet_events.jsonl` + a stderr line; NEVER blocks (the
/// caller `exec`s real git immediately after). Instrument-only — no behavior
/// change to the passthrough.
fn log_nonagent_canonical_checkout(home: &str, agent: &str, args: &[String]) {
    if !is_positional_branch_checkout(args) {
        return;
    }
    if !cwd_is_canonical_rooted() {
        return;
    }
    let subcmd = args.first().map(|s| s.as_str()).unwrap_or("");
    let target_branch = args.get(1).cloned().unwrap_or_default();
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let ppid = parent_pid();
    let ancestry = process_ancestry(8);
    let event = serde_json::json!({
        "kind": "git_event",
        "event": "canonical_passthrough_checkout",
        "agent": agent,
        "subcommand": subcmd,
        "target_branch": target_branch,
        "argv": args,
        "cwd": cwd,
        "ppid": ppid,
        "process_ancestry": ancestry,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    let events_path = PathBuf::from(home).join("fleet_events.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{event}");
    }
    eprintln!(
        "[agend-git #2234] non-agent canonical-cwd {subcmd} passthrough (HEAD-touching): target={target_branch} ppid={ppid} cwd={cwd} ancestry={ancestry:?}"
    );
}

/// #2158: build the bypass-mutating-op audit record. Pure — the caller supplies the
/// process context — so the json SHAPE is unit-testable without touching the live
/// process. Mirrors `log_nonagent_canonical_checkout`'s record + adds `bypass_layer`.
fn build_bypass_audit_event(
    agent: &str,
    subcmd: &str,
    args: &[String],
    cwd: &str,
    ppid: i32,
    ancestry: &[String],
    bypass_layer: &str,
) -> serde_json::Value {
    serde_json::json!({
        "kind": "git_event",
        "event": "bypass_mutating_op",
        "agent": agent,
        "subcommand": subcmd,
        "argv": args,
        "cwd": cwd,
        "ppid": ppid,
        "process_ancestry": ancestry,
        "bypass_layer": bypass_layer,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    })
}

/// #2158: audit a SUB-AGENT's own `AGEND_GIT_BYPASS=1 git <mutating>` op — the
/// stray-worktree vector the daemon-side bypass audit (git_helpers.rs, #2242
/// PR2(iii)) cannot see (it audits only the daemon's OWN bypass; the shim is the
/// disjoint agent-side surface). Best-effort append to fleet_events.jsonl (the
/// operator forensics surface, same sink as the #2235 checkout log) + a greppable
/// stderr line; NEVER blocks — the caller `exec`s real git immediately after. The
/// caller gates this to audited ops (Option B) at `shim_depth()==0`.
fn log_bypass_mutating_op(home: &str, agent: &str, args: &[String]) {
    let subcmd = args.first().map(|s| s.as_str()).unwrap_or("");
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let ppid = parent_pid();
    let ancestry = process_ancestry(8);
    let event = build_bypass_audit_event(
        agent,
        subcmd,
        args,
        &cwd,
        ppid,
        &ancestry,
        active_bypass_layer(),
    );
    let events_path = PathBuf::from(home).join("fleet_events.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{event}");
    }
    eprintln!(
        "[agend-git #2158] AGEND_GIT_BYPASS mutating {subcmd} (stray-worktree vector): ppid={ppid} cwd={cwd} ancestry={ancestry:?}"
    );
}

/// The git `user.email` that WOULD author/commit in `cwd` — i.e. the
/// committer identity the heartbeat commit will carry. Invokes the real git
/// (AGEND_REAL_GIT) to avoid recursing through this shim.
fn effective_git_email(cwd: &str) -> Option<String> {
    let real_git = env::var("AGEND_REAL_GIT").unwrap_or_else(|_| "git".to_string());
    let out = std::process::Command::new(real_git)
        .args(["-C", cwd, "config", "user.email"])
        .output()
        .ok()?;
    let email = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!email.is_empty()).then_some(email)
}

/// #1463: append a rich forensic record for an intercepted init-heartbeat
/// commit to the daemon-observable `fleet_events.jsonl`, plus a stderr line
/// (surfaces in the agent pane + daemon log). Best-effort; never blocks the
/// commit.
fn log_init_heartbeat_forensics(home: &str, agent: &str, args: &[String]) {
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let ppid = parent_pid();
    let ancestry = process_ancestry(8);
    let email = effective_git_email(&cwd).unwrap_or_default();
    let has_allow_empty = args.iter().any(|a| a == "--allow-empty");
    let event = serde_json::json!({
        "kind": "git_event",
        "event": "init_heartbeat_forensics",
        "agent": agent,
        "subcommand": "commit",
        "argv": args,
        "allow_empty": has_allow_empty,
        "cwd": cwd,
        "ppid": ppid,
        "process_ancestry": ancestry,
        "git_user_email": email,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    let events_path = PathBuf::from(home).join("fleet_events.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{event}");
    }
    eprintln!(
        "[agend-git #1463] init-heartbeat commit intercepted: agent={agent} email={email} ppid={ppid} cwd={cwd} ancestry={ancestry:?}"
    );
}

// ── #883 pre-push cleanup ───────────────────────────────────────────────

/// #883: drop empty `init` heartbeat commits between `origin/main..HEAD`
/// before the real `git push` fires. Targets the operator-visible case
/// (PR #882 saw 16 inits before the real commit on mobile UI). The
/// cleanup is a local soft-reset to `origin/main` ONLY when EVERY commit
/// in the range is an empty init heartbeat — that's the common case the
/// operator hit. The mixed-history case (real commits interleaved with
/// inits) is left for the existing `repo action=cleanup_init_commits`
/// MCP tool to handle via interactive rebase; we deliberately do not
/// replicate that more-complex path in the shim to keep this function
/// small + self-contained (the shim builds standalone without the
/// library surface — see comment at line ~188).
///
/// **NEVER blocks `git push`.** Any subprocess failure is logged to
/// stderr and the function returns; `main` then proceeds to
/// `exec_real_git` as usual. Cleanup is a best-effort hygiene
/// improvement, not a correctness gate.
///
/// (THIS function's never-blocks contract is unchanged. Note the push PATH can
/// now block: `#2379` `push_trust_root_denylist_violation` runs BEFORE this in
/// the `CleanupAndChdirPushPass` arm and may `exit(1)` — a separate guardrail,
/// not part of this hygiene pass.)
fn cleanup_init_pile_pre_push(worktree: &str) {
    // List commits between origin/main..HEAD with hash + subject.
    let log_out = match Command::new("git")
        .args(["log", "origin/main..HEAD", "--format=%H %s"])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            eprintln!(
                "agend-git: #883 pre-push cleanup git log failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return;
        }
        Err(e) => {
            eprintln!("agend-git: #883 pre-push cleanup git log spawn failed: {e}");
            return;
        }
    };
    let log = String::from_utf8_lossy(&log_out.stdout);
    if log.trim().is_empty() {
        return;
    }
    // Classify each commit. Collect init-heartbeat hashes; anything
    // that isn't a clean empty init is a real commit that must be
    // preserved through the cleanup.
    let mut empty_init_hashes: Vec<String> = Vec::new();
    let mut total = 0usize;
    for line in log.lines() {
        total += 1;
        let (hash, subject) = match line.split_once(' ') {
            Some(p) => p,
            None => continue,
        };
        if !is_heartbeat_subject_shim(subject) {
            continue;
        }
        if !commit_is_empty_heartbeat(worktree, hash) {
            continue;
        }
        empty_init_hashes.push(hash.to_string());
    }
    if empty_init_hashes.is_empty() {
        return;
    }
    // All-init case: soft-reset is enough — drops every commit on the
    // branch above origin/main, leaving working tree clean (since the
    // dropped commits had no file changes).
    if empty_init_hashes.len() == total {
        let reset = Command::new("git")
            .args(["reset", "--soft", "origin/main"])
            .current_dir(worktree)
            .env("AGEND_GIT_BYPASS", "1")
            .status();
        match reset {
            Ok(s) if s.success() => {
                eprintln!(
                    "agend-git: #883 pre-push cleanup soft-reset {total} empty init commit(s)"
                );
            }
            Ok(s) => {
                eprintln!("agend-git: #883 pre-push cleanup soft-reset exited with status {s:?}");
            }
            Err(e) => {
                eprintln!("agend-git: #883 pre-push cleanup soft-reset spawn failed: {e}");
            }
        }
        return;
    }
    // Mixed-history case (operator's PR #882 scenario — 16 inits
    // before the real commit): use interactive rebase with
    // `GIT_SEQUENCE_EDITOR=sed` to rewrite "pick" → "drop" for each
    // init hash. The rebase auto-completes non-interactively.
    //
    // Mirrors `src/mcp/handlers/dispatch_hook/mod.rs:862` mixed-case
    // path. On any failure we run `git rebase --abort` to leave the
    // worktree in a clean state, log to stderr, and let the real
    // `git push` proceed with the pile still in place — better to
    // ship the operator's work than block on cleanup.
    let cleaned = empty_init_hashes.len();
    let sed_parts: Vec<String> = empty_init_hashes
        .iter()
        .map(|h| {
            let short = if h.len() >= 7 { &h[..7] } else { h.as_str() };
            format!("s/^pick {short} /drop {short} /")
        })
        .collect();
    let sed_script = sed_parts.join(";");
    let rebase = Command::new("git")
        .args(["-c", "core.abbrev=7", "rebase", "-i", "origin/main"])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_SEQUENCE_EDITOR", format!("sed -i.bak '{sed_script}'"))
        .status();
    match rebase {
        Ok(s) if s.success() => {
            eprintln!(
                "agend-git: #883 pre-push cleanup dropped {cleaned} empty init commit(s) via rebase"
            );
        }
        _ => {
            // Best-effort abort: leave the worktree in a clean state
            // even if the rebase itself failed mid-flight. Failure
            // to abort is logged but doesn't block push — the user's
            // worst case is the pile-as-before that they had pre-fix.
            let _abort = Command::new("git")
                .args(["rebase", "--abort"])
                .current_dir(worktree)
                .env("AGEND_GIT_BYPASS", "1")
                .status();
            eprintln!(
                "agend-git: #883 pre-push cleanup rebase failed; aborted to leave worktree clean. \
                 Push proceeds with init pile intact ({cleaned} inits remain)."
            );
        }
    }
}

/// Heartbeat-subject whitelist. Mirrors `HEARTBEAT_NAMES` in
/// `src/mcp/handlers/dispatch_hook/mod.rs:951`. Inlined here because
/// the shim is intentionally self-contained (no library imports).
fn is_heartbeat_subject_shim(subject: &str) -> bool {
    matches!(subject, "init" | "initial")
}

/// Verify the commit at `hash` is a true empty heartbeat: empty body
/// (modulo `Agend-*` trailer keys from the prepare-commit-msg hook) AND
/// zero file changes. Either check failing → not eligible for soft-
/// reset cleanup.
///
/// Mirrors the gates in `src/mcp/handlers/dispatch_hook/mod.rs:802-811`
/// plus `commit_body_is_empty` at line 1019. Inlined to keep the shim
/// self-contained.
fn commit_is_empty_heartbeat(worktree: &str, hash: &str) -> bool {
    // Body check — must be empty (apart from the `prepare-commit-msg`
    // hook's daemon trailers which are noise from this perspective).
    let body_out = match Command::new("git")
        .args(["log", "-1", "--format=%b", hash])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let body = String::from_utf8_lossy(&body_out.stdout);
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Tolerate the four `Agend-*` trailer keys the prepare-commit-msg
        // hook injects (`Agend-Agent`, `Agend-Task`, `Agend-Branch`,
        // `Agend-Issued-At` — per `dispatch_hook/mod.rs:979`). Anything
        // else means there's a real commit message body → not a
        // heartbeat.
        if trimmed.starts_with("Agend-Agent:")
            || trimmed.starts_with("Agend-Task:")
            || trimmed.starts_with("Agend-Branch:")
            || trimmed.starts_with("Agend-Issued-At:")
        {
            continue;
        }
        return false;
    }
    // Diff check — must have zero file changes (otherwise it's a
    // legitimate commit that happens to use the `init` subject).
    let diff_out = match Command::new("git")
        .args(["diff-tree", "--no-commit-id", "--name-only", "-r", hash])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    diff_out.stdout.trim_ascii().is_empty()
}

// ── #2379 ③ denylist-core: trust-root push deny ─────────────────────────

/// Trust-root filenames an agent must never push into a shared repo. These live
/// in `$AGEND_HOME` (the config-integrity key, the fleet config, the append-only
/// audit logs); `.gitignore` blocks the common ones but `git add -f` bypasses it,
/// so this denylist is the push-time enforcement layer. Matched against a blob's
/// repo-relative BASENAME / extension (see `trust_root_basename_denied`).
const TRUST_ROOT_DENY_NAMES: &[&str] = &[".config-integrity-key", "policy.toml", "fleet.yaml"];

/// Whether a repo-relative blob path is a trust-root file: its BASENAME is an
/// exact trust-root name, or it is an audit log (`*.jsonl`). Pure — fed by the
/// impure range enumeration.
///
/// ⚠ Matches the repo-relative path's basename, NOT a `$AGEND_HOME` filesystem
/// prefix: a managed worktree lives UNDER `$AGEND_HOME/worktrees/<agent>/<branch>`
/// (binding.rs), so an abs-path-under-`$AGEND_HOME` test would match EVERY file in
/// the worktree and false-block every push. `git --name-only` yields repo-relative
/// paths, so basename matching is correct. Basename via `rsplit('/')` (NOT
/// `lstrip`/`trim_start_matches`, which would eat the leading dot of
/// `.config-integrity-key`). Basename-anywhere, so a sub-directory dodge
/// (`stash/fleet.yaml`) is still caught.
fn trust_root_basename_denied(repo_relative_path: &str) -> bool {
    let basename = repo_relative_path
        .rsplit('/')
        .next()
        .unwrap_or(repo_relative_path);
    TRUST_ROOT_DENY_NAMES.contains(&basename) || basename.ends_with(".jsonl")
}

/// The repo-relative paths touched by any commit in the push range
/// (`origin/main..HEAD`) — the union of `--name-only` across the range, so a
/// trust-root blob added in an intermediate commit is caught even if a later
/// commit deletes it (the blob is still in the pushed history). Runs real git in
/// the worktree with `AGEND_GIT_BYPASS=1` (mirrors `cleanup_init_pile_pre_push`'s
/// established range base). Returns `Err(msg)` when the range can't be computed
/// (e.g. `origin/main` not fetched) so the caller can fail CLOSED.
fn push_range_files(worktree: &str) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .args([
            "log",
            "--name-only",
            "--pretty=format:",
            "origin/main..HEAD",
        ])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map_err(|e| format!("git log spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git log origin/main..HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut files: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// Scan the push range for a trust-root file; return an actionable deny reason or
/// `None` to allow. **Fails CLOSED**: a range-computation error returns a deny
/// reason — security over best-effort. This is intentionally STRICTER than
/// `cleanup_init_pile_pre_push`, which no-ops (allows the push) on the same
/// `origin/main..HEAD` error; that cleanup is hygiene, this is a guardrail.
fn push_trust_root_denylist_violation(worktree: &str) -> Option<String> {
    match push_range_files(worktree) {
        Ok(files) => files
            .into_iter()
            .find(|p| trust_root_basename_denied(p))
            .map(|p| {
                format!(
                    "push range contains a trust-root file: `{p}` — $AGEND_HOME config / \
                     integrity key / audit logs must never be pushed into a shared repo. \
                     Drop it from the pushed commits (e.g. `git rm --cached {p}` then amend/rebase) \
                     and retry."
                )
            }),
        Err(e) => Some(format!(
            "could not verify the push against the trust-root denylist ({e}); refusing to \
             push (fail-closed). Fetch origin so `origin/main..HEAD` resolves \
             (`git fetch origin`), then retry."
        )),
    }
}

// ── #2379 S3: protected-ref push deny (policy.toml override) ─────────────

/// The ALWAYS-protected refs — the hardcode floor an operator override can only ADD to
/// (tighten-only), never shrink. Mirrors `is_protected_ref` (kept in sync with the
/// lib-side E4.5 guard).
const HARDCODE_PROTECTED_REFS: &[&str] = &["main", "master"];

/// The protected-ref set this invocation enforces: the hardcode floor (`main`/`master`)
/// PLUS the operator's `$AGEND_HOME/policy.toml` `protected_refs` override — but ONLY
/// when the file is present, HMAC-verified (hygiene, mirrors `read_binding`'s sidecar),
/// and parses. **Fail-closed, never less safe than the hardcode floor:**
/// - missing policy.toml → hardcode floor only (the default),
/// - tampered / unsigned sidecar → hardcode floor only (override ignored),
/// - unparseable array → hardcode floor only (override ignored).
///
/// The override is additive-only, so the floor is always denied regardless. HMAC is
/// hygiene, NOT a security boundary (a same-uid agent could re-sign — #1653 ceiling).
fn load_protected_refs(home: &str) -> Vec<String> {
    let mut refs: Vec<String> = HARDCODE_PROTECTED_REFS
        .iter()
        .map(|s| s.to_string())
        .collect();
    let path = PathBuf::from(home).join("policy.toml");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return refs; // missing → hardcode floor (the common default)
    };
    let tag =
        std::fs::read_to_string(PathBuf::from(home).join("policy.toml.sig")).unwrap_or_default();
    if !integrity_core::verify(Path::new(home), content.as_bytes(), &tag) {
        return refs; // tampered / unsigned → fail-closed (override ignored)
    }
    refs.extend(parse_protected_refs(&content));
    refs
}

/// MVP hand-parse of `protected_refs = ["a", "b"]` from policy.toml. The shim builds
/// STANDALONE (the `toml` crate is `tray`-gated; prod must not depend on it — same
/// convention as `codex_trust_directory`), and the MVP needs only a flat string array.
/// Locates the `protected_refs` key, captures its `[ … ]` body (single- or multi-line),
/// and collects the `"…"`-quoted entries. Anything malformed (no key / `=` / `[` / a
/// missing `]`) yields an empty list → fail-closed to the hardcode floor. Glob patterns
/// (`release/*`) are a follow-up — MVP is exact-match.
fn parse_protected_refs(content: &str) -> Vec<String> {
    let Some(key) = content.find("protected_refs") else {
        return Vec::new();
    };
    let after_key = &content[key..];
    let Some(eq) = after_key.find('=') else {
        return Vec::new();
    };
    let after_eq = &after_key[eq + 1..];
    let Some(open) = after_eq.find('[') else {
        return Vec::new();
    };
    let body = &after_eq[open + 1..];
    let Some(close) = body.find(']') else {
        return Vec::new(); // unterminated array → fail-closed
    };
    extract_quoted(&body[..close])
}

/// Collect every `"…"`-quoted substring (no escape handling — refs don't contain escaped
/// quotes at MVP). An unterminated final quote stops the scan (fail-toward-fewer).
fn extract_quoted(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        match after.find('"') {
            Some(end) => {
                out.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out
}

/// The remote DESTINATION ref each `git push` positional would write, normalized for
/// matching: drop the leading `+` force marker, take the segment after the last `:` (the
/// refspec dest), and strip a `refs/heads/` prefix. Flags (`-…`) are skipped; the remote
/// name is harmless (it just won't match a protected ref). Covers `HEAD:main`,
/// `+HEAD:main`, `:main` (delete), `--delete main`, `HEAD:refs/heads/main`, and a bare
/// `main`; leaves a normal `feat/x` / `HEAD` push untouched.
fn push_dest_refs(args: &[String]) -> Vec<String> {
    args.iter()
        .skip(1) // "push"
        .filter(|a| !a.starts_with('-'))
        .map(|a| {
            let a = a.strip_prefix('+').unwrap_or(a);
            let dest = a.rsplit(':').next().unwrap_or(a);
            dest.strip_prefix("refs/heads/").unwrap_or(dest).to_string()
        })
        .collect()
}

/// #2379 S3: a `git push` is DENIED iff it could write a protected ref. COMPREHENSIVE over
/// the push surface (r6: a positional-only parse let `--all`/`--mirror` slip through).
/// Returns an actionable deny reason, or `None` to allow:
/// - **`--all` / `--mirror`** (+ unambiguous abbreviations) push EVERY local head incl.
///   protected ones → deny (a bound agent must push an explicit refspec of its OWN branch);
/// - an **explicit refspec** whose DEST is a protected ref (exact, case-insensitive) → deny;
/// - a **wildcard** refspec dest (`refs/heads/*`) could write a protected ref → deny
///   (conservative — a bound agent pushes its explicit branch; glob-vs-protected refinement
///   is a follow-up);
/// - a **no-refspec** push (`git push` / `git push <remote>`) targets the CURRENT branch
///   under the modern `push.default` (simple/current/upstream) = a bound agent's
///   non-protected assigned branch (cross-branch deny) → allow; EXCEPT the deprecated
///   `push.default=matching`, which would ALSO push a local `main`/`master` → deny.
///
/// `--tags` is TAGS-ONLY (`refs/tags/*`, never a branch) regardless of push.default, so it
/// is exempt even from the matching deny (r6 dry-run: `git push --tags` under matching pushes
/// only tags). `--follow-tags` is NOT exempt: it pushes the would-be-pushed BRANCHES *plus*
/// tags, so under `push.default=matching` it pushes the matching heads incl. `main`
/// (empirically confirmed via dry-run) → it correctly hits the matching deny. Force flags
/// (`-f`/`--force-with-lease`/`+`) change HOW not WHAT — the refspec is still parsed above.
/// Shim-layer defense-in-depth — the remote's branch protection is the primary gate.
fn push_protected_violation(
    args: &[String],
    protected: &[String],
    push_default_matching: bool,
) -> Option<String> {
    if let Some(flag) = args.iter().skip(1).find(|a| is_bulk_push_flag(a)) {
        return Some(format!(
            "`{flag}` pushes ALL local refs (including protected ones) — push an explicit \
             refspec of your own task branch instead, not all refs at once"
        ));
    }
    for dest in push_dest_refs(args) {
        if dest.contains('*') {
            return Some(format!(
                "wildcard refspec dest `{dest}` could write a protected ref — push an \
                 explicit, single-ref refspec instead"
            ));
        }
        if protected.iter().any(|p| p.eq_ignore_ascii_case(&dest)) {
            return Some(format!(
                "protected ref — pushing to '{dest}' is denied (shim-layer guard; the \
                 remote's branch protection is the primary gate). Push your own task branch \
                 and open a PR; do NOT push directly to a protected ref."
            ));
        }
    }
    if push_default_matching && !has_explicit_refspec(args) && !is_tags_only_push(args) {
        return Some(
            "push.default=matching with no explicit refspec would push every same-named \
             branch (including a local protected ref) — set push.default=current/simple, or \
             push an explicit refspec of your own task branch"
                .to_string(),
        );
    }
    None
}

/// `--tags` makes the push TAGS-ONLY (`refs/tags/*`), regardless of `push.default` — so it is
/// exempt from the matching deny. Deliberately matches ONLY `--tags`, NOT `--follow-tags`
/// (which also pushes the would-be-pushed branches → under matching pushes `main`).
fn is_tags_only_push(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| a == "--tags")
}

/// `--all` / `--mirror`, INCLUDING git's unambiguous long-option abbreviations (`--mir`,
/// `--al`, …). Errs toward deny (#2027 flag-form lesson): an ambiguous prefix (`--a`, `--m`)
/// also matches — git itself rejects those, so denying them costs nothing. `--tags` /
/// `--follow-tags` / force flags do NOT match (they don't push a protected branch).
fn is_bulk_push_flag(arg: &str) -> bool {
    match arg.strip_prefix("--") {
        Some(name) if !name.is_empty() => "all".starts_with(name) || "mirror".starts_with(name),
        _ => false,
    }
}

/// Whether the push names an EXPLICIT refspec (≥2 positionals after `push` — a remote AND a
/// refspec). With 0–1 positionals (no-arg, or just a remote) the ref is resolved from
/// `push.default` + the current branch instead.
fn has_explicit_refspec(args: &[String]) -> bool {
    args.iter().skip(1).filter(|a| !a.starts_with('-')).count() >= 2
}

/// True iff the worktree's effective `push.default` is the (deprecated) `matching` mode —
/// the one value where a no-refspec push writes MORE than the current branch. Unset → git's
/// built-in `simple` → false. Best-effort real-git read (read-only); any failure → false.
fn push_default_is_matching(worktree: &str) -> bool {
    let git = resolve_real_git();
    Command::new(&git)
        .env("AGEND_GIT_SHIM_DEPTH", (shim_depth() + 1).to_string())
        .args(["-C", worktree, "config", "--get", "push.default"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "matching")
        .unwrap_or(false)
}

// ── Exec ────────────────────────────────────────────────────────────────

fn exec_with_conflict_guidance(
    args: &[String],
    worktree: &str,
    home: &str,
    agent: &str,
    subcmd: &str,
) -> ! {
    let git = resolve_real_git();
    // #1504 L3: propagate incremented depth (rebase/merge/pull/cherry-pick reach
    // here and also spawn real git — same recursion vector as exec_real_git).
    let status = Command::new(&git)
        .env("AGEND_GIT_SHIM_DEPTH", (shim_depth() + 1).to_string())
        .arg("-C")
        .arg(worktree)
        .args(args)
        .status();
    match status {
        Ok(st) => {
            if !st.success() && has_unmerged_files(&git, worktree) {
                emit_conflict_guidance(home, agent, subcmd);
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = st.signal() {
                    std::process::exit(128 + sig);
                }
            }
            std::process::exit(st.code().unwrap_or(1))
        }
        Err(e) => {
            eprintln!("agend-git: exec failed: {e}");
            std::process::exit(127);
        }
    }
}

fn exec_real_git(args: &[String], chdir: Option<&str>) -> ! {
    let git = resolve_real_git();
    let mut cmd = Command::new(&git);
    // #1504 L3: propagate the incremented depth so a self-resolution loop trips
    // the recursion guard at the next entry instead of spawning unbounded.
    cmd.env("AGEND_GIT_SHIM_DEPTH", (shim_depth() + 1).to_string());
    if let Some(dir) = chdir {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);

    // Unix: exec() replaces process. Windows: status() + exit(code).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        eprintln!("agend-git: exec failed: {err}");
        std::process::exit(127);
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(e) => {
                eprintln!("agend-git: exec failed: {e}");
                std::process::exit(127);
            }
        }
    }
}

fn resolve_real_git() -> String {
    // Priority 1: AGEND_REAL_GIT env (injected by daemon at spawn).
    if let Ok(path) = env::var("AGEND_REAL_GIT") {
        if !path.is_empty() && std::path::Path::new(&path).exists() {
            return path;
        }
    }
    // Priority 2: which excluding $AGEND_HOME/bin/ (the shim dir).
    // #1504 L2: exclude via canonicalized Path comparison, not a string compare.
    // `format!("{h}/bin")` (forward slash) never matched a Windows PATH entry
    // (backslash / case / trailing-slash), so the shim failed to exclude itself
    // and `which_in` resolved git back to THIS binary → recursive-spawn storm.
    // With L1 fixed the daemon injects AGEND_REAL_GIT and Priority 1 above
    // short-circuits, so this fallback rarely runs — but it must be correct when
    // it does. `split_paths` also gives the right separator + drive-colon handling.
    let agend_bin: Option<PathBuf> =
        env::var_os("AGEND_HOME").map(|h| PathBuf::from(h).join("bin"));
    let path_os = env::var_os("PATH").unwrap_or_default();
    let search_paths: Vec<PathBuf> = std::env::split_paths(&path_os)
        .filter(|p| !p.as_os_str().is_empty())
        .filter(|p| !same_dir(p, agend_bin.as_deref()))
        .collect();
    let search = std::env::join_paths(&search_paths).unwrap_or_default();
    which::which_in("git", Some(&search), ".")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/usr/bin/git".to_string())
}

/// #1504: directory equality via `canonicalize` (resolves slash form, case-folds
/// on Windows NTFS, follows symlinks), with a lexical fallback when a path can't
/// be canonicalized (e.g. `$AGEND_HOME/bin` not yet created). NEVER unwraps
/// `canonicalize` — it `Err`s on missing paths.
fn same_dir(a: &std::path::Path, b: Option<&std::path::Path>) -> bool {
    let Some(b) = b else { return false };
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => lexical_path_eq(a, b),
    }
}

/// Lexical directory equality: normalize backslashes to `/`, strip trailing
/// separators, compare case-insensitively on Windows. Fallback only.
fn lexical_path_eq(a: &std::path::Path, b: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| {
        p.to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string()
    };
    let (na, nb) = (norm(a), norm(b));
    if cfg!(windows) {
        na.eq_ignore_ascii_case(&nb)
    } else {
        na == nb
    }
}

// ── Error + Telemetry ───────────────────────────────────────────────────

fn emit_deny_error(subcmd: &str, reason: &str, agent: &str, binding: Option<&Binding>) {
    for line in format_deny_error(subcmd, reason, agent, binding) {
        eprintln!("{line}");
    }
}

/// #2379 ②: the shared, context-aware "where to run this instead" remedy block,
/// reused by every deny exit so they stay consistent. Pure `format!`, ZERO I/O —
/// `binding` is the IN-SCOPE binding (already loaded before `classify`) at the
/// `Action::Deny` / push-denylist sites, and `None` at the early canonical-bypass
/// deny (env+cwd only, no binding loaded). When the caller is bound, it names the
/// agent's own worktree so the fix is actionable ("cd there"); otherwise it points
/// at the ways to get a worktree. (Intentionally avoids "security"-flavoured
/// wording per the operator copy rule — enforced by a meta-test.)
fn deny_remedy_lines(binding: Option<&Binding>) -> Vec<String> {
    // #2379 ② (r6): decide "bound" by the SAME predicate production uses —
    // `is_bound` (task_id.is_some()) — AND require a worktree to name, so the
    // remedy can never contradict classify's deny verdict. A partial binding
    // (task_id=None, worktree=Some) is UNBOUND to classify, so it must get the
    // generic remedy here too — never a "your assigned worktree is <stale>" line
    // pointing at a path the caller isn't actually assigned to.
    match binding {
        Some(b) if is_bound(b) && b.worktree.is_some() => {
            let wt = b.worktree.as_deref().unwrap_or_default();
            let branch = b.branch.as_deref().unwrap_or("<unknown>");
            let task = b.task_id.as_deref().unwrap_or("—");
            vec![
                format!("           your assigned worktree is {wt}"),
                format!(
                    "           (branch '{branch}', task {task}) — cd there and run git, no bypass needed"
                ),
            ]
        }
        // Unbound / partial binding / no binding in scope: point at how to get one.
        _ => vec![
            "           you have no active worktree binding here:".to_string(),
            "             - if the daemon auto-bound one for this task, check `binding_state` and cd into it"
                .to_string(),
            "             - otherwise get one via the task board, `repo action=checkout bind=true`, or `bind_self`"
                .to_string(),
        ],
    }
}

/// #2379 ② (r6): the canonical-bypass deny block as a testable `Vec<String>`.
/// The header + the canonical-specific `AGEND_GIT_ALLOW_CANONICAL_MUTATE` bypass
/// are unique to this early deny (no `Binding` is loaded — env+cwd only, so the
/// generic [`deny_remedy_lines`]`(None)` remedy is used). Extracted from the
/// inline `eprintln!`s so the no-"security"-wording meta-test covers this prose
/// too (the inline form was a meta-test blind spot — r6).
fn format_canonical_bypass_deny(agent: &str, sub: &str) -> Vec<String> {
    let mut lines = vec![
        format!(
            "agend-git: DENIED — agent '{agent}' must not bypass-{sub} in a canonical-rooted repo."
        ),
        "           a stray provision here detaches the operator's canonical HEAD (#2234)."
            .to_string(),
    ];
    lines.extend(deny_remedy_lines(None));
    lines.push(
        "           or, if you genuinely must: set AGEND_GIT_ALLOW_CANONICAL_MUTATE=1 for a one-shot (or ask lead)."
            .to_string(),
    );
    lines
}

/// Sprint 54 P2-4: build the deny-error block as a `Vec<String>` so the
/// 3-form bypass hint can be unit-tested for env-var-name presence
/// without capturing stderr. `emit_deny_error` is a thin wrapper that
/// `eprintln!`s each line. Per `should_bypass` (above), three bypass
/// forms exist; the hint enumerates all of them so operators don't
/// have to grep the source to discover the agent-specific or
/// time-limited variants.
///
/// #2379 ②: now carries the in-scope binding context via [`deny_remedy_lines`]
/// so every deny tells the caller WHERE to run the command instead (its own
/// worktree, or how to get one) — not just how to bypass.
fn format_deny_error(
    subcmd: &str,
    reason: &str,
    agent: &str,
    binding: Option<&Binding>,
) -> Vec<String> {
    let mut lines = vec![
        format!("agend-git: ERROR git {subcmd} denied"),
        format!("           agent={agent}, reason: {reason}"),
    ];
    lines.extend(deny_remedy_lines(binding));
    lines.push("           or bypass with one of:".to_string());
    lines.push(
        "             AGEND_GIT_BYPASS=1               one-shot emergency override".to_string(),
    );
    lines.push(
        "             AGEND_GIT_BYPASS_AGENT=<name>    agent-specific exemption (matches AGEND_INSTANCE_NAME)"
            .to_string(),
    );
    lines.push(
        "             AGEND_GIT_BYPASS_UNTIL=<epoch>   time-limited exemption (Unix seconds, not ISO)"
            .to_string(),
    );
    lines
}

/// #2379 ②: the agent-facing DISPOSITION of a git_event — whether the agent must
/// STOP or may CONTINUE. Distinct from the fleet-events envelope (`"kind":"git_event"`)
/// and from the `event` type string; it is the single axis an agent routes its retry
/// decision on.
/// - `Deny` — terminal, fail-closed: the op was BLOCKED; the agent must fix + retry.
/// - `Warn` — advisory: the op proceeded (or a non-blocking condition was flagged); the
///   agent should heed it but is NOT blocked (e.g. merge conflict, cwd/worktree drift).
/// - `Info` — pure record (e.g. a recognized exemption); no agent action implied.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Disposition {
    Deny,
    Warn,
    Info,
}

impl Disposition {
    fn as_str(self) -> &'static str {
        match self {
            Disposition::Deny => "deny",
            Disposition::Warn => "warn",
            Disposition::Info => "info",
        }
    }
}

/// #2379 ②: the SINGLE SOURCE mapping every emitted `event_type` → its [`Disposition`],
/// so a type's disposition can never drift between call sites. An unmapped type fails
/// CLOSED to `Deny` (an unrecognized event reads as "stop + check", never silently
/// advisory); `disposition_for_covers_all_emitted_event_types_2379` pins every real type.
fn disposition_for(event_type: &str) -> Disposition {
    match event_type {
        "deny" | "deny_trust_root" | "deny_protected_ref" => Disposition::Deny,
        "cwd_worktree_drift" | "git_conflict" => Disposition::Warn,
        "post_merge_cleanup_exempt" => Disposition::Info,
        _ => Disposition::Deny,
    }
}

/// Sprint 57 Wave 2 Track D: structured audit-event writer with an
/// explicit event-type discriminator. Replaces the previous untyped
/// `write_git_event` that hardcoded `event="deny"`. `event_type` is
/// the new `kind`-style discriminator (`"deny"` or
/// `"post_merge_cleanup_exempt"`); `target_branch` carries the
/// resolved checkout target when relevant for the exemption case;
/// `detail` mirrors the human-readable reason string.
///
/// #2379 ②: every event also carries a `disposition` (deny|warn|info, via
/// [`disposition_for`]) so an agent reading `fleet_events.jsonl` can route deny
/// (must-stop) vs warn (advisory) WITHOUT re-deriving it from the `event` string.
fn write_git_event_typed(
    home: &str,
    agent: &str,
    subcmd: &str,
    event_type: &str,
    target_branch: Option<&str>,
    detail: Option<&str>,
) {
    let events_path = PathBuf::from(home).join("fleet_events.jsonl");
    let event = serde_json::json!({
        "kind": "git_event",
        "event": event_type,
        // #2379 ②: deny|warn|info — the agent's stop-vs-continue routing axis.
        "disposition": disposition_for(event_type).as_str(),
        "agent": agent,
        "subcommand": subcmd,
        "target_branch": target_branch,
        "reason": detail,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    // Best-effort append (don't block on failure).
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", event);
    }
}

fn is_conflict_capable(subcmd: &str) -> bool {
    matches!(subcmd, "rebase" | "merge" | "pull" | "cherry-pick")
}

fn has_unmerged_files(git: &str, worktree: &str) -> bool {
    Command::new(git)
        .arg("-C")
        .arg(worktree)
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

/// #2379 ②: a merge conflict is a WARN, not a deny — the op ran, git left conflict
/// markers, and the agent RESOLVES + continues (it must NOT abandon/redo). Previously
/// this guidance was stderr-only → invisible to fleet observers; mirror it into
/// `fleet_events.jsonl` as a `git_conflict` event (disposition=warn via `disposition_for`)
/// for parity with deny events, then print the unchanged stderr guidance.
fn emit_conflict_guidance(home: &str, agent: &str, subcmd: &str) {
    write_git_event_typed(
        home,
        agent,
        subcmd,
        "git_conflict",
        None,
        Some("merge conflict — resolve the markers and continue (do not abandon/redo)"),
    );
    eprint!("{}", format_conflict_guidance());
}

fn format_conflict_guidance() -> &'static str {
    "\n\u{26a0} Merge conflict detected. To resolve:\n\
     1. Edit the conflicted files listed above \u{2014} resolve all <<<<<<< / ======= / >>>>>>> markers\n\
     2. git add <resolved-files>\n\
     3. git rebase --continue (or git merge --continue / git cherry-pick --continue)\n\
     Do NOT abandon and redo from scratch unless the conflict involves complex semantic changes you cannot resolve.\n"
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "agend-git/tests.rs"]
mod tests;
