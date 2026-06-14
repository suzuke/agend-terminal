#![allow(clippy::unwrap_used, clippy::expect_used)]

//! W1.2 — daemon-git helper invariant (production-`src/` sibling of the
//! `tests/`-scoped #821 [`git_subprocess_invariant`]).
//!
//! ## Why
//!
//! Daemon-side git over a fleet-managed repo MUST always bypass the
//! `agend-git` shim (the #821/#1463 forgot-bypass latent class). The W1.2
//! refactor routes daemon git through `git_helpers::git_cmd` / `git_ok`,
//! which are always-bypass + timeout-bounded (#1897). This invariant SEALS
//! that migration: it makes a raw `Command::new("git")` re-appearing in a
//! migrated module a CI failure, not a silent regression — the seal is the
//! point of W1.2, more than the migration itself.
//!
//! ## Scope (intentionally per-slice, NOT global daemon `src/`)
//!
//! Only the modules W1.2 actually migrated are in scope. The daemon has
//! ~150 raw git sites across ~25 modules (worktree.rs, auto_release,
//! retention, admin, claim_verifier, …); migrating all of them is the
//! REFACTOR-PLAN's later slices, not W1.2. As each subsequent slice lands,
//! ADD its module to `MODULE_SCOPE` below — the guard's coverage grows
//! monotonically with the migration, never claiming a seal it hasn't earned.
//!
//! ## What it checks
//!
//! For each file in `MODULE_SCOPE`, scan the PRODUCTION portion only — every
//! line up to the first `#[cfg(test)]` (the in-module `mod tests` carries
//! its own raw git for fixtures and is exempt, the same way
//! `git_subprocess_invariant` exempts nothing-but-tests). Each
//! `Command::new("git")` / `std::process::Command::new("git")` hit in that
//! portion must carry a `// git-raw-allowed: <rationale>` marker on the line
//! or in the contiguous `//` block immediately above. No marker ⇒ violation:
//! migrate it to `git_cmd`/`git_ok`, or justify keeping it raw with the
//! marker.

use std::path::{Path, PathBuf};

const SRC_DIR: &str = "src";
const ALLOW_MARKER: &str = "git-raw-allowed:";
const PATTERN: &str = "Command::new(\"git\")";
const CFG_TEST: &str = "#[cfg(test)]";

/// Modules sealed by W1.2. GROW this list as later refactor-plan slices
/// migrate further modules — never shrink it.
const MODULE_SCOPE: &[&str] = &[
    "worktree_pool.rs",
    "worktree_cleanup.rs",
    "branch_sweep.rs",
    "binding.rs",
    // W1.2 slice (#2068 follow-up): mcp_config (git init→git_cmd),
    // instructions (rev-parse/init→git_ok), skills (rev-parse HEAD→git_cmd;
    // network clones kept raw via git-raw-allowed).
    "mcp_config.rs",
    "instructions.rs",
    "skills.rs",
    // W1.2 slice 2: claim_verifier (git diff --name-only / git diff -- path →
    // git_cmd; git show piped to rustfmt kept raw via git-raw-allowed),
    // deployments (git worktree add → git_cmd). canonical_hygiene already does
    // all its LOCAL git via git_bypass (zero raw Command::new in production) —
    // sealed here to guard against a future raw regression.
    "claim_verifier.rs",
    "deployments.rs",
    "bootstrap/canonical_hygiene.rs",
    // W1.2 slice 3: agent_ops (best-effort worktree-remove → git_ok). ci/mod's
    // worktree-remove is kept raw (deliberate non-bypass, already bounded via
    // spawn_group_bounded, surfaces stderr in JSON note) via git-raw-allowed.
    // auto_release + retention/worktrees already do all LOCAL git via git_bypass
    // (zero raw Command::new in production) — sealed against a future regression.
    "agent_ops.rs",
    "mcp/handlers/ci/mod.rs",
    "daemon/auto_release.rs",
    "daemon/retention/worktrees.rs",
];

/// One violation entry — `(file, line_number, snippet)`.
#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    pub file: PathBuf,
    pub line: usize,
    pub snippet: String,
}

/// True iff the violation line (or a contiguous `//` comment line
/// immediately preceding it) carries the allow-marker. Mirrors
/// `git_subprocess_invariant::has_allow_marker`.
fn has_allow_marker(lines: &[&str], line_idx: usize) -> bool {
    if lines
        .get(line_idx)
        .map(|l| l.contains(ALLOW_MARKER))
        .unwrap_or(false)
    {
        return true;
    }
    let mut cursor = line_idx;
    while let Some(prev_idx) = cursor.checked_sub(1) {
        let Some(prev_line) = lines.get(prev_idx) else {
            break;
        };
        let trimmed = prev_line.trim_start();
        if !trimmed.starts_with("//") {
            break;
        }
        if trimmed.contains(ALLOW_MARKER) {
            return true;
        }
        cursor = prev_idx;
    }
    false
}

