//! #942 + #943 boot-time migration: rename old-format watch files to
//! the canonical+sha256 form so operators don't see duplicate 72h
//! notifications across the hash-scheme transition.
//!
//! Pre-#942/#943 filenames used `DefaultHasher` (SipHash-2-4 truncated
//! to 64 bits) → 16 hex chars + `.json` (21 chars total) + a verbatim
//! repo string (possibly `.git`-suffixed, cased differently).
//!
//! Post-fix filenames use sha256 (256-bit) → 64 hex chars + `.json` (69
//! chars total) + canonicalized repo string.
//!
//! Migration logic:
//! 1. Scan `<home>/ci-watches/*.json`
//! 2. For each file: if filename stem length != 64 → old format
//! 3. Read body, parse `repo` + `branch`
//! 4. Canonicalize repo, compute new sha256 filename
//! 5. Rewrite `repo` field in body to canonical form
//! 6. Rename file; if target already exists, log conflict and skip
//!
//! Conflict resolution: when two old files map to the same canonical
//! target (e.g., `owner/repo.git@feat/x` and `owner/repo@feat/x`),
//! the FIRST one encountered wins; subsequent conflicts log a warning
//! and are left in place. Operator can manually clean up via
//! `rm <home>/ci-watches/<old-name>.json`. The hash-collision case is
//! cryptographically impossible (sha256 collision-resistant).
//!
//! Idempotent: re-running the migration on already-migrated state is
//! a no-op (all files have 64-hex stems).

use std::path::Path;

use super::registry::{ci_watches_dir, watch_filename};

const SHA256_HEX_LEN: usize = 64;

