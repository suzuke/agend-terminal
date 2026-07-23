//! agentic-git — transparent git shim for fleet-managed worktrees.
//!
//! Intercepts git commands via PATH shadowing. Reads binding.json to
//! determine the active worktree, then either:
//! - passthrough (unbound read-only commands)
//! - chdir + pass (bound commands routed to worktree)
//! - silent-exempt (gh post-merge cleanup checkout — Sprint 57 Wave 2 Track D)
//! - deny (forbidden operations with LLM-friendly error)
//!
//! Bypass: AGENTIC_GIT_BYPASS=1 | AGENTIC_GIT_BYPASS_AGENT=<name> | AGENTIC_GIT_BYPASS_UNTIL=<epoch>
//!
//! Cross-platform: Unix uses exec() for process replacement; Windows uses
//! status() + exit(code) for equivalent behavior.

use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// Contract modules (binding HMAC verifier #1651, protected-ref predicate
// #2550 W4) live in the `agentic-git-core` crate so any embedding system —
// this binary, a daemon, tests — links the EXACT same verifier/predicate
// source and no signer/verifier or ref-set drift is possible.
use agentic_git_core::{binding, integrity_core, protected_refs};

/// #1504 L3: max times the shim may re-enter before hard-failing. Healthy
/// operation never exceeds 1 (real git ≠ shim → no re-entry), so a small cap
/// has zero false-trip risk while containing a self-resolution spawn storm.
const MAX_SHIM_DEPTH: u32 = 3;

/// Legacy (agend-terminal) name for a given `AGENTIC_GIT_*` env var, if any.
/// The shim was extracted from agend-terminal's `agend-git`; it keeps reading
/// the legacy names as a fallback so an existing fleet can adopt this binary
/// with zero daemon-side changes.
fn legacy_env_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "AGENTIC_GIT_HOME" => "AGEND_HOME",
        "AGENTIC_GIT_AGENT" => "AGEND_INSTANCE_NAME",
        "AGENTIC_GIT_REAL_GIT" => "AGEND_REAL_GIT",
        "AGENTIC_GIT_BYPASS" => "AGEND_GIT_BYPASS",
        "AGENTIC_GIT_BYPASS_AGENT" => "AGEND_GIT_BYPASS_AGENT",
        "AGENTIC_GIT_BYPASS_UNTIL" => "AGEND_GIT_BYPASS_UNTIL",
        "AGENTIC_GIT_SHIM_DEPTH" => "AGEND_GIT_SHIM_DEPTH",
        "AGENTIC_GIT_ALLOW_CANONICAL_MUTATE" => "AGEND_GIT_ALLOW_CANONICAL_MUTATE",
        // #4 Δc: recovery-layer kill switch, legacy twin per the same
        // zero-daemon-change adoption contract every other var here follows.
        "AGENTIC_GIT_SNAPSHOTS" => "AGEND_GIT_SNAPSHOTS",
        _ => return None,
    })
}

/// `env::var` with the primary `AGENTIC_GIT_*` name, falling back to the
/// legacy `AGEND_*` name (see [`legacy_env_name`]).
fn env_compat(name: &str) -> Result<String, env::VarError> {
    env::var(name).or_else(|e| match legacy_env_name(name) {
        Some(old) => env::var(old),
        None => Err(e),
    })
}

/// `env::var_os` variant of [`env_compat`].
fn env_compat_os(name: &str) -> Option<std::ffi::OsString> {
    env::var_os(name).or_else(|| legacy_env_name(name).and_then(env::var_os))
}

/// Current shim recursion depth, read from the propagated sentinel env.
fn shim_depth() -> u32 {
    env_compat("AGENTIC_GIT_SHIM_DEPTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Session mode (argv[0] dispatch, Δ1): CLI surface lives in its own module so
/// `main.rs` churn stays minimal and shim-mode's existing body is untouched.
mod cli;

/// P2 recovery layer (issue #4): pre-destructive-op snapshots, push guard,
/// and lazy prune — lives in its own module for the same reason `cli` does.
mod snapshot;

/// Δ1: mode = shim iff `basename(argv[0])`, with a trailing `.exe`/`.EXE`
/// stripped, equals `git` — case-insensitive ONLY on `cfg(windows)`, exact on
/// Unix. Covers bare `git`, absolute-path invocation (`<home>/bin/git`), and
/// Windows `git.exe` copies. Everything else (including the bare compiled
/// binary name `agentic-git`) is CLI mode — never silently shims.
fn is_git_invocation(argv0: &std::ffi::OsStr) -> bool {
    let base = match Path::new(argv0).file_name() {
        Some(b) => b.to_string_lossy().into_owned(),
        None => return false,
    };
    let stripped = base
        .strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".EXE"))
        .unwrap_or(&base);
    if cfg!(windows) {
        stripped.eq_ignore_ascii_case("git")
    } else {
        stripped == "git"
    }
}

