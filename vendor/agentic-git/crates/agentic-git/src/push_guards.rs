//! Push-path guards: #883 pre-push init-pile cleanup, #2379 trust-root
//! and protected-ref push denies, cross-branch and force-without-lease
//! violations, and the `PushArgv` parser they share.

use std::process::Command;

use super::*;

// ── #883 pre-push cleanup ───────────────────────────────────────────────

/// #883: drop empty `init` heartbeat commits between `<default-branch>..HEAD`
/// (base via `resolve_default_branch_base`, #2390) before the real `git push`
/// fires. Targets the operator-visible case (PR #882 saw 16 inits before the
/// real commit on mobile UI). The cleanup is a local soft-reset to that base
/// ONLY when EVERY commit
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
pub(crate) fn cleanup_init_pile_pre_push(worktree: &str) {
    // #2390: resolve the default-branch base (not hardcoded origin/main); soft —
    // an undeterminable/ambiguous base just no-ops this hygiene pass (never blocks
    // push, unlike the denylist which fails CLOSED on the same Err).
    let base = match resolve_default_branch_base(worktree) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("agentic-git: #883 pre-push cleanup skipped (default branch: {e})");
            return;
        }
    };
    let range = format!("{base}..HEAD");
    // List commits between <base>..HEAD with hash + subject.
    let log_out = match Command::new("git")
        .args(["log", &range, "--format=%H %s"])
        .current_dir(worktree)
        .env("AGENTIC_GIT_BYPASS", "1").env("AGEND_GIT_BYPASS", "1")
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            eprintln!(
                "agentic-git: #883 pre-push cleanup git log failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return;
        }
        Err(e) => {
            eprintln!("agentic-git: #883 pre-push cleanup git log spawn failed: {e}");
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
            .args(["reset", "--soft", &base])
            .current_dir(worktree)
            .env("AGENTIC_GIT_BYPASS", "1").env("AGEND_GIT_BYPASS", "1")
            .status();
        match reset {
            Ok(s) if s.success() => {
                eprintln!(
                    "agentic-git: #883 pre-push cleanup soft-reset {total} empty init commit(s)"
                );
            }
            Ok(s) => {
                eprintln!("agentic-git: #883 pre-push cleanup soft-reset exited with status {s:?}");
            }
            Err(e) => {
                eprintln!("agentic-git: #883 pre-push cleanup soft-reset spawn failed: {e}");
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
        .args(["-c", "core.abbrev=7", "rebase", "-i", &base])
        .current_dir(worktree)
        .env("AGENTIC_GIT_BYPASS", "1").env("AGEND_GIT_BYPASS", "1")
        .env("GIT_SEQUENCE_EDITOR", format!("sed -i.bak '{sed_script}'"))
        .status();
    match rebase {
        Ok(s) if s.success() => {
            eprintln!(
                "agentic-git: #883 pre-push cleanup dropped {cleaned} empty init commit(s) via rebase"
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
                .env("AGENTIC_GIT_BYPASS", "1").env("AGEND_GIT_BYPASS", "1")
                .status();
            eprintln!(
                "agentic-git: #883 pre-push cleanup rebase failed; aborted to leave worktree clean. \
                 Push proceeds with init pile intact ({cleaned} inits remain)."
            );
        }
    }
}

/// Heartbeat-subject whitelist. Mirrors `HEARTBEAT_NAMES` in
/// `src/mcp/handlers/dispatch_hook/mod.rs:951`. Inlined here because
/// the shim is intentionally self-contained (no library imports).
pub(crate) fn is_heartbeat_subject_shim(subject: &str) -> bool {
    matches!(subject, "init" | "initial")
}

/// Verify the commit at `hash` is a true empty heartbeat: empty body
/// (modulo `Agentic-*` trailer keys from the prepare-commit-msg hook) AND
/// zero file changes. Either check failing → not eligible for soft-
/// reset cleanup.
///
/// Mirrors the gates in `src/mcp/handlers/dispatch_hook/mod.rs:802-811`
/// plus `commit_body_is_empty` at line 1019. Inlined to keep the shim
/// self-contained.
pub(crate) fn commit_is_empty_heartbeat(worktree: &str, hash: &str) -> bool {
    // Body check — must be empty (apart from the `prepare-commit-msg`
    // hook's daemon trailers which are noise from this perspective).
    let body_out = match Command::new("git")
        .args(["log", "-1", "--format=%b", hash])
        .current_dir(worktree)
        .env("AGENTIC_GIT_BYPASS", "1").env("AGEND_GIT_BYPASS", "1")
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
        // Tolerate the four trailer keys the prepare-commit-msg hook
        // injects (`Agentic-Agent`, `Agentic-Task`, `Agentic-Branch`,
        // `Agentic-Issued-At`) — AND their legacy `Agend-*` twins: a
        // legacy agend-terminal fleet's hooks still write the old names,
        // and heartbeat detection must recognize both generations
        // (review-1 finding: name-only drift here broke the
        // zero-daemon-change adoption guarantee). Anything else means
        // there's a real commit message body → not a heartbeat.
        if trimmed.starts_with("Agentic-Agent:")
            || trimmed.starts_with("Agentic-Task:")
            || trimmed.starts_with("Agentic-Branch:")
            || trimmed.starts_with("Agentic-Issued-At:")
            || trimmed.starts_with("Agend-Agent:")
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
        .env("AGENTIC_GIT_BYPASS", "1").env("AGEND_GIT_BYPASS", "1")
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    diff_out.stdout.trim_ascii().is_empty()
}

// ── #2379 ③ denylist-core: trust-root push deny ─────────────────────────

/// Trust-root filenames an agent must never push into a shared repo. These live
/// in `$AGENTIC_GIT_HOME` (the config-integrity key, the fleet config, the append-only
/// audit logs); `.gitignore` blocks the common ones but `git add -f` bypasses it,
/// so this denylist is the push-time enforcement layer. Matched against a blob's
/// repo-relative BASENAME / extension (see `trust_root_basename_denied`).
pub(crate) const TRUST_ROOT_DENY_NAMES: &[&str] =
    &[".config-integrity-key", "policy.toml", "fleet.yaml"];

/// Whether a repo-relative blob path is a trust-root file: its BASENAME is an
/// exact trust-root name, or it is an audit log (`*.jsonl`). Pure — fed by the
/// impure range enumeration.
///
/// ⚠ Matches the repo-relative path's basename, NOT a `$AGENTIC_GIT_HOME` filesystem
/// prefix: a managed worktree lives UNDER `$AGENTIC_GIT_HOME/worktrees/<agent>/<branch>`
/// (binding.rs), so an abs-path-under-`$AGENTIC_GIT_HOME` test would match EVERY file in
/// the worktree and false-block every push. `git --name-only` yields repo-relative
/// paths, so basename matching is correct. Basename via `rsplit('/')` (NOT
/// `lstrip`/`trim_start_matches`, which would eat the leading dot of
/// `.config-integrity-key`). Basename-anywhere, so a sub-directory dodge
/// (`stash/fleet.yaml`) is still caught.
pub(crate) fn trust_root_basename_denied(repo_relative_path: &str) -> bool {
    let basename = repo_relative_path
        .rsplit('/')
        .next()
        .unwrap_or(repo_relative_path);
    TRUST_ROOT_DENY_NAMES.contains(&basename) || basename.ends_with(".jsonl")
}

/// #2390: resolve the push guards' range base (the remote's default branch)
/// instead of hardcoding `origin/main`. A non-main-default repo (master / trunk)
/// otherwise makes `origin/main..HEAD` un-resolvable → the denylist fails CLOSED
/// → every push is blocked (usability); and a dual-trunk repo whose true default
/// is master would scan the WRONG base and MISS a trust-root commit reachable
/// only from the real default (fail-OPEN). Shared by both `push_range_files`
/// (denylist, hard) and `cleanup_init_pile_pre_push` (hygiene, soft).
///
/// Fallback order (conservative — always a TRUNK, never the branch's own upstream):
/// 1. `git symbolic-ref --short refs/remotes/origin/HEAD` — the authoritative
///    default WHEN SET. NOT `rev-parse --abbrev-ref origin/HEAD`: that echoes the
///    literal `"origin/HEAD"` (looks like success) instead of failing when the ref
///    is unset — the common managed-worktree case that never ran
///    `git remote set-head`.
/// 2. Existence-probe the conventional trunks, but ONLY when EXACTLY ONE of
///    `origin/main` / `origin/master` exists. BOTH present + `origin/HEAD` unset is
///    ambiguous — we must NOT guess `main` (could scan the wrong base and MISS a
///    trust-root file only reachable from the true default = a fail-open
///    false-negative), so it falls to path 3 (#2662).
/// 3. Otherwise `Err` — the caller fails CLOSED (denylist) / no-ops (cleanup), so
///    an undeterminable OR ambiguous base stays safe. Remedy (resolves both):
///    `git remote set-head origin -a`.
pub(crate) fn resolve_default_branch_base(worktree: &str) -> Result<String, String> {
    // 1. origin/HEAD, only when explicitly set (symbolic-ref errors cleanly if not).
    if let Ok(o) = Command::new("git")
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .current_dir(worktree)
        .env("AGENTIC_GIT_BYPASS", "1").env("AGEND_GIT_BYPASS", "1")
        .output()
    {
        if o.status.success() {
            let head = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !head.is_empty() {
                return Ok(head);
            }
        }
    }
    // 2. Conventional-trunk existence probe — ONLY when EXACTLY ONE of
    //    origin/main / origin/master exists. BOTH → ambiguous → fail (don't guess:
    //    scanning the wrong `<trunk>..HEAD` could OMIT a trust-root commit reachable
    //    only from the true default = a fail-open false-negative, #2662).
    match (
        trunk_exists(worktree, "origin/main"),
        trunk_exists(worktree, "origin/master"),
    ) {
        (true, false) => return Ok("origin/main".to_string()),
        (false, true) => return Ok("origin/master".to_string()),
        (true, true) => {
            return Err(
                "ambiguous default branch: both origin/main and origin/master exist \
                 and origin/HEAD is unset — cannot safely pick the trunk"
                    .to_string(),
            )
        }
        (false, false) => {}
    }
    // 3. Undeterminable.
    Err(
        "cannot resolve the remote default branch: origin/HEAD is unset and neither \
         origin/main nor origin/master exists"
            .to_string(),
    )
}

/// Does `<rev>` resolve to a commit in `worktree`? `.output()` (not `.status()`)
/// so the `rev-parse` SHA never leaks onto the shim's stdout.
pub(crate) fn trunk_exists(worktree: &str, rev: &str) -> bool {
    Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .current_dir(worktree)
        .env("AGENTIC_GIT_BYPASS", "1").env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The repo-relative paths touched by any commit in the push range
/// (`<default-branch>..HEAD`, base via [`resolve_default_branch_base`] #2390) —
/// the union of `--name-only` across the range, so a trust-root blob added in an
/// intermediate commit is caught even if a later commit deletes it (the blob is
/// still in the pushed history). Runs real git in the worktree with
/// `AGENTIC_GIT_BYPASS=1` (mirrors `cleanup_init_pile_pre_push`'s established range
/// base). Returns `Err(msg)` when the base or range can't be computed (e.g.
/// undeterminable/ambiguous default branch) so the caller can fail CLOSED.
pub(crate) fn push_range_files(worktree: &str) -> Result<Vec<String>, String> {
    let range = format!("{}..HEAD", resolve_default_branch_base(worktree)?);
    let out = Command::new("git")
        .args(["log", "--name-only", "--pretty=format:", &range])
        .current_dir(worktree)
        .env("AGENTIC_GIT_BYPASS", "1").env("AGEND_GIT_BYPASS", "1")
        .output()
        .map_err(|e| format!("git log spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git log {range} failed: {}",
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
pub(crate) fn push_trust_root_denylist_violation(worktree: &str) -> Option<String> {
    match push_range_files(worktree) {
        Ok(files) => files
            .into_iter()
            .find(|p| trust_root_basename_denied(p))
            .map(|p| {
                format!(
                    "push range contains a trust-root file: `{p}` — $AGENTIC_GIT_HOME config / \
                     integrity key / audit logs must never be pushed into a shared repo. \
                     Drop it from the pushed commits (e.g. `git rm --cached {p}` then amend/rebase) \
                     and retry."
                )
            }),
        Err(e) => Some(format!(
            "could not verify the push against the trust-root denylist ({e}); refusing to \
             push (fail-closed). Set the remote's default branch so the range base resolves \
             (`git remote set-head origin -a`), then retry."
        )),
    }
}

// ── #2379 S3: protected-ref push deny (policy.toml override) ─────────────

/// The protected-ref set this invocation enforces: the hardcode floor
/// (`protected_refs::PROTECTED_REFS` — #2550 W4: was its own hand-copied
/// `HARDCODE_PROTECTED_REFS` const here, now the shared list an operator
/// override can only ADD to, tighten-only, never shrink) PLUS the operator's
/// `$AGENTIC_GIT_HOME/policy.toml` `protected_refs` override — but ONLY when the
/// file is present, HMAC-verified (hygiene, mirrors `read_binding`'s
/// sidecar), and parses. **Fail-closed, never less safe than the hardcode
/// floor:**
/// - missing policy.toml → hardcode floor only (the default),
/// - tampered / unsigned sidecar → hardcode floor only (override ignored),
/// - unparseable array → hardcode floor only (override ignored).
///
/// The override is additive-only, so the floor is always denied regardless. HMAC is
/// hygiene, NOT a security boundary (a same-uid agent could re-sign — #1653 ceiling).
pub(crate) fn load_protected_refs(home: &str) -> Vec<String> {
    let mut refs: Vec<String> = protected_refs::PROTECTED_REFS
        .iter()
        .map(|s| s.to_string())
        .collect();
    let path = PathBuf::from(home).join("policy.toml");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return refs; // missing → hardcode floor (the common default)
    };
    let tag =
        std::fs::read_to_string(PathBuf::from(home).join("policy.toml.sig")).unwrap_or_default();
    if !verify_sidecar(home, content.as_bytes(), &tag) {
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
pub(crate) fn parse_protected_refs(content: &str) -> Vec<String> {
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
pub(crate) fn extract_quoted(s: &str) -> Vec<String> {
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
pub(crate) fn push_dest_refs(args: &[String]) -> Vec<String> {
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
pub(crate) fn push_protected_violation(
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
pub(crate) fn is_tags_only_push(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| a == "--tags")
}

/// `--all` / `--mirror`, INCLUDING git's unambiguous long-option abbreviations (`--mir`,
/// `--al`, …). Errs toward deny (#2027 flag-form lesson): an ambiguous prefix (`--a`, `--m`)
/// also matches — git itself rejects those, so denying them costs nothing. `--tags` /
/// `--follow-tags` / force flags do NOT match (they don't push a protected branch).
pub(crate) fn is_bulk_push_flag(arg: &str) -> bool {
    match arg.strip_prefix("--") {
        Some(name) if !name.is_empty() => "all".starts_with(name) || "mirror".starts_with(name),
        _ => false,
    }
}

/// Whether the push names an EXPLICIT refspec (≥2 positionals after `push` — a remote AND a
/// refspec). With 0–1 positionals (no-arg, or just a remote) the ref is resolved from
/// `push.default` + the current branch instead.
pub(crate) fn has_explicit_refspec(args: &[String]) -> bool {
    args.iter().skip(1).filter(|a| !a.starts_with('-')).count() >= 2
}

/// True iff the worktree's effective `push.default` is the (deprecated) `matching` mode —
/// the one value where a no-refspec push writes MORE than the current branch. Unset → git's
/// built-in `simple` → false. Best-effort real-git read (read-only); any failure → false.
pub(crate) fn push_default_is_matching(worktree: &str) -> bool {
    let git = resolve_real_git();
    Command::new(&git)
        .env("AGENTIC_GIT_SHIM_DEPTH", (shim_depth() + 1).to_string())
        .args(["-C", worktree, "config", "--get", "push.default"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "matching")
        .unwrap_or(false)
}

/// The branch the worktree's HEAD currently points at (`symbolic-ref --short
/// HEAD`), or empty on detached HEAD / any read failure. Used so the push guard
/// can refuse an implicit push when the worktree has drifted off its binding.
pub(crate) fn current_branch_of(worktree: &str) -> String {
    let git = resolve_real_git();
    Command::new(&git)
        .env("AGENTIC_GIT_SHIM_DEPTH", (shim_depth() + 1).to_string())
        .args(["-C", worktree, "symbolic-ref", "--short", "HEAD"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub(crate) struct PushArgv {
    bulk: bool,
    delete: bool,
    force: bool,
    positionals: Vec<String>,
}

/// A push option that consumes the FOLLOWING token as its value (separate-arg
/// form) — missing one would let its value be mis-read as a remote/refspec. The
/// `<opt>=<val>` forms are self-contained (caught by the `-`-prefix skip).
pub(crate) fn push_opt_takes_value(opt: &str) -> bool {
    matches!(opt, "-o" | "--push-option" | "--receive-pack" | "--exec")
}

/// A value-taking SHORT option in ATTACHED (glued-value) form, e.g. `-o+force`
/// = `-o` with push-option value `+force`. Git consumes the REST of the `-o…`
/// token as the option's value, so it is NEVER a `-f` force cluster (reviewer4 F1:
/// `git push -o+force` reaches remote push-option handling, not force). `-o` is the
/// only short value-option `git push` accepts; the separate form `-o <val>` is
/// handled by `push_opt_takes_value`, and long options use `--opt[=val]` (a `--`
/// token, already excluded from force/positional classification). This token must
/// be consumed WHOLE so its value (which may contain `f` or a leading `+`) is never
/// read as force/delete.
pub(crate) fn is_attached_short_value_opt(arg: &str) -> bool {
    arg.len() > 2 && arg.starts_with("-o")
}

/// `--delete` / `-d` (or an unambiguous long-prefix of `--delete`; `--d`/`--de`
/// collide with `--dry-run`, so require ≥3 chars). This flag turns the push
/// positionals into refs to DELETE rather than refspecs.
pub(crate) fn is_delete_push_flag(arg: &str) -> bool {
    arg == "-d"
        || matches!(arg.strip_prefix("--"), Some(n) if n.len() >= 3 && "delete".starts_with(n))
}

/// A BARE (unconditional) force FLAG: `--force` (exact) or a single-dash short
/// cluster containing `f` (`-f`, `-uf`, `-fu`). `--force-with-lease[=…]` and
/// `--force-if-includes` are the SAFE lease forms — they refuse the push if the
/// remote moved — and are deliberately NOT matched. `--force` is an exact match:
/// git rejects the ambiguous abbreviation `--forc` (shared with
/// `--force-with-lease`), so no long-form abbreviation handling is needed. The
/// `+refspec` positional force form is detected in `parse_push_argv`, not here.
///
/// The `contains('f')` short-cluster test is safe ONLY because the caller
/// (`parse_push_argv`) first skips the attached push-option token `-o<val>`
/// (`is_attached_short_value_opt`) — otherwise `-o+force`'s value would misfire it
/// (reviewer4 F1). `-f` is the sole `f`-bearing short flag `git push` has, so any
/// remaining single-dash-with-`f` token is a genuine force cluster.
pub(crate) fn is_bare_force_flag(arg: &str) -> bool {
    arg == "--force" || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('f'))
}

/// Option-aware parse of a `push` argv into (bulk?, delete?, force?, positionals) — skips
/// flags and the values of value-taking options, so a positional is genuinely a
/// remote/ref (not e.g. a `-o` push-option value). fugu design review: a bare
/// positional-count heuristic mis-reads `--delete <ref>` and option values.
pub(crate) fn parse_push_argv(args: &[String]) -> PushArgv {
    let mut p = PushArgv {
        bulk: false,
        delete: false,
        force: false,
        positionals: Vec::new(),
    };
    let mut i = 1; // skip "push"
    while i < args.len() {
        let a = &args[i];
        if push_opt_takes_value(a) {
            i += 2; // the option AND its value
            continue;
        }
        // Attached glued-value short option (`-o<val>`, e.g. `-o+force`): ONE token —
        // skip it (never a force cluster; its value may contain `f`/`+`). reviewer4 F1.
        if is_attached_short_value_opt(a) {
            i += 1;
            continue;
        }
        if a.starts_with('-') && a != "-" {
            if is_bulk_push_flag(a) {
                p.bulk = true;
            }
            if is_delete_push_flag(a) {
                p.delete = true;
            }
            if is_bare_force_flag(a) {
                p.force = true;
            }
            i += 1;
            continue;
        }
        // A `+`-prefixed positional IS a force refspec (`+src:dst`) — bare force via
        // the refspec form rather than a `--force`/`-f` flag. Detected here (not in
        // `is_bare_force_flag`) so the option-aware skip above never mistakes a
        // `-o +val` push-option VALUE for a force refspec.
        if a.starts_with('+') {
            p.force = true;
        }
        p.positionals.push(a.clone());
        i += 1;
    }
    p
}

/// Strip a leading `refs/heads/` so a ref token compares as a branch name; a
/// `refs/tags/` prefix is left intact so tag dests stay visible (and exempt).
pub(crate) fn strip_branch_ref(r: &str) -> &str {
    r.strip_prefix("refs/heads/").unwrap_or(r)
}

pub(crate) fn cross_branch_push_msg(assigned: &str, dest: &str) -> String {
    format!(
        "cross-branch push — you are bound to '{assigned}', so you may only push that \
         branch, not '{dest}'. Force-pushing another agent's branch would clobber their \
         work; push `HEAD:refs/heads/{assigned}` (or open a PR)."
    )
}

/// A BOUND agent may push ONLY its OWN assigned branch — the symmetric partner
/// to the cross-branch checkout deny. Prevents one agent clobbering (or deleting)
/// another agent's branch on a shared remote, which the protected-ref guard
/// (main/master/policy only) does NOT catch. Runs ALONGSIDE the existing push
/// guards (fugu: coexist; protected gives better messages for main/master).
/// `None` = allow. Tag pushes (`--tags`, `refs/tags/…`, `tag <name>`) are exempt
/// from the BRANCH guard.
pub(crate) fn push_cross_branch_violation(
    args: &[String],
    assigned: &str,
    current_branch: &str,
    push_default_matching: bool,
) -> Option<String> {
    if assigned.is_empty() {
        return None; // unbound: every mutation is already denied upstream
    }
    let p = parse_push_argv(args);
    if p.bulk {
        return Some(format!(
            "`--all`/`--mirror` pushes EVERY local branch, not just your assigned \
             '{assigned}' — push an explicit refspec of your own branch"
        ));
    }
    if p.delete {
        // positionals = [remote?] + refs-to-delete; a remote is present only with
        // ≥2 positionals (with 1 it IS the ref to delete — never skip it, fugu #d).
        let refs: &[String] = if p.positionals.len() >= 2 {
            &p.positionals[1..]
        } else {
            &p.positionals
        };
        if let Some(r) = refs.first() {
            let name = strip_branch_ref(r);
            return Some(format!(
                "cross-branch delete — bound to '{assigned}'; deleting '{name}' through the \
                 shim is refused ({}). Branch lifecycle is the orchestrator's job.",
                if name == assigned {
                    "your own task branch"
                } else {
                    "another agent's branch"
                }
            ));
        }
        return None; // `--delete` with no ref → git errors on its own
    }
    // Normal mode: first positional (if any) is the remote; the rest are refspecs.
    let refspecs: &[String] = p.positionals.get(1..).unwrap_or(&[]);
    if refspecs.is_empty() {
        // Implicit push — dest inferred from the current branch + push.default.
        if current_branch != assigned {
            return Some(format!(
                "your worktree HEAD is on '{current_branch}', not your assigned '{assigned}' \
                 — an implicit push's destination is ambiguous; push \
                 `HEAD:refs/heads/{assigned}` explicitly"
            ));
        }
        if push_default_matching {
            return Some(
                "push.default=matching with no refspec could push branches other than your \
                 assigned one — push an explicit refspec of your own branch"
                    .to_string(),
            );
        }
        return None; // current == assigned, non-matching → pushes only assigned
    }
    let mut idx = 0;
    while idx < refspecs.len() {
        let rs = refspecs[idx].strip_prefix('+').unwrap_or(refspecs[idx].as_str());
        if rs == "tag" {
            idx += 2; // `tag <name>` — a tag push, not a branch (skip both tokens)
            continue;
        }
        let dest: &str = match rs.split_once(':') {
            None => {
                // single ref: pushes branch <ref> (or HEAD → the current branch)
                if rs == "HEAD" {
                    current_branch
                } else {
                    strip_branch_ref(rs)
                }
            }
            Some((_src, dst)) => {
                if dst == "HEAD" {
                    // pushing to the remote's HEAD is not your assigned branch
                    return Some(cross_branch_push_msg(assigned, "HEAD"));
                }
                strip_branch_ref(dst)
            }
        };
        if dest.starts_with("refs/tags/") {
            idx += 1;
            continue; // tag dest — not a branch
        }
        if dest.contains('*') {
            return Some(format!(
                "wildcard refspec dest `{dest}` could write another agent's branch — push an \
                 explicit single-ref refspec of your own branch '{assigned}'"
            ));
        }
        if dest != assigned {
            return Some(cross_branch_push_msg(assigned, dest));
        }
        idx += 1;
    }
    None
}

/// #t-…93550-2 (embedder P0, ports agend-terminal #2677): a BARE force-push
/// (`--force`/`-f`/`+refspec`) to a NON-protected branch can SILENTLY OVERWRITE
/// commits already on the remote branch — another agent's or session's work, or a
/// wrong-based branch. `push_protected_violation` deliberately ignores force ("HOW
/// not WHAT") and only guards protected refs, so it never catches this. Require a
/// LEASE (`--force-with-lease`/`--force-if-includes`) instead: it refuses the push
/// if the remote moved since the pusher's last fetch, REMOVING the footgun while
/// KEEPING the legitimate rebase-then-force workflow (footgun-removal, not
/// capability-removal). Returns an actionable deny reason (with an executable retry
/// sequence), or `None` to allow.
///
/// Runs AFTER `push_protected_violation` in the push arm, so a force-push to a
/// protected ref is already denied there. Deletions don't overwrite history →
/// exempt (`is_pure_delete_push`). NB: git makes a trailing `--force` override a
/// `--force-with-lease`, so ANY bare force present = unconditional and is denied
/// even if a lease flag co-occurs (`p.force` is set by any bare `--force`/`-f`/`+`).
pub(crate) fn push_force_without_lease_violation(args: &[String]) -> Option<String> {
    let p = parse_push_argv(args);
    if !p.force || is_pure_delete_push(&p) {
        return None;
    }
    let (remote, branch) = force_push_target(&p);
    let seq = match (remote.as_deref(), branch.as_deref()) {
        (Some(r), Some(b)) => format!("git fetch {r} {b} && git push --force-with-lease {r} {b}"),
        _ => "git fetch <remote> <branch> && git push --force-with-lease <remote> <branch>"
            .to_string(),
    };
    Some(format!(
        "bare force-push denied: `--force` / `-f` / a `+refspec` can SILENTLY OVERWRITE \
         commits already on the remote branch (another agent's or session's work). Re-run \
         with a lease — it refuses the push if the remote moved since your last fetch, so you \
         cannot clobber commits you have not seen:\n  {seq}\nProtected refs stay hard-denied \
         regardless; this guards feature branches. If you genuinely intend to discard remote \
         commits, fetch first so the lease baseline is current."
    ))
}

/// A PURE deletion push removes refs rather than overwriting history → exempt from
/// the force gate. `--delete`/`-d` (`p.delete`) makes EVERY named ref a deletion, so
/// it is unconditionally exempt. Otherwise a push is a pure deletion only when it
/// names a remote + ≥1 refspec and EVERY refspec is a `:<dest>` colon-deletion (after
/// an optional `+`).
///
/// #2677 F1 (agend-terminal, CONFIRMED bypass): this MUST be ALL-not-ANY. A mixed
/// `git push --force origin :del real` deletes `:del` AND force-overwrites `real`; an
/// any-refspec exemption would let `real`'s force through. Exempting only when ALL
/// refspecs are deletions keeps a lone `:del` (or `--delete`) exempt while a mixed
/// push stays gated.
///
/// Deliberate fail-CLOSED over-deny (safe — a footgun guard may over-deny when a
/// clean alternative exists): a CLUSTERED `-df` (force+delete of a non-colon ref)
/// is denied rather than exempted, because `is_delete_push_flag` matches only the
/// standalone `-d`/`--delete`, not a `-d` glued into a short cluster. The user can
/// re-run as `git push --delete …` (exempt) or `--force-with-lease`. Never a
/// fail-open: the clustered form is denied, not allowed.
pub(crate) fn is_pure_delete_push(p: &PushArgv) -> bool {
    if p.delete {
        return true;
    }
    p.positionals.len() > 1
        && p.positionals[1..]
            .iter()
            .all(|a| a.strip_prefix('+').unwrap_or(a).starts_with(':'))
}

/// Best-effort `(remote, branch)` for the deny message's retry sequence — only when
/// BOTH are explicitly on the command line (≥2 positionals: a remote followed by a
/// refspec). With fewer we cannot tell a remote from a refspec, so we return
/// `(None, None)` and the caller falls back to the generic `<remote> <branch>`
/// template. Reuses the option-aware `PushArgv.positionals`, so a `-o <val>` value is
/// never mistaken for a remote/refspec.
pub(crate) fn force_push_target(p: &PushArgv) -> (Option<String>, Option<String>) {
    if p.positionals.len() < 2 {
        return (None, None);
    }
    let remote = Some(p.positionals[0].trim_start_matches('+').to_string());
    let branch = p.positionals.get(1).map(|a| {
        let a = a.strip_prefix('+').unwrap_or(a);
        let dest = a.rsplit(':').next().unwrap_or(a);
        dest.strip_prefix("refs/heads/").unwrap_or(dest).to_string()
    });
    (remote, branch)
}
