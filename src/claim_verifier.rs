//! Push-time claim verifier — parses dev push claims and mechanically checks
//! them against the actual git diff. Grammar v1.0: 5 sentence patterns.
//!
//! Unknown phrases pass through (no false-positive blocking).
//! Hard reject on divergence from day 1.

use serde::{Deserialize, Serialize};
use std::path::Path;

// ---------------------------------------------------------------------------
// Claim AST (versioned via SchemaVersioned)
// ---------------------------------------------------------------------------

/// A single parsed claim extracted from push text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Claim {
    /// "no other changes" — diff must be scoped to dispatch spec files only.
    NoOtherChanges,
    /// "byte-equal verified" — listed paths must have empty diff.
    ByteEqualVerified { paths: Vec<String> },
    /// "scope follows dispatch spec X" — diff files ⊆ task spec allowed files.
    ScopeFollowsDispatchSpec { task_id: String },
    /// "only formatting" — rustfmt --check must pass on all changed files.
    OnlyFormatting,
    /// "deps unchanged" — Cargo.lock must have empty diff.
    DepsUnchanged,
    /// Unrecognised phrase — pass through, no check.
    Unknown(String),
}

/// Result of verifying a single claim against the diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimResult {
    pub claim: String,
    pub passed: bool,
    pub detail: String,
}

/// Full verification result for a push.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResult {
    pub ok: bool,
    pub results: Vec<ClaimResult>,
}

// ---------------------------------------------------------------------------
// Parser — extract claims from free-text
// ---------------------------------------------------------------------------

/// Parse claim text into a list of `Claim` values.
/// Each line (or semicolon-separated segment) is matched against the grammar.
pub fn parse_claims(text: &str) -> Vec<Claim> {
    let mut claims = Vec::new();
    for segment in text.split(['\n', ';']) {
        let s = segment.trim();
        if s.is_empty() {
            continue;
        }
        claims.push(parse_one(s));
    }
    claims
}

fn parse_one(s: &str) -> Claim {
    let lower = s.to_lowercase();

    if lower.contains("no other changes") {
        return Claim::NoOtherChanges;
    }
    if lower.contains("byte-equal verified") || lower.contains("byte equal verified") {
        let paths = extract_paths(s);
        return Claim::ByteEqualVerified { paths };
    }
    if lower.contains("scope follows dispatch spec") {
        let task_id = extract_task_id(s).unwrap_or_default();
        return Claim::ScopeFollowsDispatchSpec { task_id };
    }
    if lower.contains("only formatting") {
        return Claim::OnlyFormatting;
    }
    if lower.contains("deps unchanged") {
        return Claim::DepsUnchanged;
    }
    Claim::Unknown(s.to_string())
}

