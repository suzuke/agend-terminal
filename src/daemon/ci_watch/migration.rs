//! #942 + #943 boot-time migration — RED STUB.
//!
//! GREEN commit replaces with real implementation: scan ci-watches dir,
//! rename old-format files (DefaultHasher 16-hex + non-canonical repo)
//! to new sha256+canonical form. Active migration (option b from
//! dev-2 cross-audit Pushback 3) prevents 72h duplicate-notification
//! window operators were seeing.

use std::path::Path;

use super::registry::{ci_watches_dir, watch_filename};

/// RED STUB — no-op. GREEN commit lands real impl.
pub fn migrate_legacy_watch_filenames(_home: &Path) -> usize {
    0
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
