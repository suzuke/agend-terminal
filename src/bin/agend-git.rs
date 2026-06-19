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
            exec_with_conflict_guidance(&strip_target_overrides(&args), &worktree);
        }
        Action::ChdirPass(worktree) => {
            exec_real_git(&strip_target_overrides(&args), Some(&worktree))
        }
        Action::CleanupAndChdirPushPass(worktree) => {
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
            emit_deny_error(subcommand, &reason, &agent);
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
    let cwd = match env::current_dir() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let dot_git = cwd.join(".git");
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
    if !deny_agent_canonical_bypass(!agent.is_empty(), escape, cwd_is_canonical_rooted(), args) {
        return;
    }
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    eprintln!(
        "agend-git: DENIED — agent '{agent}' must not bypass-{sub} in a canonical-rooted repo.\n\
         If the daemon auto-bound a worktree for this task (check `binding_state`), cd into it and use normal git \
         (no AGEND_GIT_BYPASS); otherwise use `bind_self` or ask lead. A stray provision here detaches the operator's canonical HEAD (#2234).\n\
         If you genuinely must: ask lead, or set AGEND_GIT_ALLOW_CANONICAL_MUTATE=1 for a one-shot."
    );
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

// ── Exec ────────────────────────────────────────────────────────────────

fn exec_with_conflict_guidance(args: &[String], worktree: &str) -> ! {
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
                eprint!("{}", format_conflict_guidance());
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

fn emit_deny_error(subcmd: &str, reason: &str, agent: &str) {
    for line in format_deny_error(subcmd, reason, agent) {
        eprintln!("{line}");
    }
}

/// Sprint 54 P2-4: build the deny-error block as a `Vec<String>` so the
/// 3-form bypass hint can be unit-tested for env-var-name presence
/// without capturing stderr. `emit_deny_error` is a thin wrapper that
/// `eprintln!`s each line. Per `should_bypass` (above), three bypass
/// forms exist; the hint enumerates all of them so operators don't
/// have to grep the source to discover the agent-specific or
/// time-limited variants.
fn format_deny_error(subcmd: &str, reason: &str, agent: &str) -> Vec<String> {
    vec![
        format!("agend-git: ERROR git {subcmd} denied"),
        format!("           agent={agent}, reason: {reason}"),
        "           HINT: use the task board for a worktree assignment, or bypass with one of:".to_string(),
        "             AGEND_GIT_BYPASS=1               one-shot emergency override".to_string(),
        "             AGEND_GIT_BYPASS_AGENT=<name>    agent-specific exemption (matches AGEND_INSTANCE_NAME)".to_string(),
        "             AGEND_GIT_BYPASS_UNTIL=<epoch>   time-limited exemption (Unix seconds, not ISO)".to_string(),
    ]
}

/// Sprint 57 Wave 2 Track D: structured audit-event writer with an
/// explicit event-type discriminator. Replaces the previous untyped
/// `write_git_event` that hardcoded `event="deny"`. `event_type` is
/// the new `kind`-style discriminator (`"deny"` or
/// `"post_merge_cleanup_exempt"`); `target_branch` carries the
/// resolved checkout target when relevant for the exemption case;
/// `detail` mirrors the human-readable reason string.
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

fn format_conflict_guidance() -> &'static str {
    "\n\u{26a0} Merge conflict detected. To resolve:\n\
     1. Edit the conflicted files listed above \u{2014} resolve all <<<<<<< / ======= / >>>>>>> markers\n\
     2. git add <resolved-files>\n\
     3. git rebase --continue (or git merge --continue / git cherry-pick --continue)\n\
     Do NOT abandon and redo from scratch unless the conflict involves complex semantic changes you cannot resolve.\n"
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── CR-2026-06-14: shim is_protected_ref must mirror the lib-side
    // case-insensitive E4.5 guard (kept in sync; see agent_ops.rs). ──
    #[test]
    fn shim_is_protected_ref_case_insensitive() {
        for v in ["main", "master", "Main", "MAIN", "Master", "MASTER"] {
            assert!(is_protected_ref(v), "{v:?} must be protected");
        }
        // Full-string compare: substrings / case-only-prefix are not over-blocked.
        for v in [
            "mainline",
            "maintenance",
            "main-feature",
            "upstream-main",
            "",
        ] {
            assert!(!is_protected_ref(v), "{v:?} must NOT be protected");
        }
    }

    // ── #1651: binding.json HMAC verify (push-authority integrity) ──
    fn home_1651(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("agend-git-1651-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        // The shared integrity key (both signer + verifier read it).
        std::fs::write(p.join(".config-integrity-key"), [7u8; 32]).unwrap();
        p
    }

    fn write_binding_1651(home: &Path, agent: &str, body: &str, signed: bool) {
        let dir = home.join("runtime").join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("binding.json"), body).unwrap();
        if signed {
            let tag = integrity_core::sign_for_test(home, body.as_bytes());
            std::fs::write(dir.join("binding.json.sig"), tag).unwrap();
        }
    }

    // No `worktree` field → the orphan-worktree guard is skipped, so the binding
    // outcome reflects the HMAC verify alone.
    const BODY_1651: &str = r#"{"version":1,"task_id":"T-1","branch":"feat/x"}"#;

    #[test]
    fn valid_signed_binding_is_bound_1651() {
        let home = home_1651("valid");
        write_binding_1651(&home, "ag", BODY_1651, true);
        let b = read_binding(home.to_str().unwrap(), "ag");
        assert!(is_bound(&b), "a validly-signed binding must be bound");
        assert_eq!(b.branch.as_deref(), Some("feat/x"));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn tampered_branch_without_resign_is_unbound_and_denied_1651() {
        let home = home_1651("tamper");
        // Daemon wrote + signed the original (branch feat/x).
        write_binding_1651(&home, "ag", BODY_1651, true);
        // Injection blind-write: self-authorize `main`, WITHOUT re-signing
        // (the agent doesn't know the key/scheme).
        let tampered = r#"{"version":1,"task_id":"T-1","branch":"main"}"#;
        std::fs::write(
            home.join("runtime").join("ag").join("binding.json"),
            tampered,
        )
        .unwrap();
        let b = read_binding(home.to_str().unwrap(), "ag");
        assert!(
            !is_bound(&b),
            "#1651: a tampered (unsigned-for-new-content) binding must read as unbound"
        );
        // …and a mutating op on unbound takes the EXISTING fail-closed deny path.
        assert!(matches!(
            deny_unbound_else_chdir(is_bound(&b), &b),
            Action::Deny(_)
        ));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn missing_sidecar_is_unbound_1651() {
        let home = home_1651("nosig");
        write_binding_1651(&home, "ag", BODY_1651, false); // NO sidecar
        let b = read_binding(home.to_str().unwrap(), "ag");
        assert!(
            !is_bound(&b),
            "#1651: a binding with no HMAC sidecar must fail closed (unbound)"
        );
        std::fs::remove_dir_all(home).ok();
    }

    // ── #1463: init-heartbeat argv detection (forensic hook gate) ──
    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // ── #1504 L2: shim self-exclusion via canonicalize + lexical fallback ──
    #[test]
    fn same_dir_lexical_slash_fallback_1504() {
        // Nonexistent dirs → lexical fallback → backslash normalized to `/`, so a
        // forward-slash `$AGEND_HOME/bin` still matches a backslash PATH entry
        // (the Windows self-exclusion miss that caused the recursion).
        assert!(same_dir(
            std::path::Path::new("C:/h/bin"),
            Some(std::path::Path::new("C:\\h\\bin")),
        ));
        assert!(!same_dir(
            std::path::Path::new("/usr/bin"),
            Some(std::path::Path::new("/h/bin")),
        ));
        assert!(!same_dir(std::path::Path::new("/usr/bin"), None));
    }

    #[test]
    fn extract_commit_message_all_forms() {
        assert_eq!(
            extract_commit_message(&s(&["commit", "-m", "init"])),
            Some("init")
        );
        assert_eq!(
            extract_commit_message(&s(&["commit", "-minit"])),
            Some("init")
        );
        assert_eq!(
            extract_commit_message(&s(&["commit", "--message", "init"])),
            Some("init")
        );
        assert_eq!(
            extract_commit_message(&s(&["commit", "--message=init"])),
            Some("init")
        );
        assert_eq!(
            extract_commit_message(&s(&["commit", "--allow-empty"])),
            None
        );
    }

    #[test]
    fn init_heartbeat_argv_detected() {
        assert!(commit_is_init_heartbeat_argv(&s(&[
            "commit",
            "--allow-empty",
            "-m",
            "init"
        ])));
        // `--allow-empty` not required (forensics errs toward catching).
        assert!(commit_is_init_heartbeat_argv(&s(&[
            "commit", "-m", "initial"
        ])));
        assert!(commit_is_init_heartbeat_argv(&s(&["commit", "-minit"])));
    }

    #[test]
    fn init_heartbeat_argv_rejects_non_heartbeat() {
        // Real work commits / other subcommands must NOT trigger the hook.
        assert!(!commit_is_init_heartbeat_argv(&s(&[
            "commit",
            "-m",
            "fix: real work"
        ])));
        assert!(!commit_is_init_heartbeat_argv(&s(&[
            "commit",
            "--allow-empty"
        ])));
        assert!(!commit_is_init_heartbeat_argv(&s(&["status"])));
        assert!(!commit_is_init_heartbeat_argv(&s(&["push"])));
    }

    fn bound_binding(branch: &str, worktree: &str) -> Binding {
        Binding {
            task_id: Some("T-test".into()),
            branch: Some(branch.into()),
            worktree: Some(worktree.into()),
        }
    }

    // ── #1511: index-mutating plumbing folded into the mutating arm ──

    /// Unbound `read-tree` (the bug shape: `git read-tree -m <base> a b` from a
    /// canonical-rooted cwd) must now DENY instead of passing through to the
    /// shared index. Pre-#1511 it fell to the `_` arm → `unbound → Passthrough`.
    #[test]
    fn read_tree_unbound_denied_1511() {
        let action = classify(
            "read-tree",
            &[
                "read-tree".into(),
                "-m".into(),
                "base".into(),
                "a".into(),
                "b".into(),
            ],
            &Binding::default(), // unbound
            false,
            false,
            true,
        );
        match action {
            Action::Deny(reason) => assert!(
                reason.contains("unbound"),
                "unbound read-tree must deny: {reason}"
            ),
            other => {
                panic!("unbound read-tree MUST deny (was the index-pollution hole), got {other:?}")
            }
        }
    }

    /// A BOUND agent's `read-tree` routes to its private worktree (ChdirPass)
    /// regardless of cwd — so it never touches the canonical shared index. No
    /// canonical_cwd gate is needed; ChdirPass redirects away from cwd.
    #[test]
    fn read_tree_bound_routes_to_worktree_1511() {
        let action = classify(
            "read-tree",
            &["read-tree".into(), "-m".into(), "base".into()],
            &bound_binding("feat/x", "/tmp/.worktrees/dev"),
            false,
            false,
            true,
        );
        assert_eq!(
            action,
            Action::ChdirPass("/tmp/.worktrees/dev".into()),
            "bound read-tree must route to the private worktree, not deny"
        );
    }

    /// #2027 chain precondition: a BOUND agent's ref-naming `git branch <name>`
    /// lands in the read-only group → `ChdirPass(worktree)`. That ChdirPass is the
    /// exact input `apply_foreign_repo_passthrough` must flip to `Passthrough` in a
    /// foreign repo — pinning the full classify→apply chain that produced the
    /// success-lie (the redirect ran the create against the worktree, so the
    /// foreign repo silently got nothing yet the shim exited 0).
    #[test]
    fn bound_branch_create_classifies_to_chdirpass_2027() {
        let action = classify(
            "branch",
            &["branch".into(), "feat-x".into()],
            &bound_binding("feat/x", "/tmp/.worktrees/dev"),
            false,
            false,
            true,
        );
        assert_eq!(
            action,
            Action::ChdirPass("/tmp/.worktrees/dev".into()),
            "bound `git branch <name>` must classify to the read-only group's \
             ChdirPass — the #2027 redirect apply_foreign_repo_passthrough flips"
        );
    }

    /// `update-index` and `apply` join the same arm (clear index plumbing).
    #[test]
    fn update_index_and_apply_unbound_denied_1511() {
        for sub in ["update-index", "apply"] {
            let action = classify(sub, &[sub.into()], &Binding::default(), false, false, true);
            assert!(
                matches!(action, Action::Deny(_)),
                "unbound {sub} must deny, got {action:?}"
            );
        }
    }

    /// Precise-match guard: `merge-tree` is READ-ONLY (writes only to the object
    /// DB, never the index) — it must NOT be caught by the `read-tree`/`merge`
    /// tokens. It falls to the `_` arm → unbound Passthrough, so the daemon's
    /// `merge-tree --write-tree` conflict check (and agents using it) keep working.
    #[test]
    fn merge_tree_not_caught_by_1511() {
        let action = classify(
            "merge-tree",
            &[
                "merge-tree".into(),
                "--write-tree".into(),
                "a".into(),
                "b".into(),
            ],
            &Binding::default(),
            false,
            false,
            true,
        );
        assert_eq!(
            action,
            Action::Passthrough,
            "merge-tree is read-only and must NOT be denied/caught by #1511"
        );
    }

    // ── #1511 follow-up: flag-discriminated index/ref plumbing ──

    /// The MUTATING forms — `restore --staged`/`-S`, `update-ref` (always),
    /// `symbolic-ref` write (`<name> <ref>` or `-d`) — must DENY when unbound
    /// (closes the canonical-write hole they had via the `_` Passthrough arm).
    #[test]
    fn fu1511_mutating_plumbing_forms_unbound_denied() {
        let cases: &[&[&str]] = &[
            &["restore", "--staged", "file.rs"],
            &["restore", "-S", "file.rs"],
            &["restore", "--staged", "--worktree", "file.rs"], // both → has --staged
            &["update-ref", "refs/heads/main", "deadbeef"],
            &["update-ref", "-d", "refs/heads/tmp"],
            &["symbolic-ref", "HEAD", "refs/heads/feat"], // 2 non-flag args → write
            &["symbolic-ref", "-d", "HEAD"],              // delete → write
        ];
        for argv in cases {
            let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let action = classify(argv[0], &args, &Binding::default(), false, false, true);
            assert!(
                matches!(action, Action::Deny(ref reason) if reason.contains("unbound")),
                "unbound `{}` must deny, got {action:?}",
                argv.join(" ")
            );
        }
    }

    /// The READ / working-tree forms must NOT be over-denied (the operator's
    /// "don't block bare restore" + don't break read-only `symbolic-ref`):
    /// they fall through to `unbound → Passthrough` like any read-only command.
    #[test]
    fn fu1511_readonly_and_worktree_forms_not_overdenied() {
        let cases: &[&[&str]] = &[
            &["restore", "file.rs"],               // bare → working tree
            &["restore", "--worktree", "file.rs"], // explicit working tree, no --staged
            &["symbolic-ref", "HEAD"],             // 1 non-flag arg → read
            &["symbolic-ref", "--short", "HEAD"],  // read with a flag
        ];
        for argv in cases {
            let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let action = classify(argv[0], &args, &Binding::default(), false, false, true);
            assert_eq!(
                action,
                Action::Passthrough,
                "unbound read/working-tree form `{}` must NOT be denied",
                argv.join(" ")
            );
        }
    }

    /// A BOUND agent's mutating-plumbing forms route to its PRIVATE worktree
    /// (ChdirPass), never deny. (Shared-ref caveat noted in the arm: ChdirPass
    /// can't isolate ref writes, but bound agents are trusted — Policy A.)
    #[test]
    fn fu1511_bound_routes_to_worktree() {
        let wt = "/tmp/.worktrees/dev";
        let cases: &[&[&str]] = &[
            &["restore", "--staged", "file.rs"],
            &["update-ref", "refs/heads/main", "deadbeef"],
            &["symbolic-ref", "HEAD", "refs/heads/feat"],
        ];
        for argv in cases {
            let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let action = classify(
                argv[0],
                &args,
                &bound_binding("feat/x", wt),
                false,
                false,
                true,
            );
            assert_eq!(
                action,
                Action::ChdirPass(wt.into()),
                "bound `{}` must route to the worktree",
                argv.join(" ")
            );
        }
    }

    /// `git reset --hard` stays the agent's self-recovery tool: a bound agent
    /// must be able to run it (routes to its worktree), not be blocked.
    #[test]
    fn reset_hard_not_blocked_for_bound_agent_1511() {
        let action = classify(
            "reset",
            &["reset".into(), "--hard".into(), "origin/main".into()],
            &bound_binding("feat/x", "/tmp/.worktrees/dev"),
            false,
            false,
            true,
        );
        assert_eq!(
            action,
            Action::ChdirPass("/tmp/.worktrees/dev".into()),
            "reset --hard must remain available to a bound agent (self-recovery)"
        );
    }

    #[test]
    fn deny_hint_lists_all_three_bypass_forms() {
        let lines = format_deny_error("commit", "unbound", "dev");
        let joined = lines.join("\n");
        for var in [
            "AGEND_GIT_BYPASS=1",
            "AGEND_GIT_BYPASS_AGENT=",
            "AGEND_GIT_BYPASS_UNTIL=",
        ] {
            assert!(
                joined.contains(var),
                "deny hint must list {var}, got:\n{joined}"
            );
        }
        assert!(
            joined.contains("epoch") && joined.contains("Unix seconds"),
            "AGEND_GIT_BYPASS_UNTIL hint must clarify epoch wording (not ISO), got:\n{joined}"
        );
    }

    // ----- Sprint 57 Wave 2 Track D — gh post-merge exemption -----

    #[test]
    fn gh_post_merge_checkout_exempted_from_e45_fence() {
        // Happy path: agent is bound to a feat branch, gh just merged
        // it + deleted remote, now runs `git checkout main` to clean
        // up local state. parent=gh signal fires → SilentExempt.
        let binding = bound_binding("sprint57-track-x", "/tmp/.worktrees/dev");
        let action = classify(
            "checkout",
            &["checkout".into(), "main".into()],
            &binding,
            true, // parent_is_gh = true
            false,
            false, // is_agent_caller — operator default; the gh-exemption
                   // is independent of #852's agent-vs-operator gate
        );
        match action {
            Action::SilentExempt {
                target_branch,
                reason,
            } => {
                assert_eq!(target_branch, "main");
                assert!(
                    reason.contains("gh post-merge"),
                    "reason must label the exemption: {reason}"
                );
            }
            other => panic!("expected SilentExempt for gh post-merge cleanup, got {other:?}"),
        }
    }

    #[test]
    fn gh_post_merge_exemption_also_covers_master() {
        // master is part of the protected set per `is_protected_ref`;
        // legacy repos using `master` as default branch must also
        // trigger the exemption.
        let binding = bound_binding("sprint57-track-y", "/tmp/.worktrees/dev");
        let action = classify(
            "checkout",
            &["checkout".into(), "master".into()],
            &binding,
            true,
            false,
            false, // is_agent_caller — operator default
        );
        assert!(
            matches!(action, Action::SilentExempt { .. }),
            "master target must also be exempted, got {action:?}"
        );
    }

    #[test]
    fn interactive_checkout_to_main_still_blocked() {
        // Regression-proof of E4.5 normal protection: when parent is
        // NOT gh (interactive shell, script, IDE), the cross-branch
        // fence must still fire. Without this guarantee Track D
        // would silently weaken the rule.
        let binding = bound_binding("sprint57-track-z", "/tmp/.worktrees/dev");
        let action = classify(
            "checkout",
            &["checkout".into(), "main".into()],
            &binding,
            false, // parent_is_gh = false (interactive shell)
            false,
            false, // is_agent_caller — operator default
        );
        match action {
            Action::Deny(reason) => {
                assert!(
                    reason.contains("cross-branch"),
                    "interactive case must still trip the cross-branch fence: {reason}"
                );
                assert!(
                    reason.contains("'main'"),
                    "deny message must mention target branch: {reason}"
                );
            }
            other => panic!("interactive checkout to main MUST be denied, got {other:?}"),
        }
    }

    #[test]
    fn switch_subcommand_also_routes_through_gate() {
        // `git switch main` is the modern equivalent of `git checkout
        // main`; the gate must apply to both subcommands so the
        // exemption + the normal block both work via either spelling.
        let binding = bound_binding("sprint57-track-q", "/tmp/.worktrees/dev");
        // gh path → exempt
        let action_gh = classify(
            "switch",
            &["switch".into(), "main".into()],
            &binding,
            true,
            false,
            false, // is_agent_caller — operator default
        );
        assert!(matches!(action_gh, Action::SilentExempt { .. }));
        // interactive path → deny
        let action_interactive = classify(
            "switch",
            &["switch".into(), "main".into()],
            &binding,
            false,
            false,
            false, // is_agent_caller — operator default
        );
        match action_interactive {
            Action::Deny(_) => {}
            other => panic!("interactive `switch main` must deny, got {other:?}"),
        }
    }

    #[test]
    fn cross_branch_to_non_protected_target_never_exempted() {
        // Heuristic correctness: even with parent_is_gh=true, a
        // checkout to a NON-protected branch must still be denied.
        // The exemption is narrow by design — protected refs only.
        // gh in normal operation never checks out feature branches
        // post-merge, so this case represents a heuristic false-
        // positive boundary we explicitly guard.
        let binding = bound_binding("sprint57-track-r", "/tmp/.worktrees/dev");
        let action = classify(
            "checkout",
            &["checkout".into(), "feat-other".into()],
            &binding,
            true, // parent_is_gh — but target isn't protected.
            false,
            false, // is_agent_caller — operator default
        );
        match action {
            Action::Deny(reason) => {
                assert!(
                    reason.contains("cross-branch"),
                    "non-protected cross-branch must deny even with parent=gh: {reason}"
                );
            }
            other => panic!(
                "non-protected cross-branch with parent=gh must deny (NOT exempt), got {other:?}"
            ),
        }
    }

    #[test]
    fn gh_invocation_detection_robust_against_simulated_external_invocation() {
        // The detection helper must reject `gh`-lookalike basenames
        // that aren't the canonical CLI binary. This pins the
        // basename matcher: only the literal `gh` (or `gh.exe`)
        // qualifies — common false-positives like `github`,
        // `gh-cli-helper`, or empty strings must NOT.
        assert!(process_basename_is_gh("gh"));
        assert!(process_basename_is_gh("/usr/local/bin/gh"));
        assert!(process_basename_is_gh("/opt/homebrew/bin/gh"));
        assert!(process_basename_is_gh(
            "C:\\Program Files\\GitHub CLI\\gh.exe"
        ));
        assert!(process_basename_is_gh("gh.exe"));

        // Negative cases — must NOT fire the heuristic.
        assert!(!process_basename_is_gh(""));
        assert!(!process_basename_is_gh("github"));
        assert!(!process_basename_is_gh("/usr/bin/github"));
        assert!(!process_basename_is_gh("gh-cli-helper"));
        assert!(!process_basename_is_gh("not-gh"));
        assert!(!process_basename_is_gh("/path/to/gh.sh")); // shell wrapper
        assert!(!process_basename_is_gh("agh")); // adjacent letters
    }

    #[test]
    fn audit_event_logged_when_exemption_fires() {
        // Round-trip: classify produces SilentExempt → main() writes a
        // structured `post_merge_cleanup_exempt` event. We can't run
        // main() in a unit test (it calls std::process::exit), but
        // we can call the underlying `write_git_event_typed` writer
        // directly and assert the on-disk shape, which is what main
        // would emit.
        let home = std::env::temp_dir().join(format!(
            "agend-git-d-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&home).ok();

        write_git_event_typed(
            home.to_str().unwrap(),
            "dev",
            "checkout",
            "post_merge_cleanup_exempt",
            Some("main"),
            Some("gh post-merge cleanup checkout — test fixture"),
        );

        let events_path = home.join("fleet_events.jsonl");
        assert!(events_path.exists(), "audit event file must be created");

        let content = std::fs::read_to_string(&events_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(v["kind"], "git_event");
        assert_eq!(v["event"], "post_merge_cleanup_exempt");
        assert_eq!(v["agent"], "dev");
        assert_eq!(v["subcommand"], "checkout");
        assert_eq!(v["target_branch"], "main");
        assert!(
            v["reason"]
                .as_str()
                .map(|s| s.contains("post-merge"))
                .unwrap_or(false),
            "reason must record the exemption rationale"
        );
        assert!(
            v["timestamp"].as_str().is_some(),
            "timestamp must be RFC3339 string"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deny_event_still_uses_typed_writer() {
        // Defensive bonus pin: the legacy `event="deny"` shape must
        // continue to work via the new `write_git_event_typed`
        // function. Previously the wrapper had a separate
        // `write_git_event` for deny-only; consolidating to a typed
        // writer must not change the on-disk shape for the deny
        // event-type so downstream parsers keep working.
        let home = std::env::temp_dir().join(format!(
            "agend-git-d-deny-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&home).ok();

        write_git_event_typed(
            home.to_str().unwrap(),
            "dev",
            "checkout",
            "deny",
            None,
            Some("cross-branch — assigned to 'feat-x', cannot switch to 'main'"),
        );

        let events_path = home.join("fleet_events.jsonl");
        let content = std::fs::read_to_string(&events_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(v["event"], "deny");
        assert_eq!(v["target_branch"], serde_json::Value::Null);
        assert!(
            v["reason"]
                .as_str()
                .map(|s| s.contains("cross-branch"))
                .unwrap_or(false),
            "deny reason must round-trip"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // ----- #778 Option 3 — canonical-worktree leniency for unbound -----

    #[test]
    fn p778_unbound_canonical_worktree_checkout_branch_passes_through() {
        // Empirical regression-proof anchor for #778 Option 3:
        // commenting out the `if !target_branch.is_empty() && ...
        // canonical_cwd { Action::Passthrough }` block makes this
        // FAIL with Action::Deny.
        let action = classify(
            "checkout",
            &["checkout".into(), "feat/p778".into()],
            &Binding::default(), // unbound
            false,               // parent_is_gh = no
            true,                // canonical_cwd = yes
            false,               // is_agent_caller — operator default; the
                                 // #778 leniency must still fire for the
                                 // operator-driven validation-canary flow
        );
        assert!(
            matches!(action, Action::Passthrough),
            "unbound + canonical worktree + positional branch must Passthrough, got {action:?}"
        );
    }

    #[test]
    fn p778_unbound_canonical_switch_subcommand_also_passes() {
        // `git switch` is the modern equivalent and must benefit from
        // the same leniency — otherwise the rule is partial and the
        // validation-canary workflow stays broken on the recommended
        // `switch` path.
        let action = classify(
            "switch",
            &["switch".into(), "feat/p778".into()],
            &Binding::default(),
            false,
            true,
            false, // is_agent_caller — operator default
        );
        assert!(
            matches!(action, Action::Passthrough),
            "switch must also benefit from the leniency, got {action:?}"
        );
    }

    #[test]
    fn p778_unbound_non_canonical_worktree_still_denied() {
        // Negative: when cwd is not a canonical worktree (placeholder
        // repo with no origin, or no worktree at all), the original
        // unbound deny must still fire — this is the security
        // guarantee that keeps the leniency narrow.
        let action = classify(
            "checkout",
            &["checkout".into(), "feat/p778".into()],
            &Binding::default(),
            false,
            false, // canonical_cwd = no
            false, // is_agent_caller — operator default
        );
        match action {
            Action::Deny(reason) => assert!(
                reason.contains("unbound"),
                "non-canonical cwd must keep the unbound deny: {reason}"
            ),
            other => panic!("non-canonical unbound must deny, got {other:?}"),
        }
    }

    #[test]
    fn p778_unbound_canonical_flag_arg_still_denied() {
        // Heuristic safety: when the next arg is a flag (`-b
        // newbranch`, `-B foo`, `--orphan`) the leniency must NOT
        // fire — those create branches or detach in ways that aren't
        // "just navigation". Keep the deny for the unbound case so
        // we don't accidentally widen the surface.
        let action = classify(
            "checkout",
            &["checkout".into(), "-b".into(), "evil".into()],
            &Binding::default(),
            false,
            true,  // canonical_cwd = yes, but arg is a flag
            false, // is_agent_caller — operator default
        );
        match action {
            Action::Deny(reason) => assert!(
                reason.contains("unbound"),
                "flag arg in unbound canonical must deny: {reason}"
            ),
            other => panic!("flag arg leniency leak: {other:?}"),
        }
    }

    #[test]
    fn p778_unbound_canonical_no_branch_arg_still_denied() {
        // `git checkout` with no positional branch (just to inspect
        // status) shouldn't even hit the leniency block — keep the
        // existing unbound deny for the no-target case.
        let action = classify(
            "checkout",
            &["checkout".into()],
            &Binding::default(),
            false,
            true,
            false, // is_agent_caller — operator default
        );
        match action {
            Action::Deny(reason) => assert!(reason.contains("unbound"), "got {reason}"),
            other => panic!("no-arg unbound must deny, got {other:?}"),
        }
    }

    #[test]
    fn p778_bound_path_unchanged_when_canonical_cwd_true() {
        // Regression-proof of the bound path: canonical_cwd must NOT
        // alter behavior when the agent is bound. The existing
        // cross-branch check + ChdirPass dispatch are the source of
        // truth; the leniency only opens when bound=false.
        let binding = bound_binding("feat/p778", "/tmp/.worktrees/dev");
        let action = classify(
            "checkout",
            &["checkout".into(), "feat/p778".into()],
            &binding,
            false,
            true,  // canonical_cwd — should NOT route through leniency
            false, // is_agent_caller — operator default
        );
        match action {
            Action::ChdirPass(ref wt) => assert_eq!(wt, "/tmp/.worktrees/dev"),
            other => panic!("bound same-branch must ChdirPass, got {other:?}"),
        }
    }

    // ----- #852 PR-B — agent caller + canonical cwd → Deny -----
    //
    // The pre-#852 `!bound + canonical_cwd + positional non-flag arg →
    // Passthrough` leniency was designed for the operator-typed
    // validation-canary flow (`repo action=checkout` provisions a
    // worktree in detached-HEAD; operator's natural `git switch
    // <branch>` follow-up needed to pass). It accidentally also
    // covered agent callers whose binding lookup failed for the
    // current cwd — reviewers especially, who inspect PRs via
    // canonical-rooted worktrees and end up creating `pr*_head` /
    // `tmp*` / `review/*` refs that pollute the canonical's branch
    // list. PR-B gates the leniency on agent-vs-operator identity:
    // operators keep the leniency, agents are routed to the
    // `repo action=checkout bind=true` MCP tool (which gives them a
    // properly-bound worktree) or `gh pr diff/view` (read-only).

    /// #852 PR-B core: when caller is an agent (AGEND_INSTANCE_NAME
    /// set) AND cwd is a canonical-rooted worktree, the leniency must
    /// NOT fire — checkout is denied with an actionable hint pointing
    /// to the supported alternatives.
    #[test]
    fn shim_denies_agent_checkout_in_canonical() {
        let action = classify(
            "checkout",
            &["checkout".into(), "abc1234".into()], // SHA — reviewer's
            // "let me see this
            // PR's tree" workflow
            &Binding::default(), // unbound (binding lookup failed for
            // canonical cwd)
            false, // parent_is_gh = no
            true,  // canonical_cwd = yes
            true,  // is_agent_caller = yes
        );
        match action {
            Action::Deny(reason) => {
                assert!(
                    reason.contains("agent"),
                    "deny reason must explicitly call out the agent-caller \
                     identity so reviewers see WHY their workflow is rejected: \
                     {reason}"
                );
                assert!(
                    reason.contains("repo action=checkout") || reason.contains("gh pr diff"),
                    "deny reason must surface the supported alternative \
                     (repo action=checkout MCP or gh pr diff): {reason}"
                );
                assert!(
                    reason.contains("#852"),
                    "deny reason should reference the issue for operator \
                     traceability: {reason}"
                );
            }
            other => panic!(
                "agent caller in canonical worktree must Deny, not {other:?} \
                 — that's the reviewer-pollution bug fix"
            ),
        }
    }

    /// #852 PR-B operator preservation: when caller is NOT an agent
    /// (operator's interactive shell, no AGEND_INSTANCE_NAME), the
    /// existing #778 leniency must continue to fire — the validation-
    /// canary flow must not regress.
    #[test]
    fn shim_allows_operator_checkout_in_canonical() {
        let action = classify(
            "checkout",
            &["checkout".into(), "feat/canary".into()],
            &Binding::default(),
            false, // parent_is_gh = no
            true,  // canonical_cwd = yes
            false, // is_agent_caller = no (operator shell)
        );
        assert!(
            matches!(action, Action::Passthrough),
            "operator in canonical worktree must keep the #778 leniency, \
             got {action:?}"
        );
    }

    /// #852 PR-B narrowness check: when the agent IS a caller but cwd
    /// is NOT canonical (e.g. agent invoked git from a non-worktree
    /// path), the gate must NOT fire — only the canonical-pollution
    /// surface is targeted. Operator's `unbound + non-canonical →
    /// Deny` outcome is preserved (different code path).
    #[test]
    fn shim_agent_outside_canonical_unchanged() {
        let action = classify(
            "checkout",
            &["checkout".into(), "feat/x".into()],
            &Binding::default(),
            false, // parent_is_gh = no
            false, // canonical_cwd = NO — gate must NOT fire
            true,  // is_agent_caller = yes
        );
        // Falls through to the existing `unbound — no active task
        // assignment` Deny (different from the new #852 Deny). The
        // pre-existing safety net stays intact.
        match action {
            Action::Deny(reason) => {
                assert!(
                    reason.contains("unbound"),
                    "non-canonical agent path must keep the original \
                     unbound deny (not the new #852 agent-canonical deny): \
                     {reason}"
                );
                assert!(
                    !reason.contains("#852"),
                    "non-canonical agent path must NOT trigger the #852 \
                     gate (gate is narrow by design): {reason}"
                );
            }
            other => panic!(
                "non-canonical unbound must keep the pre-existing deny, \
                 got {other:?}"
            ),
        }
    }

    // ----- #852 residual PR-A — cwd_is_canonical_rooted detection -----
    //
    // Pre-#852-residual, the detection helper required `.git` to be a
    // FILE (worktree marker). This excluded canonical source repos
    // where `.git` is a DIRECTORY — reviewers `cd`'ing into source
    // sailed past the `is_agent_caller && canonical_cwd` deny because
    // canonical_cwd was always false for source. Operator's reflog
    // evidenced two such checkouts (21:46 + 22:24 today) AFTER
    // #858+#859 shipped. PR-A broadens the helper to cover BOTH
    // shapes.
    //
    // Tests use `std::env::set_current_dir` to position cwd inside
    // the synthetic fixtures. Mutex-serialized so parallel test
    // threads don't race the process-global cwd.

    fn with_cwd<R>(dir: &std::path::Path, f: impl FnOnce() -> R) -> R {
        use std::sync::{Mutex, OnceLock};
        static CWD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = CWD_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::current_dir().expect("snapshot cwd");
        std::env::set_current_dir(dir).expect("set test cwd");
        let result = f();
        std::env::set_current_dir(&prior).expect("restore cwd");
        result
    }

    fn make_source_repo_with_origin(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "agend-852-pr-a-source-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir source-base");
        let repo = base.join("repo");
        let git_dir = repo.join(".git");
        std::fs::create_dir_all(&git_dir).expect("mkdir .git");
        // Synthetic config: matches the canonical-detection criterion
        // (contains `[remote "origin"]`).
        std::fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\
             [remote \"origin\"]\n\turl = https://example.test/foo.git\n",
        )
        .expect("write .git/config");
        repo
    }

    fn make_source_repo_without_origin(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "agend-852-pr-a-no-origin-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir no-origin-base");
        let repo = base.join("repo");
        let git_dir = repo.join(".git");
        std::fs::create_dir_all(&git_dir).expect("mkdir .git");
        // Orphan workspace-placeholder shape: `.git` directory but
        // no `[remote "origin"]`. Daemon startup creates these
        // before fleet config resolves; they must NOT trigger the
        // canonical-rooted gate.
        std::fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .expect("write .git/config");
        repo
    }

    fn make_canonical_worktree(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        // Two-step: build a source repo with origin, then a synthetic
        // worktree pointing into it via the gitdir: marker. Mirrors
        // git's real worktree layout at <source>/.git/worktrees/<name>.
        let base =
            std::env::temp_dir().join(format!("agend-852-pr-a-wt-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir wt-base");
        let source = base.join("source");
        let source_git = source.join(".git");
        let worktrees_dir = source_git.join("worktrees").join("agent-1");
        std::fs::create_dir_all(&worktrees_dir).expect("mkdir worktree entry");
        std::fs::write(
            source_git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\
             [remote \"origin\"]\n\turl = https://example.test/foo.git\n",
        )
        .expect("write source .git/config");
        // Worktree dir with `.git` FILE pointing at the worktrees entry.
        let wt = base.join("worktree-cwd");
        std::fs::create_dir_all(&wt).expect("mkdir worktree dir");
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", worktrees_dir.display()),
        )
        .expect("write worktree .git pointer");
        (wt, base)
    }

    fn cleanup_base(repo: &std::path::Path) {
        if let Some(base) = repo.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }

    /// #2234 defect#2: the pure gate for the non-agent canonical-checkout log.
    /// `checkout`/`switch <branch>` (positional, non-flag target) → true; a
    /// flag-led / empty target or a non-checkout subcommand → false.
    #[test]
    fn is_positional_branch_checkout_gate() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // canonical-HEAD-touching nav shapes → log candidate.
        assert!(is_positional_branch_checkout(&s(&[
            "checkout",
            "origin/main"
        ])));
        assert!(is_positional_branch_checkout(&s(&["switch", "main"])));
        assert!(is_positional_branch_checkout(&s(&[
            "checkout",
            "feature/x",
            "--",
            "file"
        ])));
        // not a branch nav / not checkout → not a candidate.
        assert!(!is_positional_branch_checkout(&s(&["checkout"])));
        assert!(!is_positional_branch_checkout(&s(&[
            "checkout", "--detach"
        ])));
        assert!(!is_positional_branch_checkout(&s(&[
            "checkout", "-b", "tmp"
        ])));
        assert!(!is_positional_branch_checkout(&s(&["status"])));
        assert!(!is_positional_branch_checkout(&s(&["commit", "main"])));
    }

    /// #2234 fix B: the pure deny decision. Deny only when agent + !escape +
    /// canonical + provisioning op (worktree / positional checkout|switch <ref>).
    #[test]
    fn deny_agent_canonical_bypass_decision_matrix_2234() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // DENY: agent, not escaped, canonical, provisioning ops.
        assert!(deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["worktree", "add", "x", "origin/main"])
        ));
        assert!(deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["checkout", "origin/main"])
        ));
        assert!(deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["switch", "main"])
        ));

        // ALLOW — non-`add` worktree subcommands are NOT stray/detach vectors
        // (r4 #2316 over-block fix): `list` is read-only; remove/prune/move
        // don't detach or stray. Bare `worktree` (no subcommand) → not add.
        assert!(!deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["worktree", "list"])
        ));
        assert!(!deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["worktree", "remove", "x"])
        ));
        assert!(!deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["worktree", "prune"])
        ));
        assert!(!deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["worktree"])
        ));

        // ALLOW — carve-outs / non-provisioning:
        // non-agent caller (daemon-correlated / operator shell).
        assert!(!deny_agent_canonical_bypass(
            false,
            false,
            true,
            &s(&["worktree", "add", "x"])
        ));
        // explicit escape env set.
        assert!(!deny_agent_canonical_bypass(
            true,
            true,
            true,
            &s(&["worktree", "add", "x"])
        ));
        // cwd not canonical-rooted (e.g. /tmp nextest fixture).
        assert!(!deny_agent_canonical_bypass(
            true,
            false,
            false,
            &s(&["checkout", "origin/main"])
        ));
        // non-provisioning ops agents legitimately bypass in their own worktree.
        assert!(!deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["push", "origin", "feat/x"])
        ));
        assert!(!deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["commit", "-m", "wip"])
        ));
        assert!(!deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["add", "-A"])
        ));
        assert!(!deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["reset", "--hard", "HEAD"])
        ));
        // checkout flag/pathspec forms are NOT positional-branch → not denied.
        assert!(!deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["checkout", "-b", "tmp"])
        ));
        assert!(!deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["checkout", "--", "file.rs"])
        ));
        // read-only.
        assert!(!deny_agent_canonical_bypass(
            true,
            false,
            true,
            &s(&["status"])
        ));
    }

    /// #852 residual core: canonical source repo (`.git` directory +
    /// `[remote "origin"]`) must classify as canonical-rooted. This
    /// is the path that pre-#852-residual missed entirely.
    #[test]
    fn cwd_is_canonical_rooted_returns_true_for_source_repo_with_origin() {
        let repo = make_source_repo_with_origin("with-origin");
        let result = with_cwd(&repo, cwd_is_canonical_rooted);
        cleanup_base(&repo);
        assert!(
            result,
            "canonical source repo with `[remote \"origin\"]` must classify \
             as canonical-rooted (this is the #852 residual fix)"
        );
    }

    /// Defense against orphan workspace-placeholder repos: `.git`
    /// directory present but no remote configured. Daemon startup
    /// creates these before fleet config resolves; the canonical-
    /// rooted gate must NOT fire on them.
    #[test]
    fn cwd_is_canonical_rooted_returns_false_for_source_repo_without_origin() {
        let repo = make_source_repo_without_origin("no-origin");
        let result = with_cwd(&repo, cwd_is_canonical_rooted);
        cleanup_base(&repo);
        assert!(
            !result,
            "orphan workspace-placeholder (`.git` directory but no \
             `[remote \"origin\"]`) must NOT classify as canonical-rooted"
        );
    }

    /// Preserves the #858 contract: canonical-rooted worktree
    /// (`.git` FILE with `gitdir:` pointer to source carrying origin)
    /// still classifies. This is the pre-PR-A path; the broadening
    /// must NOT regress it.
    #[test]
    fn cwd_is_canonical_rooted_returns_true_for_canonical_worktree() {
        let (wt, _base) = make_canonical_worktree("worktree");
        let result = with_cwd(&wt, cwd_is_canonical_rooted);
        cleanup_base(&wt);
        assert!(
            result,
            "canonical worktree (`.git` FILE + gitdir: pointer to source \
             with origin) must still classify (pre-#852-residual contract)"
        );
    }

    // ── #883 pre-push cleanup tests ───────────────────────────────

    /// Build a synthetic repo with a real `origin` remote pointing at
    /// a sibling bare repo, then create N empty `init` heartbeat
    /// commits on `feat/test` followed by one real commit. Returns
    /// `(worktree_path, real_commit_sha)`. The fixture mirrors the
    /// operator's PR #882 case (multiple init heartbeats before the
    /// real commit) — exactly the scenario the shim cleanup must
    /// handle.
    fn setup_branch_with_init_pile(init_count: usize) -> (std::path::PathBuf, String) {
        let id = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let base = std::env::temp_dir().join(format!("agend-883-fixture-{id}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let origin_bare = base.join("origin.git");
        let worktree = base.join("worktree");
        let git_env = [
            ("AGEND_GIT_BYPASS", "1"),
            ("GIT_AUTHOR_NAME", "test"),
            ("GIT_AUTHOR_EMAIL", "test@test"),
            ("GIT_COMMITTER_NAME", "test"),
            ("GIT_COMMITTER_EMAIL", "test@test"),
        ];
        let git_run = |args: &[&str], dir: &std::path::Path| -> std::process::Output {
            let mut cmd = Command::new("git");
            cmd.args(args).current_dir(dir);
            for (k, v) in git_env.iter() {
                cmd.env(k, v);
            }
            cmd.output().expect("git spawn")
        };

        // Create a bare origin repo with one commit on main.
        assert!(git_run(
            &[
                "init",
                "--bare",
                "-b",
                "main",
                origin_bare.to_str().unwrap()
            ],
            &base
        )
        .status
        .success());
        // Create the worktree by cloning the bare repo.
        assert!(git_run(
            &[
                "clone",
                origin_bare.to_str().unwrap(),
                worktree.to_str().unwrap()
            ],
            &base
        )
        .status
        .success());
        // #883 r1: configure user identity + gpgsign in the LOCAL repo
        // config so the production cleanup's `git rebase` (which spawns
        // its own Command without inheriting test env vars) has the
        // info it needs to materialize the cherry-picked real commit.
        // macOS GH-Actions runners auto-derive user.name from
        // /etc/passwd, but Ubuntu / Windows runners do not — that's
        // why CI failed at 7fd4628. Local config takes precedence over
        // global (which is /dev/null in `GIT_CONFIG_GLOBAL=/dev/null`
        // pre-push baseline) AND over /etc/passwd derivation.
        assert!(git_run(&["config", "user.name", "test"], &worktree)
            .status
            .success());
        assert!(
            git_run(&["config", "user.email", "test@test.local"], &worktree)
                .status
                .success()
        );
        // Belt-and-suspenders: disable commit signing in case the CI
        // runner has `commit.gpgsign=true` baked in somewhere.
        assert!(git_run(&["config", "commit.gpgsign", "false"], &worktree)
            .status
            .success());
        // Seed origin/main with an initial real commit so origin/main..HEAD has a valid base.
        std::fs::write(worktree.join("README.md"), "initial\n").unwrap();
        assert!(git_run(&["add", "README.md"], &worktree).status.success());
        assert!(git_run(&["commit", "-m", "initial real"], &worktree)
            .status
            .success());
        assert!(git_run(&["push", "origin", "main"], &worktree)
            .status
            .success());

        // Create the feature branch and pile N empty init heartbeats.
        assert!(git_run(&["checkout", "-b", "feat/test"], &worktree)
            .status
            .success());
        for _ in 0..init_count {
            assert!(
                git_run(&["commit", "--allow-empty", "-m", "init"], &worktree)
                    .status
                    .success()
            );
        }
        // Add a real commit on top — the operator's PR #882 scenario.
        std::fs::write(worktree.join("feature.txt"), "real work\n").unwrap();
        assert!(git_run(&["add", "feature.txt"], &worktree).status.success());
        assert!(git_run(&["commit", "-m", "feat: real work"], &worktree)
            .status
            .success());
        let real_sha = String::from_utf8(git_run(&["rev-parse", "HEAD"], &worktree).stdout)
            .unwrap()
            .trim()
            .to_string();

        (worktree, real_sha)
    }

    /// Count commits between `<base>..HEAD` in a worktree. Used to
    /// assert how many commits survive the cleanup.
    fn count_commits_above_base(worktree: &std::path::Path, base: &str) -> usize {
        let output = Command::new("git")
            .args(["log", &format!("{base}..HEAD"), "--format=%H"])
            .current_dir(worktree)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git log spawn");
        if !output.status.success() {
            panic!(
                "git log {base}..HEAD failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
    }

    /// Read the rev pointed at by a ref / HEAD.
    fn rev_parse(worktree: &std::path::Path, refname: &str) -> String {
        let output = Command::new("git")
            .args(["rev-parse", refname])
            .current_dir(worktree)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("rev-parse spawn");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// #883 RED→GREEN: the shim's pre-push cleanup must drop the
    /// init pile before push so origin only sees the real commit.
    /// Pre-fix the cleanup function doesn't exist (compile-fail RED).
    /// Post-fix the mixed-history rebase path runs and the
    /// `origin/main..HEAD` count drops from N+1 to 1.
    ///
    /// This test calls `cleanup_init_pile_pre_push` directly rather
    /// than running the full `git push` (no need to wire a real
    /// remote round-trip; the cleanup operates on local refs).
    #[test]
    fn shim_push_cleans_init_pile_before_push() {
        let (worktree, real_sha) = setup_branch_with_init_pile(3);
        // Pre-cleanup: 3 inits + 1 real = 4 commits above origin/main.
        assert_eq!(
            count_commits_above_base(&worktree, "origin/main"),
            4,
            "fixture must build 3 inits + 1 real commit above origin/main"
        );

        // Run the production cleanup function directly. This is what
        // the shim's `Action::CleanupAndChdirPushPass` arm calls
        // before `exec_real_git`.
        cleanup_init_pile_pre_push(worktree.to_str().unwrap());

        // Post-cleanup: only the real commit should remain above
        // origin/main; the 3 inits must be dropped.
        assert_eq!(
            count_commits_above_base(&worktree, "origin/main"),
            1,
            "post-#883: shim cleanup must drop the 3 init heartbeats"
        );

        // The real commit's TREE must be preserved — rebase keeps
        // identical content even if the SHA changes (rebase rewrites
        // parent pointers). Assert by checking the file content.
        let feature = std::fs::read_to_string(worktree.join("feature.txt")).unwrap();
        assert_eq!(
            feature.trim(),
            "real work",
            "real commit's tree must survive"
        );

        // The real commit's SHA may or may not change depending on
        // whether interactive rebase rewrites parents. Both are
        // valid; if the SHA stayed identical that's an even stronger
        // signal (cherry-pick optimization), but either way the
        // origin/main..HEAD count must be exactly 1.
        let _ = real_sha; // intentionally not asserted equal

        // Cleanup tempdir (best-effort).
        let _ = std::fs::remove_dir_all(worktree.parent().unwrap());
    }

    /// Negative regression: no-op when origin/main..HEAD already has
    /// only real commits. The cleanup must not mutate a clean branch.
    #[test]
    fn shim_push_cleanup_noop_when_no_inits() {
        let id = format!(
            "{}-noop-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let base = std::env::temp_dir().join(format!("agend-883-noop-{id}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let origin_bare = base.join("origin.git");
        let worktree = base.join("worktree");
        let env = [
            ("AGEND_GIT_BYPASS", "1"),
            ("GIT_AUTHOR_NAME", "t"),
            ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"),
            ("GIT_COMMITTER_EMAIL", "t@t"),
        ];
        let run = |args: &[&str], dir: &std::path::Path| {
            let mut c = Command::new("git");
            c.args(args).current_dir(dir);
            for (k, v) in env.iter() {
                c.env(k, v);
            }
            c.output().expect("spawn")
        };
        assert!(run(
            &[
                "init",
                "--bare",
                "-b",
                "main",
                origin_bare.to_str().unwrap()
            ],
            &base
        )
        .status
        .success());
        assert!(run(
            &[
                "clone",
                origin_bare.to_str().unwrap(),
                worktree.to_str().unwrap()
            ],
            &base
        )
        .status
        .success());
        // #883 r1: configure local repo user identity + gpgsign so the
        // production cleanup's `git rebase` doesn't fail on CI runners
        // (Ubuntu / Windows) lacking global gitconfig. See
        // `setup_branch_with_init_pile` for the full rationale.
        assert!(run(&["config", "user.name", "test"], &worktree)
            .status
            .success());
        assert!(run(&["config", "user.email", "test@test.local"], &worktree)
            .status
            .success());
        assert!(run(&["config", "commit.gpgsign", "false"], &worktree)
            .status
            .success());
        std::fs::write(worktree.join("R.md"), "x\n").unwrap();
        assert!(run(&["add", "R.md"], &worktree).status.success());
        assert!(run(&["commit", "-m", "initial"], &worktree)
            .status
            .success());
        assert!(run(&["push", "origin", "main"], &worktree).status.success());
        assert!(run(&["checkout", "-b", "feat/clean"], &worktree)
            .status
            .success());
        std::fs::write(worktree.join("real.txt"), "work\n").unwrap();
        assert!(run(&["add", "real.txt"], &worktree).status.success());
        assert!(run(&["commit", "-m", "feat: real"], &worktree)
            .status
            .success());
        let head_before = rev_parse(&worktree, "HEAD");

        cleanup_init_pile_pre_push(worktree.to_str().unwrap());

        let head_after = rev_parse(&worktree, "HEAD");
        assert_eq!(head_before, head_after, "no-op on a clean branch");
        let _ = std::fs::remove_dir_all(&base);
    }

    // ----- #1225: conflict resolution guidance -----

    #[test]
    fn is_conflict_capable_covers_rebase_merge_pull_cherry_pick() {
        for cmd in ["rebase", "merge", "pull", "cherry-pick"] {
            assert!(is_conflict_capable(cmd), "{cmd} should be conflict-capable");
        }
        for cmd in ["commit", "add", "push", "status", "reset"] {
            assert!(
                !is_conflict_capable(cmd),
                "{cmd} should NOT be conflict-capable"
            );
        }
    }

    #[test]
    fn has_unmerged_files_false_on_clean_repo() {
        let repo = std::env::temp_dir().join(format!(
            "agend-conflict-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&repo).unwrap();
        // #1748: drive fixture git through the REAL git binary, not the bare
        // `git` PATH entry which resolves to this agend-git shim — whose #1463
        // ChdirPass strips the `-C <tempdir>` and redirects the op onto the
        // caller's bound worktree, corrupting it. `resolve_real_git()` is the
        // same resolver the shim uses to exec real git (excludes $AGEND_HOME/bin).
        let git_bin = resolve_real_git();
        assert!(Command::new(&git_bin)
            .arg("-C")
            .arg(&repo)
            .args(["init", "-b", "main"])
            .output()
            .unwrap()
            .status
            .success());
        Command::new(&git_bin)
            .arg("-C")
            .arg(&repo)
            .args(["config", "user.name", "test"])
            .output()
            .unwrap();
        Command::new(&git_bin)
            .arg("-C")
            .arg(&repo)
            .args(["config", "user.email", "test@test.com"])
            .output()
            .unwrap();
        assert!(Command::new(&git_bin)
            .arg("-C")
            .arg(&repo)
            .args(["commit", "--allow-empty", "-m", "init"])
            .output()
            .unwrap()
            .status
            .success());
        assert!(!has_unmerged_files(&git_bin, repo.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn has_unmerged_files_true_on_conflict() {
        let repo = std::env::temp_dir().join(format!(
            "agend-conflict-pos-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&repo).unwrap();
        // #1748: real git, not the shim — see has_unmerged_files_false_on_clean_repo.
        let git_bin = resolve_real_git();
        let git = |args: &[&str]| {
            Command::new(&git_bin)
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "test@test.com"]);
        git(&["config", "user.name", "test"]);
        std::fs::write(repo.join("f.txt"), "base\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-m", "base"]);
        git(&["checkout", "-b", "side"]);
        std::fs::write(repo.join("f.txt"), "side\n").unwrap();
        git(&["commit", "-am", "side"]);
        git(&["checkout", "main"]);
        std::fs::write(repo.join("f.txt"), "main\n").unwrap();
        git(&["commit", "-am", "main"]);
        let merge = git(&["merge", "side", "--no-edit"]);
        assert!(!merge.status.success(), "merge should fail with conflict");
        assert!(has_unmerged_files(&git_bin, repo.to_str().unwrap()));
        git(&["merge", "--abort"]);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn conflict_guidance_contains_resolution_steps() {
        let guidance = format_conflict_guidance();
        assert!(guidance.contains("resolve"), "should mention resolving");
        assert!(guidance.contains("git add"), "should mention git add");
        assert!(guidance.contains("--continue"), "should mention --continue");
        assert!(
            guidance.contains("Do NOT abandon"),
            "should discourage abandoning"
        );
    }

    // ── #1463: foreign-repo passthrough (A) + target-override strip (B) ──

    use std::sync::atomic::{AtomicU32, Ordering};
    static TMP_CTR_1463: AtomicU32 = AtomicU32::new(0);

    fn uniq_tmp_1463(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agend-1463-{}-{}-{}",
            tag,
            std::process::id(),
            TMP_CTR_1463.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Fabricate a canonical-style source repo (`<root>/.git/` DIR) — hermetic,
    /// no `git` invocation (so the shim is never re-entered by the test).
    fn fake_source_repo(root: &Path) {
        let git = root.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("config"), "[core]\n").unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    /// Fabricate a linked worktree of `source_root`: a `worktrees/<name>/` entry
    /// carrying a `commondir` file, plus the worktree dir's `.git` FILE pointer.
    fn fake_worktree(source_root: &Path, wt_root: &Path, name: &str) {
        let entry = source_root.join(".git").join("worktrees").join(name);
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("commondir"), "../..\n").unwrap();
        std::fs::create_dir_all(wt_root).unwrap();
        std::fs::write(
            wt_root.join(".git"),
            format!("gitdir: {}\n", entry.display()),
        )
        .unwrap();
    }

    // (a) an independent `git init` temp repo is FOREIGN → passthrough.
    #[test]
    fn foreign_scratch_repo_is_foreign_1463() {
        let src = uniq_tmp_1463("src");
        fake_source_repo(&src);
        let wt = uniq_tmp_1463("wt");
        fake_worktree(&src, &wt, "w1");
        let scratch = uniq_tmp_1463("scratch");
        fake_source_repo(&scratch); // its own, separate object store
        assert!(
            paths_are_foreign(&scratch, &wt),
            "an independent scratch repo has a separate commondir → foreign"
        );
    }

    // (b) the canonical SOURCE repo shares the worktree's commondir → NOT foreign.
    #[test]
    fn canonical_source_not_foreign_1463() {
        let src = uniq_tmp_1463("src");
        fake_source_repo(&src);
        let wt = uniq_tmp_1463("wt");
        fake_worktree(&src, &wt, "w1");
        assert!(
            !paths_are_foreign(&src, &wt),
            "canonical source shares the worktree's commondir → must ChdirPass"
        );
    }

    // (c) a SIBLING worktree (`.git` FILE) shares canonical's commondir → NOT foreign.
    #[test]
    fn sibling_worktree_not_foreign_1463() {
        let src = uniq_tmp_1463("src");
        fake_source_repo(&src);
        let mine = uniq_tmp_1463("mine");
        fake_worktree(&src, &mine, "mine");
        let sib = uniq_tmp_1463("sib");
        fake_worktree(&src, &sib, "sibling");
        assert!(
            !paths_are_foreign(&sib, &mine),
            "a sibling worktree shares canonical's commondir → must ChdirPass (no sibling-write)"
        );
    }

    // (e) craft: a `.git` FILE pointing `gitdir: <canonical>/.git` resolves to
    // canonical's commondir → NOT foreign (no canonical-write bypass).
    #[test]
    fn craft_gitfile_pointing_canonical_not_foreign_1463() {
        let src = uniq_tmp_1463("src");
        fake_source_repo(&src);
        let wt = uniq_tmp_1463("wt");
        fake_worktree(&src, &wt, "w1");
        let evil = uniq_tmp_1463("evil");
        std::fs::write(
            evil.join(".git"),
            format!("gitdir: {}\n", src.join(".git").display()),
        )
        .unwrap();
        assert!(
            !paths_are_foreign(&evil, &wt),
            "craft pointing at canonical's .git resolves to canonical commondir → NOT foreign"
        );
    }

    // (e) a `.git` SYMLINK → fail-closed (NOT foreign → ChdirPass).
    #[cfg(unix)]
    #[test]
    fn symlink_gitfile_fails_closed_1463() {
        let src = uniq_tmp_1463("src");
        fake_source_repo(&src);
        let wt = uniq_tmp_1463("wt");
        fake_worktree(&src, &wt, "w1");
        let evil = uniq_tmp_1463("evilsym");
        std::os::unix::fs::symlink(src.join(".git"), evil.join(".git")).unwrap();
        assert!(
            !paths_are_foreign(&evil, &wt),
            "a `.git` symlink is an irregular shape → fail-closed → NOT foreign"
        );
    }

    // parse-fail / no-repo cwd → fail-closed (NOT foreign).
    #[test]
    fn unresolvable_cwd_fails_closed_1463() {
        let src = uniq_tmp_1463("src");
        fake_source_repo(&src);
        let wt = uniq_tmp_1463("wt");
        fake_worktree(&src, &wt, "w1");
        let nonrepo = uniq_tmp_1463("nonrepo"); // no `.git` anywhere up the tree
        let garbage = uniq_tmp_1463("garbage");
        std::fs::write(garbage.join(".git"), "not a gitdir pointer\n").unwrap();
        assert!(
            !paths_are_foreign(&nonrepo, &wt),
            "no resolvable repo → fail-closed NOT foreign"
        );
        assert!(
            !paths_are_foreign(&garbage, &wt),
            "garbage `.git` file (no gitdir:) → fail-closed NOT foreign"
        );
    }

    // (A) the ChdirPass→Passthrough conversion matrix.
    #[test]
    fn foreign_passthrough_action_matrix_1463() {
        use Action::*;
        let a = |toks: &[&str]| toks.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // local mutating + foreign → Passthrough
        assert_eq!(
            apply_foreign_repo_passthrough(ChdirPass("wt".into()), "commit", &a(&["commit"]), true),
            Passthrough
        );
        assert_eq!(
            apply_foreign_repo_passthrough(ChdirPass("wt".into()), "add", &a(&["add"]), true),
            Passthrough
        );
        // local mutating + NOT foreign → unchanged ChdirPass
        assert_eq!(
            apply_foreign_repo_passthrough(
                ChdirPass("wt".into()),
                "commit",
                &a(&["commit"]),
                false
            ),
            ChdirPass("wt".into())
        );
        // push / checkout are NOT local-mutating → stay ChdirPass even if foreign
        assert_eq!(
            apply_foreign_repo_passthrough(ChdirPass("wt".into()), "push", &a(&["push"]), true),
            ChdirPass("wt".into())
        );
        assert_eq!(
            apply_foreign_repo_passthrough(
                ChdirPass("wt".into()),
                "checkout",
                &a(&["checkout"]),
                true
            ),
            ChdirPass("wt".into())
        );
        // non-ChdirPass inputs are returned verbatim
        assert_eq!(
            apply_foreign_repo_passthrough(Deny("x".into()), "commit", &a(&["commit"]), true),
            Deny("x".into())
        );
        assert_eq!(
            apply_foreign_repo_passthrough(Passthrough, "commit", &a(&["commit"]), true),
            Passthrough
        );
    }

    // #2027 (§3.9): a ref-naming `branch`/`tag` in a FOREIGN repo must pass through
    // (run against THAT repo), never be ChdirPass'd into the worktree — the
    // worktree-redirect is the success-lie (silent no-op + exit 0 for a create; a
    // fake `already exists` for a name the worktree already holds). Mirrors the
    // issue's deterministic 3-shape repro matrix.
    #[test]
    fn branch_tag_names_ref_classifier_2027() {
        let a = |toks: &[&str]| toks.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // create: positional ref name → names a ref
        assert!(branch_tag_names_ref("branch", &a(&["branch", "feat-x"])));
        assert!(branch_tag_names_ref("tag", &a(&["tag", "v1.0"])));
        // delete / move / copy flags → name a ref
        assert!(branch_tag_names_ref(
            "branch",
            &a(&["branch", "-d", "feat-x"])
        ));
        assert!(branch_tag_names_ref(
            "branch",
            &a(&["branch", "-m", "old", "new"])
        ));
        assert!(branch_tag_names_ref("tag", &a(&["tag", "-d", "v1.0"])));
        // #2030 (codex): CURRENT-branch mutators with NO positional token — they
        // write `branch.<cur>.merge` / the description, so a foreign-repo redirect
        // would still lie. All four forms must name a ref.
        assert!(branch_tag_names_ref(
            "branch",
            &a(&["branch", "--set-upstream-to=origin/main"])
        ));
        assert!(branch_tag_names_ref(
            "branch",
            &a(&["branch", "--set-upstream-to", "origin/main"])
        ));
        assert!(branch_tag_names_ref(
            "branch",
            &a(&["branch", "-u", "origin/main"])
        ));
        // codex r2: the GLUED short form `-u<up>` (value attached, one token) —
        // missed by both the exact `-u` match and the positional check.
        assert!(branch_tag_names_ref(
            "branch",
            &a(&["branch", "-uorigin/main"])
        ));
        assert!(branch_tag_names_ref(
            "branch",
            &a(&["branch", "--unset-upstream"])
        ));
        assert!(branch_tag_names_ref(
            "branch",
            &a(&["branch", "--edit-description"])
        ));
        // bare LIST form (no positional, list/inspect flags only) → does NOT name a ref
        assert!(!branch_tag_names_ref("branch", &a(&["branch"])));
        assert!(!branch_tag_names_ref("branch", &a(&["branch", "-a"])));
        assert!(!branch_tag_names_ref("branch", &a(&["branch", "-v", "-v"])));
        assert!(!branch_tag_names_ref("tag", &a(&["tag", "-l"])));
        // non-branch/tag subcommands are never ref-naming here (handled elsewhere)
        assert!(!branch_tag_names_ref("commit", &a(&["commit", "-m", "x"])));
        assert!(!branch_tag_names_ref("status", &a(&["status"])));
    }

    // #2027 (§3.9): the end-to-end conversion — bound-agent `git branch <name>` in
    // a foreign repo flips ChdirPass→Passthrough (no lie); fleet (non-foreign) and
    // the bare LIST form stay ChdirPass byte-identical.
    #[test]
    fn foreign_passthrough_branch_tag_matrix_2027() {
        use Action::*;
        let a = |toks: &[&str]| toks.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Shape 1 — never-seen create name, foreign → Passthrough (was: silent no-op exit 0)
        assert_eq!(
            apply_foreign_repo_passthrough(
                ChdirPass("wt".into()),
                "branch",
                &a(&["branch", "zz-new"]),
                true
            ),
            Passthrough
        );
        // Shape 2 — name the worktree already holds, foreign → Passthrough
        // (was: fake `already exists` exit 128 from the worktree)
        assert_eq!(
            apply_foreign_repo_passthrough(
                ChdirPass("wt".into()),
                "branch",
                &a(&["branch", "feat-x"]),
                true
            ),
            Passthrough
        );
        // tag create, foreign → Passthrough
        assert_eq!(
            apply_foreign_repo_passthrough(
                ChdirPass("wt".into()),
                "tag",
                &a(&["tag", "v1.0"]),
                true
            ),
            Passthrough
        );
        // #2030 (codex): a no-positional current-branch mutator, foreign → Passthrough
        // (was: ChdirPass wrote branch.<cur>.merge into the worktree, the lie)
        assert_eq!(
            apply_foreign_repo_passthrough(
                ChdirPass("wt".into()),
                "branch",
                &a(&["branch", "--set-upstream-to=origin/main"]),
                true
            ),
            Passthrough
        );
        // #2030 codex r2: the glued short form `-u<up>`, foreign → Passthrough
        assert_eq!(
            apply_foreign_repo_passthrough(
                ChdirPass("wt".into()),
                "branch",
                &a(&["branch", "-uorigin/main"]),
                true
            ),
            Passthrough
        );
        // FLEET (non-foreign) ref-naming branch → unchanged ChdirPass (byte-identical)
        assert_eq!(
            apply_foreign_repo_passthrough(
                ChdirPass("wt".into()),
                "branch",
                &a(&["branch", "feat-x"]),
                false
            ),
            ChdirPass("wt".into())
        );
        // bare LIST form, even foreign → unchanged ChdirPass (read-only, no lie)
        assert_eq!(
            apply_foreign_repo_passthrough(ChdirPass("wt".into()), "branch", &a(&["branch"]), true),
            ChdirPass("wt".into())
        );
    }

    // (B) GATED to mutating-local: a leading `-C`/`--git-dir`/`--work-tree` is
    // stripped ONLY when the real subcommand mutates (so the shim's
    // `-C <worktree>` wins and `<elsewhere>` — e.g. canonical — is not touched);
    // non-mutating `-C` and a post-subcommand `-C` (reuse-message) are preserved.
    #[test]
    fn strip_target_overrides_1463() {
        // ── MUTATING-local + leading override → STRIPPED ──
        assert_eq!(
            strip_target_overrides(&s(&["-C", "/tmp/x", "commit", "-m", "z"])),
            s(&["commit", "-m", "z"])
        );
        assert_eq!(
            strip_target_overrides(&s(&["--git-dir", "/g", "add", "."])),
            s(&["add", "."])
        );
        assert_eq!(
            strip_target_overrides(&s(&["--git-dir=/g", "commit"])),
            s(&["commit"])
        );
        assert_eq!(
            strip_target_overrides(&s(&["--work-tree", "/w", "reset", "--hard"])),
            s(&["reset", "--hard"])
        );
        // glued -C<path>
        assert_eq!(
            strip_target_overrides(&s(&["-C/tmp/x", "commit"])),
            s(&["commit"])
        );
        // repeated -C all dropped (the left-to-right override chain)
        assert_eq!(
            strip_target_overrides(&s(&["-C", "/a", "-C", "/b", "commit"])),
            s(&["commit"])
        );
        // value-taking non-target global (-c) kept WITH its value, -C dropped
        assert_eq!(
            strip_target_overrides(&s(&["-c", "k=v", "-C", "/x", "commit"])),
            s(&["-c", "k=v", "commit"])
        );
        // lead matrix: `git -C <canonical> commit` (mutating) → strip → worktree.
        assert_eq!(
            strip_target_overrides(&s(&["-C", "/canonical", "commit"])),
            s(&["commit"])
        );

        // ── NON-mutating + leading -C → PRESERVED (gating) ──
        assert_eq!(
            strip_target_overrides(&s(&["-C", "/tmp/x", "rev-parse", "--is-inside-work-tree"])),
            s(&["-C", "/tmp/x", "rev-parse", "--is-inside-work-tree"])
        );
        assert_eq!(
            strip_target_overrides(&s(&["-C", "/tmp/x", "init", "--quiet"])),
            s(&["-C", "/tmp/x", "init", "--quiet"])
        );
        // lead matrix: `git -C <canonical> rev-parse` (non-mutating) → read in place.
        assert_eq!(
            strip_target_overrides(&s(&["-C", "/canonical", "rev-parse"])),
            s(&["-C", "/canonical", "rev-parse"])
        );

        // ── preserved / no-op forms ──
        // POST-subcommand `-C` (git commit reuse-message) PRESERVED
        assert_eq!(
            strip_target_overrides(&s(&["commit", "-C", "HEAD"])),
            s(&["commit", "-C", "HEAD"])
        );
        // no globals → unchanged
        assert_eq!(
            strip_target_overrides(&s(&["commit", "-m", "x"])),
            s(&["commit", "-m", "x"])
        );
        // globals-only / malformed (no subcommand) → unchanged, no panic
        assert_eq!(strip_target_overrides(&s(&["-C"])), s(&["-C"]));
        assert_eq!(strip_target_overrides(&s(&["-C", "/x"])), s(&["-C", "/x"]));
    }

    // ── #2234 (C): cwd↔bound-worktree drift detection ──────────────────
    // Drive fixture git through the REAL git binary (not the bare `git` PATH
    // entry, which IS this shim — its #1463 ChdirPass would hijack the op). See
    // the #1748 note on `has_unmerged_files_false_on_clean_repo`.
    fn drift_git_init(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let git = resolve_real_git();
        assert!(
            Command::new(&git)
                .arg("-C")
                .arg(dir)
                .args(["init", "-b", "main"])
                .output()
                .unwrap()
                .status
                .success(),
            "git init {dir:?}"
        );
    }

    fn drift_home(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("agend-2234-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn workspace_clone_is_drift_but_scratch_repo_is_not_2234() {
        let home = drift_home("detect");
        let agent = "ag";
        let h = home.to_str().unwrap();
        // The agent's configured workspace dir — a SEPARATE clone (own store).
        let ws = home.join("workspace").join(agent);
        drift_git_init(&ws);
        // The bound worktree — a different, separate repo (only object-store
        // identity matters for the foreign check).
        let worktree = home.join("wt");
        drift_git_init(&worktree);
        // A legit foreign scratch repo OUTSIDE the workspace dir (#1463 incubator).
        let scratch = home.join("scratch");
        drift_git_init(&scratch);

        // cwd = workspace clone, foreign to worktree → DRIFT.
        assert!(
            is_workspace_clone_drift(h, agent, &ws, &worktree),
            "workspace clone foreign to the bound worktree must be drift"
        );
        // cwd = scratch repo OUTSIDE the workspace dir → NOT drift (no FP).
        assert!(
            !is_workspace_clone_drift(h, agent, &scratch, &worktree),
            "a foreign scratch repo outside the workspace dir must NOT warn"
        );
        // cwd == worktree (aligned, same object store) → NOT drift.
        assert!(
            !is_workspace_clone_drift(h, agent, &worktree, &worktree),
            "cwd aligned with the bound worktree must NOT be drift"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn drift_warns_per_class_then_latches_2234() {
        let home = drift_home("latch");
        let agent = "ag";
        let h = home.to_str().unwrap();
        let ws = home.join("workspace").join(agent);
        drift_git_init(&ws);
        let worktree = home.join("wt");
        drift_git_init(&worktree);

        // First read-class sighting → warns, writes the .read latch + a fleet_events
        // line.
        assert!(
            warn_workspace_drift_once(h, agent, &ws, &worktree, false),
            "first read-class drift sighting must warn"
        );
        let read_marker = home
            .join("runtime")
            .join(agent)
            .join("cwd_drift_warned.read");
        assert!(
            read_marker.exists(),
            "read-class latch marker must be written"
        );
        let events = std::fs::read_to_string(home.join("fleet_events.jsonl")).unwrap_or_default();
        assert!(
            events.contains("cwd_worktree_drift"),
            "a cwd_worktree_drift event must be logged: {events}"
        );

        // Second read-class call, same drifted cwd → latched, no second warn.
        assert!(
            !warn_workspace_drift_once(h, agent, &ws, &worktree, false),
            "standing read-class drift must warn only once (latched)"
        );

        // The FIRST mutating-class op on the SAME cwd is a distinct latch class →
        // warns again (so the agent sees the hint before its first write). ≤2/cwd.
        assert!(
            warn_workspace_drift_once(h, agent, &ws, &worktree, true),
            "first mutating-class drift sighting must warn (independent latch class)"
        );
        let mut_marker = home
            .join("runtime")
            .join(agent)
            .join("cwd_drift_warned.mut");
        assert!(
            mut_marker.exists(),
            "mutating-class latch marker must be written"
        );

        // Second mutating-class call → latched, no third warn (cadence capped at 2).
        assert!(
            !warn_workspace_drift_once(h, agent, &ws, &worktree, true),
            "standing mutating-class drift must warn only once (latched)"
        );

        // Aligned cwd never warns (guard), in either class.
        assert!(
            !warn_workspace_drift_once(h, agent, &worktree, &worktree, false),
            "aligned cwd must not warn (read class)"
        );
        assert!(
            !warn_workspace_drift_once(h, agent, &worktree, &worktree, true),
            "aligned cwd must not warn (mutating class)"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // #2234: a NEW drifted cwd re-warns even after a prior cwd latched — the latch
    // keys on the exact cwd string, so moving to a different clone is new info.
    #[test]
    fn drift_new_cwd_rewarns_2234() {
        let home = drift_home("newcwd");
        let agent = "ag";
        let h = home.to_str().unwrap();
        let ws1 = home.join("workspace").join(agent);
        drift_git_init(&ws1);
        let worktree = home.join("wt");
        drift_git_init(&worktree);

        assert!(
            warn_workspace_drift_once(h, agent, &ws1, &worktree, false),
            "first cwd must warn"
        );
        assert!(
            !warn_workspace_drift_once(h, agent, &ws1, &worktree, false),
            "same cwd latched"
        );
        // A DIFFERENT clone path under the workspace dir → not yet latched → re-warns.
        let ws2 = ws1.join("nested");
        drift_git_init(&ws2);
        assert!(
            warn_workspace_drift_once(h, agent, &ws2, &worktree, false),
            "a new drifted cwd must re-warn (new info)"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // #2234: the warning carries the ACTIONABLE recovery hint (operator ruling:
    // replace the cd-only message). Routes through the SAME `drift_warning_message`
    // producer the emit uses (#1493 — no hand-copied shape) and pins the concrete
    // recovery tokens so the message can't silently regress to cd-only.
    #[test]
    fn drift_message_has_actionable_recovery_hint_2234() {
        let msg = drift_warning_message(Path::new("/home/ws/ag"), Path::new("/home/wt"));
        // The actionable recovery contract: a check command, absolute-path guidance,
        // and the explicit "cd alone does NOT" correction (r2's finding).
        assert!(
            msg.contains("status --short"),
            "must tell the agent how to CHECK what mislanded: {msg}"
        );
        assert!(
            msg.contains("ABSOLUTE paths"),
            "must give the absolute-path recovery action: {msg}"
        );
        assert!(
            msg.contains("`cd` alone does NOT"),
            "must correct the insufficient cd-only advice (r2): {msg}"
        );
        // Names both endpoints so the agent knows which dir is which.
        assert!(
            msg.contains("/home/ws/ag") && msg.contains("/home/wt"),
            "must name both the cwd clone and the worktree: {msg}"
        );
    }

    // ── #2158: shim-side bypass mutating-op audit ──────────────────────────
    /// Option B gate: the stray-worktree / drift / stray-tree-push vector IS
    /// audited; read-only ops and the high-frequency `commit`/`add` (agent
    /// self-worktree, ~zero forensic value) are NOT.
    #[test]
    fn bypass_op_is_audited_option_b_gate_2158() {
        let a = |toks: &[&str]| toks.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Audited: worktree-lifecycle + drift + destructive + stray-tree push.
        for op in ["worktree", "checkout", "switch", "reset", "clean", "push"] {
            assert!(
                bypass_op_is_audited(op, &a(&[op])),
                "bypass `{op}` must be audited (Option B vector)"
            );
        }
        // `branch` audited ONLY in its ref-mutating form, not the bare list.
        assert!(
            bypass_op_is_audited("branch", &a(&["branch", "-D", "feat/x"])),
            "bypass `branch -D` (ref delete) must be audited"
        );
        assert!(
            !bypass_op_is_audited("branch", &a(&["branch"])),
            "bare `branch` (list, read) must NOT be audited"
        );
        // NOT audited: high-frequency self-worktree mutators (Option B exclusion).
        for op in ["commit", "add"] {
            assert!(
                !bypass_op_is_audited(op, &a(&[op])),
                "bypass `{op}` must NOT be audited (Option B: floods, ~0 value)"
            );
        }
        // NOT audited: read-only ops.
        for op in ["status", "log", "diff", "rev-parse", "show", "tag"] {
            assert!(
                !bypass_op_is_audited(op, &a(&[op])),
                "bypass read-only `{op}` must NOT be audited"
            );
        }
    }

    /// The audit record carries the forensic fields (event type, subcommand, argv,
    /// process ancestry, bypass layer) so a stray-worktree culprit is traceable.
    #[test]
    fn build_bypass_audit_event_shape_2158() {
        let a = |toks: &[&str]| toks.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let argv = a(&["worktree", "add", "/tmp/stray", "origin/main"]);
        let ancestry = vec!["100 1 git".to_string(), "1 0 launchd".to_string()];
        let ev =
            build_bypass_audit_event("dev-2", "worktree", &argv, "/cwd/x", 4242, &ancestry, "env");
        assert_eq!(ev["event"], "bypass_mutating_op");
        assert_eq!(ev["agent"], "dev-2");
        assert_eq!(ev["subcommand"], "worktree");
        assert_eq!(ev["ppid"], 4242);
        assert_eq!(ev["cwd"], "/cwd/x");
        assert_eq!(ev["bypass_layer"], "env");
        assert_eq!(ev["argv"][2], "/tmp/stray");
        assert_eq!(ev["process_ancestry"][0], "100 1 git");
        assert!(ev["timestamp"].is_string(), "must carry a timestamp");
    }
}