/// Extract file paths from a claim string (backtick-delimited or whitespace tokens ending in known extensions).
fn extract_paths(s: &str) -> Vec<String> {
    let mut paths = Vec::new();
    // Backtick-delimited paths
    let mut rest = s;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        if let Some(end) = rest.find('`') {
            let p = rest[..end].trim();
            if !p.is_empty() {
                paths.push(p.to_string());
            }
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    paths
}

/// Extract a task ID from a "scope follows dispatch spec X" claim.
fn extract_task_id(s: &str) -> Option<String> {
    let lower = s.to_lowercase();
    let marker = "scope follows dispatch spec";
    let idx = lower.find(marker)? + marker.len();
    let rest = s[idx..].trim();
    // Task ID is the next whitespace-delimited token
    let id = rest.split_whitespace().next()?;
    // Strip surrounding quotes/backticks
    let id = id.trim_matches(|c| c == '`' || c == '"' || c == '\'');
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

// ---------------------------------------------------------------------------
// Verifier — run mechanical checks against git diff
// ---------------------------------------------------------------------------

/// Verify all claims against the diff between `base` and `head`.
/// `repo_dir` is the git working directory.
pub fn verify(repo_dir: &Path, base: &str, head: &str, claims: &[Claim]) -> VerifyResult {
    let mut results = Vec::new();
    for claim in claims {
        let r = match claim {
            Claim::NoOtherChanges => check_no_other_changes(repo_dir, base, head),
            Claim::ByteEqualVerified { paths } => check_byte_equal(repo_dir, base, head, paths),
            Claim::ScopeFollowsDispatchSpec { task_id } => {
                check_scope_follows_spec(repo_dir, base, head, task_id)
            }
            Claim::OnlyFormatting => check_only_formatting(repo_dir, base, head),
            Claim::DepsUnchanged => check_deps_unchanged(repo_dir, base, head),
            Claim::Unknown(text) => ClaimResult {
                claim: text.clone(),
                passed: true,
                detail: "unknown phrase — pass-through".to_string(),
            },
        };
        results.push(r);
    }
    let ok = results.iter().all(|r| r.passed);
    VerifyResult { ok, results }
}

/// Run `git diff --stat base..head` and return the list of changed file paths.
fn git_diff_files(repo_dir: &Path, base: &str, head: &str) -> Result<Vec<String>, String> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", &format!("{base}..{head}")])
        .current_dir(repo_dir)
        .output()
        .map_err(|e| format!("git diff failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git diff exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Run `git diff base..head -- <path>` and return whether it's empty.
fn git_diff_path_empty(
    repo_dir: &Path,
    base: &str,
    head: &str,
    path: &str,
) -> Result<bool, String> {
    let output = std::process::Command::new("git")
        .args(["diff", &format!("{base}..{head}"), "--", path])
        .current_dir(repo_dir)
        .output()
        .map_err(|e| format!("git diff failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git diff exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(output.stdout.is_empty())
}

// --- Individual claim checks ---

fn check_no_other_changes(repo_dir: &Path, base: &str, head: &str) -> ClaimResult {
    // Without a dispatch spec to compare against, we can only report the
    // changed files. The caller (or a higher-level check) compares against
    // the expected set. For now, just list the files.
    match git_diff_files(repo_dir, base, head) {
        Ok(files) if files.is_empty() => ClaimResult {
            claim: "no other changes".to_string(),
            passed: true,
            detail: "no files changed".to_string(),
        },
        Ok(files) => ClaimResult {
            claim: "no other changes".to_string(),
            passed: true,
            detail: format!("changed files: {}", files.join(", ")),
        },
        Err(e) => ClaimResult {
            claim: "no other changes".to_string(),
            passed: false,
            detail: e,
        },
    }
}

fn check_byte_equal(repo_dir: &Path, base: &str, head: &str, paths: &[String]) -> ClaimResult {
    if paths.is_empty() {
        return ClaimResult {
            claim: "byte-equal verified".to_string(),
            passed: false,
            detail: "no paths specified in claim".to_string(),
        };
    }
    let mut failures = Vec::new();
    for p in paths {
        match git_diff_path_empty(repo_dir, base, head, p) {
            Ok(true) => {} // byte-equal confirmed
            Ok(false) => failures.push(p.clone()),
            Err(e) => failures.push(format!("{p}: {e}")),
        }
    }
    if failures.is_empty() {
        ClaimResult {
            claim: "byte-equal verified".to_string(),
            passed: true,
            detail: format!("{} path(s) confirmed unchanged", paths.len()),
        }
    } else {
        ClaimResult {
            claim: "byte-equal verified".to_string(),
            passed: false,
            detail: format!("modified: {}", failures.join(", ")),
        }
    }
}

fn check_scope_follows_spec(repo_dir: &Path, base: &str, head: &str, task_id: &str) -> ClaimResult {
    if task_id.is_empty() {
        return ClaimResult {
            claim: "scope follows dispatch spec".to_string(),
            passed: false,
            detail: "no task_id in claim".to_string(),
        };
    }
    let diff_files = match git_diff_files(repo_dir, base, head) {
        Ok(f) => f,
        Err(e) => {
            return ClaimResult {
                claim: format!("scope follows dispatch spec {task_id}"),
                passed: false,
                detail: e,
            }
        }
    };
    // Load dispatch tracking to find allowed files for this task.
    // If no AGEND_HOME or no dispatch entry, pass through (can't verify).
    let home = std::env::var("AGEND_HOME")
        .map(std::path::PathBuf::from)
        .ok();
    let allowed = home.and_then(|h| load_spec_files(&h, task_id));
    match allowed {
        Some(spec_files) => {
            let out_of_scope: Vec<_> = diff_files
                .iter()
                .filter(|f| !spec_files.iter().any(|s| f.contains(s.as_str())))
                .cloned()
                .collect();
            if out_of_scope.is_empty() {
                ClaimResult {
                    claim: format!("scope follows dispatch spec {task_id}"),
                    passed: true,
                    detail: format!("{} file(s) within spec scope", diff_files.len()),
                }
            } else {
                ClaimResult {
                    claim: format!("scope follows dispatch spec {task_id}"),
                    passed: false,
                    detail: format!("out-of-scope files: {}", out_of_scope.join(", ")),
                }
            }
        }
        None => ClaimResult {
            claim: format!("scope follows dispatch spec {task_id}"),
            passed: true,
            detail: "no dispatch spec found — pass-through".to_string(),
        },
    }
}

/// Load allowed files from dispatch_tracking.json for a given task_id.
fn load_spec_files(home: &Path, task_id: &str) -> Option<Vec<String>> {
    let path = home.join("dispatch_tracking.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let store: serde_json::Value = serde_json::from_str(&content).ok()?;
    let entries = store.get("entries")?.as_array()?;
    for entry in entries {
        if entry.get("task_id")?.as_str()? == task_id {
            // Use "allowed_files" if present, otherwise pass-through
            if let Some(files) = entry.get("allowed_files").and_then(|v| v.as_array()) {
                return Some(
                    files
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect(),
                );
            }
        }
    }
    None
}

fn check_only_formatting(repo_dir: &Path, base: &str, head: &str) -> ClaimResult {
    let files = match git_diff_files(repo_dir, base, head) {
        Ok(f) => f,
        Err(e) => {
            return ClaimResult {
                claim: "only formatting".to_string(),
                passed: false,
                detail: e,
            }
        }
    };
    let rs_files: Vec<_> = files.iter().filter(|f| f.ends_with(".rs")).collect();
    if rs_files.is_empty() {
        return ClaimResult {
            claim: "only formatting".to_string(),
            passed: true,
            detail: "no .rs files changed".to_string(),
        };
    }
    // Run rustfmt --check on the changed .rs files at HEAD
    let fmt_result = std::process::Command::new("rustfmt")
        .arg("--edition")
        .arg("2021")
        .arg("--check")
        .args(&rs_files)
        .current_dir(repo_dir)
        .output();
    match fmt_result {
        Ok(o) if o.status.success() => ClaimResult {
            claim: "only formatting".to_string(),
            passed: true,
            detail: format!("{} .rs file(s) pass rustfmt", rs_files.len()),
        },
        Ok(o) => ClaimResult {
            claim: "only formatting".to_string(),
            passed: false,
            detail: format!(
                "rustfmt reports diffs: {}",
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .take(5)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        },
        Err(e) => ClaimResult {
            claim: "only formatting".to_string(),
            passed: false,
            detail: format!("rustfmt not available: {e}"),
        },
    }
}

fn check_deps_unchanged(repo_dir: &Path, base: &str, head: &str) -> ClaimResult {
    match git_diff_path_empty(repo_dir, base, head, "Cargo.lock") {
        Ok(true) => ClaimResult {
            claim: "deps unchanged".to_string(),
            passed: true,
            detail: "Cargo.lock unchanged".to_string(),
        },
        Ok(false) => ClaimResult {
            claim: "deps unchanged".to_string(),
            passed: false,
            detail: "Cargo.lock has changes".to_string(),
        },
        Err(e) => ClaimResult {
            claim: "deps unchanged".to_string(),
            passed: false,
            detail: e,
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // --- Parser tests ---

    #[test]
    fn parse_no_other_changes() {
        let claims = parse_claims("no other changes");
        assert_eq!(claims, vec![Claim::NoOtherChanges]);
    }

    #[test]
    fn parse_byte_equal_with_paths() {
        let claims = parse_claims("byte-equal verified `src/main.rs` `Cargo.toml`");
        assert_eq!(
            claims,
            vec![Claim::ByteEqualVerified {
                paths: vec!["src/main.rs".into(), "Cargo.toml".into()]
            }]
        );
    }

    #[test]
    fn parse_scope_follows_dispatch() {
        let claims = parse_claims("scope follows dispatch spec t-123-456");
        assert_eq!(
            claims,
            vec![Claim::ScopeFollowsDispatchSpec {
                task_id: "t-123-456".into()
            }]
        );
    }

    #[test]
    fn parse_only_formatting() {
        let claims = parse_claims("only formatting");
        assert_eq!(claims, vec![Claim::OnlyFormatting]);
    }

    #[test]
    fn parse_deps_unchanged() {
        let claims = parse_claims("deps unchanged");
        assert_eq!(claims, vec![Claim::DepsUnchanged]);
    }

    #[test]
    fn parse_unknown_passes_through() {
        let claims = parse_claims("all tests passing");
        assert_eq!(claims, vec![Claim::Unknown("all tests passing".into())]);
    }

    #[test]
    fn parse_multiple_claims() {
        let claims = parse_claims("no other changes; deps unchanged\nonly formatting");
        assert_eq!(
            claims,
            vec![
                Claim::NoOtherChanges,
                Claim::DepsUnchanged,
                Claim::OnlyFormatting,
            ]
        );
    }

    #[test]
    fn parse_case_insensitive() {
        let claims = parse_claims("No Other Changes");
        assert_eq!(claims, vec![Claim::NoOtherChanges]);
    }

    // --- Verifier tests (using a temp git repo) ---

    fn setup_git_repo(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-claim-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .unwrap()
        };
        run(&["init", "-b", "main"]);
        std::fs::write(dir.join("Cargo.lock"), "# lock v1\n").unwrap();
        std::fs::write(dir.join("src.rs"), "fn main() {}\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
        dir
    }

    fn git_head(dir: &Path) -> String {
        let o = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn git_commit(dir: &Path, msg: &str) {
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", msg, "--allow-empty-message"])
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
    }

    // --- deps unchanged: GREEN (no lock change) ---
    #[test]
    fn deps_unchanged_green() {
        let dir = setup_git_repo("deps_green");
        let base = git_head(&dir);
        std::fs::write(dir.join("src.rs"), "fn main() { println!(\"hi\"); }\n").unwrap();
        git_commit(&dir, "add print");
        let head = git_head(&dir);
        let r = verify(&dir, &base, &head, &[Claim::DepsUnchanged]);
        assert!(r.ok, "deps unchanged should pass: {:?}", r.results);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- deps unchanged: RED (lock changed) ---
    #[test]
    fn deps_unchanged_red() {
        let dir = setup_git_repo("deps_red");
        let base = git_head(&dir);
        std::fs::write(dir.join("Cargo.lock"), "# lock v2 — changed\n").unwrap();
        git_commit(&dir, "change lock");
        let head = git_head(&dir);
        let r = verify(&dir, &base, &head, &[Claim::DepsUnchanged]);
        assert!(!r.ok, "deps unchanged should fail: {:?}", r.results);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- byte-equal verified: GREEN (path unchanged) ---
    #[test]
    fn byte_equal_green() {
        let dir = setup_git_repo("byte_green");
        let base = git_head(&dir);
        std::fs::write(dir.join("new.rs"), "fn new() {}\n").unwrap();
        git_commit(&dir, "add new file");
        let head = git_head(&dir);
        let r = verify(
            &dir,
            &base,
            &head,
            &[Claim::ByteEqualVerified {
                paths: vec!["src.rs".into()],
            }],
        );
        assert!(r.ok, "byte-equal should pass: {:?}", r.results);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- byte-equal verified: RED (path modified) ---
    #[test]
    fn byte_equal_red() {
        let dir = setup_git_repo("byte_red");
        let base = git_head(&dir);
        std::fs::write(dir.join("src.rs"), "fn main() { changed(); }\n").unwrap();
        git_commit(&dir, "modify src");
        let head = git_head(&dir);
        let r = verify(
            &dir,
            &base,
            &head,
            &[Claim::ByteEqualVerified {
                paths: vec!["src.rs".into()],
            }],
        );
        assert!(!r.ok, "byte-equal should fail: {:?}", r.results);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- no other changes: GREEN (reports files) ---
    #[test]
    fn no_other_changes_green() {
        let dir = setup_git_repo("noc_green");
        let base = git_head(&dir);
        std::fs::write(dir.join("src.rs"), "fn main() { v2(); }\n").unwrap();
        git_commit(&dir, "update");
        let head = git_head(&dir);
        let r = verify(&dir, &base, &head, &[Claim::NoOtherChanges]);
        // NoOtherChanges currently passes and reports files
        assert!(r.ok);
        assert!(r.results[0].detail.contains("src.rs"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- only formatting: GREEN (no .rs changes) ---
    #[test]
    fn only_formatting_green_no_rs() {
        let dir = setup_git_repo("fmt_green");
        let base = git_head(&dir);
        std::fs::write(dir.join("README.md"), "# hello\n").unwrap();
        git_commit(&dir, "add readme");
        let head = git_head(&dir);
        let r = verify(&dir, &base, &head, &[Claim::OnlyFormatting]);
        assert!(
            r.ok,
            "only formatting should pass with no .rs: {:?}",
            r.results
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- scope follows dispatch spec: pass-through when no spec ---
    #[test]
    fn scope_follows_spec_passthrough() {
        let dir = setup_git_repo("scope_pt");
        let base = git_head(&dir);
        std::fs::write(dir.join("src.rs"), "fn v2() {}\n").unwrap();
        git_commit(&dir, "update");
        let head = git_head(&dir);
        let r = verify(
            &dir,
            &base,
            &head,
            &[Claim::ScopeFollowsDispatchSpec {
                task_id: "t-nonexistent".into(),
            }],
        );
        // No dispatch spec → pass-through
        assert!(r.ok, "scope spec should pass-through: {:?}", r.results);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- unknown claim passes through ---
    #[test]
    fn unknown_claim_passes() {
        let dir = setup_git_repo("unknown");
        let base = git_head(&dir);
        let head = base.clone();
        let r = verify(
            &dir,
            &base,
            &head,
            &[Claim::Unknown("all tests passing".into())],
        );
        assert!(r.ok);
        assert!(r.results[0].detail.contains("pass-through"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
