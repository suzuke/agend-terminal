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
    /// Grammar v1.1: "fn name() exists" — verify function exists in repo via syn AST + rg fallback.
    FunctionExists { fn_names: Vec<String> },
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
    // Grammar v1.1: detect fn references — "fn name() exists" or "fn name(" patterns
    let fn_names = extract_fn_names(s);
    if !fn_names.is_empty() {
        return Claim::FunctionExists { fn_names };
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

/// Extract function names from claim text. Matches `fn name(` patterns.
fn extract_fn_names(s: &str) -> Vec<String> {
    let re_pattern = "fn\\s+(\\w+)\\s*\\(";
    let mut names = Vec::new();
    // Simple regex-like scan without regex crate
    let mut rest = s;
    while let Some(idx) = rest.find("fn ") {
        rest = &rest[idx + 3..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            let after_name = &rest[name.len()..].trim_start();
            if after_name.starts_with('(') {
                names.push(name);
            }
        }
    }
    let _ = re_pattern; // suppress unused warning — documents the pattern
    names
}

// ---------------------------------------------------------------------------
// Verifier — run mechanical checks against git diff
// ---------------------------------------------------------------------------

/// Verify all claims against the diff between `base` and `head`.
/// `repo_dir` is the git working directory.
pub fn verify(
    repo_dir: &Path,
    base: &str,
    head: &str,
    claims: &[Claim],
    home_override: Option<&Path>,
) -> VerifyResult {
    // B1: check if NoOtherChanges is paired with ScopeFollowsDispatchSpec
    let has_scope_spec = claims
        .iter()
        .any(|c| matches!(c, Claim::ScopeFollowsDispatchSpec { .. }));

    let mut results = Vec::new();
    for claim in claims {
        let r = match claim {
            Claim::NoOtherChanges => check_no_other_changes(repo_dir, base, head, has_scope_spec),
            Claim::ByteEqualVerified { paths } => check_byte_equal(repo_dir, base, head, paths),
            Claim::ScopeFollowsDispatchSpec { task_id } => {
                check_scope_follows_spec(repo_dir, base, head, task_id, home_override)
            }
            Claim::OnlyFormatting => check_only_formatting(repo_dir, base, head),
            Claim::DepsUnchanged => check_deps_unchanged(repo_dir, base, head),
            Claim::FunctionExists { fn_names } => check_fn_exists(repo_dir, fn_names),
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

fn check_no_other_changes(
    repo_dir: &Path,
    base: &str,
    head: &str,
    has_scope_spec: bool,
) -> ClaimResult {
    // B1: standalone "no other changes" without a paired ScopeFollowsDispatchSpec
    // is unverifiable — reject and guide toward explicit scope claim.
    if !has_scope_spec {
        return ClaimResult {
            claim: "no other changes".to_string(),
            passed: false,
            detail: "standalone 'no other changes' is unverifiable — \
                     pair with 'scope follows dispatch spec <task_id>' to specify scope"
                .to_string(),
        };
    }
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

fn check_scope_follows_spec(
    repo_dir: &Path,
    base: &str,
    head: &str,
    task_id: &str,
    home_override: Option<&Path>,
) -> ClaimResult {
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
    let home = home_override.map(std::path::PathBuf::from).or_else(|| {
        std::env::var("AGEND_HOME")
            .ok()
            .map(std::path::PathBuf::from)
    });
    let allowed = home.and_then(|h| load_spec_files(&h, task_id));
    match allowed {
        Some(spec_files) => {
            // B2a: path-equality or directory-prefix, not substring
            let out_of_scope: Vec<_> = diff_files
                .iter()
                .filter(|f| {
                    !spec_files.iter().any(|s| {
                        // Exact path match, or spec is a directory prefix
                        *f == s || f.starts_with(&format!("{s}/"))
                    })
                })
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
            passed: false,
            detail: format!(
                "claimed dispatch spec {task_id} but no entry found in dispatch_tracking.json"
            ),
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
    // B3: non-.rs file changes under "only formatting" → reject
    let non_rs: Vec<_> = files.iter().filter(|f| !f.ends_with(".rs")).collect();
    if !non_rs.is_empty() {
        return ClaimResult {
            claim: "only formatting".to_string(),
            passed: false,
            detail: format!(
                "non-.rs files changed: {}",
                non_rs
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
    }
    if files.is_empty() {
        return ClaimResult {
            claim: "only formatting".to_string(),
            passed: true,
            detail: "no files changed".to_string(),
        };
    }
    // B3: compare rustfmt(base-content) vs rustfmt(head-content) for each .rs file.
    // If they match, the diff is formatting-only.
    let mut failures = Vec::new();
    for f in &files {
        let base_fmt = git_show_and_fmt(repo_dir, base, f);
        let head_fmt = git_show_and_fmt(repo_dir, head, f);
        match (base_fmt, head_fmt) {
            (Ok(b), Ok(h)) if b == h => {} // fmt-only ✓
            (Ok(_), Ok(_)) => failures.push(f.clone()),
            (Err(e), _) | (_, Err(e)) => failures.push(format!("{f}: {e}")),
        }
    }
    if failures.is_empty() {
        ClaimResult {
            claim: "only formatting".to_string(),
            passed: true,
            detail: format!("{} .rs file(s) confirmed fmt-only", files.len()),
        }
    } else {
        ClaimResult {
            claim: "only formatting".to_string(),
            passed: false,
            detail: format!("non-formatting changes in: {}", failures.join(", ")),
        }
    }
}

/// Get file content at a given revision and run rustfmt on it.
fn git_show_and_fmt(repo_dir: &Path, rev: &str, path: &str) -> Result<String, String> {
    let show = std::process::Command::new("git")
        .args(["show", &format!("{rev}:{path}")])
        .current_dir(repo_dir)
        .output()
        .map_err(|e| format!("git show failed: {e}"))?;
    if !show.status.success() {
        // File may not exist at base (new file) — treat as empty
        return Ok(String::new());
    }
    let mut fmt = std::process::Command::new("rustfmt")
        .arg("--edition")
        .arg("2021")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("rustfmt spawn failed: {e}"))?;
    if let Some(mut stdin) = fmt.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(&show.stdout);
    }
    let output = fmt
        .wait_with_output()
        .map_err(|e| format!("rustfmt wait failed: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn check_fn_exists(repo_dir: &Path, fn_names: &[String]) -> ClaimResult {
    if fn_names.is_empty() {
        return ClaimResult {
            claim: "function exists".to_string(),
            passed: false,
            detail: "no function names in claim".to_string(),
        };
    }
    // Build fn name set from repo via syn AST walk + rg fallback
    let known_fns = collect_fn_names_from_repo(repo_dir);
    let mut missing = Vec::new();
    for name in fn_names {
        if !known_fns.contains(name.as_str()) {
            // Fallback: ripgrep for fn declaration
            if !rg_fn_exists(repo_dir, name) {
                missing.push(name.clone());
            }
        }
    }
    if missing.is_empty() {
        ClaimResult {
            claim: "function exists".to_string(),
            passed: true,
            detail: format!("{} function(s) verified", fn_names.len()),
        }
    } else {
        ClaimResult {
            claim: "function exists".to_string(),
            passed: false,
            detail: format!("function(s) not found in repo: {}", missing.join(", ")),
        }
    }
}

/// Walk all .rs files under repo_dir/src/ and extract fn names via syn AST.
fn collect_fn_names_from_repo(repo_dir: &Path) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let src_dir = repo_dir.join("src");
    if !src_dir.exists() {
        return names;
    }
    fn walk_dir(dir: &Path, names: &mut std::collections::HashSet<String>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_dir(&path, names);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(file) = syn::parse_file(&content) {
                        extract_fn_names_from_file(&file, names);
                    }
                }
            }
        }
    }
    walk_dir(&src_dir, &mut names);
    names
}

/// Extract fn names from a parsed syn File.
fn extract_fn_names_from_file(file: &syn::File, names: &mut std::collections::HashSet<String>) {
    for item in &file.items {
        extract_fn_names_from_item(item, names);
    }
}

fn extract_fn_names_from_item(item: &syn::Item, names: &mut std::collections::HashSet<String>) {
    match item {
        syn::Item::Fn(f) => {
            names.insert(f.sig.ident.to_string());
        }
        syn::Item::Impl(imp) => {
            for item in &imp.items {
                if let syn::ImplItem::Fn(f) = item {
                    names.insert(f.sig.ident.to_string());
                }
            }
        }
        syn::Item::Mod(m) => {
            if let Some((_, items)) = &m.content {
                for item in items {
                    extract_fn_names_from_item(item, names);
                }
            }
        }
        _ => {}
    }
}

/// Fallback: use ripgrep to check if `fn name` exists in repo.
fn rg_fn_exists(repo_dir: &Path, name: &str) -> bool {
    let pattern = format!("\\bfn {name}\\b");
    let output = std::process::Command::new("grep")
        .args(["-r", "-l", &pattern, "src/"])
        .current_dir(repo_dir)
        .output();
    match output {
        Ok(o) => !o.stdout.is_empty(),
        Err(_) => false,
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
        let r = verify(&dir, &base, &head, &[Claim::DepsUnchanged], None);
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
        let r = verify(&dir, &base, &head, &[Claim::DepsUnchanged], None);
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
            None,
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
            None,
        );
        assert!(!r.ok, "byte-equal should fail: {:?}", r.results);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- no other changes: RED (standalone without ScopeFollowsDispatchSpec) ---
    #[test]
    fn no_other_changes_standalone_red() {
        let dir = setup_git_repo("noc_standalone");
        let base = git_head(&dir);
        std::fs::write(dir.join("src.rs"), "fn main() { v2(); }\n").unwrap();
        git_commit(&dir, "update");
        let head = git_head(&dir);
        let r = verify(&dir, &base, &head, &[Claim::NoOtherChanges], None);
        // B1: standalone NoOtherChanges → REJECT
        assert!(
            !r.ok,
            "standalone no-other-changes should reject: {:?}",
            r.results
        );
        assert!(r.results[0].detail.contains("scope follows dispatch spec"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- only formatting: RED (non-.rs file changed) ---
    #[test]
    fn only_formatting_red_non_rs() {
        let dir = setup_git_repo("fmt_non_rs");
        let base = git_head(&dir);
        std::fs::write(dir.join("README.md"), "# hello\n").unwrap();
        git_commit(&dir, "add readme");
        let head = git_head(&dir);
        let r = verify(&dir, &base, &head, &[Claim::OnlyFormatting], None);
        // B3: non-.rs files under "only formatting" → REJECT
        assert!(
            !r.ok,
            "only formatting with non-.rs should reject: {:?}",
            r.results
        );
        assert!(r.results[0].detail.contains("non-.rs"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- scope follows dispatch spec: RED when no spec found (fail-closed) ---
    #[test]
    fn scope_follows_spec_missing_red() {
        let dir = setup_git_repo("scope_missing");
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
            None,
        );
        // B2b: missing spec → fail-closed
        assert!(!r.ok, "missing spec should reject: {:?}", r.results);
        assert!(r.results[0].detail.contains("no entry found"));
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
            None,
        );
        assert!(r.ok);
        assert!(r.results[0].detail.contains("pass-through"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- B5: RED tests for reject paths ---

    // B1: NoOtherChanges paired with ScopeFollowsDispatchSpec → passes (the NoOtherChanges part)
    #[test]
    fn no_other_changes_paired_with_scope_spec() {
        let dir = setup_git_repo("noc_paired");
        let base = git_head(&dir);
        let head = base.clone(); // no changes
        let r = verify(
            &dir,
            &base,
            &head,
            &[
                Claim::NoOtherChanges,
                Claim::ScopeFollowsDispatchSpec {
                    task_id: "t-x".into(),
                },
            ],
            None,
        );
        // NoOtherChanges passes when paired (ScopeFollowsDispatchSpec may fail separately)
        let noc_result = &r.results[0];
        assert!(
            noc_result.passed,
            "paired NoOtherChanges should pass: {:?}",
            noc_result
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // B2a: substring collision — spec allows "src/foo.rs" but "src/foo.rs.bak" modified → REJECT
    #[test]
    fn scope_spec_substring_collision_red() {
        let dir = setup_git_repo("scope_substr");
        let base = git_head(&dir);
        // Create a file that is a substring-collision with "src.rs"
        std::fs::write(dir.join("src.rs.bak"), "backup\n").unwrap();
        git_commit(&dir, "add bak file");
        let head = git_head(&dir);
        // Set up dispatch tracking with allowed file "src.rs"
        let home =
            std::env::temp_dir().join(format!("agend-scope-substr-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let tracking = serde_json::json!({
            "schema_version": 1,
            "entries": [{"task_id": "t-substr", "allowed_files": ["src.rs"]}]
        });
        std::fs::write(
            home.join("dispatch_tracking.json"),
            serde_json::to_string(&tracking).unwrap(),
        )
        .unwrap();
        let r = verify(
            &dir,
            &base,
            &head,
            &[Claim::ScopeFollowsDispatchSpec {
                task_id: "t-substr".into(),
            }],
            Some(&home),
        );
        // B2a: "src.rs.bak" should NOT match spec "src.rs" — out of scope
        assert!(!r.ok, "substring collision should reject: {:?}", r.results);
        assert!(r.results[0].detail.contains("out-of-scope"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    // B2: scope spec GREEN — in-scope file passes
    #[test]
    fn scope_spec_in_scope_green() {
        let dir = setup_git_repo("scope_green");
        let base = git_head(&dir);
        std::fs::write(dir.join("src.rs"), "fn v2() {}\n").unwrap();
        git_commit(&dir, "update src");
        let head = git_head(&dir);
        let home =
            std::env::temp_dir().join(format!("agend-scope-green-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let tracking = serde_json::json!({
            "schema_version": 1,
            "entries": [{"task_id": "t-green", "allowed_files": ["src.rs"]}]
        });
        std::fs::write(
            home.join("dispatch_tracking.json"),
            serde_json::to_string(&tracking).unwrap(),
        )
        .unwrap();
        let r = verify(
            &dir,
            &base,
            &head,
            &[Claim::ScopeFollowsDispatchSpec {
                task_id: "t-green".into(),
            }],
            Some(&home),
        );
        assert!(r.ok, "in-scope file should pass: {:?}", r.results);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    // B2: scope spec RED — out-of-scope file
    #[test]
    fn scope_spec_out_of_scope_red() {
        let dir = setup_git_repo("scope_oos");
        let base = git_head(&dir);
        std::fs::write(dir.join("other.rs"), "fn other() {}\n").unwrap();
        git_commit(&dir, "add other");
        let head = git_head(&dir);
        let home =
            std::env::temp_dir().join(format!("agend-scope-oos-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let tracking = serde_json::json!({
            "schema_version": 1,
            "entries": [{"task_id": "t-oos", "allowed_files": ["src.rs"]}]
        });
        std::fs::write(
            home.join("dispatch_tracking.json"),
            serde_json::to_string(&tracking).unwrap(),
        )
        .unwrap();
        let r = verify(
            &dir,
            &base,
            &head,
            &[Claim::ScopeFollowsDispatchSpec {
                task_id: "t-oos".into(),
            }],
            Some(&home),
        );
        assert!(!r.ok, "out-of-scope should reject: {:?}", r.results);
        assert!(r.results[0].detail.contains("out-of-scope"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    // B3: only formatting RED — logic change in .rs file
    #[test]
    fn only_formatting_logic_change_red() {
        let dir = setup_git_repo("fmt_logic");
        let base = git_head(&dir);
        // Change logic, not just formatting
        std::fs::write(dir.join("src.rs"), "fn main() { new_logic(); }\n").unwrap();
        git_commit(&dir, "logic change");
        let head = git_head(&dir);
        let r = verify(&dir, &base, &head, &[Claim::OnlyFormatting], None);
        assert!(
            !r.ok,
            "logic change should reject only-formatting: {:?}",
            r.results
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // B3: only formatting GREEN — pure whitespace/fmt change
    #[test]
    fn only_formatting_pure_fmt_green() {
        let dir = setup_git_repo("fmt_pure");
        let base = git_head(&dir);
        // Change only formatting (add trailing whitespace that rustfmt normalizes)
        std::fs::write(dir.join("src.rs"), "fn  main(  )  {  }\n").unwrap();
        git_commit(&dir, "fmt change");
        let head = git_head(&dir);
        let r = verify(&dir, &base, &head, &[Claim::OnlyFormatting], None);
        assert!(r.ok, "pure fmt change should pass: {:?}", r.results);
        std::fs::remove_dir_all(&dir).ok();
    }

    // B4: byte-equal with empty paths → RED
    #[test]
    fn byte_equal_no_paths_red() {
        let dir = setup_git_repo("byte_no_paths");
        let base = git_head(&dir);
        let head = base.clone();
        let r = verify(
            &dir,
            &base,
            &head,
            &[Claim::ByteEqualVerified { paths: vec![] }],
            None,
        );
        assert!(
            !r.ok,
            "byte-equal with no paths should reject: {:?}",
            r.results
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- M4: FunctionExists tests ---

    #[test]
    fn parse_fn_exists_claim() {
        let claims = parse_claims("references fn verify_push()");
        assert_eq!(
            claims,
            vec![Claim::FunctionExists {
                fn_names: vec!["verify_push".into()]
            }]
        );
    }

    #[test]
    fn fn_exists_green_real_fn() {
        let dir = setup_git_repo("fn_green");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn real_function() {}\n").unwrap();
        let base = git_head(&dir);
        let r = verify(
            &dir,
            &base,
            &base,
            &[Claim::FunctionExists {
                fn_names: vec!["real_function".into()],
            }],
            None,
        );
        assert!(r.ok, "real fn should pass: {:?}", r.results);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fn_exists_red_fake_fn() {
        let dir = setup_git_repo("fn_red");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn real_function() {}\n").unwrap();
        let base = git_head(&dir);
        let r = verify(
            &dir,
            &base,
            &base,
            &[Claim::FunctionExists {
                fn_names: vec!["hallucinated_function".into()],
            }],
            None,
        );
        assert!(!r.ok, "fake fn should reject: {:?}", r.results);
        assert!(r.results[0].detail.contains("hallucinated_function"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fn_exists_impl_method() {
        let dir = setup_git_repo("fn_impl");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "struct Foo;\nimpl Foo { fn method_one(&self) {} }\n",
        )
        .unwrap();
        let base = git_head(&dir);
        let r = verify(
            &dir,
            &base,
            &base,
            &[Claim::FunctionExists {
                fn_names: vec!["method_one".into()],
            }],
            None,
        );
        assert!(r.ok, "impl method should be found: {:?}", r.results);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fn_exists_multi_one_missing() {
        let dir = setup_git_repo("fn_multi");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn exists_fn() {}\n").unwrap();
        let base = git_head(&dir);
        let r = verify(
            &dir,
            &base,
            &base,
            &[Claim::FunctionExists {
                fn_names: vec!["exists_fn".into(), "missing_fn".into()],
            }],
            None,
        );
        assert!(!r.ok, "one missing fn should reject: {:?}", r.results);
        assert!(r.results[0].detail.contains("missing_fn"));
        assert!(!r.results[0].detail.contains("exists_fn"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
