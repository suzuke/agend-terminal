use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Global readonly flag — set when available disk space drops below threshold.
pub(super) static DISK_READONLY: AtomicBool = AtomicBool::new(false);

/// Minimum free-space ratio before entering readonly mode.
const LOW_DISK_THRESHOLD: f64 = 0.05;

/// Check available disk space at `path`. Returns true if below threshold.
fn is_disk_low(path: &Path) -> bool {
    use fs4::available_space;
    use fs4::total_space;
    let avail = match available_space(path) {
        Ok(s) => s,
        Err(_) => return false, // can't check → assume OK
    };
    let total = match total_space(path) {
        Ok(s) if s > 0 => s,
        _ => return false,
    };
    (avail as f64 / total as f64) < LOW_DISK_THRESHOLD
}

/// Update the global readonly flag based on disk space at `home`.
/// Called at daemon startup and before each enqueue.
pub fn check_disk_space(home: &Path) {
    let readonly = is_disk_low(home);
    let was = DISK_READONLY.swap(readonly, Ordering::Relaxed);
    if readonly && !was {
        tracing::warn!("inbox entering readonly mode — disk space < 5%");
    } else if !readonly && was {
        tracing::info!("inbox leaving readonly mode — disk space recovered");
    }
}

/// Returns true when inbox is in readonly mode (disk full).
pub fn is_readonly() -> bool {
    DISK_READONLY.load(Ordering::Relaxed)
}

/// Scan the inbox directory for stale `.tmp` files and corrupt JSONL,
/// moving them to `inbox.recovery/<timestamp>/` for forensics.
/// Call once at daemon startup.
pub fn recover_half_writes(home: &Path) {
    let inbox_dir = home.join("inbox");
    if !inbox_dir.exists() {
        return;
    }
    let entries = match std::fs::read_dir(&inbox_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let recovery_dir = home.join("inbox.recovery").join(&ts);
    let mut recovered = 0u32;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Stale tmp files from interrupted atomic appends
        if name_str.ends_with(".tmp") {
            ensure_recovery_dir(&recovery_dir);
            let dest = recovery_dir.join(&name);
            if std::fs::rename(&path, &dest).is_ok() {
                recovered += 1;
            }
            continue;
        }

        // Check JSONL files for corrupt trailing lines
        if name_str.ends_with(".jsonl") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let lines: Vec<&str> = content.lines().collect();
                let bad: Vec<&&str> = lines
                    .iter()
                    .filter(|l| {
                        !l.trim().is_empty()
                            && serde_json::from_str::<super::InboxMessage>(l).is_err()
                    })
                    .collect();
                if !bad.is_empty() {
                    // Move entire file to recovery, agent gets a fresh start
                    ensure_recovery_dir(&recovery_dir);
                    let dest = recovery_dir.join(&name);
                    if std::fs::rename(&path, &dest).is_ok() {
                        recovered += 1;
                    }
                }
            }
        }
    }
    if recovered > 0 {
        tracing::warn!(
            count = recovered,
            dir = %recovery_dir.display(),
            "inbox: recovered half-written files"
        );
    }
}

fn ensure_recovery_dir(dir: &Path) {
    std::fs::create_dir_all(dir).ok();
}
