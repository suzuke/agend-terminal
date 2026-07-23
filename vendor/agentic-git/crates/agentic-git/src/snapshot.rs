//! P2 recovery layer (agentic-git issue #4): a pre-destructive-op safety
//! net, rebuilt on plain git plumbing (jj's auto-snapshot idea, without
//! leaving git). Implements the issue's FINAL contract literally — base
//! design + Δa **v5** (push guard, re-scoped to two cheap layers + an
//! honest boundary) + Δb (HEAD-less commit-tree) + Δc (default-off
//! activation, `run` opts in) + Δd (forced snapshot dates, self-prune
//! immunity) + Δe (content-level semantics) — where later review rounds
//! superseded earlier ones, only the final spelling is implemented here.
//!
//! Positioning (hard constraint, carried from the issue): this is a safety
//! net, not a gate. Snapshot **creation** fails open (never blocks the git
//! op it's protecting against); the push **guard** stays fail-closed
//! (prevention, mirrors the existing trust-root/protected-ref denylists).

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{
    env_compat, resolve_real_git, subcommand_index, submodule_op_is_write,
    write_git_event_typed,
};

/// Amortized + manual prune default TTL (Δd: committer-date based).
pub(crate) const DEFAULT_TTL_SECS: u64 = 7 * 24 * 60 * 60;

const SNAPSHOT_REF_PREFIX: &str = "refs/agentic-git/snapshots/";

fn git_bypass(git: &str) -> Command {
    let mut cmd = Command::new(git);
    // Mirrors the existing pattern (`cleanup_init_pile_pre_push` et al.): all
    // child git calls use the resolved real git + BOTH bypass env twins so
    // they never re-enter this shim.
    cmd.env("AGENTIC_GIT_BYPASS", "1").env("AGEND_GIT_BYPASS", "1");
    cmd
}

// ── Destructive-op classification (v1 list) ─────────────────────────────

/// Resolve the destructive-op slug for a raw shim `args` vector, or `None`
/// if this invocation is not in the v1 destructive set. Reuses
/// `subcommand_index` (main.rs) to find the real subcommand past any
/// leading global options — the same approach `strip_target_overrides`
/// takes — so e.g. `-C <dir> reset --hard` classifies correctly, not just
/// the bare form.
pub(crate) fn destructive_op_slug(args: &[String]) -> Option<&'static str> {
    let idx = subcommand_index(args)?;
    let subcmd = args[idx].as_str();
    let rest = &args[idx + 1..];
    match subcmd {
        // Mid-op manglers — the v1 table's set (unconditional; these can
        // leave half-applied state regardless of flags).
        "merge" => Some("merge"),
        "rebase" => Some("rebase"),
        "pull" => Some("pull"),
        "cherry-pick" => Some("cherry-pick"),
        "revert" => Some("revert"),
        "am" => Some("am"),
        // Working-tree discard forms.
        "reset" if reset_is_destructive(rest) => Some("reset"),
        "clean" if clean_is_forced(rest) => Some("clean"),
        "stash" if stash_is_destructive(rest) => Some("stash"),
        "checkout" if checkout_is_destructive(rest) => Some("checkout"),
        "restore" if restore_touches_worktree(rest) => Some("restore"),
        "switch" if switch_is_destructive(rest) => Some("switch"),
        "submodule" if submodule_op_is_write(rest) => Some("submodule"),
        _ => None,
    }
}

fn reset_is_destructive(rest: &[String]) -> bool {
    rest.iter()
        .any(|a| matches!(a.as_str(), "--hard" | "--merge" | "--keep"))
}

/// `clean` with `-f` in any combination (`-fd`, `-fdx`, …) or `--force`.
fn clean_is_forced(rest: &[String]) -> bool {
    rest.iter().any(|a| {
        a == "--force" || (a.starts_with('-') && !a.starts_with("--") && a[1..].contains('f'))
    })
}

/// `stash drop|clear` — the destructive stash forms (drops working-tree-
/// recoverable state); `push`/`pop`/`list`/`show`/apply are not.
fn stash_is_destructive(rest: &[String]) -> bool {
    matches!(rest.first().map(String::as_str), Some("drop") | Some("clear"))
}

