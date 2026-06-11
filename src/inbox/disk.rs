use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Global readonly flag — set when available disk space drops below threshold.
pub(super) static DISK_READONLY: AtomicBool = AtomicBool::new(false);

/// Default minimum free disk space floor (1 GiB) before entering readonly mode.
const DEFAULT_LOW_DISK_FLOOR_BYTES: u64 = 1024 * 1024 * 1024;

/// Get the low disk space threshold in bytes from `AGEND_LOW_DISK_THRESHOLD` environment variable,
/// falling back to 1 GiB.
pub(super) fn get_low_disk_threshold_bytes() -> u64 {
    std::env::var("AGEND_LOW_DISK_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_LOW_DISK_FLOOR_BYTES)
}

/// Check available disk space at `path`. Returns true if below threshold.
fn is_disk_low(path: &Path) -> bool {
    use fs4::available_space;
    let avail = match available_space(path) {
        Ok(s) => s,
        Err(_) => return false, // can't check → assume OK
    };
    let limit = get_low_disk_threshold_bytes();
    avail < limit
}

/// Update the global readonly flag based on disk space at `home`.
/// Called at daemon startup and before each enqueue.
pub fn check_disk_space(home: &Path) {
    let readonly = is_disk_low(home);
    let was = DISK_READONLY.swap(readonly, Ordering::Relaxed);
    if readonly && !was {
        let limit = get_low_disk_threshold_bytes();
        let limit_gb = limit as f64 / (1024.0 * 1024.0 * 1024.0);
        if limit_gb >= 0.1 {
            tracing::warn!(
                "inbox entering readonly mode — available disk space < {:.1} GB",
                limit_gb
            );
        } else {
            let limit_mb = limit as f64 / (1024.0 * 1024.0);
            tracing::warn!(
                "inbox entering readonly mode — available disk space < {:.0} MB",
                limit_mb
            );
        }
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

        // A corrupt line in a live JSONL inbox (e.g. a truncated trailing line
        // from a crash mid-append — `enqueue` appends in place, not via
        // tmp+rename) must NOT cost the agent its entire queue. Every read path
        // (drain/sweep/unread_count/…) already skips an unparseable line, so the
        // fail-open fix is to rewrite the file keeping only the good lines and
        // preserve the dropped line(s) under inbox.recovery/ for forensics —
        // never silently destroy a valid message. Done under the per-file flock
        // (the same lock `enqueue` takes for this physical file) so a concurrent
        // early-boot send cannot race the rewrite.
        if name_str.ends_with(".jsonl") {
            use std::io::Write;
            let lock_path = path.with_extension("jsonl.lock");
            let Ok(_lock) = crate::store::acquire_file_lock(&lock_path) else {
                continue;
            };
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut kept: Vec<&str> = Vec::new();
            let mut bad: Vec<&str> = Vec::new();
            for l in content.lines() {
                if l.trim().is_empty() || serde_json::from_str::<super::InboxMessage>(l).is_ok() {
                    kept.push(l);
                } else {
                    bad.push(l);
                }
            }
            if bad.is_empty() {
                continue;
            }
            // Forensics: append only the corrupt line(s) to recovery.
            ensure_recovery_dir(&recovery_dir);
            if let Ok(mut rf) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(recovery_dir.join(&name))
            {
                for l in &bad {
                    let _ = writeln!(rf, "{l}");
                }
            }
            // Rewrite the inbox with only the good lines via tmp + atomic rename
            // (mirrors drain/sweep write-back) so every valid message survives.
            let tmp = path.with_extension("jsonl.tmp");
            let rewrite = (|| -> std::io::Result<()> {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&tmp)?;
                for l in &kept {
                    writeln!(f, "{l}")?;
                }
                f.sync_all()?;
                std::fs::rename(&tmp, &path)?;
                Ok(())
            })();
            if rewrite.is_ok() {
                recovered += 1;
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