/// #1504 L3: recursion guard. If git ever resolves back to THIS shim (e.g.
/// AGENTIC_GIT_REAL_GIT unset + self-exclusion miss), each real-git spawn
/// re-enters here; on Windows `exec_real_git` uses `status()` (spawn), so
/// it's an unbounded process storm, not a single exec-replace. Cap the depth
/// and hard-fail with an actionable message. Checked BEFORE mode dispatch
/// (and, redundantly but harmlessly, again at the top of `shim_main` below —
/// that inline copy is left byte-identical to preserve "shim flow is
/// untouched") because this is a cross-cutting recursion safety net, not
/// shim-specific business logic: whichever entry point this process took, a
/// propagated `AGENTIC_GIT_SHIM_DEPTH` at the cap means something upstream
/// already re-entered this binary and must hard-fail before doing anything
/// else, including CLI-mode `run`'s own git child-process spawns.
fn recursion_guard_or_exit() {
    let depth = shim_depth();
    if depth >= MAX_SHIM_DEPTH {
        eprintln!(
            "agentic-git: FATAL recursion guard tripped (AGENTIC_GIT_SHIM_DEPTH={depth}) — the \
             shim resolved git to itself. AGENTIC_GIT_REAL_GIT is unset/unresolvable and \
             $AGENTIC_GIT_HOME/bin was not excluded from PATH. Set AGENTIC_GIT_REAL_GIT to the real \
             git binary. (#1504)"
        );
        std::process::exit(70); // EX_SOFTWARE
    }
}

pub fn shim_entry() {
    recursion_guard_or_exit();
    let argv0 = env::args_os().next().unwrap_or_default();
    if is_git_invocation(&argv0) {
        shim_main();
    } else {
        cli::cli_main();
    }
}