/// `checkout` has a large worktree-overwriting surface (`-- <paths>`, `-f`,
/// `<path>`, `<tree-ish> <path>`, `-p`, `--ours`/`--theirs <path>`,
/// `--pathspec-from-file=<f>`, …). Two rounds of impl review showed that
/// enumerating the *destructive* flag set is a losing game (each round found
/// another spelling: `checkout HEAD f.txt`, then `--pathspec-from-file`). So —
/// exactly as the push guard was re-scoped — checkout uses the **fail-safe
/// default**: a checkout that reaches this hook is treated as destructive
/// UNLESS it is a pure branch-creation (`-b`/`-B`/`--orphan`, no force). For a
/// recovery net the asymmetry is deliberate: an over-snapshot is a wasted ref
/// (pruned in 7d, and only ever taken on a DIRTY tree via skip-when-clean),
/// whereas an under-snapshot is lost work. Cross-branch `checkout <branch>` is
/// denied upstream and never reaches here; same-branch is a harmless no-op.
fn checkout_is_destructive(rest: &[String]) -> bool {
    // `git checkout` with no arguments errors — nothing runs, nothing to save.
    if rest.is_empty() {
        return false;
    }
    if rest.iter().any(|a| a == "--" || a == "-f" || a == "--force") {
        return true;
    }
    // Pure branch creation does not discard worktree content (a dirty tree is
    // carried onto the new branch). Everything else that reaches the hook
    // touches the worktree — snapshot it.
    let branch_create = rest
        .iter()
        .any(|a| a == "-b" || a == "-B" || a == "--orphan");
    !branch_create
}

/// `restore` overwrites the WORKING TREE unless it is a pure `--staged`
/// (index-only) form. `--staged --worktree` together still touch the
/// worktree, so only a `--staged` WITHOUT `--worktree` is exempt.
fn restore_touches_worktree(rest: &[String]) -> bool {
    let staged = rest.iter().any(|a| a == "--staged" || a == "-S");
    let worktree = rest.iter().any(|a| a == "--worktree" || a == "-W");
    !staged || worktree
}

/// `switch -f`/`--force`/`--discard-changes` discards uncommitted worktree
/// changes. Verified empirically: `git switch --discard-changes <current>`
/// resets the tree to the branch tip even when already on that branch — a
/// reachable working-tree-discard that a cross-branch deny does NOT catch (a
/// bound agent may run it against its OWN branch). Plain `switch <branch>`
/// that would clobber changes is refused by git (not destructive), so only
/// the force/discard forms qualify. (Self-review addendum: the v1 op table
/// said "checkout/restore" but switch's discard form is the same worktree
/// hazard.)
fn switch_is_destructive(rest: &[String]) -> bool {
    rest.iter()
        .any(|a| matches!(a.as_str(), "-f" | "--force" | "--discard-changes"))
}

// ── Activation (Δc) ─────────────────────────────────────────────────────