/// Index of the first `#[cfg(test)]` line, or `len` if none. The
/// production portion is `lines[..boundary]`; the in-module test module
/// (with its own fixture git) lives at/after it and is out of scope.
fn prod_boundary(lines: &[&str]) -> usize {
    lines
        .iter()
        .position(|l| l.trim_start().starts_with(CFG_TEST))
        .unwrap_or(lines.len())
}

/// Scan one file's production portion for unmarked raw git. Public so the
/// RED/GREEN probes drive the SAME scanner CI runs — no mock branches.
pub fn scan_file(path: &Path, content: &str) -> Vec<Violation> {
    let lines: Vec<&str> = content.lines().collect();
    let boundary = prod_boundary(&lines);
    let mut violations = Vec::new();
    for (idx, line) in lines.iter().enumerate().take(boundary) {
        if !line.contains(PATTERN) {
            continue;
        }
        if has_allow_marker(&lines, idx) {
            continue;
        }
        violations.push(Violation {
            file: path.to_path_buf(),
            line: idx + 1,
            snippet: (*line).trim().to_string(),
        });
    }
    violations
}

/// Scan every in-scope module under `src_dir`.
pub fn scan_scope(src_dir: &Path) -> Vec<Violation> {
    let mut violations = Vec::new();
    for name in MODULE_SCOPE {
        let path = src_dir.join(name);
        let Ok(content) = std::fs::read_to_string(&path) else {
            // A missing in-scope module is itself a regression (rename
            // without updating the guard) — surface it as a violation.
            violations.push(Violation {
                file: path.clone(),
                line: 0,
                snippet: format!("MODULE_SCOPE entry not found: {}", path.display()),
            });
            continue;
        };
        violations.extend(scan_file(&path, &content));
    }
    violations
}

#[test]
fn invariant_passes_on_migrated_modules() {
    let violations = scan_scope(Path::new(SRC_DIR));
    if !violations.is_empty() {
        let summary: Vec<String> = violations
            .iter()
            .map(|v| format!("  {}:{}: {}", v.file.display(), v.line, v.snippet))
            .collect();
        panic!(
            "W1.2 daemon-git invariant FAILED — {} unmarked raw `Command::new(\"git\")` \
             site(s) in a migrated module's production code. Either route through \
             `git_helpers::git_cmd`/`git_ok` (always-bypass) OR justify keeping it raw \
             with a `// {}` <rationale> marker:\n\n{}",
            violations.len(),
            ALLOW_MARKER,
            summary.join("\n"),
        );
    }
}

#[test]
fn scanner_flags_unmarked_raw_git_in_prod() {
    let src = "use std::process::Command;\n\
               fn naughty() {\n    \
                   let _ = Command::new(\"git\").args([\"status\"]).output();\n\
               }\n";
    let v = scan_file(Path::new("synthetic.rs"), src);
    assert_eq!(v.len(), 1, "must flag the one unmarked prod site: {v:?}");
    assert_eq!(v[0].line, 3);
}

#[test]
fn scanner_respects_allow_marker_line_and_block() {
    let inline = "fn ok() {\n    \
                      let _ = Command::new(\"git\").args([\"x\"]).output(); // git-raw-allowed: ok\n\
                  }\n";
    assert!(
        scan_file(Path::new("a.rs"), inline).is_empty(),
        "inline marker must suppress"
    );

    let block = "fn ok() {\n    \
                     // git-raw-allowed: network op, LOCAL_GIT_TIMEOUT too tight\n    \
                     let _ = Command::new(\"git\").args([\"fetch\"]).output();\n\
                 }\n";
    assert!(
        scan_file(Path::new("b.rs"), block).is_empty(),
        "preceding-comment-block marker must suppress"
    );
}

#[test]
fn scanner_ignores_raw_git_in_cfg_test_module() {
    // Raw git below `#[cfg(test)]` is a test fixture — out of scope.
    let src = "fn prod() { let _ = git_helpers::git_ok(p, &[\"x\"]); }\n\
               #[cfg(test)]\n\
               mod tests {\n    \
                   fn fixture() { let _ = Command::new(\"git\").args([\"init\"]).output(); }\n\
               }\n";
    assert!(
        scan_file(Path::new("c.rs"), src).is_empty(),
        "raw git under #[cfg(test)] must NOT be flagged"
    );
}
