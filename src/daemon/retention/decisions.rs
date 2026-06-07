//! Decisions retention — archive expired decisions under per-id lock.
//!
//! Uses `pub(crate) with_decision_lock` from src/decisions.rs for
//! serialization with concurrent post/update operations.

use std::path::Path;

const DEFAULT_TTL_DAYS: u64 = 90;
const MIN_AGE_DAYS: u64 = 14;

fn archive_dir(home: &Path) -> std::path::PathBuf {
    crate::decisions::decisions_dir(home).join(".archive")
}

fn protected_tags(home: &Path) -> Vec<String> {
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    let Ok(content) = std::fs::read_to_string(&fleet_path) else {
        return Vec::new();
    };
    let Ok(doc): Result<serde_yaml_ng::Value, _> = serde_yaml_ng::from_str(&content) else {
        return Vec::new();
    };
    doc.get("retention")
        .and_then(|r| r.get("protected_decision_tags"))
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn is_protected(decision: &crate::decisions::Decision, protected: &[String]) -> bool {
    if !protected.is_empty() {
        for tag in &decision.tags {
            if protected.contains(tag) {
                return true;
            }
        }
    }
    false
}

fn age_days(created_at: &str) -> Option<u64> {
    let created = chrono::DateTime::parse_from_rfc3339(created_at).ok()?;
    let now = chrono::Utc::now();
    let duration = now.signed_duration_since(created);
    Some(duration.num_days().max(0) as u64)
}

fn ttl_for(decision: &crate::decisions::Decision) -> u64 {
    decision.ttl_days.unwrap_or(DEFAULT_TTL_DAYS)
}

pub(crate) fn archive_decision(home: &Path, id: &str) -> anyhow::Result<bool> {
    let archive = archive_dir(home);
    std::fs::create_dir_all(&archive)?;

    let archived = crate::decisions::with_decision_lock(home, id, || {
        let src = crate::decisions::decision_path(home, id);
        if !src.exists() {
            return false;
        }
        let dst = archive.join(format!("{id}.json"));
        match std::fs::rename(&src, &dst) {
            Ok(()) => {
                tracing::info!(id, "decision archived");
                true
            }
            Err(e) => {
                tracing::warn!(id, error = %e, "decision archive failed, keeping");
                false
            }
        }
    })?;
    Ok(archived)
}

/// True when the decisions retention sweep is enabled.
///
/// #env-cleanup decouple: the decisions sweep now has its OWN opt-in flag,
/// `AGEND_RETENTION_DECISIONS_CUTOVER=1`, independent of the pending-dispatch
/// kill-switch `AGEND_RETENTION_CUTOVER` (read in `retention/mod.rs` with the
/// OPPOSITE polarity — opt-OUT). Sharing one name made the `pending-OFF +
/// decisions-ON` combination unreachable (reviewer-2 finding). Legacy fallback
/// (deprecation window): the old shared `AGEND_RETENTION_CUTOVER=1` STILL
/// enables the decisions sweep, so operators who already opted in aren't broken
/// — prefer the new flag going forward.
///
/// Decisions-sweep truth table:
///   `AGEND_RETENTION_DECISIONS_CUTOVER=1`        → ON
///   else `AGEND_RETENTION_CUTOVER=1` (legacy)    → ON
///   else                                         → OFF (default)
fn decisions_cutover_enabled() -> bool {
    std::env::var("AGEND_RETENTION_DECISIONS_CUTOVER").as_deref() == Ok("1")
        || std::env::var("AGEND_RETENTION_CUTOVER").as_deref() == Ok("1")
}

/// Sweep expired decisions. Gated on [`decisions_cutover_enabled`].
/// Returns number of decisions archived.
pub(super) fn sweep(home: &Path) -> usize {
    if !decisions_cutover_enabled() {
        return 0;
    }
    let dir = crate::decisions::decisions_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let protected = protected_tags(home);
    let _sentinel = crate::store::acquire_file_lock(&dir.join(".archive.lock"));

    let mut archived = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(decision): Result<crate::decisions::Decision, _> = serde_json::from_str(&content)
        else {
            continue;
        };
        if decision.archived {
            continue;
        }

        let Some(age) = age_days(&decision.created_at) else {
            continue;
        };

        // 14d floor: never archive decisions newer than MIN_AGE_DAYS
        if age < MIN_AGE_DAYS {
            continue;
        }

        if is_protected(&decision, &protected) {
            continue;
        }

        let ttl = ttl_for(&decision);
        if age < ttl {
            continue;
        }

        match archive_decision(home, &decision.id) {
            Ok(true) => {
                archived += 1;
                crate::event_log::log(
                    home,
                    "retention_decision_archived",
                    &decision.id,
                    &format!("age={age}d ttl={ttl}d"),
                );
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(id = %decision.id, error = %e, "retention: decision archive error");
            }
        }
    }
    archived
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    // The `sweep_*` tests set/remove the process-global `AGEND_RETENTION_CUTOVER`
    // env var that `sweep()` reads — running them concurrently lets one test's
    // `remove_var` clear the gate before another's `sweep()` reads it, so the
    // latter early-returns 0 and its `assert_eq!(archived, 1)` flakes (reddened
    // #1752 CI). Serialize them under a named group (mirrors capture.rs's
    // `#[serial(capture_env)]`); non-env retention tests stay parallel.
    use serial_test::serial;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-retention-decisions-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn write_decision(home: &Path, id: &str, age_days: i64, tags: &[&str]) {
        let dir = crate::decisions::decisions_dir(home);
        std::fs::create_dir_all(&dir).unwrap();
        let created_at =
            (chrono::Utc::now() - chrono::TimeDelta::try_days(age_days).unwrap()).to_rfc3339();
        let decision = serde_json::json!({
            "id": id,
            "title": format!("test decision {id}"),
            "content": "test",
            "scope": "project",
            "author": "test",
            "tags": tags,
            "ttl_days": 90,
            "created_at": created_at,
            "updated_at": created_at,
            "archived": false,
            "supersedes": null,
            "working_directory": null,
        });
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string_pretty(&decision).unwrap(),
        )
        .unwrap();
    }

    /// T6: post + archive same-id serialization via with_decision_lock
    #[test]
    fn archive_serializes_with_decision_lock() {
        let home = tmp_home("archive-lock");
        write_decision(&home, "d-lock-test", 100, &[]);

        let home1 = home.clone();
        let home2 = home.clone();
        let h1 = std::thread::spawn(move || archive_decision(&home1, "d-lock-test"));
        let h2 = std::thread::spawn(move || archive_decision(&home2, "d-lock-test"));

        let r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();

        // Exactly one succeeds (file moved), one returns Ok(false) (file gone)
        let successes = [&r1, &r2].iter().filter(|r| matches!(r, Ok(true))).count();
        assert_eq!(successes, 1, "exactly one archive should succeed");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial(retention_cutover)]
    fn sweep_archives_expired_decisions() {
        let home = tmp_home("sweep-expired");
        write_decision(&home, "d-old", 100, &[]);
        write_decision(&home, "d-young", 5, &[]);

        std::env::set_var("AGEND_RETENTION_DECISIONS_CUTOVER", "1");
        let archived = sweep(&home);
        std::env::remove_var("AGEND_RETENTION_DECISIONS_CUTOVER");

        assert_eq!(archived, 1);
        let archive = archive_dir(&home);
        assert!(archive.join("d-old.json").exists());
        assert!(!archive.join("d-young.json").exists());
        // Young decision still in place
        assert!(crate::decisions::decision_path(&home, "d-young").exists());

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial(retention_cutover)]
    fn sweep_respects_protected_tags() {
        let home = tmp_home("sweep-protected");
        write_decision(&home, "d-protected", 100, &["SPRINT_99"]);
        write_decision(&home, "d-unprotected", 100, &[]);

        // Write fleet.yaml with protected tags
        let fleet_path = crate::fleet::fleet_yaml_path(&home);
        std::fs::write(
            &fleet_path,
            "retention:\n  protected_decision_tags:\n    - SPRINT_99\n",
        )
        .unwrap();

        std::env::set_var("AGEND_RETENTION_DECISIONS_CUTOVER", "1");
        let archived = sweep(&home);
        std::env::remove_var("AGEND_RETENTION_DECISIONS_CUTOVER");

        assert_eq!(archived, 1);
        // Protected decision still in place
        assert!(crate::decisions::decision_path(&home, "d-protected").exists());
        // Unprotected archived
        assert!(archive_dir(&home).join("d-unprotected.json").exists());

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial(retention_cutover)]
    fn sweep_enforces_14d_floor() {
        let home = tmp_home("sweep-floor");
        // Decision with ttl_days=1 but only 10 days old — 14d floor protects it
        let dir = crate::decisions::decisions_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let created_at =
            (chrono::Utc::now() - chrono::TimeDelta::try_days(10).unwrap()).to_rfc3339();
        let decision = serde_json::json!({
            "id": "d-floor",
            "title": "test",
            "content": "test",
            "scope": "project",
            "author": "test",
            "tags": [],
            "ttl_days": 1,
            "created_at": created_at,
            "updated_at": created_at,
            "archived": false,
            "supersedes": null,
            "working_directory": null,
        });
        std::fs::write(
            dir.join("d-floor.json"),
            serde_json::to_string_pretty(&decision).unwrap(),
        )
        .unwrap();

        std::env::set_var("AGEND_RETENTION_DECISIONS_CUTOVER", "1");
        let archived = sweep(&home);
        std::env::remove_var("AGEND_RETENTION_DECISIONS_CUTOVER");

        assert_eq!(
            archived, 0,
            "14d floor should prevent archive of 10d-old decision"
        );
        assert!(crate::decisions::decision_path(&home, "d-floor").exists());

        std::fs::remove_dir_all(&home).ok();
    }

    /// T14: pub(crate) visibility compiles — archive_decision calls
    /// with_decision_lock via crate::decisions::with_decision_lock
    #[test]
    fn pub_crate_visibility_compiles() {
        let home = tmp_home("visibility");
        // This test exists solely to verify that the pub(crate) upgrade
        // allows cross-module access. If it compiles, the contract holds.
        let _ = archive_decision(&home, "d-nonexistent");
        std::fs::remove_dir_all(&home).ok();
    }

    /// #env-cleanup decouple: legacy fallback — the OLD shared
    /// `AGEND_RETENTION_CUTOVER=1` must STILL enable the decisions sweep so
    /// operators who already opted in via the old name aren't broken.
    #[test]
    #[serial(retention_cutover)]
    fn legacy_retention_cutover_still_enables_decisions_sweep() {
        let home = tmp_home("legacy-cutover");
        write_decision(&home, "d-old", 100, &[]);

        std::env::remove_var("AGEND_RETENTION_DECISIONS_CUTOVER");
        std::env::set_var("AGEND_RETENTION_CUTOVER", "1");
        let archived = sweep(&home);
        std::env::remove_var("AGEND_RETENTION_CUTOVER");

        assert_eq!(
            archived, 1,
            "legacy AGEND_RETENTION_CUTOVER=1 must still enable the decisions sweep"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #env-cleanup decouple: the formerly-unreachable combination
    /// (reviewer-2 finding). Pending-dispatch kill-switch OFF
    /// (`AGEND_RETENTION_CUTOVER=0`) while the decisions sweep is ON via its
    /// own `AGEND_RETENTION_DECISIONS_CUTOVER=1` — the whole point of the
    /// decouple. The pending `=="0"` must NOT suppress the decisions sweep.
    #[test]
    #[serial(retention_cutover)]
    fn decisions_decoupled_from_pending_killswitch() {
        let home = tmp_home("decoupled");
        write_decision(&home, "d-old", 100, &[]);

        std::env::set_var("AGEND_RETENTION_CUTOVER", "0"); // pending OFF
        std::env::set_var("AGEND_RETENTION_DECISIONS_CUTOVER", "1"); // decisions ON
        let archived = sweep(&home);
        std::env::remove_var("AGEND_RETENTION_CUTOVER");
        std::env::remove_var("AGEND_RETENTION_DECISIONS_CUTOVER");

        assert_eq!(
            archived, 1,
            "decisions sweep must run independently of the pending-dispatch \
             kill-switch (AGEND_RETENTION_CUTOVER=0 must not disable it)"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