/// Δc: default OFF in raw shim mode; `run` (cli.rs) injects `=1` into its
/// child's env unless the user already set either env name, so `=0|off`
/// still force-disables inside a run session. Legacy twin
/// `AGEND_GIT_SNAPSHOTS` resolved automatically via `env_compat`.
fn snapshots_enabled() -> bool {
    env_compat("AGENTIC_GIT_SNAPSHOTS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

// ── Snapshot creation (fail-open) ───────────────────────────────────────

/// Entry point invoked from the shim dispatch BEFORE a destructive op
/// executes. Fails open + loud on any internal error — this must NEVER
/// block the op it is protecting against.
pub(crate) fn maybe_snapshot(args: &[String], target_dir: &Path, home: &str, agent: &str) {
    // Cheap check FIRST — a non-destructive op costs nothing beyond a pure
    // argv scan (perf note in the issue).
    let Some(op_slug) = destructive_op_slug(args) else {
        return;
    };
    if !snapshots_enabled() {
        return;
    }
    let git = resolve_real_git();
    if is_clean(&git, target_dir) {
        // Skip-when-clean fast path: nothing uncommitted to lose.
        return;
    }
    match create_snapshot(&git, target_dir, agent, op_slug) {
        Ok(refname) => {
            // Amortized prune (Δd): piggyback on this rare event, exclude
            // the ref just created.
            let _ = prune_refs(&git, target_dir, DEFAULT_TTL_SECS, Some(refname.as_str()));
        }
        Err(reason) => {
            eprintln!(
                "agentic-git: warning \u{2014} pre-op snapshot FAILED ({reason}); proceeding \
                 without a safety net"
            );
            // Best-effort telemetry: only when we have a home to write it to.
            // The solo-opt-in path (no AGENTIC_GIT_HOME) still snapshots — it
            // just can't journal a failure. An empty home would otherwise
            // land `fleet_events.jsonl` in the user's cwd.
            if !home.is_empty() {
                write_git_event_typed(home, agent, op_slug, "snapshot_failed", None, Some(&reason));
            }
        }
    }
}

fn is_clean(git: &str, dir: &Path) -> bool {
    let Ok(o) = git_bypass(git)
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain"])
        .output()
    else {
        return false;
    };
    if !o.status.success() {
        return false;
    }
    // Review finding: a `run`-provisioned worktree carries an untracked
    // `.agend-managed` marker (disk contract). Left counted, it makes EVERY
    // fresh session's tree look dirty, so skip-when-clean never fires and a
    // clean-tree destructive op still snapshots. Treat a tree whose ONLY
    // untracked entry is that marker as clean — any real change still snapshots.
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .all(|l| l.trim().is_empty() || l == "?? .agend-managed")
}

fn who_for(agent: &str) -> &str {
    if agent.is_empty() {
        "noagent"
    } else {
        agent
    }
}

fn nanos_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// The current time in git's "ident date" grammar (`@<unix-epoch> <tz>`),
/// the format `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` require — NOT the
/// free-form `--date` approxidate parser, which is the only place the
/// literal `now` is special-cased. Always UTC (`+0000`) — the snapshot's
/// displayed tz doesn't matter, only that its instant is genuinely now.
fn now_ident_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("@{secs} +0000")
}

fn create_snapshot(git: &str, dir: &Path, agent: &str, op_slug: &str) -> Result<String, String> {
    let who = who_for(agent);
    let tmp_index = std::env::temp_dir().join(format!(
        "agentic-git-snapshot-index-{}-{}",
        std::process::id(),
        nanos_now()
    ));
    let result = create_snapshot_inner(git, dir, who, op_slug, &tmp_index);
    let _ = std::fs::remove_file(&tmp_index);
    result
}

fn create_snapshot_inner(
    git: &str,
    dir: &Path,
    who: &str,
    op_slug: &str,
    tmp_index: &Path,
) -> Result<String, String> {
    // 1. `add -A` into a private temp index — never touches the real index.
    let add = git_bypass(git)
        .arg("-C")
        .arg(dir)
        .env("GIT_INDEX_FILE", tmp_index)
        .args(["add", "-A"])
        .output()
        .map_err(|e| format!("spawn `add -A`: {e}"))?;
    if !add.status.success() {
        return Err(format!(
            "`add -A` failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        ));
    }

    // 2. `write-tree` from that same temp index.
    let wt = git_bypass(git)
        .arg("-C")
        .arg(dir)
        .env("GIT_INDEX_FILE", tmp_index)
        .args(["write-tree"])
        .output()
        .map_err(|e| format!("spawn `write-tree`: {e}"))?;
    if !wt.status.success() {
        return Err(format!(
            "`write-tree` failed: {}",
            String::from_utf8_lossy(&wt.stderr).trim()
        ));
    }
    let tree = String::from_utf8_lossy(&wt.stdout).trim().to_string();
    if tree.is_empty() {
        return Err("`write-tree` produced no tree SHA".to_string());
    }

    // 3. Δb: omit `-p HEAD` when HEAD is unborn (parented form fatals on a
    //    first-commit repo — exactly when the net matters most).
    let head_sha = git_bypass(git)
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "-q", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    // Δd: force author/committer date to NOW, overriding any ambient
    // GIT_AUTHOR_DATE/GIT_COMMITTER_DATE (our OWN test fixtures set such
    // vars) so a brand-new snapshot never looks pre-expired to the
    // amortized prune below. `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` use
    // git's strict "ident date" grammar, NOT the free-form `--date`
    // approxidate parser — the literal `now` is rejected there
    // (`fatal: invalid date format: now`), so this spells it out as
    // `@<unix-epoch> +0000` (UTC), which that grammar does accept.
    let mut commit_cmd = git_bypass(git);
    commit_cmd
        .arg("-C")
        .arg(dir)
        // Force a self-contained identity: `commit-tree` REQUIRES an
        // author+committer, and a fresh CI runner / container / un-configured
        // machine has no git `user.name`/`user.email` — there, the snapshot's
        // commit-tree would silently fail (fail-open → NO safety net) exactly
        // where the recovery layer is supposed to work. A fixed identity makes
        // snapshots environment-independent. (Dates are forced below.)
        .env("GIT_AUTHOR_NAME", "agentic-git")
        .env("GIT_AUTHOR_EMAIL", "agentic-git@localhost")
        .env("GIT_COMMITTER_NAME", "agentic-git")
        .env("GIT_COMMITTER_EMAIL", "agentic-git@localhost")
        .env("GIT_AUTHOR_DATE", now_ident_date())
        .env("GIT_COMMITTER_DATE", now_ident_date())
        .arg("commit-tree")
        .arg(&tree);
    if let Some(ref parent) = head_sha {
        commit_cmd.arg("-p").arg(parent);
    }
    commit_cmd.args(["-m", &format!("{op_slug} snapshot")]);
    let commit_out = commit_cmd
        .output()
        .map_err(|e| format!("spawn `commit-tree`: {e}"))?;
    if !commit_out.status.success() {
        return Err(format!(
            "`commit-tree` failed: {}",
            String::from_utf8_lossy(&commit_out.stderr).trim()
        ));
    }
    let commit_sha = String::from_utf8_lossy(&commit_out.stdout).trim().to_string();
    if commit_sha.is_empty() {
        return Err("`commit-tree` produced no commit SHA".to_string());
    }

    // 4. `update-ref` into a unique, ref-name-safe slot.
    let utc_ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let refname = unique_ref_name(git, dir, who, &utc_ts, op_slug)
        .ok_or_else(|| "could not find a free snapshot ref slot".to_string())?;
    let update = git_bypass(git)
        .arg("-C")
        .arg(dir)
        .args(["update-ref", &refname, &commit_sha])
        .output()
        .map_err(|e| format!("spawn `update-ref`: {e}"))?;
    if !update.status.success() {
        return Err(format!(
            "`update-ref` failed: {}",
            String::from_utf8_lossy(&update.stderr).trim()
        ));
    }
    Ok(refname)
}

/// `<who>` is ref-name-safe by construction (agent name already
/// charset-validated, or the literal `noagent`); `<seq>` disambiguates the
/// rare same-second collision.
fn snapshot_ref_name(who: &str, utc_ts: &str, seq: u64, op_slug: &str) -> String {
    format!("{SNAPSHOT_REF_PREFIX}{who}/{utc_ts}-{seq}-{op_slug}")
}

fn ref_exists(git: &str, dir: &Path, refname: &str) -> bool {
    git_bypass(git)
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "--quiet", refname])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn unique_ref_name(git: &str, dir: &Path, who: &str, utc_ts: &str, op_slug: &str) -> Option<String> {
    for seq in 0..1000u64 {
        let candidate = snapshot_ref_name(who, utc_ts, seq, op_slug);
        if !ref_exists(git, dir, &candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Parse `who`/`op` back out of a snapshot ref's own name (authoritative —
/// we control the format). `None` if `refname` isn't one of ours.
fn parse_snapshot_ref(refname: &str) -> Option<(String, String)> {
    let rest = refname.strip_prefix(SNAPSHOT_REF_PREFIX)?;
    let (who, tail) = rest.split_once('/')?;
    // tail = "<utc-ts>-<seq>-<op-slug>"; op-slug may itself contain dashes
    // (`cherry-pick`), so split only the first two.
    let mut parts = tail.splitn(3, '-');
    let _ts = parts.next()?;
    let _seq = parts.next()?;
    let op = parts.next()?;
    Some((who.to_string(), op.to_string()))
}

/// The `<seq>` field of a snapshot ref (the monotonic same-second
/// disambiguator), or 0 if the name isn't one of ours / is malformed. Used
/// only as a tiebreak when ordering by the second-granular committer date.
fn snapshot_seq(refname: &str) -> u64 {
    let Some(rest) = refname.strip_prefix(SNAPSHOT_REF_PREFIX) else {
        return 0;
    };
    let Some((_who, tail)) = rest.split_once('/') else {
        return 0;
    };
    let mut parts = tail.splitn(3, '-');
    let _ts = parts.next();
    parts.next().and_then(|s| s.parse().ok()).unwrap_or(0)
}

// ── Prune (lazy, no daemon) ──────────────────────────────────────────────

/// Delete snapshot refs under `refs/agentic-git/` older than `ttl_secs`
/// (committer-date based), excluding `exclude_ref` if given (Δd: belt and
/// suspenders against the amortized pass pruning the ref it just created).
/// Best-effort: any spawn/parse failure for the LISTING is a hard `Err`
/// (nothing to prune, nothing lost); a single ref's delete failing is
/// silently skipped (not reported as pruned) rather than aborting the pass.
pub(crate) fn prune_refs(
    git: &str,
    dir: &Path,
    ttl_secs: u64,
    exclude_ref: Option<&str>,
) -> Result<Vec<String>, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let out = git_bypass(git)
        .arg("-C")
        .arg(dir)
        .args([
            "for-each-ref",
            "--format=%(committerdate:unix) %(refname)",
            "refs/agentic-git/",
        ])
        .output()
        .map_err(|e| format!("spawn `for-each-ref`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`for-each-ref` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut pruned = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.splitn(2, ' ');
        let (Some(ts_str), Some(refname)) = (it.next(), it.next()) else {
            continue;
        };
        if exclude_ref == Some(refname) {
            continue;
        }
        let Ok(ts) = ts_str.trim().parse::<i64>() else {
            continue;
        };
        let age = now.saturating_sub(ts.max(0) as u64);
        if age <= ttl_secs {
            continue;
        }
        let deleted = git_bypass(git)
            .arg("-C")
            .arg(dir)
            .args(["update-ref", "-d", refname])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if deleted {
            pruned.push(refname.to_string());
        }
    }
    Ok(pruned)
}

// ── CLI surface (list / prune) ──────────────────────────────────────────

pub(crate) struct SnapshotRow {
    pub(crate) refname: String,
    pub(crate) when: String,
    pub(crate) op: String,
    pub(crate) who: String,
    pub(crate) subject: String,
}

pub(crate) fn list_snapshots(git: &str, dir: &Path) -> Result<Vec<SnapshotRow>, String> {
    let out = git_bypass(git)
        .arg("-C")
        .arg(dir)
        .args([
            "for-each-ref",
            "--format=%(refname)%09%(committerdate:iso-strict)%09%(subject)",
            SNAPSHOT_REF_PREFIX,
        ])
        .output()
        .map_err(|e| format!("spawn `for-each-ref`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`for-each-ref` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut rows = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.splitn(3, '\t');
        let refname = parts.next().unwrap_or_default().to_string();
        let when = parts.next().unwrap_or_default().to_string();
        let subject = parts.next().unwrap_or_default().to_string();
        let (who, op) = parse_snapshot_ref(&refname).unwrap_or_default();
        rows.push(SnapshotRow {
            refname,
            when,
            op,
            who,
            subject,
        });
    }
    Ok(rows)
}

// ── Restore (one-command recovery — issue #4 P2 follow-up) ───────────────

/// Options for [`restore_snapshot`]. Hand-built (no clap) — mirrors the rest
/// of this crate's dependency discipline.
pub(crate) struct RestoreOpts<'a> {
    /// Explicit snapshot ref, or `None` to auto-resolve (the only one / the
    /// newest with `--yes`).
    pub(crate) target_ref: Option<&'a str>,
    /// Proceed with the newest when several snapshots exist (else the caller
    /// gets [`RestoreError::Ambiguous`] and must choose — we never guess).
    pub(crate) assume_yes: bool,
    /// Leave the restored paths staged (the classic `checkout <tree> -- .`
    /// side effect). Default false — a user recovering lost files does not
    /// expect them silently staged into the next commit.
    pub(crate) staged: bool,
    /// Agent name for the pre-restore safety snapshot's `who` slot.
    pub(crate) agent: &'a str,
}

pub(crate) struct RestoreOutcome {
    pub(crate) restored_from: String,
    pub(crate) when: String,
    pub(crate) op: String,
    /// The safety snapshot of the pre-restore state (undo target), taken only
    /// when the tree had uncommitted work to lose. `None` when it was clean.
    pub(crate) pre_restore_ref: Option<String>,
    pub(crate) paths_written: usize,
    pub(crate) staged: bool,
    /// The working tree already matched the snapshot — nothing was written.
    pub(crate) already_current: bool,
}

pub(crate) enum RestoreError {
    /// No snapshot refs exist at all.
    NoSnapshots,
    /// The ref is not under `refs/agentic-git/snapshots/` — restore only ever
    /// reads from our own snapshots, never an arbitrary branch/tag.
    NotOurRef(String),
    /// A well-formed snapshot ref that doesn't resolve.
    NoSuchSnapshot(String),
    /// Several snapshots and no explicit ref / `--yes`: refuse to guess.
    /// Carries the candidates (newest first) for the caller to display.
    Ambiguous(Vec<SnapshotRow>),
    /// The pre-restore safety snapshot failed — fail CLOSED. Unlike
    /// `maybe_snapshot` (which fails open so it never blocks the git op it is
    /// protecting), restore is OUR OWN command: there is no reason to overwrite
    /// unsaved work once the safety net is known to be down.
    PreRestoreFailed(String),
    /// A git plumbing step failed.
    Git(String),
}

/// Restore the working tree from a snapshot — the one-command form of the
/// documented `git checkout <snapshot-ref> -- .`. **Non-destructive**: writes
/// the snapshot's paths back, but never deletes files created after it. Takes
/// a pre-restore safety snapshot first (so the restore itself is undoable).
pub(crate) fn restore_snapshot(
    git: &str,
    dir: &Path,
    opts: &RestoreOpts,
) -> Result<RestoreOutcome, RestoreError> {
    let refname = resolve_restore_target(git, dir, opts)?;
    let (_who, op) = parse_snapshot_ref(&refname).unwrap_or_default();
    let when = ref_committerdate(git, dir, &refname);

    let changed = restore_changed_paths(git, dir, &refname).map_err(RestoreError::Git)?;
    if changed.is_empty() {
        return Ok(RestoreOutcome {
            restored_from: refname,
            when,
            op,
            pre_restore_ref: None,
            paths_written: 0,
            staged: opts.staged,
            already_current: true,
        });
    }

    // Safety net FIRST, fail-closed: restore overwrites the working tree, so
    // capture whatever uncommitted state exists before touching it. Skip only
    // when there is genuinely nothing uncommitted to lose.
    let pre_restore_ref = if is_clean(git, dir) {
        None
    } else {
        Some(create_snapshot(git, dir, opts.agent, "restore").map_err(RestoreError::PreRestoreFailed)?)
    };

    let pathspec = join_nul(&changed);
    checkout_paths(git, dir, &refname, &pathspec).map_err(RestoreError::Git)?;
    if !opts.staged {
        // Best-effort: the files ARE recovered; a failure to unstage is a
        // cosmetic index nuisance, not a reason to fail a successful recovery.
        if let Err(e) = unstage_paths(git, dir, &pathspec) {
            eprintln!(
                "agentic-git: warning \u{2014} restored files are staged; could not unstage ({e})"
            );
        }
    }

    Ok(RestoreOutcome {
        restored_from: refname,
        when,
        op,
        pre_restore_ref,
        paths_written: changed.len(),
        staged: opts.staged,
        already_current: false,
    })
}

fn resolve_restore_target(git: &str, dir: &Path, opts: &RestoreOpts) -> Result<String, RestoreError> {
    if let Some(r) = opts.target_ref {
        if !r.starts_with(SNAPSHOT_REF_PREFIX) {
            return Err(RestoreError::NotOurRef(r.to_string()));
        }
        if !ref_exists(git, dir, r) {
            return Err(RestoreError::NoSuchSnapshot(r.to_string()));
        }
        return Ok(r.to_string());
    }
    let mut rows = list_snapshots(git, dir).map_err(RestoreError::Git)?;
    // `iso-strict` timestamps sort lexicographically == chronologically, but
    // are only second-granular AND every snapshot's date is forced to `now`
    // (Δd) — so two ops in the same second tie. Break the tie by the ref's own
    // `<seq>` (monotonic within a `<who>/<ts>` bucket), so "newest" is the
    // genuinely-last snapshot, not whichever `for-each-ref` happened to list
    // last. (Cross-`who` same-second ties are truly concurrent — best-effort.)
    rows.sort_by(|a, b| {
        b.when
            .cmp(&a.when)
            .then_with(|| snapshot_seq(&b.refname).cmp(&snapshot_seq(&a.refname)))
    });
    match rows.len() {
        0 => Err(RestoreError::NoSnapshots),
        1 => Ok(rows.remove(0).refname),
        _ if opts.assume_yes => Ok(rows.remove(0).refname),
        _ => Err(RestoreError::Ambiguous(rows)),
    }
}

fn ref_committerdate(git: &str, dir: &Path, refname: &str) -> String {
    git_bypass(git)
        .arg("-C")
        .arg(dir)
        .args(["for-each-ref", "--format=%(committerdate:iso-strict)", refname])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Paths the restore will write: everything in the snapshot that differs from
/// the current working tree, EXCLUDING paths added since the snapshot (`A` —
/// newer files we must never delete). Because the snapshot committed untracked
/// files into its tree (`add -A`), a file lost from the working tree shows up
/// here as `D` and is recovered. `-z` keeps paths with spaces/newlines intact;
/// each path is kept as RAW BYTES (never lossily decoded) so a non-UTF-8
/// filename round-trips unchanged into the pathspec fed back to git.
fn restore_changed_paths(git: &str, dir: &Path, refname: &str) -> Result<Vec<Vec<u8>>, String> {
    let out = git_bypass(git)
        .arg("-C")
        .arg(dir)
        .args(["diff", "--name-status", "--no-renames", "-z", refname, "--", "."])
        .output()
        .map_err(|e| format!("spawn `diff`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`diff` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    // `-z --name-status --no-renames` emits `<status>\0<path>\0` per entry.
    let mut tokens = out.stdout.split(|&b| b == 0).filter(|t| !t.is_empty());
    let mut paths = Vec::new();
    while let (Some(status), Some(path)) = (tokens.next(), tokens.next()) {
        if status.first() == Some(&b'A') {
            continue;
        }
        paths.push(path.to_vec());
    }
    Ok(paths)
}

/// NUL-join pathspecs for `--pathspec-file-nul` (each entry terminated by NUL).
fn join_nul(paths: &[Vec<u8>]) -> Vec<u8> {
    let mut buf = Vec::new();
    for p in paths {
        buf.extend_from_slice(p);
        buf.push(0);
    }
    buf
}

/// Feed `pathspec_nul` (NUL-separated raw path bytes) to a git command on
/// stdin via `--pathspec-from-file=- --pathspec-file-nul`. This is the whole
/// reason restore does NOT pass paths as argv: a large snapshot (a generated
/// tree, a vendored dir — 10k+ files) would blow `ARG_MAX` (`E2BIG`) and
/// restore ZERO files, exactly when recovery matters most. stdin has no such
/// limit and one invocation handles any count. (fugu PR #10 review: repro'd
/// with 60k paths.)
fn run_pathspec_stdin(cmd: &mut Command, pathspec_nul: &[u8], label: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = cmd
        // Treat every fed pathspec as a LITERAL path, never magic. A perfectly
        // legal filename that begins with git's pathspec-magic sigil — e.g.
        // `:(glob)literal.txt` or `:/foo` — would otherwise be parsed as a
        // pathspec expression and fail to match, so restore could not recover a
        // file it had faithfully snapshotted. (fugu PR #10 review, repro'd.)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn `{label}`: {e}"))?;
    // Take + write + drop the handle so its EOF unblocks the child before we
    // wait (a held-open stdin would deadlock).
    child
        .stdin
        .take()
        .ok_or_else(|| format!("`{label}`: no stdin pipe"))?
        .write_all(pathspec_nul)
        .map_err(|e| format!("`{label}` write stdin: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("`{label}` wait: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`{label}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn checkout_paths(git: &str, dir: &Path, refname: &str, pathspec_nul: &[u8]) -> Result<(), String> {
    run_pathspec_stdin(
        git_bypass(git).arg("-C").arg(dir).args([
            "checkout",
            refname,
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]),
        pathspec_nul,
        "checkout",
    )
}

/// Undo `checkout`'s index side effect for exactly the restored paths, so the
/// recovery lands in the working tree UNSTAGED. Scoped to the restored paths
/// (not `reset -- .`) so any unrelated pre-existing staged content survives.
fn unstage_paths(git: &str, dir: &Path, pathspec_nul: &[u8]) -> Result<(), String> {
    run_pathspec_stdin(
        git_bypass(git).arg("-C").arg(dir).args([
            "reset",
            "-q",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]),
        pathspec_nul,
        "reset",
    )
}

// ── Push guard (Δa v5) ──────────────────────────────────────────────────

/// Δa v5 — two cheap layers, re-scoped after 4 rounds of "another refspec
/// spelling evades the parser" (see the issue's Design v5 for the full
/// rationale on why this does NOT attempt to be an exfiltration boundary —
/// it stops accidental/casual-explicit leaks, which is all a same-uid,
/// bypassable-by-construction shim can honestly claim). Returns an
/// actionable deny reason (tagged `SNAPSHOT_REF_PUSH`), or `None` to allow.
pub(crate) fn snapshot_push_violation(args: &[String], worktree: &str) -> Option<String> {
    // Layer 1 — text: any refspec side containing the substring
    // `agentic-git/` (after stripping a leading `+`), or `--mirror`.
    if push_carries_mirror(args) {
        return Some(
            "SNAPSHOT_REF_PUSH: `--mirror` pushes EVERY ref, including the agentic-git \
             snapshot namespace — push an explicit refspec of your own branch instead."
                .to_string(),
        );
    }
    for side in push_refspec_sides(args) {
        if side.contains("agentic-git/") {
            return Some(format!(
                "SNAPSHOT_REF_PUSH: refspec `{side}` references the agentic-git snapshot \
                 namespace (refs/agentic-git/…) — snapshot refs are a local safety net, not \
                 for a shared remote. Drop it from the refspec and retry."
            ));
        }
    }

    // Layer 2 — commit: any resolvable src whose commit tip IS a snapshot
    // (launder-into-branch, `^{}`/`~0` peels, a raw snapshot SHA typed
    // directly). Skipped entirely (never a false negative vs. layer 1) when
    // real git can't run at all, or when there are no snapshot refs yet.
    let git = resolve_real_git();
    let snapshot_tips = snapshot_tip_shas(&git, worktree);
    if snapshot_tips.is_empty() {
        return None;
    }
    for src in push_refspec_srcs(args) {
        if let Some(sha) = resolve_commit(&git, worktree, &src) {
            if snapshot_tips.contains(&sha) {
                return Some(format!(
                    "SNAPSHOT_REF_PUSH: push source `{src}` resolves to a snapshot commit \
                     ({sha}) — snapshot content is a local safety net, not for a shared \
                     remote. Push your own branch content instead."
                ));
            }
        }
    }
    None
}

fn push_carries_mirror(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| match a.strip_prefix("--") {
        Some(name) if !name.is_empty() => "mirror".starts_with(name),
        _ => false,
    })
}

/// Every refspec SIDE (both halves of a `src:dst` positional, or the bare
/// token when there is no `:`), leading `+` stripped. Flags are skipped.
fn push_refspec_sides(args: &[String]) -> Vec<String> {
    args.iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .flat_map(|a| {
            let a = a.strip_prefix('+').unwrap_or(a);
            a.split(':').map(str::to_string).collect::<Vec<_>>()
        })
        .collect()
}

/// The SRC half of every positional refspec (before the first `:`, or the
/// whole token when there is no `:`), leading `+` stripped, empty (a
/// delete-form `:dst`) skipped.
fn push_refspec_srcs(args: &[String]) -> Vec<String> {
    args.iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .filter_map(|a| {
            let a = a.strip_prefix('+').unwrap_or(a);
            let src = a.split(':').next().unwrap_or(a);
            if src.is_empty() {
                None
            } else {
                Some(src.to_string())
            }
        })
        .collect()
}

fn resolve_commit(git: &str, worktree: &str, src: &str) -> Option<String> {
    let spec = format!("{src}^{{commit}}");
    let out = git_bypass(git)
        .args(["-C", worktree, "rev-parse", "--verify", &spec])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

fn snapshot_tip_shas(git: &str, worktree: &str) -> HashSet<String> {
    let out = git_bypass(git)
        .args([
            "-C",
            worktree,
            "for-each-ref",
            "--format=%(objectname)",
            "refs/agentic-git/",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        _ => HashSet::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
