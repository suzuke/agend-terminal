use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bounded lifetime of a merge receipt. Once the assignee's binding has been
/// released, the receipt is the sole completion authority, so the window must
/// outlast a normal settle-after-CI cycle (CI queue + retries + operator
/// latency) without becoming an indefinite grant.
pub(crate) const RECEIPT_TTL_HOURS: i64 = 7 * 24;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MergeReceipt {
    pub repo: String,
    pub merge_sha: String,
    pub task_id: String,
    pub task_assignee: String,
    pub merge_authority: String,
    pub pr_number: u64,
    pub created_at: String,
    pub expires_at: String,
}

fn receipts_dir(home: &Path) -> PathBuf {
    home.join("merge-receipts")
}

fn receipt_key(repo: &str, merge_sha: &str, task_id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    repo.hash(&mut h);
    merge_sha.hash(&mut h);
    task_id.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn task_completion_settlement_path(home: &Path, receipt: &MergeReceipt) -> PathBuf {
    let key = receipt_key(&receipt.repo, &receipt.merge_sha, &receipt.task_id);
    receipts_dir(home).join(format!("{key}.task-completion"))
}

/// Remove a receipt JSON together with its `.task-completion` sidecar. An
/// expired or malformed receipt must not leave the sidecar behind: the sidecar
/// is keyed off the same hash, so its sibling name is derivable from the JSON
/// path alone. Only invalid receipts reach here, so a live receipt's sidecar is
/// never touched.
fn remove_receipt_and_sidecar(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("task-completion"));
}

fn load_valid(path: &Path) -> Option<MergeReceipt> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return None;
    };
    let receipt: MergeReceipt = match serde_json::from_str(&content) {
        Ok(r) => r,
        Err(_) => {
            remove_receipt_and_sidecar(path);
            return None;
        }
    };
    let expected_name = format!(
        "{}.json",
        receipt_key(&receipt.repo, &receipt.merge_sha, &receipt.task_id)
    );
    let identity_valid = path.file_name().and_then(|name| name.to_str())
        == Some(expected_name.as_str())
        && !receipt.repo.is_empty()
        && crate::daemon::ci_watch::is_full_commit_sha(&receipt.merge_sha)
        && !receipt.task_id.is_empty()
        && !receipt.task_assignee.is_empty()
        && receipt.pr_number > 0;
    if !identity_valid {
        return None;
    }
    let created = match chrono::DateTime::parse_from_rfc3339(&receipt.created_at) {
        Ok(created) => created,
        Err(_) => {
            remove_receipt_and_sidecar(path);
            return None;
        }
    };
    let expires = match chrono::DateTime::parse_from_rfc3339(&receipt.expires_at) {
        Ok(expires) => expires,
        Err(_) => {
            remove_receipt_and_sidecar(path);
            return None;
        }
    };
    if expires <= created || chrono::Utc::now() > expires {
        remove_receipt_and_sidecar(path);
        return None;
    }
    Some(receipt)
}

pub(crate) fn persist(home: &Path, receipt: &MergeReceipt) -> Result<(), String> {
    let dir = receipts_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create merge-receipts dir: {e}"))?;
    let key = receipt_key(&receipt.repo, &receipt.merge_sha, &receipt.task_id);
    let path = dir.join(format!("{key}.json"));
    let body = serde_json::to_string_pretty(receipt)
        .map_err(|e| format!("serialize merge receipt: {e}"))?;
    crate::store::atomic_write(&path, body.as_bytes())
        .map_err(|e| format!("write merge receipt: {e}"))
}

pub(crate) fn find(
    home: &Path,
    repo: &str,
    merge_sha: &str,
    task_id: &str,
) -> Option<MergeReceipt> {
    let key = receipt_key(repo, merge_sha, task_id);
    let path = receipts_dir(home).join(format!("{key}.json"));
    let receipt = load_valid(&path)?;
    if receipt.repo != repo || receipt.merge_sha != merge_sha || receipt.task_id != task_id {
        return None;
    }
    Some(receipt)
}

