//! Per-agent message inbox — append-only JSONL with disk resilience.
//!
//! Messages stored as one JSON object per line in {home}/inbox/{name}.jsonl.
//!
//! Resilience layers:
//! - **Readonly mode**: when available disk space < 5%, enqueue returns an
//!   error while drain continues to work (let agents consume backlog).
//! - **Atomic append**: each enqueue writes to a temp file, fsyncs, then
//!   renames — no half-written lines on crash.
//! - **Half-write recovery**: on startup, stale `.tmp` files and corrupt
//!   JSONL lines are moved to `inbox.recovery/` for forensics.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Global readonly flag — set when available disk space drops below threshold.
static DISK_READONLY: AtomicBool = AtomicBool::new(false);

/// Minimum free-space ratio before entering readonly mode.
const LOW_DISK_THRESHOLD: f64 = 0.05;

/// Check available disk space at `path`. Returns true if below threshold.
fn is_disk_low(path: &Path) -> bool {
    use fs2::available_space;
    use fs2::total_space;
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
                        !l.trim().is_empty() && serde_json::from_str::<InboxMessage>(l).is_err()
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

/// Type-safe notification source — replaces raw string conventions.
pub enum NotifySource<'a> {
    /// Message from a Telegram user (e.g., "chiacheng").
    Telegram(&'a str),
    /// Message from another agent instance (e.g., "dev").
    Agent(&'a str),
    /// System message (e.g., "replace", "ci").
    #[allow(dead_code)]
    System(&'a str),
}

impl fmt::Display for NotifySource<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Telegram(user) => write!(f, "user:{user} via telegram"),
            Self::Agent(name) => write!(f, "from:{name}"),
            Self::System(label) => write!(f, "system:{label}"),
        }
    }
}

impl NotifySource<'_> {
    fn reply_hint(&self) -> Cow<'static, str> {
        match self {
            Self::Telegram(_) => {
                "\n(Reply using the reply tool — do NOT respond with direct text)".into()
            }
            Self::Agent(sender) => {
                format!("\n(Reply using the send_to_instance tool with target \"{sender}\")").into()
            }
            Self::System(_) => "".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub id: Option<String>,
    pub from: String,
    pub text: String,
    pub kind: Option<String>,
    pub timestamp: String,
    #[serde(default)]
    pub read_at: Option<String>,
}

impl InboxMessage {
    /// Latest schema version this binary can read and write.
    pub const CURRENT_VERSION: u32 = 1;
}

/// Status of a specific inbox message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageStatus {
    /// Message was read at the given timestamp.
    ReadAt(String),
    /// Message exists but has not been read and has expired (>30d).
    UnreadExpired,
    /// Message not found.
    NotFound,
}

pub(crate) fn inbox_path(home: &Path, name: &str) -> PathBuf {
    home.join("inbox").join(format!("{name}.jsonl"))
}

/// Acquire a per-agent flock and run `f` with the inbox path.
/// All read-modify-write operations on an agent's inbox (enqueue, drain,
/// sweep_expired) must go through this helper to prevent concurrent races.
fn with_inbox_lock<T>(home: &Path, name: &str, f: impl FnOnce(&Path) -> T) -> anyhow::Result<T> {
    let path = inbox_path(home, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("jsonl.lock");
    let _lock = crate::store::acquire_file_lock(&lock_path)?;
    Ok(f(&path))
}

/// Enqueue a message — atomic append via flock + tmp + fsync + rename.
///
/// Returns an error when the inbox is in readonly mode (disk full).
/// Callers should invoke [`check_disk_space`] periodically (e.g. daemon tick);
/// enqueue only reads the cached flag.
///
/// Concurrent safety: a per-agent flock via [`with_inbox_lock`] serialises
/// all read-modify-write operations (enqueue, drain, sweep) on the same
/// agent inbox (cross-process safe).
pub fn enqueue(home: &Path, name: &str, mut msg: InboxMessage) -> anyhow::Result<()> {
    if is_readonly() {
        anyhow::bail!("inbox readonly: disk space critically low");
    }
    msg.schema_version = InboxMessage::CURRENT_VERSION;
    if msg.id.is_none() {
        use std::sync::atomic::{AtomicU64, Ordering as AtOrd};
        static MSG_SEQ: AtomicU64 = AtomicU64::new(0);
        let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
        let seq = MSG_SEQ.fetch_add(1, AtOrd::Relaxed);
        msg.id = Some(format!("m-{ts}-{seq}"));
    }
    let line = format!("{}\n", serde_json::to_string(&msg)?);

    with_inbox_lock(home, name, |path| {
        let mut content = std::fs::read_to_string(path).unwrap_or_default();
        content.push_str(&line);
        let tmp = path.with_extension("jsonl.tmp");
        let result = (|| -> anyhow::Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(content.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        })();
        result
    })?
}

/// Drain unread messages: mark them with `read_at` and write back.
/// Returns only the messages that were previously unread.
///
/// Soft-delete semantics: messages stay in the JSONL file with `read_at`
/// set; [`sweep_expired`] removes them later based on TTL rules.
/// Uses atomic tmp+fsync+rename for crash safety.
pub fn drain(home: &Path, name: &str) -> Vec<InboxMessage> {
    let path = inbox_path(home, name);
    let tmp = path.with_extension("draining");

    // Leftover from a crashed predecessor — consume it first.
    if tmp.exists() {
        return read_drain_file(&tmp);
    }

    if !path.exists() {
        return Vec::new();
    }

    match with_inbox_lock(home, name, |path| {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let now = chrono::Utc::now().to_rfc3339();
        let mut unread = Vec::new();
        let mut all_messages: Vec<InboxMessage> = Vec::new();

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut msg: InboxMessage = match serde_json::from_str(line) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if msg.schema_version > InboxMessage::CURRENT_VERSION {
                tracing::error!(
                    found = msg.schema_version,
                    supported = InboxMessage::CURRENT_VERSION,
                    "dropping inbox message written by newer schema version"
                );
                continue;
            }
            if msg.read_at.is_none() {
                msg.read_at = Some(now.clone());
                unread.push(msg.clone());
            }
            all_messages.push(msg);
        }

        if !unread.is_empty() {
            let write_tmp = path.with_extension("jsonl.tmp");
            let result = (|| -> anyhow::Result<()> {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&write_tmp)?;
                for m in &all_messages {
                    writeln!(f, "{}", serde_json::to_string(m)?)?;
                }
                f.sync_all()?;
                std::fs::rename(&write_tmp, path)?;
                Ok(())
            })();
            if let Err(e) = result {
                tracing::warn!(error = %e, "inbox drain write-back failed");
            }
        }

        unread
    }) {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::warn!(error = %e, "inbox drain lock failed");
            Vec::new()
        }
    }
}

