use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MergeReceipt {
    pub repo: String,
    pub merge_sha: String,
    pub task_id: String,
    pub requesting_agent: String,
    pub pr_number: u64,
    pub created_at: String,
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
    let content = std::fs::read_to_string(path).ok()?;
    let receipt: MergeReceipt = serde_json::from_str(&content).ok()?;
    if receipt.repo == repo && receipt.merge_sha == merge_sha && receipt.task_id == task_id {
        Some(receipt)
    } else {
        None
    }
}
