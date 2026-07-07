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
pub(crate) fn push_protected_violation(
    args: &[String],
    protected: &[String],
    push_default_matching: bool,
) -> Option<String> {
    if let Some(flag) = args.iter().skip(1).find(|a| super::is_bulk_push_flag(a)) {
        return Some(format!(
            "`{flag}` pushes ALL local refs (including protected ones) — push an explicit \
             refspec of your own task branch instead, not all refs at once"
        ));
    }
    for dest in super::push_dest_refs(args) {
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
    if push_default_matching
        && !super::has_explicit_refspec(args)
        && !super::is_tags_only_push(args)
    {
        return Some(
            "push.default=matching with no explicit refspec would push every same-named \
             branch (including a local protected ref) — set push.default=current/simple, or \
             push an explicit refspec of your own task branch"
                .to_string(),
        );
    }
    None
}

/// #t-…78445-1 (fleet-safety, dev3 #2673 review): a BARE force-push
/// (`--force`/`-f`/`+refspec`) to a NON-protected branch can silently overwrite commits
/// already on the remote branch — another agent's or session's work, or a wrong-based
/// branch (#2673 state-3's residual edge). `push_protected_violation` above intentionally
/// ignores force ("HOW not WHAT") and only guards protected refs, so it never catches this.
/// Require `--force-with-lease` instead: it refuses the push if the remote moved since the
/// pusher's last fetch, which REMOVES the footgun while KEEPING the legitimate
/// rebase-then-force workflow (footgun-removal, not capability-removal). Returns an
/// actionable deny reason (with an executable retry sequence), or `None` to allow.
///
/// Runs AFTER `push_protected_violation` in the push arm, so a force-push to a protected ref
/// is already denied (with `deny_protected_ref`) before reaching here. Deletions don't
/// overwrite history → exempt. NB: git makes a trailing `--force` override a
/// `--force-with-lease`, so ANY bare `--force`/`-f`/`+refspec` present = unconditional and is
/// denied even if a lease flag co-occurs.
pub(crate) fn push_force_without_lease_violation(args: &[String]) -> Option<String> {
    if !push_has_bare_force(args) || is_delete_push(args) {
        return None;
    }
    let (remote, branch) = force_push_target(args);
    let seq = match (remote.as_deref(), branch.as_deref()) {
        (Some(r), Some(b)) => {
            format!("git fetch {r} {b} && git push --force-with-lease {r} {b}")
        }
        _ => "git fetch <remote> <branch> && git push --force-with-lease <remote> <branch>"
            .to_string(),
    };
    Some(format!(
        "bare force-push denied: `--force` / `-f` / a `+refspec` can SILENTLY OVERWRITE \
         commits already on the remote branch (another agent's or session's work). Re-run \
         with a lease — it refuses the push if the remote moved since your last fetch, so you \
         cannot clobber commits you have not seen:\n  {seq}\nProtected refs stay hard-denied \
         regardless; this guards feature branches (t-…78445-1). If you genuinely intend to \
         discard remote commits, fetch first so the lease baseline is current."
    ))
}

/// A push carries a BARE (unconditional) force: `--force`, a single-dash short cluster
/// containing `f` (`-f`, `-fu`, `-uf`, …), or a `+`-prefixed refspec positional.
/// `--force-with-lease[=…]` / `--force-if-includes` are the SAFE forms and are NOT bare
/// force (they gate on the remote not having moved). `--force` is an exact match — git
/// rejects an ambiguous abbreviation like `--forc` (shared with `--force-with-lease`), so no
/// abbreviation handling is needed for the long form.
fn push_has_bare_force(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| {
        a == "--force"
            || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
            || a.starts_with('+')
    })
}

/// A PURE deletion push removes refs rather than overwriting history → exempt from the force
/// gate. The `--delete` flag (or its short `-d` cluster) makes EVERY named ref a deletion, so
/// it is unconditionally exempt. Otherwise a push is a pure deletion only when it names at
/// least one refspec and EVERY refspec is a `:<dest>` deletion.
///
/// #t-…78445-1 F1 (dev2 review, CONFIRMED bypass): this MUST NOT use any-arg detection. A
/// mixed `git push --force origin :del real` deletes `:del` AND force-overwrites `real`; an
/// any-arg exemption would let `real`'s force through. Exempting only when ALL refspecs are
/// deletions keeps a lone `:del` (or `--delete`) exempt while a mixed push stays gated.
fn is_delete_push(args: &[String]) -> bool {
    // `--delete` / `-d` → all named refs are deletions.
    if args
        .iter()
        .skip(1)
        .any(|a| a == "--delete" || (a.starts_with('-') && !a.starts_with("--") && a.contains('d')))
    {
        return true;
    }
    // Otherwise inspect the refspec positionals. The first positional is the remote; a push
    // is a pure deletion only when it names >=1 refspec and EVERY refspec is a `:<dest>`
    // deletion (after an optional `+`). A single non-delete refspec means the force applies to
    // a real overwrite and must stay gated.
    let non_flag: Vec<&str> = args
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .map(|s| s.as_str())
        .collect();
    non_flag.len() > 1
        && non_flag[1..]
            .iter()
            .all(|&a| a.strip_prefix('+').unwrap_or(a).starts_with(':'))
}

/// Best-effort `(remote, branch)` for the deny message's retry sequence. Only reported when
/// BOTH are explicitly on the command line — i.e. >=2 positionals (a remote followed by a
/// refspec). With fewer we cannot tell a remote from a refspec (dev2 NIT: a lone `+branch`
/// was mistaken for the remote, yielding `git fetch mybranch mybranch`), so we return
/// `(None, None)` and the caller falls back to the generic `<remote> <branch>` template.
fn force_push_target(args: &[String]) -> (Option<String>, Option<String>) {
    let positionals: Vec<&String> = args
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    if positionals.len() < 2 {
        return (None, None);
    }
    let remote = Some(positionals[0].trim_start_matches('+').to_string());
    let branch = positionals.get(1).map(|a| {
        let a = a.strip_prefix('+').unwrap_or(a);
        let dest = a.rsplit(':').next().unwrap_or(a);
        dest.strip_prefix("refs/heads/").unwrap_or(dest).to_string()
    });
    (remote, branch)
}