/// Scan `<home>/ci-watches/` and rename any old-format (non-sha256)
/// watch files to the canonical+sha256 form. Idempotent. Best-effort:
/// errors are logged but never abort caller (daemon boot path).
///
/// Returns the count of files actually renamed (for logging /
/// test assertions).
pub fn migrate_legacy_watch_filenames(home: &Path) -> usize {
    let ci_dir = ci_watches_dir(home);
    let Ok(entries) = std::fs::read_dir(&ci_dir) else {
        return 0;
    };
    let mut renamed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        // sha256 hex digests are 64 chars. Anything else is old format.
        if stem.len() == SHA256_HEX_LEN && stem.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            tracing::warn!(
                path = %path.display(),
                "#942/#943 migration: failed to read old-format watch file"
            );
            continue;
        };
        let mut watch: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "#942/#943 migration: failed to parse old-format watch JSON"
                );
                continue;
            }
        };
        let raw_repo = watch.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        let raw_branch = watch.get("branch").and_then(|v| v.as_str()).unwrap_or("");
        if raw_repo.is_empty() || raw_branch.is_empty() {
            tracing::warn!(
                path = %path.display(),
                "#942/#943 migration: old-format watch missing repo/branch fields"
            );
            continue;
        }
        let canonical_repo = match crate::mcp::handlers::dispatch_hook::canonicalize_repo_slug(
            raw_repo,
        ) {
            Some(c) => c,
            None => {
                tracing::warn!(
                    path = %path.display(),
                    repo = %raw_repo,
                    "#942/#943 migration: repo string cannot be canonicalized; leaving file in place"
                );
                continue;
            }
        };
        let new_filename = watch_filename(&canonical_repo, raw_branch);
        let new_path = ci_dir.join(&new_filename);
        if new_path == path {
            // Same filename — only possible if the OLD file already had
            // canonical repo + sha256 stem somehow. Idempotent no-op.
            continue;
        }
        if new_path.exists() {
            tracing::warn!(
                old = %path.display(),
                new = %new_path.display(),
                "#942/#943 migration: target already exists (likely two old files mapped to same canonical); skipping rename"
            );
            continue;
        }
        // Update body's `repo` field to canonical form before write.
        watch["repo"] = serde_json::json!(canonical_repo);
        let new_body = match serde_json::to_string_pretty(&watch) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "#942/#943 migration: failed to re-serialize watch JSON"
                );
                continue;
            }
        };
        // Atomic-ish: write new file first, then remove old. Both safe to
        // re-run if interrupted (idempotent on retry).
        if let Err(e) = std::fs::write(&new_path, new_body) {
            tracing::warn!(
                path = %new_path.display(),
                error = %e,
                "#942/#943 migration: failed to write new-format file"
            );
            continue;
        }
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "#942/#943 migration: wrote new file but failed to remove old; manual cleanup needed"
            );
            // Don't increment counter — partial migration.
            continue;
        }
        tracing::info!(
            old = %path.display(),
            new = %new_path.display(),
            canonical_repo = %canonical_repo,
            "#942/#943 migration: renamed legacy watch file to canonical+sha256 form"
        );
        renamed += 1;
    }
    renamed
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-942-943-mig-{}-{}-{}",
            tag,
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// #942/#943 mandatory test 4 — boot with old-format files: renamed
    /// to new format, body `repo` field rewritten to canonical.
    #[test]
    fn migration_renames_old_format_to_canonical_sha256() {
        let home = tmp_home("rename");
        let ci_dir = ci_watches_dir(&home);
        std::fs::create_dir_all(&ci_dir).unwrap();
        // Old DefaultHasher-style filename: 16 hex chars. Body's repo is
        // non-canonical (`.git` suffix).
        let old_name = "aabbccdd11223344.json";
        let body = serde_json::json!({
            "repo": "owner/repo.git",
            "branch": "feat/x",
            "interval_secs": 60,
            "subscribers": [{"instance": "dev", "subscribed_at": "2026-05-19T00:00:00Z"}],
        });
        std::fs::write(
            ci_dir.join(old_name),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .unwrap();

        let renamed = migrate_legacy_watch_filenames(&home);
        assert_eq!(renamed, 1, "exactly one file must be renamed");

        // Old file gone.
        assert!(
            !ci_dir.join(old_name).exists(),
            "old-format file must be removed post-migration"
        );

        // New file: 64-hex sha256 stem; body has canonical repo.
        let new_filename = watch_filename("owner/repo", "feat/x");
        let new_path = ci_dir.join(&new_filename);
        assert!(
            new_path.exists(),
            "new-format file must exist at sha256(canonical:branch).json"
        );
        let new_body: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&new_path).unwrap()).unwrap();
        assert_eq!(
            new_body["repo"].as_str(),
            Some("owner/repo"),
            "body's `repo` field must be rewritten to canonical form"
        );
        assert_eq!(
            new_body["branch"].as_str(),
            Some("feat/x"),
            "branch field preserved verbatim"
        );
        // Subscribers preserved.
        let subs = new_body["subscribers"].as_array().unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0]["instance"].as_str(), Some("dev"));

        std::fs::remove_dir_all(&home).ok();
    }

    /// #942/#943 mandatory test 5 — new-format file (canonical sha256
    /// filename + canonical body) is NOT touched by migration. Idempotent.
    #[test]
    fn migration_leaves_new_format_files_intact() {
        let home = tmp_home("idempotent");
        let ci_dir = ci_watches_dir(&home);
        std::fs::create_dir_all(&ci_dir).unwrap();

        // Pre-migration target file with canonical name.
        let new_filename = watch_filename("owner/repo", "feat/y");
        let original_body = serde_json::json!({
            "repo": "owner/repo",
            "branch": "feat/y",
            "subscribers": [{"instance": "agent", "subscribed_at": "2026-05-19T00:00:00Z"}],
        });
        let original_serialized = serde_json::to_string_pretty(&original_body).unwrap();
        std::fs::write(ci_dir.join(&new_filename), &original_serialized).unwrap();

        let renamed = migrate_legacy_watch_filenames(&home);
        assert_eq!(renamed, 0, "no renames for already-canonical files");
        assert!(
            ci_dir.join(&new_filename).exists(),
            "new-format file must remain on disk"
        );
        // Body byte-identical (no spurious rewrite).
        let after = std::fs::read_to_string(ci_dir.join(&new_filename)).unwrap();
        assert_eq!(
            after, original_serialized,
            "new-format file body must be byte-identical post-migration"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Conflict scenario: two old-format files map to the same canonical
    /// target. First-encountered wins; second logged + skipped (NOT a
    /// panic).
    #[test]
    fn migration_handles_conflict_without_panic() {
        let home = tmp_home("conflict");
        let ci_dir = ci_watches_dir(&home);
        std::fs::create_dir_all(&ci_dir).unwrap();
        // Two old files with same canonical (repo, branch).
        let a = "1111111111111111.json";
        let b = "2222222222222222.json";
        let body_a = serde_json::json!({"repo": "owner/repo.git", "branch": "feat/z"});
        let body_b = serde_json::json!({"repo": "owner/repo", "branch": "feat/z"});
        std::fs::write(
            ci_dir.join(a),
            serde_json::to_string_pretty(&body_a).unwrap(),
        )
        .unwrap();
        std::fs::write(
            ci_dir.join(b),
            serde_json::to_string_pretty(&body_b).unwrap(),
        )
        .unwrap();

        // Should not panic.
        let _ = migrate_legacy_watch_filenames(&home);

        // New canonical file exists.
        let new_filename = watch_filename("owner/repo", "feat/z");
        assert!(ci_dir.join(&new_filename).exists());

        std::fs::remove_dir_all(&home).ok();
    }
}