/// Find the one unconsumed merge proof that can replace an intentionally
/// released task binding. The receipt's filename re-proves its repo + merge SHA
/// + task identity; the caller must additionally match the persisted assignee.
///
/// Ambiguity fails closed rather than guessing which merge authorized closure.
pub(crate) fn find_for_task_completion(
    home: &Path,
    task_id: &str,
    task_assignee: &str,
) -> Option<MergeReceipt> {
    let entries = std::fs::read_dir(receipts_dir(home)).ok()?;
    let mut matched = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("json") => {}
            Some("task-completion") => {
                // A sidecar whose receipt JSON is gone is an orphan left by an
                // older expiry path. With no sibling `.json` its key can never
                // be read again, so collect it now instead of letting it
                // accumulate forever. A live receipt's sidecar always has its
                // JSON sibling and is left untouched.
                if !path.with_extension("json").exists() {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }
            _ => continue,
        }
        let Some(receipt) = load_valid(&path) else {
            continue;
        };
        if receipt.task_id == task_id
            && receipt.task_assignee == task_assignee
            && !task_completion_settlement_path(home, &receipt).exists()
        {
            matched.push(receipt);
        }
    }
    if matched.len() == 1 {
        matched.pop()
    } else {
        None
    }
}

/// Mark a merge proof as consumed for task completion without deleting the
/// receipt itself: notification-only CI watch/unwatch still needs that receipt
/// until its own terminal lifecycle removes it.
pub(crate) fn settle_task_completion(home: &Path, receipt: &MergeReceipt) -> Result<(), String> {
    let path = task_completion_settlement_path(home, receipt);
    let body = serde_json::json!({
        "task_id": receipt.task_id,
        "task_assignee": receipt.task_assignee,
        "repo": receipt.repo,
        "merge_sha": receipt.merge_sha,
        "settled_at": chrono::Utc::now().to_rfc3339(),
    });
    let body = serde_json::to_vec_pretty(&body)
        .map_err(|e| format!("serialize task-completion settlement: {e}"))?;
    crate::store::atomic_write(&path, &body)
        .map_err(|e| format!("write task-completion settlement: {e}"))
}