fn shim_main() {
    let args: Vec<String> = env::args().skip(1).collect();

    // #1504 L3: recursion guard. If git ever resolves back to THIS shim (e.g.
    // AGENTIC_GIT_REAL_GIT unset + self-exclusion miss), each real-git spawn re-enters
    // here; on Windows `exec_real_git` uses `status()` (spawn), so it's an
    // unbounded process storm, not a single exec-replace. Cap the depth and
    // hard-fail with an actionable message. Checked BEFORE `should_bypass`
    // because the bypass path also execs real git.
    let depth = shim_depth();
    if depth >= MAX_SHIM_DEPTH {
        eprintln!(
            "agentic-git: FATAL recursion guard tripped (AGENTIC_GIT_SHIM_DEPTH={depth}) — the \
             shim resolved git to itself. AGENTIC_GIT_REAL_GIT is unset/unresolvable and \
             $AGENTIC_GIT_HOME/bin was not excluded from PATH. Set AGENTIC_GIT_REAL_GIT to the real \
             git binary. (#1504)"
        );
        std::process::exit(70); // EX_SOFTWARE
    }

    // Bypass checks (3-layer per §7).
    if should_bypass() {
        // #2158: audit a SUB-AGENT's own `AGENTIC_GIT_BYPASS=1 git <mutating>` op —
        // the stray-worktree / drift vector the daemon-side bypass audit
        // (git_helpers.rs, #2242 PR2(iii)) cannot see. The shim is a SEPARATE
        // binary on the AGENT PATH; the daemon PATH is shim-free, so the two audits
        // cover DISJOINT callers (no double-log). Top-level ops only
        // (`shim_depth()==0` skips git re-invoked by a hook); commit/add excluded
        // (Option B — agents bypass-commit into their OWN worktree constantly, so
        // logging those floods fleet_events for ~zero forensic value). Best-effort,
        // never blocks: the `exec_real_git` below is unchanged.
        if shim_depth() == 0 {
            // #27: audit the REAL subcommand past leading globals (mirrors the
            // classify normalization + #2234-Patch-A's deny below), so a bypassed
            // `git -C x worktree add` is audited as a worktree op, not the `-C`
            // token. `log_bypass_mutating_op` still records the FULL args verbatim.
            let sub_idx = subcommand_index(&args);
            let subcommand = sub_idx
                .and_then(|i| args.get(i))
                .map(|s| s.as_str())
                .unwrap_or("");
            let audit_args: &[String] = sub_idx.map_or(&args[..], |i| &args[i..]);
            if bypass_op_is_audited(subcommand, audit_args) {
                let home = env_compat("AGENTIC_GIT_HOME").unwrap_or_default();
                if !home.is_empty() {
                    let agent = env_compat("AGENTIC_GIT_AGENT").unwrap_or_default();
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
            // carry no AGENTIC_GIT_AGENT, so this only bites true agents. Exits
            // non-zero before the passthrough exec when the deny fires.
            enforce_agent_canonical_bypass_deny(&args);
        }
        exec_real_git(&args, None);
    }

    let agent = env_compat("AGENTIC_GIT_AGENT").unwrap_or_default();
    let home = env_compat("AGENTIC_GIT_HOME").unwrap_or_default();

    if agent.is_empty() || home.is_empty() {
        // #2234 defect#2: a NON-agent caller (no AGENTIC_GIT_AGENT) early-exits
        // here to the TERMINAL `exec_real_git` BEFORE reaching `classify` — so a
        // canonical-HEAD-touching `git checkout|switch <branch>` (e.g.
        // `git checkout origin/main`, which detaches canonical HEAD) passed
        // through with zero attribution. Record one fleet_events line with
        // process ancestry first (only when we have a home to write to — the
        // daemon-correlated culprit inherits AGENTIC_GIT_HOME but dropped
        // AGENTIC_GIT_AGENT). Instrument-only: the passthrough exec is unchanged.
        if !home.is_empty() {
            // instrument-only: D3 #2234 — non-agent canonical-checkout audit,
            // control-flow-inert; the `exec_real_git` below is unchanged.
            log_nonagent_canonical_checkout(&home, &agent, &args);
        }
        // Impl-review finding (deviation #4): a solo user who EXPLICITLY opted
        // into the recovery net (AGENTIC_GIT_SNAPSHOTS=1) but has no agent
        // context must still get it — otherwise the `noagent` fallback the
        // design promises is dead code. `maybe_snapshot` is a no-op unless the
        // op is destructive AND snapshots are enabled, so this only fires on a
        // consented opt-in; it snapshots to the repo's own refs (no home
        // needed), with who=noagent, and is additive + fail-open — the
        // passthrough `exec_real_git` below is unchanged.
        if let Some(idx) = subcommand_index(&args) {
            let dir = effective_cwd_through_globals(&args, idx);
            snapshot::maybe_snapshot(&args, &dir, &home, &agent);
        }
        exec_real_git(&args, None);
    }

    // Read binding.
    let binding = read_binding(&home, &agent);
    // #27: the REAL subcommand, resolved past leading git globals (-C/-c/--git-dir/…)
    // via subcommand_index — so classify/deny/audit/events key on the actual op, not
    // a leading flag. `norm_args` is the subcommand-onward arg view fed to classify /
    // apply_foreign_repo_passthrough so their positional reads stay aligned; the FULL
    // `args` is still used for exec / strip_target_overrides / snapshot below.
    let sub_idx = subcommand_index(&args);
    let subcommand = sub_idx
        .and_then(|i| args.get(i))
        .map(|s| s.as_str())
        .unwrap_or("");

    // #34: nested submodule--helper at depth > 0 — invoked by real git
    // internally, not a direct agent call. Pass through without classify/
    // snapshot/audit; the depth-0 shim already routed the parent operation.
    if shim_depth() > 0 && subcommand == "submodule--helper" {
        exec_real_git(&args, None);
    }
    let norm_args: &[String] = match sub_idx {
        Some(i) => &args[i..],
        None => &args,
    };

    // Arch14: cross-agent sibling read boundary. A bound agent whose effective
    // read target (cwd or leading -C) is another agent's daemon-managed
    // same-source worktree must fail loudly — never silently return fabricated
    // data via ChdirPass or leak the target's real data via Passthrough.
    if is_bound(&binding) {
        let effective_target =
            effective_cwd_through_globals(&args, sub_idx.unwrap_or(args.len()));
        if let Some(target_agent) =
            detect_cross_agent_sibling_target(&agent, &binding, &effective_target)
        {
            eprintln!(
                "agentic-git: DENIED \u{2014} agent '{agent}' cannot read from agent \
                 '{target_agent}'\u{2019}s managed worktree '{}'. Cross-agent \
                 worktree reads are structurally refused to prevent fabricated \
                 target data. (#arch14)",
                effective_target.display()
            );
            write_git_event_typed(
                &home,
                &agent,
                subcommand,
                "deny_cross_agent_sibling",
                None,
                Some(&format!(
                    "caller={agent} target_agent={target_agent} target={}",
                    effective_target.display()
                )),
            );
            std::process::exit(1);
        }
    }

    // #2234: loud WARN for the cwd↔bound-worktree drift. When a bound agent's cwd
    // is its `<home>/workspace/<agent>` clone — a SEPARATE git object store from its
    // bound worktree — the shim routes git to the worktree (ChdirPass) while the
    // agent's file edits / cargo / `AGENTIC_GIT_BYPASS=1 git` act on the clone, so git
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
    // bypasses via AGENTIC_GIT_BYPASS and never reaches here). Desk research
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
    // daemon-spawned subprocesses (AGENTIC_GIT_AGENT set); operators
    // are interactive shells with no such env. Used by classify's
    // canonical-checkout gate to prevent reviewer-style PR-inspection
    // from polluting canonical worktrees with stale refs.
    let is_agent_caller = env_compat_os("AGENTIC_GIT_AGENT").is_some();

    // #1463 (A): a bound agent's mutating command whose cwd is a FOREIGN git
    // repo (separate object store — e.g. a test scratch repo) should operate on
    // THAT repo, not be redirected into the worktree. Post-process the classify
    // result so the (unchanged, unit-tested) `classify` stays cwd-agnostic.
    let action = apply_foreign_repo_passthrough(
        classify_argv(
            &args,
            &binding,
            parent_is_gh,
            canonical_cwd,
            is_agent_caller,
        ),
        subcommand,
        norm_args,
        cwd_is_foreign_repo(&binding),
    );

    // #4: pre-destructive-op snapshot, BEFORE the op executes. Only actions
    // that actually run something can destroy anything (Deny/SilentExempt
    // never reach real git). Target dir mirrors exactly where the op is
    // about to run: the bound worktree for ChdirPass/CleanupAndChdirPushPass,
    // else the caller's own (possibly `-C`-redirected) cwd for Passthrough —
    // snapshotting a different tree than the one about to be destroyed would
    // be a lie. `snapshot::maybe_snapshot` itself no-ops in <1 argv scan for
    // every non-destructive op (perf note) and is fail-open + loud on any
    // internal error (see the issue's failure-policy contract).
    match &action {
        Action::ChdirPass(wt) | Action::CleanupAndChdirPushPass(wt) => {
            snapshot::maybe_snapshot(&args, Path::new(wt), &home, &agent);
        }
        Action::Passthrough => {
            if let Some(idx) = subcommand_index(&args) {
                let dir = effective_cwd_through_globals(&args, idx);
                snapshot::maybe_snapshot(&args, &dir, &home, &agent);
            }
        }
        Action::SilentExempt { .. } | Action::Deny(_) => {}
    }

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
                norm_args, // #27: subcommand-rooted (leading globals stripped)
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
            // #2677 (embedder P0): a BARE force-push (`--force`/`-f`/`+refspec`) to a
            // NON-protected branch can silently overwrite another agent's/session's
            // commits — `push_protected_violation` above ignores force ("HOW not WHAT"),
            // so it never catches this. Require a lease (footgun-removal, not
            // capability-removal); pure deletions are exempt (ALL-not-ANY, #2677 F1).
            // Runs AFTER the protected guard so a protected-ref force is denied there.
            if let Some(reason) = push_force_without_lease_violation(norm_args) {
                emit_deny_error(subcommand, &reason, &agent, Some(&binding));
                write_git_event_typed(
                    &home,
                    &agent,
                    subcommand,
                    "deny_force_no_lease",
                    None,
                    Some(&reason),
                );
                std::process::exit(1);
            }
            // #4 Δa v5: the snapshot namespace must be shim-unpushable — a
            // "private" ref namespace is not private to `git push`. Two
            // cheap layers (text substring + resolved commit-tip match);
            // fail-closed like the other push denylists above. See
            // `snapshot::snapshot_push_violation` for the full contract
            // (and its documented, deliberate non-goal: this is accident /
            // casual-explicit prevention, not an exfiltration boundary).
            if let Some(reason) = snapshot::snapshot_push_violation(norm_args, &worktree) {
                emit_deny_error(subcommand, &reason, &agent, Some(&binding));
                write_git_event_typed(
                    &home,
                    &agent,
                    subcommand,
                    "deny_snapshot_ref_push",
                    None,
                    Some(&reason),
                );
                std::process::exit(1);
            }
            // Cross-branch push guard: a bound agent may push ONLY its assigned
            // branch (symmetric to the cross-branch checkout deny), so it can't
            // clobber or delete another agent's branch on a shared remote — which
            // the protected-ref guard above (main/master/policy only) misses.
            if let Some(reason) = push_cross_branch_violation(
                norm_args, // #27: subcommand-rooted (leading globals stripped)
                binding.branch.as_deref().unwrap_or(""),
                &current_branch_of(&worktree),
                push_default_is_matching(&worktree),
            ) {
                emit_deny_error(subcommand, &reason, &agent, Some(&binding));
                write_git_event_typed(
                    &home,
                    &agent,
                    subcommand,
                    "deny_cross_branch_push",
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
    if env_compat("AGENTIC_GIT_BYPASS").is_ok() {
        return true;
    }
    if let Ok(agent_bypass) = env_compat("AGENTIC_GIT_BYPASS_AGENT") {
        if let Ok(current) = env_compat("AGENTIC_GIT_AGENT") {
            if agent_bypass == current {
                return true;
            }
        }
    }
    if let Ok(until_str) = env_compat("AGENTIC_GIT_BYPASS_UNTIL") {
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

/// Fail-closed HMAC verify with LOUD diagnosis on scheme skew (embedder P1a).
/// Preserves today's "anything but authentic → not authentic" posture (every
/// non-`Ok` fails closed, identical to the former `!verify(..)` bool), but an
/// `UnsupportedScheme` — the sidecar was signed with an HMAC scheme this shim was
/// NOT built to verify (signer/verifier from different `agentic-git-core` versions)
/// — is surfaced LOUD to stderr instead of a silent "unbound", so the drift is
/// diagnosable rather than a mysterious fleet-wide unbind.
fn verify_sidecar(home: &str, content: &[u8], tag: &str) -> bool {
    match integrity_core::verify(Path::new(home), content, tag) {
        Ok(()) => true,
        Err(integrity_core::VerifyError::UnsupportedScheme {
            tag_scheme,
            runtime_scheme,
        }) => {
            eprintln!(
                "agentic-git: HMAC scheme skew — binding signed with scheme {tag_scheme}, this \
                 shim implements {runtime_scheme}; signer and verifier were built from different \
                 agentic-git-core versions — rebuild-together / rebind."
            );
            false
        }
        Err(_) => false,
    }
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
    if !verify_sidecar(home, content.as_bytes(), &tag) {
        return Binding::default();
    }
    // #26: decode through the core-owned typed v1 codec — the same
    // representation the reference `run` writer (and the agend daemon) sign.
    // An UNSUPPORTED version is surfaced LOUD (mirrors the HMAC scheme-skew
    // posture) and fails closed to unbound: a v2 document may carry authority
    // semantics this shim cannot enforce. A plain parse failure stays the
    // silent unbound fail-safe it always was.
    let doc = match binding::decode(&content) {
        Ok(doc) => doc,
        Err(binding::BindingDecodeError::UnsupportedVersion { found }) => {
            eprintln!(
                "agentic-git: binding format version {found} is not supported by this shim                  (implements v{}) — signer and shim were built from different contract                  versions; rebuild-together / rebind.",
                binding::BINDING_FORMAT_VERSION
            );
            return Binding::default();
        }
        Err(_) => return Binding::default(), // parse failure = unbound (fail-safe)
    };
    let b = Binding {
        task_id: doc.task_id,
        branch: doc.branch,
        worktree: doc.worktree,
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

// ── #30 module split ────────────────────────────────────────────────────
// The former single-file shim body lives in five internal modules. The
// `pub(crate) use` globs keep every item addressable from the crate root,
// so `tests.rs` (`use super::*`) and cross-module callers are unchanged.
mod classify;
mod exec;
mod paths;
mod push_guards;
mod telemetry;
pub(crate) use classify::*;
pub(crate) use exec::*;
pub(crate) use paths::*;
pub(crate) use push_guards::*;
pub(crate) use telemetry::*;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