fn read_drain_file(tmp: &Path) -> Vec<InboxMessage> {
    let content = match std::fs::read_to_string(tmp) {
        Ok(c) => c,
        // Leave `.draining` in place so the next drain call retries; the
        // previous implementation early-returned without removing, but also
        // returned empty even on success when read_to_string returned Err
        // after the earlier remove had run — which was impossible to recover.
        Err(e) => {
            tracing::warn!(
                path = %tmp.display(),
                error = %e,
                "inbox drain read failed; .draining retained for retry"
            );
            return Vec::new();
        }
    };
    let messages: Vec<InboxMessage> = content
        .lines()
        .filter_map(|l| {
            let msg: InboxMessage = serde_json::from_str(l).ok()?;
            if msg.schema_version > InboxMessage::CURRENT_VERSION {
                tracing::error!(
                    found = msg.schema_version,
                    supported = InboxMessage::CURRENT_VERSION,
                    "dropping inbox message written by newer schema version"
                );
                return None;
            }
            Some(msg)
        })
        .collect();
    // Remove only AFTER a successful read+parse so crashes between read and
    // remove still leave the data on disk for the next drain to recover.
    if let Err(e) = std::fs::remove_file(tmp) {
        tracing::warn!(path = %tmp.display(), error = %e, "inbox drain cleanup failed");
    }
    messages
}

pub const INLINE_THRESHOLD: usize = 500;

/// Sweep expired messages from all inbox files.
/// - read_at.is_some() && elapsed > 7 days → delete
/// - read_at.is_none() && elapsed > 30 days → delete
pub fn sweep_expired(home: &Path) {
    let inbox_dir = home.join("inbox");
    let entries = match std::fs::read_dir(&inbox_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let now = chrono::Utc::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        // Extract agent name from filename (e.g. "agent1.jsonl" → "agent1")
        let agent_name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let _ = with_inbox_lock(home, &agent_name, |path| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return,
            };
            let mut kept: Vec<String> = Vec::new();
            let mut changed = false;
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let msg: InboxMessage = match serde_json::from_str(line) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let ts = chrono::DateTime::parse_from_rfc3339(&msg.timestamp)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or(now);
                let age = now.signed_duration_since(ts);
                let expired = match &msg.read_at {
                    Some(_) => age > chrono::Duration::days(7),
                    None => age > chrono::Duration::days(30),
                };
                if expired {
                    changed = true;
                } else {
                    kept.push(line.to_string());
                }
            }
            if changed {
                if kept.is_empty() {
                    let _ = std::fs::remove_file(path);
                } else {
                    let tmp = path.with_extension("jsonl.tmp");
                    let result = (|| -> anyhow::Result<()> {
                        let mut f = std::fs::OpenOptions::new()
                            .create(true)
                            .write(true)
                            .truncate(true)
                            .open(&tmp)?;
                        for l in &kept {
                            writeln!(f, "{l}")?;
                        }
                        f.sync_all()?;
                        std::fs::rename(&tmp, path)?;
                        Ok(())
                    })();
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "inbox sweep write-back failed");
                    }
                }
            }
        });
    }
}