pub(crate) fn remove(home: &Path, repo: &str, merge_sha: &str, task_id: &str) {
    let key = receipt_key(repo, merge_sha, task_id);
    let path = receipts_dir(home).join(format!("{key}.json"));
    let _ = std::fs::remove_file(path);
    let settlement = receipts_dir(home).join(format!("{key}.task-completion"));
    let _ = std::fs::remove_file(settlement);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "agend-merge-receipt-{name}-{}-{n}",
            std::process::id()
        ))
    }

    fn valid_receipt(task_id: &str, assignee: &str, sha_byte: char) -> MergeReceipt {
        let created = chrono::Utc::now();
        MergeReceipt {
            repo: "owner/repo".into(),
            merge_sha: sha_byte.to_string().repeat(40),
            task_id: task_id.into(),
            task_assignee: assignee.into(),
            merge_authority: "lead".into(),
            pr_number: 42,
            created_at: created.to_rfc3339(),
            expires_at: (created + chrono::TimeDelta::hours(1)).to_rfc3339(),
        }
    }

    #[test]
    fn task_completion_receipt_is_exact_and_settled_without_breaking_ci_proof() {
        let home = tmp_home("exact-settlement");
        let receipt = valid_receipt("t-exact", "dev", 'a');
        persist(&home, &receipt).unwrap();

        assert!(find_for_task_completion(&home, "t-other", "dev").is_none());
        assert!(find_for_task_completion(&home, "t-exact", "other").is_none());
        assert!(find_for_task_completion(&home, "t-exact", "dev").is_some());

        settle_task_completion(&home, &receipt).unwrap();
        assert!(
            find_for_task_completion(&home, "t-exact", "dev").is_none(),
            "settled proof must not authorize task completion replay"
        );
        assert!(
            find(&home, &receipt.repo, &receipt.merge_sha, &receipt.task_id).is_some(),
            "settlement must preserve the receipt used by CI watch/unwatch"
        );

        remove(&home, &receipt.repo, &receipt.merge_sha, &receipt.task_id);
        assert!(!task_completion_settlement_path(&home, &receipt).exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn multiple_unconsumed_receipts_for_the_same_task_fail_closed() {
        let home = tmp_home("ambiguous");
        let first = valid_receipt("t-ambiguous", "dev", 'c');
        let second = valid_receipt("t-ambiguous", "dev", 'd');
        persist(&home, &first).unwrap();
        persist(&home, &second).unwrap();

        assert!(
            find_for_task_completion(&home, "t-ambiguous", "dev").is_none(),
            "multiple candidate merges must not authorize task completion"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn expired_and_malformed_receipts_never_authorize_task_completion() {
        let home = tmp_home("invalid");
        let mut expired = valid_receipt("t-expired", "dev", 'b');
        let created = chrono::Utc::now() - chrono::TimeDelta::hours(2);
        expired.created_at = created.to_rfc3339();
        expired.expires_at = (created + chrono::TimeDelta::hours(1)).to_rfc3339();
        persist(&home, &expired).unwrap();
        assert!(find_for_task_completion(&home, "t-expired", "dev").is_none());

        let dir = receipts_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let malformed = dir.join("malformed.json");
        std::fs::write(&malformed, b"{not-json").unwrap();
        assert!(find_for_task_completion(&home, "t-any", "dev").is_none());
        assert!(!malformed.exists(), "malformed receipt must be quarantined");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn expired_receipt_scan_collects_its_task_completion_sidecar() {
        let home = tmp_home("expired-sidecar-gc");
        let mut expired = valid_receipt("t-expired-sidecar", "dev", 'e');
        let created = chrono::Utc::now() - chrono::TimeDelta::hours(2);
        expired.created_at = created.to_rfc3339();
        expired.expires_at = (created + chrono::TimeDelta::hours(1)).to_rfc3339();
        persist(&home, &expired).unwrap();
        settle_task_completion(&home, &expired).unwrap();
        let sidecar = task_completion_settlement_path(&home, &expired);
        assert!(sidecar.exists(), "sidecar fixture must be written");

        assert!(find_for_task_completion(&home, "t-expired-sidecar", "dev").is_none());
        assert!(
            !sidecar.exists(),
            "expired receipt scan must collect its task-completion sidecar"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn valid_receipt_scan_preserves_its_task_completion_sidecar() {
        let home = tmp_home("valid-sidecar-preserve");
        let receipt = valid_receipt("t-keep-sidecar", "dev", 'f');
        persist(&home, &receipt).unwrap();
        settle_task_completion(&home, &receipt).unwrap();
        let sidecar = task_completion_settlement_path(&home, &receipt);
        assert!(sidecar.exists());

        assert!(find_for_task_completion(&home, "t-keep-sidecar", "dev").is_none());
        assert!(
            sidecar.exists(),
            "a live receipt's settlement sidecar must survive the scan"
        );
        assert!(find(&home, &receipt.repo, &receipt.merge_sha, &receipt.task_id).is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn orphan_task_completion_sidecar_without_receipt_is_collected_on_scan() {
        let home = tmp_home("orphan-sidecar");
        let receipt = valid_receipt("t-orphan-sidecar", "dev", '1');
        settle_task_completion(&home, &receipt).unwrap();
        let sidecar = task_completion_settlement_path(&home, &receipt);
        assert!(sidecar.exists(), "orphan fixture must exist");

        assert!(find_for_task_completion(&home, "t-nobody", "dev").is_none());
        assert!(
            !sidecar.exists(),
            "a sidecar with no receipt JSON must be collected"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
