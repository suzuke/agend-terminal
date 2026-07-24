//! Command classification: `Action`, `classify`/`classify_argv`, the
//! checkout/switch predicates, foreign-repo & workspace-drift detection,
//! gh parent-process heuristics, and the #1463 forensics argv probes.

use std::env;
use std::path::{Path, PathBuf};

use super::*;

// ── Classification ──────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
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

// #2550 W4: the local mirror (a hand-copied `eq_ignore_ascii_case` literal
// that "MUST stay in sync" with the daemon's copy) is gone —
// `protected_refs::is_protected_ref` (from `agentic-git-core`) IS the single
// source, so there is nothing left to drift. Call sites use it directly.
pub(crate) use agentic_git_core::protected_refs::is_protected_ref;

/// #1463 (A): is the current working directory a git repo whose object store is
/// SEPARATE from the bound worktree's (a foreign / scratch repo, e.g. a test
/// incubator)? When true a mutating command was aimed at THAT repo, not the
/// worktree — passing it through avoids hijacking it into the worktree (the
/// init-pile pollution). Fail-closed: a retargeting env var, an unresolved
/// commondir, or a missing worktree → `false` (→ ChdirPass keeps the existing
/// protection). SAFETY: canonical and EVERY sibling worktree resolve to
/// canonical's commondir, so a `true` here can ONLY mean a genuinely-separate
/// store the agent cannot use to reach canonical / shared refs / a sibling.
pub(crate) fn cwd_is_foreign_repo(binding: &Binding) -> bool {
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
pub(crate) fn is_workspace_clone_drift(
    home: &str,
    agent: &str,
    cwd: &Path,
    worktree: &Path,
) -> bool {
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
pub(crate) fn warn_workspace_drift_once(
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
pub(crate) fn subcmd_for_drift_event() -> &'static str {
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
pub(crate) fn drift_warning_message(cwd: &Path, worktree: &Path) -> String {
    let cwd_d = cwd.display();
    let wt_d = worktree.display();
    format!(
        "agentic-git: \u{26a0} #2234 cwd/worktree drift — your cwd '{cwd_d}' is a \
         SEPARATE git repo from your bound worktree '{wt_d}'. git (via this shim) \
         runs against the worktree, but file edits made with a cwd-relative path \
         land in THIS cwd clone where git can't see them — so reads look \
         stale/'fake' and commits can come up empty.\n  \
         CHECK what mislanded:  git -C '{cwd_d}' status --short   \
         (any real source files listed there were written to the wrong repo)\n  \
         RECOVER: re-make those edits using ABSOLUTE paths under '{wt_d}', or copy \
         them across (`cp '{cwd_d}/<file>' '{wt_d}/<file>'`) — `cd` alone does NOT \
         move edits already written by absolute path. git already routes to the \
         worktree; verify with  AGENTIC_GIT_BYPASS=1 git -C '{wt_d}' status"
    )
}

/// #2234: thin env wrapper wiring `warn_workspace_drift_once` to the live process
/// cwd + binding. No-op when unbound (no worktree to compare against) or the cwd is
/// unreadable. Called once per shim invocation from `main`. `subcmd` selects the
/// per-class latch via `is_mutating_local` so the read- and mutating-class warnings
/// latch independently (≤2 per cwd).
pub(crate) fn maybe_warn_workspace_drift(home: &str, agent: &str, binding: &Binding, subcmd: &str) {
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
pub(crate) fn is_mutating_local(subcmd: &str) -> bool {
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
            | "submodule"
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
pub(crate) fn branch_tag_names_ref(subcmd: &str, args: &[String]) -> bool {
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

/// Sparse-pollution fix (port of the 2026-07-20 agend-git local patch):
/// repo-local CONFIG-writing subcommands that reach `apply_foreign_repo_passthrough`
/// as `ChdirPass` (`sparse-checkout` via classify's `_` default arm) yet mutate
/// per-repo state on disk. In a FOREIGN repo they must run against THAT repo,
/// exactly like the #1463 mutating passthrough — otherwise a third-party tool's
/// cwd-targeted invocation (e.g. Claude Code's plugin-cache
/// `git sparse-checkout set plugins/<x>` run with cwd = the marketplace clone)
/// is ChdirPass'd into the caller's bound worktree, writes `core.sparseCheckout`
/// plus cone patterns into `config.worktree`, and silently EMPTIES the agent's
/// working tree while `git status` keeps reporting clean (four fleet incidents,
/// pattern `plugins/grafana-mcp`; the fourth hit a worktree whose owner session
/// was NOT the one syncing). `config` shares the same shape and is kept for
/// defense in depth even though classify currently passes it through
/// unconditionally. Deliberately NOT added to `is_mutating_local`: that list
/// also gates `strip_target_overrides`, and stripping `-C` from
/// `git -C <dir> config/sparse-checkout` would break legit helpers (see the
/// #1463 (B) rationale).
pub(crate) fn is_repo_local_config_write(subcmd: &str) -> bool {
    matches!(subcmd, "sparse-checkout" | "config")
}

/// #2158: which `AGENTIC_GIT_BYPASS=1 git <subcmd>` ops are worth auditing — Option B
/// (lead-chosen), the stray-worktree / drift / stray-tree-push vector. EXCLUDES
/// `commit`/`add`: agents bypass-commit into their OWN worktree constantly, so
/// logging those would flood fleet_events for ~zero forensic value (a bypass commit
/// lands in the agent's own tree, not a stray one). Read-only ops never match.
/// `branch` is audited only in its ref-MUTATING form (create/delete/move — reuse
/// `branch_tag_names_ref`), not the bare list; `tag` is excluded (not a worktree
/// vector). Pure → unit-testable.
pub(crate) fn bypass_op_is_audited(subcmd: &str, args: &[String]) -> bool {
    matches!(
        subcmd,
        "worktree" | "checkout" | "switch" | "reset" | "clean" | "push" | "submodule--helper"
    ) || (subcmd == "branch" && branch_tag_names_ref(subcmd, args))
        || (subcmd == "submodule" && {
            let sub_idx = subcommand_index(args).unwrap_or(0);
            submodule_op_is_write(&args[sub_idx + 1..])
        })
}

/// #2158: which bypass layer authorized this op — forensics that distinguishes a
/// blanket env grant from a scoped/time-boxed one. Mirrors `should_bypass`'s
/// precedence order; `"unknown"` only if the env changed between the two checks.
pub(crate) fn active_bypass_layer() -> &'static str {
    if env_compat("AGENTIC_GIT_BYPASS").is_ok() {
        "env"
    } else if env_compat("AGENTIC_GIT_BYPASS_AGENT").is_ok() {
        "agent"
    } else if env_compat("AGENTIC_GIT_BYPASS_UNTIL").is_ok() {
        "until"
    } else {
        "unknown"
    }
}

/// #1463 (A) + #2027 + sparse-pollution fix: convert a bound-agent `ChdirPass`
/// into `Passthrough` when cwd is a foreign repo AND the command is a local
/// mutating one (#1463), a ref-naming `branch`/`tag` (#2027), or a repo-local
/// config write (`sparse-checkout`/`config` — see `is_repo_local_config_write`).
/// Pure so the matrix is unit-testable; everything else (push/checkout
/// ChdirPass, Deny, already-Passthrough) is returned unchanged.
pub(crate) fn apply_foreign_repo_passthrough(
    action: Action,
    subcmd: &str,
    args: &[String],
    cwd_foreign: bool,
) -> Action {
    if !cwd_foreign || !matches!(action, Action::ChdirPass(_)) {
        return action;
    }
    if subcmd == "submodule" {
        let sub_idx = subcommand_index(args).unwrap_or(0);
        let rest = &args[sub_idx + 1..];
        return if submodule_op_is_write(rest) {
            Action::Deny("submodule writes are denied in foreign repositories".into())
        } else {
            action
        };
    }
    if is_mutating_local(subcmd)
        || branch_tag_names_ref(subcmd, args)
        || is_repo_local_config_write(subcmd)
    {
        return Action::Passthrough;
    }
    action
}

/// #1463: index of the real subcommand in `args` — the first non-option token,
/// skipping leading git global options and CONSUMING the value of value-taking
/// ones (so a `-C <path>` value is not mistaken for the subcommand). `None` if
/// there is no subcommand (globals only). Mirrors the subset of git's
/// global-option grammar that takes a separate value.
pub(crate) fn subcommand_index(args: &[String]) -> Option<usize> {
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
pub(crate) fn effective_cwd_through_globals(args: &[String], sub_idx: usize) -> PathBuf {
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
pub(crate) fn strip_target_overrides(args: &[String]) -> Vec<String> {
    let sub_idx = match subcommand_index(args) {
        Some(i) if is_mutating_local(args[i].as_str()) => {
            if args[i].as_str() == "submodule" && !submodule_op_is_write(&args[i + 1..]) {
                return args.to_vec();
            }
            i
        }
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

/// #34: whether the tokens after `submodule` represent a WRITE operation.
/// Validates the COMPLETE token sequence against the supported read grammar:
///   [--quiet|-q|--cached]* [status|summary]? [--quiet|-q|--cached]*
/// Any unconsumed, duplicate-class, or unrecognized token/flag fails closed
/// as write. Bare (empty rest) is read.
pub(crate) fn submodule_op_is_write(rest: &[String]) -> bool {
    const RECOGNIZED: [&str; 3] = ["--quiet", "-q", "--cached"];
    let mut saw_op = false;
    for t in rest {
        let s = t.as_str();
        if RECOGNIZED.contains(&s) {
            continue;
        }
        if s.starts_with('-') {
            return true;
        }
        if !saw_op && matches!(s, "status" | "summary") {
            saw_op = true;
            continue;
        }
        return true;
    }
    false
}

/// #1511 follow-up: a MUTATING-form action — deny when unbound, else route to
/// the caller's PRIVATE bound worktree. Mirrors the porcelain mutating arm body
/// so the flag-discriminated plumbing arms (`restore --staged`, `update-ref`,
/// `symbolic-ref` write) share one contract.
pub(crate) fn deny_unbound_else_chdir(bound: bool, binding: &Binding) -> Action {
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
pub(crate) fn pass_unbound_else_chdir(bound: bool, binding: &Binding) -> Action {
    if bound {
        if let Some(ref wt) = binding.worktree {
            return Action::ChdirPass(wt.clone());
        }
    }
    Action::Passthrough
}

pub(crate) fn classify(
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
        // cwd. Daemon-internal callers set `AGENTIC_GIT_BYPASS=1` and never reach
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
                // #852: leniency below (#778) is for the operator-typed
                // validation-canary flow, not agent callers — deny first.
                if is_agent_checkout_in_canonical(is_agent_caller, canonical_cwd) {
                    return Action::Deny(
                        "agent callers must not checkout in canonical \
                         (use `repo action=checkout` for PR inspection or \
                         `gh pr diff/view` for read-only). #852."
                            .into(),
                    );
                }
                // #778 Option 3: canonical-rooted unbound checkout leniency.
                if is_canonical_unbound_checkout_leniency(target_branch, canonical_cwd) {
                    return Action::Passthrough;
                }
                return Action::Deny("unbound — no active task assignment".into());
            }
            // A `checkout <tree-ish> -- <pathspec>` restores working-tree paths
            // from that tree; it does NOT switch branches (the bound branch is
            // unchanged). Denying it as "cross-branch" broke the recovery
            // layer's OWN documented restore — `git checkout <snapshot-ref> -- .`
            // — leaving snapshots un-restorable without bypass (impl-review
            // finding). Only `switch`, and `checkout` WITHOUT a `--` pathspec,
            // are branch-switch shapes the cross-branch guard should judge.
            let is_pathspec_restore =
                args.first().is_some_and(|s| s == "checkout") && args.iter().any(|a| a == "--");
            // Check for cross-branch attempt.
            if let Some(ref assigned) = binding.branch {
                if !is_pathspec_restore
                    && !target_branch.is_empty()
                    && target_branch != assigned
                    && !target_branch.starts_with('-')
                {
                    // Sprint 57 Wave 2 Track D: gh post-merge cleanup exemption.
                    if is_gh_post_merge_cleanup_checkout(target_branch, parent_is_gh) {
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
        "worktree" => Action::Deny(
            "worktree lifecycle is session-managed — use `agentic-git run` (or your \
             orchestrator's worktree tool), not raw `git worktree`"
                .into(),
        ),
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
        "submodule" => {
            let sub_idx = subcommand_index(args).unwrap_or(0);
            let rest = &args[sub_idx + 1..];
            if submodule_op_is_write(rest) {
                deny_unbound_else_chdir(bound, binding)
            } else {
                pass_unbound_else_chdir(bound, binding)
            }
        }
        "submodule--helper" => Action::Deny(
            "direct submodule--helper invocation is not allowed — \
             submodule operations must go through `git submodule`"
                .into(),
        ),
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

/// #27: classify from a RAW argv, NORMALIZING leading git globals first. The deny
/// matrix (`classify`) keys on the subcommand token + its positional args; a
/// caller that puts a leading global BEFORE the subcommand — `git -C <path>
/// commit`, `git -c k=v push`, `git --git-dir=<x> worktree add` (common outside
/// agend's "cd into worktree then bare git" pattern) — otherwise made the
/// subcommand a FLAG, so `classify` fell to its `_` default arm and returned
/// `Passthrough` (unbound) / plain `ChdirPass` (bound), SILENTLY SKIPPING every
/// deny arm: worktree-lifecycle, push guards (protected-ref / force-lease /
/// trust-root), the cross-branch fence, and unbound-write. `subcommand_index`
/// already skips leading globals (it feeds snapshot + `ChdirPass`'s
/// `strip_target_overrides`); this wires the SAME normalization into the
/// classify/deny/audit path. `classify` sees the arg view `[sub_idx..]` so its
/// positional reads (checkout target = `args[1]`, symbolic-ref / restore / branch
/// scans over `args[1..]`) stay correct. No leading global, or globals with NO
/// subcommand (`git`, `git --version`, `git --help`), leaves args unchanged —
/// there is no op hidden behind them, so today's Passthrough is preserved (there
/// is nothing to fail closed on; denying would break bare `git --version`).
/// #27 (reviewer4 ②③): the git subcommands `classify` has an explicit policy for.
/// MUST mirror `classify`'s match arms (kept honest by
/// `known_subcommands_mirror_classify_arms_27`). Used ONLY by the fail-closed
/// gate in `classify_argv` — a token resolved from BEHIND leading globals that is
/// NOT in this set is treated as "a global option is hiding the real subcommand"
/// and DENIED, so the value-global-drift bypass can't slip an unrecognized token
/// past `classify`'s `_` default arm. (A bare unknown subcommand with NO leading
/// global — `git gc`, an alias — is unaffected; there is no global redirecting it.)
// Groups (in `classify` order): read-only (status..reflog); config/help/passthrough
// (config..clone); push; mutating (commit..apply); checkout/switch/worktree;
// flag-discriminated (restore/update-ref/symbolic-ref).
pub(crate) const KNOWN_SUBCOMMANDS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "ls-files",
    "ls-tree",
    "rev-parse",
    "fetch",
    "remote",
    "branch",
    "tag",
    "describe",
    "shortlog",
    "reflog",
    "config",
    "help",
    "version",
    "init",
    "clone",
    "push",
    "commit",
    "pull",
    "reset",
    "revert",
    "cherry-pick",
    "stash",
    "merge",
    "rebase",
    "am",
    "add",
    "rm",
    "mv",
    "read-tree",
    "update-index",
    "apply",
    "checkout",
    "switch",
    "worktree",
    "restore",
    "update-ref",
    "symbolic-ref",
    "submodule",
    "submodule--helper",
];

pub(crate) fn is_known_git_subcommand(s: &str) -> bool {
    KNOWN_SUBCOMMANDS.contains(&s)
}

/// #27: classify from a RAW argv, NORMALIZING leading git globals first. The deny
/// matrix (`classify`) keys on the subcommand token + its positional args; a
/// caller that puts a leading global BEFORE the subcommand — `git -C <path>
/// commit`, `git -c k=v push`, `git --git-dir=<x> worktree add` (common outside
/// agend's "cd into worktree then bare git" pattern) — otherwise made the
/// subcommand a FLAG, so `classify` fell to its `_` default arm and returned
/// `Passthrough` (unbound) / plain `ChdirPass` (bound), SILENTLY SKIPPING every
/// deny arm: worktree-lifecycle, push guards (protected-ref / force-lease /
/// trust-root), the cross-branch fence, and unbound-write. `subcommand_index`
/// already skips leading globals (it feeds snapshot + `ChdirPass`'s
/// `strip_target_overrides`); this wires the SAME normalization into the
/// classify/deny/audit path. `classify` sees the arg view `[sub_idx..]` so its
/// positional reads (checkout target = `args[1]`, symbolic-ref / restore / branch
/// scans over `args[1..]`) stay correct.
///
/// FAIL-CLOSED against value-global drift (reviewer4 ②③): `subcommand_index`'s
/// value-taking-global set is fixed, so an unknown SPACE-value global (a future
/// `git --foo <val> <subcmd>`) makes it return `<val>` as the subcommand →
/// `classify`'s `_` arm → the SAME bypass. So when a leading global IS present and
/// the resolved token is NOT a subcommand `classify` recognizes, DENY. Scope
/// (reviewer4): glued `-C<path>` / `--` / `--end-of-options` are rejected by real
/// git itself (exit 129) and are not vectors — this covers space + `=` + multi
/// globals. Globals with NO subcommand (`git`, `git --version`, `git --help`)
/// leave args unchanged and Passthrough — no op to hide, nothing to fail closed on
/// (denying would break bare `git --version`).
pub(crate) fn classify_argv(
    args: &[String],
    binding: &Binding,
    parent_is_gh: bool,
    canonical_cwd: bool,
    is_agent_caller: bool,
) -> Action {
    let sub_idx = subcommand_index(args);
    let subcmd = sub_idx
        .and_then(|i| args.get(i))
        .map(|s| s.as_str())
        .unwrap_or("");
    // reviewer4 ②③: a token resolved from BEHIND ≥1 leading global (`sub_idx > 0`)
    // that classify has no policy for is treated as an option hiding the real
    // subcommand → fail closed. `sub_idx == Some(0)` (no leading global) and
    // `None` (globals-only / empty) never trip this — their unknown subcommands
    // keep the pre-#27 Passthrough.
    if sub_idx.is_some_and(|i| i > 0) && !is_known_git_subcommand(subcmd) {
        return Action::Deny(format!(
            "unrecognized subcommand '{subcmd}' resolved from behind leading git \
             global options — refusing, since a global may be hiding the real \
             subcommand from the seatbelt. Run the plain `git <subcommand>` form \
             (from inside the target directory) instead of prefixing globals."
        ));
    }
    let norm_args: &[String] = match sub_idx {
        Some(i) => &args[i..],
        None => args,
    };
    classify(
        subcmd,
        norm_args,
        binding,
        parent_is_gh,
        canonical_cwd,
        is_agent_caller,
    )
}

// ── `classify`'s checkout/switch arm — named predicates (#2550 W4) ──────
//
// Pure structural extraction of the three special cases the arm carries
// (#852, #778, Sprint 57 Track D) — each predicate is a byte-for-byte move
// of the condition it replaces; no branch/return site changed.

/// #852: agent callers must NOT use the #778 Option-3 leniency below. The
/// leniency was designed for the operator-typed validation-canary flow
/// (operator runs `repo action=checkout` to provision a worktree in
/// detached-HEAD, then `git switch <branch>` to land on the branch; that
/// follow-up needs to pass without a BYPASS). But the gate wasn't
/// agent-aware, so reviewer agents whose binding lookup failed for the
/// canonical-rooted cwd fell through to the same leniency — and the
/// resulting `git checkout <sha>` / `git checkout -b tmp_review` calls
/// polluted canonical's branch list with stale `pr*_head` / `tmp*` /
/// `review/*` refs. Operator surfaced the recurrence on PR #805 morning +
/// PR #850 afternoon. Fix: route agents to either `repo action=checkout
/// bind=true` (gives them a properly-bound worktree) or `gh pr diff/view`
/// (read-only). Operator path unchanged.
pub(crate) fn is_agent_checkout_in_canonical(is_agent_caller: bool, canonical_cwd: bool) -> bool {
    is_agent_caller && canonical_cwd
}

/// #778 Option 3: shim leniency for canonical-rooted unbound worktrees. When
/// cwd is inside a worktree whose `.git` pointer resolves to a source repo
/// carrying a `[remote "origin"]` config entry (i.e. a canonical repo, not
/// the orphan workspace-placeholder daemon startup leaves), allow `git
/// checkout`/`git switch <branch>` as a Passthrough. Closes the
/// chicken-and-egg surfaced by validation canary 2026-05-14: `repo
/// action=checkout` provisions the worktree in detached-HEAD but doesn't
/// bind, so the natural follow-up `git switch <branch>` would otherwise need
/// a BYPASS. Narrow by design — `target_branch` must be a positional
/// argument (not a flag) and the worktree must be daemon-provisioned
/// canonical-rooted, so the surface is limited to navigation within an
/// already-materialized worktree.
pub(crate) fn is_canonical_unbound_checkout_leniency(
    target_branch: &str,
    canonical_cwd: bool,
) -> bool {
    !target_branch.is_empty() && !target_branch.starts_with('-') && canonical_cwd
}

/// Sprint 57 Wave 2 Track D: gh post-merge cleanup exemption. Trigger
/// requires ALL of:
///   - target is a protected ref (main / master)
///   - parent process is `gh` (signal that this invocation is from `gh pr
///     merge --delete-branch` post-merge local-state cleanup)
///   - we're in the agent-invoked path (AGENTIC_GIT_AGENT was set; bound
///     binding is the consequence of that)
///
/// Heuristic robustness: a non-gh parent (interactive shell, script, IDE)
/// reaches the cross-branch deny unchanged, preserving E4.5 protection for
/// the operator-typed case the rule was originally built for.
pub(crate) fn is_gh_post_merge_cleanup_checkout(target_branch: &str, parent_is_gh: bool) -> bool {
    is_protected_ref(target_branch) && parent_is_gh
}

// ── Parent-process detection (gh post-merge cleanup heuristic) ──────────

/// Sprint 57 Wave 2 Track D: detect that this `agentic-git` invocation
/// is a child of `gh`. Returns `true` only when AGENTIC_GIT_AGENT is
/// set (i.e. we're inside the agent-invoked path the cross-branch
/// fence guards) AND the parent process name is `gh`. Conservative
/// by design: any platform-specific lookup failure returns `false`,
/// letting the fence fire as it would have pre-Track-D rather than
/// silently weakening E4.5.
pub(crate) fn invocation_is_gh_post_merge() -> bool {
    // Operator-shell invocations don't have AGENTIC_GIT_AGENT set;
    // those already hit the early passthrough at the top of `main()`,
    // so the cross-branch fence never fires for them. Restricting the
    // exemption to AGENTIC_GIT_AGENT-set invocations keeps the
    // surface tight.
    if env_compat("AGENTIC_GIT_AGENT")
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
pub(crate) fn process_basename_is_gh(raw: &str) -> bool {
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
pub(crate) fn parent_process_name() -> Option<String> {
    let ppid = unsafe { libc::getppid() };
    let path = format!("/proc/{ppid}/comm");
    std::fs::read_to_string(&path).ok().map(|s| {
        s.trim_end_matches(['\n', '\r', '\0', ' '])
            .trim()
            .to_string()
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn parent_process_name() -> Option<String> {
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
pub(crate) fn parent_process_name() -> Option<String> {
    // Deliberately `None` on Windows. The original body reached for the
    // `sysinfo` crate, but it was never declared as a dependency — so this has
    // never compiled on Windows (the reason the advisory CI build was red).
    // `None` is the SAFE degrade: `invocation_is_gh_post_merge` treats a lookup
    // failure as "not a gh child" and lets the cross-branch fence fire exactly
    // as it would without this optimization (see its doc — conservative by
    // design, never weakens the fence). The gh-post-merge noise suppression
    // just doesn't apply on Windows until this is reimplemented (Toolhelp32 via
    // `windows-sys`, or `sysinfo` as a `cfg(windows)` dep) AND tested on a real
    // Windows host — still an unverified platform.
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(crate) fn parent_process_name() -> Option<String> {
    None
}

// ── #1463: init-heartbeat forensic capture ──────────────────────────────

/// Extract the `-m` / `--message` value from a `commit` argv. Supports
/// `-m x`, `-mx`, `--message x`, `--message=x`. Returns the first message.
pub(crate) fn extract_commit_message(args: &[String]) -> Option<&str> {
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
pub(crate) fn commit_is_init_heartbeat_argv(args: &[String]) -> bool {
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
pub(crate) fn parent_pid() -> i32 {
    unsafe { libc::getppid() }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn parent_pid() -> i32 {
    -1
}

/// Walk the process ancestry from this shim's parent up to `max` levels.
/// Each entry is `pid ppid comm args` (one ancestor). The immediate parent
/// is the process that invoked `git` — i.e. the backend CLI we want to pin.
/// Best-effort; empty on unsupported platforms or `ps` failure.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn process_ancestry(max: usize) -> Vec<String> {
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
pub(crate) fn process_ancestry(_max: usize) -> Vec<String> {
    Vec::new()
}

/// #2234 defect#2: is this argv a POSITIONAL-branch `checkout`/`switch` — the
/// canonical-HEAD-touching shape (`git checkout <branch>`, e.g. `origin/main`)?
/// A flag-only / empty target (`git checkout -b …` is still positional after the
/// flag, but a bare `-`/`--detach` lead arg is not a branch nav) is excluded.
/// Pure (no cwd / IO) so the gate is unit-testable.
pub(crate) fn is_positional_branch_checkout(args: &[String]) -> bool {
    matches!(
        args.first().map(|s| s.as_str()),
        Some("checkout") | Some("switch")
    ) && args
        .get(1)
        .is_some_and(|t| !t.is_empty() && !t.starts_with('-'))
}

/// #2234 fix B (pure decision — unit-testable without env/cwd/git). Should an
/// agent's `AGENTIC_GIT_BYPASS` op be DENIED for canonical-repo safety?
///
/// Deny iff ALL hold:
/// - `agent_present` — `AGENTIC_GIT_AGENT` set (a real fleet agent; daemon
///   internals never reach this shim and carry no instance name);
/// - NOT `escape` — the one-shot `AGENTIC_GIT_ALLOW_CANONICAL_MUTATE` override is
///   absent (deliberately a SEPARATE env from `AGENTIC_GIT_BYPASS`, which is what
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
pub(crate) fn deny_agent_canonical_bypass(
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
pub(crate) fn enforce_agent_canonical_bypass_deny(args: &[String]) {
    let agent = env_compat("AGENTIC_GIT_AGENT").unwrap_or_default();
    let escape = env_compat("AGENTIC_GIT_ALLOW_CANONICAL_MUTATE").as_deref() == Ok("1");
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

// ── Arch14: cross-agent sibling read boundary ─────────────────────────

/// Walk the full `ancestors()` chain of `dir` to find the nearest
/// `.agend-managed` marker. Returns `(agent_name, managed_root)`.
pub(crate) fn resolve_managed_marker(dir: &Path) -> Option<(String, PathBuf)> {
    for ancestor in dir.ancestors() {
        if let Ok(content) = std::fs::read_to_string(ancestor.join(".agend-managed")) {
            let agent = content
                .lines()
                .find_map(|line| line.strip_prefix("agent="))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if let Some(a) = agent {
                return Some((a, ancestor.to_path_buf()));
            }
        }
    }
    None
}

/// Detect when the effective read target is another agent's daemon-managed
/// same-source worktree. Canonicalizes the target first so symlink aliases
/// are resolved. Uses the canonical target (not the marker root) for the
/// same-source commondir check, so a scratch repo nested under a managed
/// worktree is correctly identified as foreign.
pub(crate) fn detect_cross_agent_sibling_target(
    agent: &str,
    binding: &Binding,
    target_dir: &Path,
) -> Option<String> {
    let canonical = std::fs::canonicalize(target_dir)
        .unwrap_or_else(|_| target_dir.to_path_buf());
    let (target_agent, _managed_root) = resolve_managed_marker(&canonical)?;
    if target_agent == agent {
        return None;
    }
    let caller_wt = binding.worktree.as_deref()?;
    if paths_are_foreign(&canonical, Path::new(caller_wt)) {
        return None;
    }
    Some(target_agent)
}