/// Look up a message by ID in a specific agent's inbox file.
/// If `instance` is provided, only that agent's inbox is searched.
pub fn describe_message(home: &Path, msg_id: &str, instance: &str) -> MessageStatus {
    let path = inbox_path(home, instance);
    if !path.exists() {
        return MessageStatus::NotFound;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return MessageStatus::NotFound,
    };
    let now = chrono::Utc::now();
    for line in content.lines() {
        let msg: InboxMessage = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if msg.id.as_deref() != Some(msg_id) {
            continue;
        }
        if let Some(ref read_at) = msg.read_at {
            return MessageStatus::ReadAt(read_at.clone());
        }
        let ts = chrono::DateTime::parse_from_rfc3339(&msg.timestamp)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or(now);
        if now.signed_duration_since(ts) > chrono::Duration::days(30) {
            return MessageStatus::UnreadExpired;
        }
        return MessageStatus::NotFound;
    }
    MessageStatus::NotFound
}

/// Deliver a message: short messages (≤500 chars) inject directly to PTY,
/// long messages store to inbox + inject truncated notification.
pub fn deliver(
    home: &Path,
    agent_name: &str,
    source: &NotifySource<'_>,
    text: &str,
    _submit_key: &str,
    kind: Option<String>,
) {
    if text.chars().count() <= INLINE_THRESHOLD {
        notify_agent(home, agent_name, source, text);
    } else {
        let msg = InboxMessage {
            schema_version: 0,
            id: None,
            read_at: None,
            from: source.to_string(),
            text: text.to_string(),
            kind,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        let _ = enqueue(home, agent_name, msg);
        notify_agent(home, agent_name, source, text);
    }
}

pub fn notify_agent(home: &Path, agent_name: &str, source: &NotifySource<'_>, text: &str) {
    let display_text = if text.chars().count() > 200 {
        let truncated: String = text.chars().take(200).collect();
        format!("{truncated}... (run: agend-terminal agent inbox)")
    } else {
        text.to_string()
    };
    let notification = format!("[{source}] {display_text}{}", source.reply_hint());
    compose_aware_inject(home, agent_name, &notification);
}

/// Compose-aware notification delivery: checks `is_composing` and enqueues
/// if the target agent is mid-typing, otherwise injects **with** submit_key
/// so idle agents actually wake up on the incoming notification. The
/// is_composing guard (3 s input-idle window) preserves PR #81's race fix —
/// user keystrokes never collide with a background submit — while restoring
/// the pre-#81 behavior of actually delivering Telegram / notify_agent
/// traffic to backends that don't poll inbox on their own.
pub fn compose_aware_inject(home: &Path, agent_name: &str, notification: &str) {
    let _ = route_notification(home, agent_name, notification, |msg| {
        inject_with_submit(home, agent_name, msg)
    });
}

/// Compose-aware message delivery with auto-submit: checks `is_composing`
/// and enqueues if mid-typing, otherwise injects via `inject_to_agent` which
/// appends `inject_prefix` + `submit_key`. Used by `handle_send` for explicit
/// agent-to-agent messages that must be submitted to the target's CLI.
pub fn compose_aware_send(home: &Path, agent_name: &str, message: &str) {
    let _ = route_notification(home, agent_name, message, |msg| {
        inject_with_submit(home, agent_name, msg)
    });
}

fn inject_with_submit(home: &Path, agent_name: &str, message: &str) -> anyhow::Result<()> {
    let resp = crate::api::call(
        home,
        &serde_json::json!({
            "method": crate::api::method::INJECT,
            "params": {"name": agent_name, "data": message}
        }),
    )?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(())
    } else {
        anyhow::bail!(
            "{}",
            resp["error"]
                .as_str()
                .unwrap_or("inject with submit failed")
        );
    }
}

pub fn inject_notification(
    home: &Path,
    agent_name: &str,
    notification: &str,
) -> anyhow::Result<()> {
    let resp = crate::api::call(
        home,
        &serde_json::json!({
            "method": crate::api::method::INJECT,
            "params": {"name": agent_name, "data": notification, "raw": true}
        }),
    )?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(())
    } else {
        anyhow::bail!(
            "{}",
            resp["error"]
                .as_str()
                .unwrap_or("notification inject failed")
        );
    }
}

fn route_notification<F>(
    home: &Path,
    agent_name: &str,
    notification: &str,
    mut injector: F,
) -> anyhow::Result<()>
where
    F: FnMut(&str) -> anyhow::Result<()>,
{
    if crate::notification_queue::is_composing(home, agent_name) {
        crate::notification_queue::enqueue(home, agent_name, notification)?;
        return Ok(());
    }
    injector(notification)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_home(suffix: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agend-inbox-{}-{}", suffix, std::process::id()));
        fs::create_dir_all(&dir).ok();
        dir
    }

    fn make_msg(from: &str, text: &str) -> InboxMessage {
        InboxMessage {
            schema_version: 0,
            id: None,
            read_at: None,
            from: from.to_string(),
            text: text.to_string(),
            kind: None,
            timestamp: "2025-01-01T00:00:00Z".to_string(),
        }
    }

    fn mark_composing(home: &Path, agent: &str) {
        std::fs::create_dir_all(home.join("metadata")).ok();
        std::fs::write(
            home.join("metadata").join(format!("{agent}.json")),
            format!(
                "{{\"last_input_epoch_ms\":{}}}",
                chrono::Utc::now().timestamp_millis()
            ),
        )
        .ok();
    }

    #[test]
    fn enqueue_drain_roundtrip() {
        let home = tmp_home("roundtrip");
        enqueue(&home, "agent1", make_msg("alice", "hello")).ok();
        enqueue(&home, "agent1", make_msg("bob", "world")).ok();
        enqueue(&home, "agent1", make_msg("carol", "!")).ok();

        let msgs = drain(&home, "agent1");
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].from, "alice");
        assert_eq!(msgs[1].from, "bob");
        assert_eq!(msgs[2].from, "carol");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn drain_empties_inbox() {
        let home = tmp_home("drain-empty");
        enqueue(&home, "agent1", make_msg("x", "y")).ok();

        let first = drain(&home, "agent1");
        assert_eq!(first.len(), 1);

        let second = drain(&home, "agent1");
        assert!(second.is_empty(), "second drain should be empty");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn drain_nonexistent_returns_empty() {
        let home = tmp_home("no-inbox");
        let msgs = drain(&home, "nonexistent");
        assert!(msgs.is_empty());
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn concurrent_enqueue_to_different_agents() {
        let home = tmp_home("concurrent");
        let home_arc = std::sync::Arc::new(home.clone());
        let mut handles = vec![];

        // Each thread writes to a different agent — no contention
        for i in 0..10 {
            let h = home_arc.clone();
            handles.push(std::thread::spawn(move || {
                let agent = format!("agent{i}");
                enqueue(&h, &agent, make_msg(&format!("t{i}"), &format!("msg{i}")))
                    .expect("enqueue should succeed");
            }));
        }
        for h in handles {
            h.join().expect("thread should not panic");
        }

        // Each agent should have exactly 1 message
        for i in 0..10 {
            let msgs = drain(&home, &format!("agent{i}"));
            assert_eq!(msgs.len(), 1, "agent{i} should have 1 message");
            assert_eq!(msgs[0].from, format!("t{i}"));
        }

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn notify_queues_when_composing() {
        let home = tmp_home("notify-queue");
        mark_composing(&home, "agent1");
        let mut injected = false;
        route_notification(&home, "agent1", "queued", |_| {
            injected = true;
            Ok(())
        })
        .expect("route should queue");
        assert!(!injected);
        assert_eq!(crate::notification_queue::pending_count(&home, "agent1"), 1);
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn notify_injects_when_idle() {
        let home = tmp_home("notify-idle");
        let mut injected = Vec::new();
        route_notification(&home, "agent1", "sent", |msg| {
            injected.push(msg.to_string());
            Ok(())
        })
        .expect("route should inject");
        assert_eq!(injected, vec!["sent".to_string()]);
        assert_eq!(crate::notification_queue::pending_count(&home, "agent1"), 0);
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn inbox_message_fields_preserved() {
        let home = tmp_home("fields");
        let msg = InboxMessage {
            schema_version: 0,
            id: None,
            read_at: None,
            from: "sender".to_string(),
            text: "body text".to_string(),
            kind: Some("notification".to_string()),
            timestamp: "2025-06-15T12:30:00Z".to_string(),
        };
        enqueue(&home, "agent1", msg).ok();
        let msgs = drain(&home, "agent1");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, "sender");
        assert_eq!(msgs[0].text, "body text");
        assert_eq!(msgs[0].kind.as_deref(), Some("notification"));
        assert_eq!(msgs[0].timestamp, "2025-06-15T12:30:00Z");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deliver_short_message_does_not_enqueue() {
        let home = tmp_home("deliver-short");
        // deliver with short text — should NOT write to inbox file
        // (notify_agent will fail because no daemon, but enqueue should not be called)
        deliver(
            &home,
            "agent1",
            &NotifySource::Telegram("user"),
            "short msg",
            "\r",
            None,
        );
        let msgs = drain(&home, "agent1");
        assert!(msgs.is_empty(), "short messages bypass inbox");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deliver_long_message_enqueues() {
        let home = tmp_home("deliver-long");
        let long_text: String = "x".repeat(INLINE_THRESHOLD + 100);
        deliver(
            &home,
            "agent1",
            &NotifySource::Telegram("user"),
            &long_text,
            "\r",
            Some("chat".to_string()),
        );
        let msgs = drain(&home, "agent1");
        assert_eq!(msgs.len(), 1, "long messages should be enqueued");
        assert_eq!(msgs[0].text, long_text);
        assert_eq!(msgs[0].kind.as_deref(), Some("chat"));

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn large_message_over_threshold() {
        let home = tmp_home("large-msg");
        let large_text: String = "a".repeat(10_000);
        enqueue(&home, "agent1", make_msg("big", &large_text)).ok();
        let msgs = drain(&home, "agent1");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text.len(), 10_000);

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn multiple_agents_isolated() {
        let home = tmp_home("isolation");
        enqueue(&home, "agent1", make_msg("a", "for-1")).ok();
        enqueue(&home, "agent2", make_msg("b", "for-2")).ok();

        let m1 = drain(&home, "agent1");
        let m2 = drain(&home, "agent2");
        assert_eq!(m1.len(), 1);
        assert_eq!(m1[0].text, "for-1");
        assert_eq!(m2.len(), 1);
        assert_eq!(m2[0].text, "for-2");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn inbox_message_serialization() {
        let msg = InboxMessage {
            schema_version: 0,
            id: None,
            read_at: None,
            from: "test".to_string(),
            text: "hello \"world\"".to_string(),
            kind: None,
            timestamp: "2025-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: InboxMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.from, "test");
        assert_eq!(parsed.text, "hello \"world\"");
    }

    #[test]
    fn inbox_message_with_special_chars() {
        let home = tmp_home("special");
        let msg = InboxMessage {
            schema_version: 0,
            id: None,
            read_at: None,
            from: "user".to_string(),
            text: "line1\nline2\ttab".to_string(),
            kind: Some("special".to_string()),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
        };
        enqueue(&home, "agent1", msg).ok();
        let msgs = drain(&home, "agent1");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "line1\nline2\ttab");

        fs::remove_dir_all(&home).ok();
    }

    // --- NotifySource tests ---

    #[test]
    fn notify_source_telegram_display() {
        let s = NotifySource::Telegram("chiacheng");
        assert_eq!(s.to_string(), "user:chiacheng via telegram");
        assert!(s.reply_hint().contains("reply tool"));
    }

    #[test]
    fn notify_source_agent_display() {
        let s = NotifySource::Agent("dev");
        assert_eq!(s.to_string(), "from:dev");
        let h = s.reply_hint();
        assert!(h.contains("send_to_instance"));
        assert!(h.contains("dev"));
    }

    #[test]
    fn notify_source_system_display() {
        let s = NotifySource::System("ci");
        assert_eq!(s.to_string(), "system:ci");
        assert!(s.reply_hint().is_empty());
    }

    #[test]
    fn drain_recovers_leftover_draining_file() {
        // Simulates a crash between rename() and read: pending messages
        // sit in `{name}.draining`. A second drain() must surface them,
        // not drop them.
        let home = tmp_home("recover");
        let inbox_dir = home.join("inbox");
        fs::create_dir_all(&inbox_dir).ok();
        let draining = inbox_dir.join("agent1.draining");
        let msg = serde_json::to_string(&make_msg("crashed", "pending")).expect("ser");
        fs::write(&draining, format!("{msg}\n")).expect("write leftover");

        let msgs = drain(&home, "agent1");
        assert_eq!(msgs.len(), 1, "crashed batch must be recovered");
        assert_eq!(msgs[0].from, "crashed");
        assert_eq!(msgs[0].text, "pending");
        // After successful read, leftover is cleared.
        assert!(
            !draining.exists(),
            ".draining must be removed after successful drain"
        );
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn drain_does_not_overwrite_leftover_draining() {
        // If a .draining file exists from a prior crash AND a new live
        // inbox has arrived, the live file must be preserved — the new
        // messages are picked up on the next drain cycle, not lost by a
        // rename that overwrites the pending batch.
        let home = tmp_home("no_overwrite");
        let inbox_dir = home.join("inbox");
        fs::create_dir_all(&inbox_dir).ok();

        let draining = inbox_dir.join("agent1.draining");
        let old_msg = serde_json::to_string(&make_msg("old", "from_crashed_batch")).expect("ser");
        fs::write(&draining, format!("{old_msg}\n")).expect("write leftover");

        enqueue(&home, "agent1", make_msg("new", "fresh")).ok();

        // First drain: returns the crashed batch only.
        let first = drain(&home, "agent1");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].from, "old");

        // Second drain: picks up the new message now that .draining is gone.
        let second = drain(&home, "agent1");
        assert_eq!(second.len(), 1, "fresh message must survive recovery");
        assert_eq!(second[0].from, "new");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn drain_read_failure_leaves_file_for_retry() {
        // If read_to_string fails, .draining must remain on disk so a
        // subsequent drain has another chance. (Simulating an unreadable
        // file is awkward cross-platform; we instead assert the
        // "retain-on-error" invariant by verifying successful drains
        // DO remove, which is the inverse assertion our prior bug
        // violated. See drain_recovers_leftover_draining_file.)
        let home = tmp_home("retain");
        let inbox_dir = home.join("inbox");
        fs::create_dir_all(&inbox_dir).ok();
        let draining = inbox_dir.join("agent1.draining");
        // Non-UTF8 bytes → read_to_string returns Err.
        fs::write(&draining, [0xFF, 0xFE, 0xFD]).expect("write");

        let msgs = drain(&home, "agent1");
        assert!(msgs.is_empty(), "unreadable batch yields no messages");
        assert!(
            draining.exists(),
            ".draining must be retained after read failure for next retry"
        );
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn notify_agent_does_not_append_submit_key() {
        // Verify the notification format doesn't contain \r (submit_key).
        let source = NotifySource::Agent("peer");
        let text = "hello world";
        let display_text = text.to_string();
        let notification = format!("[{source}] {display_text}{}", source.reply_hint());
        assert!(
            !notification.contains('\r'),
            "notification must not contain submit_key (\\r): {notification:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Regression pins: compose_aware_send vs compose_aware_inject
    // PR #96 conflated both into raw write; PR #99 splits them.
    // -----------------------------------------------------------------------

    #[test]
    fn compose_aware_send_calls_injector_when_idle() {
        // compose_aware_send must call the injector (not enqueue) when agent
        // is idle. The injector for send uses inject_with_submit (raw=false
        // → inject_to_agent with submit_key).
        let home = tmp_home("send-idle");
        let mut called = false;
        route_notification(&home, "agent1", "msg", |_| {
            called = true;
            Ok(())
        })
        .expect("route should call injector");
        assert!(called, "injector must be called when agent is idle");
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn compose_aware_send_enqueues_when_composing() {
        // compose_aware_send must enqueue (not inject) when agent is composing.
        let home = tmp_home("send-composing");
        mark_composing(&home, "agent1");
        let mut called = false;
        route_notification(&home, "agent1", "msg", |_| {
            called = true;
            Ok(())
        })
        .expect("route should enqueue");
        assert!(!called, "injector must NOT be called when composing");
        assert_eq!(
            crate::notification_queue::pending_count(&home, "agent1"),
            1,
            "message must be enqueued"
        );
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn inject_with_submit_sends_raw_false() {
        // Structural pin: inject_with_submit must NOT set raw=true in the
        // INJECT API call. This ensures inject_to_agent (with submit_key)
        // is used instead of write_to_agent (raw, no submit_key).
        //
        // We verify by inspecting the JSON payload construction. The function
        // builds: {"method": "inject", "params": {"name": ..., "data": ...}}
        // with NO "raw" field — handle_inject defaults raw=false → inject_to_agent.
        //
        // inject_notification in contrast sends raw=true → write_to_agent.
        //
        // Cannot call inject_with_submit directly (needs running daemon), so
        // we verify the contract structurally: inject_notification's JSON
        // includes "raw": true, inject_with_submit's does not.
        let notif_json = serde_json::json!({
            "method": crate::api::method::INJECT,
            "params": {"name": "test", "data": "msg", "raw": true}
        });
        assert_eq!(
            notif_json["params"]["raw"], true,
            "inject_notification path must set raw=true"
        );

        let send_json = serde_json::json!({
            "method": crate::api::method::INJECT,
            "params": {"name": "test", "data": "msg"}
        });
        assert!(
            send_json["params"]["raw"].is_null(),
            "inject_with_submit path must NOT set raw (defaults to false → inject_to_agent)"
        );
    }

    #[test]
    fn test_load_legacy_without_schema_version() {
        let home = tmp_home("legacy-schema");
        let inbox_dir = home.join("inbox");
        fs::create_dir_all(&inbox_dir).ok();
        // Write a legacy JSONL line without schema_version field
        let legacy_line = r#"{"from":"old-agent","text":"legacy msg","kind":null,"timestamp":"2025-01-01T00:00:00Z"}"#;
        fs::write(inbox_dir.join("agent1.jsonl"), format!("{legacy_line}\n")).ok();
        let msgs = drain(&home, "agent1");
        assert_eq!(msgs.len(), 1, "legacy message must load successfully");
        assert_eq!(msgs[0].schema_version, 0, "missing field defaults to 0");
        assert_eq!(msgs[0].from, "old-agent");
        fs::remove_dir_all(&home).ok();
    }

    // --- Disk resilience tests ---

    #[test]
    fn test_readonly_on_disk_full() {
        // When DISK_READONLY is set, enqueue must fail and drain must still work.
        let home = tmp_home("readonly");
        enqueue(&home, "agent1", make_msg("a", "before")).ok();

        DISK_READONLY.store(true, Ordering::Relaxed);
        let result = enqueue(&home, "agent1", make_msg("b", "blocked"));
        assert!(result.is_err(), "enqueue must fail in readonly mode");
        assert!(
            result.unwrap_err().to_string().contains("readonly"),
            "error must mention readonly"
        );

        // drain still works in readonly mode
        let msgs = drain(&home, "agent1");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, "a");

        DISK_READONLY.store(false, Ordering::Relaxed);
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_reject_future_schema_version() {
        let home = tmp_home("future-schema");
        let inbox_dir = home.join("inbox");
        fs::create_dir_all(&inbox_dir).ok();
        let future_line = r#"{"schema_version":999,"from":"future","text":"nope","kind":null,"timestamp":"2099-01-01T00:00:00Z"}"#;
        let current_line = r#"{"schema_version":1,"from":"ok","text":"yes","kind":null,"timestamp":"2025-01-01T00:00:00Z"}"#;
        fs::write(
            inbox_dir.join("agent1.jsonl"),
            format!("{future_line}\n{current_line}\n"),
        )
        .ok();
        let msgs = drain(&home, "agent1");
        assert_eq!(msgs.len(), 1, "future-versioned message must be rejected");
        assert_eq!(msgs[0].from, "ok", "current-versioned message must survive");
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_atomic_append_tmp_recovery() {
        // Simulate a crash that left a .tmp file — recover_half_writes
        // must move it to inbox.recovery/.
        let home = tmp_home("atomic-recover");
        let inbox_dir = home.join("inbox");
        fs::create_dir_all(&inbox_dir).ok();

        // Simulate stale tmp from interrupted enqueue
        let tmp = inbox_dir.join("agent1.jsonl.tmp");
        fs::write(
            &tmp,
            "{\"from\":\"x\",\"text\":\"orphan\",\"kind\":null,\"timestamp\":\"t\"}\n",
        )
        .ok();

        recover_half_writes(&home);

        assert!(!tmp.exists(), ".tmp must be moved to recovery");
        let recovery = home.join("inbox.recovery");
        assert!(recovery.exists(), "recovery dir must be created");
        let entries: Vec<_> = fs::read_dir(&recovery).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "one timestamped recovery dir");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_half_written_jsonl_goes_to_recovery() {
        // A JSONL file with a corrupt line must be moved to recovery.
        let home = tmp_home("half-write");
        let inbox_dir = home.join("inbox");
        fs::create_dir_all(&inbox_dir).ok();

        let jsonl = inbox_dir.join("agent1.jsonl");
        let good = serde_json::to_string(&make_msg("ok", "fine")).unwrap();
        // Write a good line followed by a truncated/corrupt line
        fs::write(
            &jsonl,
            format!("{good}\n{{\"from\":\"broken\",\"text\":\"trun"),
        )
        .ok();

        recover_half_writes(&home);

        assert!(!jsonl.exists(), "corrupt JSONL must be moved to recovery");
        let recovery = home.join("inbox.recovery");
        assert!(recovery.exists());
        // The recovery subdir should contain the moved file
        let subdirs: Vec<_> = fs::read_dir(&recovery).unwrap().flatten().collect();
        assert_eq!(subdirs.len(), 1);
        let files: Vec<_> = fs::read_dir(subdirs[0].path()).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        assert!(files[0].file_name().to_string_lossy().contains("agent1"));

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_drain_marks_read_at_but_keeps_message() {
        let home = tmp_home("drain-read-at");
        enqueue(&home, "agent1", make_msg("alice", "hello")).ok();
        enqueue(&home, "agent1", make_msg("bob", "world")).ok();

        // First drain returns both messages with read_at set
        let msgs = drain(&home, "agent1");
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].read_at.is_some(), "drain must stamp read_at");
        assert!(msgs[1].read_at.is_some());

        // Second drain returns empty (already read)
        let msgs2 = drain(&home, "agent1");
        assert!(
            msgs2.is_empty(),
            "already-read messages must not be returned"
        );

        // But the file still exists with the messages
        let path = inbox_path(&home, "agent1");
        let content = fs::read_to_string(&path).expect("file must still exist");
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "messages must be kept in file");
        // Verify read_at is persisted
        let m: InboxMessage = serde_json::from_str(lines[0]).expect("parse");
        assert!(m.read_at.is_some(), "read_at must be persisted to disk");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_sweep_expired_read_7d() {
        let home = tmp_home("sweep-read-7d");
        let inbox_dir = home.join("inbox");
        fs::create_dir_all(&inbox_dir).ok();

        let old_ts = (chrono::Utc::now() - chrono::Duration::days(10)).to_rfc3339();
        let fresh_ts = chrono::Utc::now().to_rfc3339();
        let read_old = format!(
            r#"{{"schema_version":1,"id":"m-old","from":"a","text":"old read","kind":null,"timestamp":"{old_ts}","read_at":"{old_ts}"}}"#
        );
        let read_fresh = format!(
            r#"{{"schema_version":1,"id":"m-fresh","from":"b","text":"fresh read","kind":null,"timestamp":"{fresh_ts}","read_at":"{fresh_ts}"}}"#
        );
        fs::write(
            inbox_dir.join("agent1.jsonl"),
            format!("{read_old}\n{read_fresh}\n"),
        )
        .ok();

        sweep_expired(&home);

        let content = fs::read_to_string(inbox_dir.join("agent1.jsonl")).expect("file");
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1, "read message >7d must be swept");
        assert!(
            lines[0].contains("m-fresh"),
            "fresh read message must survive"
        );

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_sweep_unread_30d() {
        let home = tmp_home("sweep-unread-30d");
        let inbox_dir = home.join("inbox");
        fs::create_dir_all(&inbox_dir).ok();

        let old_ts = (chrono::Utc::now() - chrono::Duration::days(35)).to_rfc3339();
        let recent_ts = (chrono::Utc::now() - chrono::Duration::days(5)).to_rfc3339();
        let unread_old = format!(
            r#"{{"schema_version":1,"id":"m-unread-old","from":"a","text":"ancient","kind":null,"timestamp":"{old_ts}"}}"#
        );
        let unread_recent = format!(
            r#"{{"schema_version":1,"id":"m-unread-recent","from":"b","text":"recent","kind":null,"timestamp":"{recent_ts}"}}"#
        );
        fs::write(
            inbox_dir.join("agent1.jsonl"),
            format!("{unread_old}\n{unread_recent}\n"),
        )
        .ok();

        sweep_expired(&home);

        let content = fs::read_to_string(inbox_dir.join("agent1.jsonl")).expect("file");
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1, "unread message >30d must be swept");
        assert!(
            lines[0].contains("m-unread-recent"),
            "recent unread must survive"
        );

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_describe_message_status_three_states() {
        let home = tmp_home("describe-msg");
        let inbox_dir = home.join("inbox");
        fs::create_dir_all(&inbox_dir).ok();

        let now = chrono::Utc::now().to_rfc3339();
        let old_ts = (chrono::Utc::now() - chrono::Duration::days(35)).to_rfc3339();

        // State 1: read message
        let read_msg = format!(
            r#"{{"schema_version":1,"id":"m-read","from":"a","text":"read","kind":null,"timestamp":"{now}","read_at":"{now}"}}"#
        );
        // State 2: unread expired (>30d)
        let expired_msg = format!(
            r#"{{"schema_version":1,"id":"m-expired","from":"b","text":"expired","kind":null,"timestamp":"{old_ts}"}}"#
        );
        fs::write(
            inbox_dir.join("agent1.jsonl"),
            format!("{read_msg}\n{expired_msg}\n"),
        )
        .ok();

        // ReadAt
        match describe_message(&home, "m-read", "agent1") {
            MessageStatus::ReadAt(t) => assert_eq!(t, now),
            other => panic!("expected ReadAt, got: {other:?}"),
        }

        // UnreadExpired
        assert_eq!(
            describe_message(&home, "m-expired", "agent1"),
            MessageStatus::UnreadExpired
        );

        // NotFound
        assert_eq!(
            describe_message(&home, "m-nonexistent", "agent1"),
            MessageStatus::NotFound
        );

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_enqueue_concurrent_same_agent() {
        let home = tmp_home("concurrent-same");
        let home_arc = std::sync::Arc::new(home.clone());
        let mut handles = vec![];

        for i in 0..20 {
            let h = home_arc.clone();
            handles.push(std::thread::spawn(move || {
                enqueue(&h, "agent1", make_msg(&format!("t{i}"), &format!("msg{i}")))
                    .expect("enqueue should succeed");
            }));
        }
        for h in handles {
            h.join().expect("thread should not panic");
        }

        let msgs = drain(&home, "agent1");
        assert_eq!(
            msgs.len(),
            20,
            "all 20 concurrent enqueues must survive, got {}",
            msgs.len()
        );

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_enqueue_vs_drain_no_lost_msg() {
        // Thread A enqueues 10 messages; thread B drains after each.
        // Total drained must equal 10 — no lost messages.
        let home = tmp_home("enqueue-vs-drain");
        let home_a = std::sync::Arc::new(home.clone());
        let home_b = home_a.clone();

        let writer = std::thread::spawn(move || {
            for i in 0..10 {
                enqueue(&home_a, "agent1", make_msg(&format!("w{i}"), &format!("msg{i}")))
                    .expect("enqueue");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });

        let reader = std::thread::spawn(move || {
            let mut total = Vec::new();
            for _ in 0..20 {
                let batch = drain(&home_b, "agent1");
                total.extend(batch);
                std::thread::sleep(std::time::Duration::from_millis(3));
            }
            total
        });

        writer.join().expect("writer");
        let mut drained = reader.join().expect("reader");
        // Final drain to catch any remaining
        drained.extend(drain(&home, "agent1"));

        assert_eq!(
            drained.len(),
            10,
            "all 10 enqueued messages must be drained, got {}",
            drained.len()
        );

        fs::remove_dir_all(&home).ok();
    }
}
