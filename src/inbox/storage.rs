use std::io::Write;
use std::path::{Path, PathBuf};

use super::message::{InboxMessage, MessageStatus};

pub(crate) fn inbox_path(home: &Path, name: &str) -> PathBuf {
    home.join("inbox").join(format!("{name}.jsonl"))
}

/// Sprint 46 P2: resolve inbox path by InstanceId when available.
/// Migrates legacy name-based files to id-based on first access.
pub(crate) fn inbox_path_resolved(home: &Path, name: &str) -> PathBuf {
    // Only use id-based path when the instance has a real ID in fleet.yaml
    // (backfilled by P1). Instances without an ID use name-based paths.
    // #1441: route through the single authoritative resolver shared with the
    // agent registry, so inbox identity and live-process identity cannot drift.
    let Some(id) = crate::fleet::resolve_uuid(home, name) else {
        return inbox_path(home, name);
    };
    let id_path = home.join("inbox").join(format!("{}.jsonl", id.full()));
    if id_path.exists() {
        return id_path;
    }
    let name_path = inbox_path(home, name);
    if name_path.exists() {
        // Migrate: create symlink from id-based to name-based (or copy on Windows)
        if let Some(parent) = id_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&name_path, &id_path);
        }
        #[cfg(windows)]
        {
            let _ = std::fs::copy(&name_path, &id_path);
        }
        return id_path;
    }
    // New instance — use id-based path directly
    id_path
}

/// Acquire a per-agent flock and run `f` with the inbox path.
/// All read-modify-write operations on an agent's inbox (enqueue, drain,
/// sweep_expired) must go through this helper to prevent concurrent races.
fn with_inbox_lock<T>(home: &Path, name: &str, f: impl FnOnce(&Path) -> T) -> anyhow::Result<T> {
    let path = inbox_path_resolved(home, name);
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
    if super::disk::is_readonly() {
        anyhow::bail!("inbox readonly: disk space critically low");
    }
    msg.schema_version = InboxMessage::CURRENT_VERSION;
    ensure_msg_id(&mut msg);
    let line = format!("{}\n", serde_json::to_string(&msg)?);

    with_inbox_lock(home, name, |path| {
        // H1: append-only write — O(1) instead of O(n) read-all+rewrite.
        // The file is a JSONL append log; we only need to add one line.
        let result = (|| -> anyhow::Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            f.write_all(line.as_bytes())?;
            f.sync_all()?;
            Ok(())
        })();
        result
    })?
}

/// Enqueue a message and return the post-enqueue unread count in one lock
/// scope. Avoids the double-read of separate `enqueue` + `unread_count` calls.
pub fn enqueue_returning_unread_count(
    home: &Path,
    name: &str,
    mut msg: InboxMessage,
) -> anyhow::Result<usize> {
    if super::disk::is_readonly() {
        anyhow::bail!("inbox readonly: disk space critically low");
    }
    msg.schema_version = InboxMessage::CURRENT_VERSION;
    ensure_msg_id(&mut msg);
    let line = format!("{}\n", serde_json::to_string(&msg)?);

    with_inbox_lock(home, name, |path| {
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let mut count = 0usize;
        for l in existing.lines() {
            if l.trim().is_empty() {
                continue;
            }
            if let Ok(m) = serde_json::from_str::<InboxMessage>(l) {
                if m.read_at.is_none() {
                    count += 1;
                }
            }
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        f.write_all(line.as_bytes())?;
        f.sync_all()?;
        Ok(count + 1) // +1 for the message we just appended
    })?
}

/// Assign a stable `msg.id` when absent. Shared between [`enqueue`] and
/// [`enqueue_with_idle_hint`] so the latter can pre-stamp an id before
/// the enqueue, then reference it in the PTY hint without consuming the
/// message-by-value twice.
pub(super) fn ensure_msg_id(msg: &mut InboxMessage) {
    if msg.id.is_some() {
        return;
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static MSG_SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
    let seq = MSG_SEQ.fetch_add(1, Ordering::Relaxed);
    msg.id = Some(format!("m-{ts}-{seq}"));
}

/// Mark prior unread ci-watch messages for the same repo+branch as superseded.
/// Called before enqueuing a new ci-watch notification so stale events don't surface.
pub fn mark_ci_watch_superseded(
    home: &Path,
    instance: &str,
    repo_branch_key: &str,
    new_msg_id: &str,
) {
    let path = inbox_path_resolved(home, instance);
    if !path.exists() {
        return;
    }
    let _ = with_inbox_lock(home, instance, |path| -> anyhow::Result<()> {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let mut changed = false;
        let mut lines: Vec<String> = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                lines.push(line.to_string());
                continue;
            }
            // Pre-filter: skip JSON parse for lines that can't match criteria.
            // Matching lines must contain "ci-watch", "system:ci", and the
            // repo_branch_key, and must NOT already have a non-null read_at.
            if !line.contains("ci-watch")
                || !line.contains("system:ci")
                || !line.contains(repo_branch_key)
            {
                lines.push(line.to_string());
                continue;
            }
            if let Ok(mut msg) = serde_json::from_str::<InboxMessage>(line) {
                if msg.read_at.is_none()
                    && msg.superseded_by.is_none()
                    && msg.kind.as_deref() == Some("ci-watch")
                    && msg.from == "system:ci"
                    && msg.text.contains(repo_branch_key)
                {
                    msg.superseded_by = Some(new_msg_id.to_string());
                    changed = true;
                }
                lines.push(serde_json::to_string(&msg).unwrap_or_else(|_| line.to_string()));
            } else {
                lines.push(line.to_string());
            }
        }
        if changed {
            let tmp = path.with_extension("jsonl.tmp");
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            use std::io::Write;
            for l in &lines {
                writeln!(f, "{l}")?;
            }
            f.sync_all()?;
            std::fs::rename(&tmp, path)?;
        }
        Ok(())
    });
}

/// Drain unread messages: mark them with `read_at` and write back.
/// Returns only the messages that were previously unread.
///
/// Soft-delete semantics: messages stay in the JSONL file with `read_at`
/// set; [`sweep_expired`] removes them later based on TTL rules.
/// Uses atomic tmp+fsync+rename for crash safety.
pub fn drain(home: &Path, name: &str) -> Vec<InboxMessage> {
    let path = inbox_path_resolved(home, name);

    if !path.exists() && !path.with_extension("draining").exists() {
        return Vec::new();
    }

    // Phase 1 (locked): read file, mark read_at, write back.
    // Side effects (dedup, heartbeat) are deferred to phase 2.
    let (all_messages, newly_read) = match with_inbox_lock(home, name, |path| {
        let tmp = path.with_extension("draining");
        if tmp.exists() {
            let msgs = read_drain_file(&tmp);
            let flags = vec![true; msgs.len()];
            return (msgs, flags);
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return (Vec::new(), Vec::new()),
        };

        let now = chrono::Utc::now().to_rfc3339();
        let mut all_messages: Vec<InboxMessage> = Vec::new();
        let mut newly_read: Vec<bool> = Vec::new();

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
                if msg.superseded_by.is_some() {
                    msg.read_at = Some(now.clone());
                    newly_read.push(false);
                    all_messages.push(msg);
                    continue;
                }
                msg.read_at = Some(now.clone());
                newly_read.push(true);
            } else {
                newly_read.push(false);
            }
            all_messages.push(msg);
        }

        let has_unread = newly_read.iter().any(|&b| b);
        if has_unread {
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

        (all_messages, newly_read)
    }) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(error = %e, "inbox drain lock failed");
            return Vec::new();
        }
    };

    // Phase 2 (unlocked): side effects that don't need file exclusion.
    for (msg, &is_new) in all_messages.iter().zip(&newly_read) {
        if is_new {
            if let Some(ref id) = msg.id {
                crate::daemon::notification_dedup::global().mark_consumed(name, id);
            }
        }
    }

    if let Some(channel_msg) = all_messages
        .iter()
        .zip(newly_read.iter())
        .rev()
        .find(|(m, &nr)| nr && m.channel.is_some())
        .map(|(m, _)| m)
    {
        let channel_name = match channel_msg.channel.as_ref().expect("checked") {
            crate::channel::ChannelKind::Telegram => "telegram",
            crate::channel::ChannelKind::Discord => "discord",
        };
        crate::daemon::heartbeat_pair::update_with(name, |p| {
            p.reply_to_channel = Some(channel_name.to_string());
            p.reply_to_input_id = Some(p.reply_to_input_id.unwrap_or(0) + 1);
            p.reply_to_set_at_ms = crate::daemon::heartbeat_pair::now_ms() as i64;
            p.mirror_dispatched_for_turn = false;
            p.mirror_skip_until_next_turn = false;
        });
    }

    all_messages
        .into_iter()
        .zip(newly_read)
        .filter_map(|(msg, nr)| nr.then_some(msg))
        .collect()
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

/// Count unread messages (read_at == None) for an agent.
pub fn unread_count(home: &Path, name: &str) -> (usize, Option<chrono::DateTime<chrono::Utc>>) {
    let path = inbox_path_resolved(home, name);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return (0, None),
    };
    let mut count = 0usize;
    let mut oldest: Option<chrono::DateTime<chrono::Utc>> = None;
    for line in content.lines() {
        if let Ok(msg) = serde_json::from_str::<InboxMessage>(line) {
            if msg.read_at.is_none() {
                count += 1;
                if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&msg.timestamp) {
                    let ts_utc = ts.with_timezone(&chrono::Utc);
                    if oldest.is_none_or(|t| t > ts_utc) {
                        oldest = Some(ts_utc);
                    }
                }
            }
        }
    }
    (count, oldest)
}

/// #1491: unread messages of a given `kind` in `name`'s inbox, returned as
/// `(correlation_id, timestamp)`. Used by the handoff-timeout watchdog to find
/// `ci-ready-for-action` handoffs an agent received but never read. Messages
/// with an unparseable timestamp are skipped.
pub fn unread_of_kind(
    home: &Path,
    name: &str,
    kind: &str,
) -> Vec<(Option<String>, chrono::DateTime<chrono::Utc>)> {
    let path = inbox_path_resolved(home, name);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in content.lines() {
        let Ok(msg) = serde_json::from_str::<InboxMessage>(line) else {
            continue;
        };
        if msg.read_at.is_none() && msg.kind.as_deref() == Some(kind) {
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&msg.timestamp) {
                out.push((msg.correlation_id.clone(), ts.with_timezone(&chrono::Utc)));
            }
        }
    }
    out
}

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
    let path = inbox_path_resolved(home, instance);
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
            return MessageStatus::ReadAt(read_at.clone(), msg.delivery_mode.clone());
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

/// Get all messages in a thread, ordered by timestamp.
/// If `instance` is Some, only scan that agent's inbox; otherwise scan all.
pub fn get_thread(home: &Path, thread_id: &str, instance: Option<&str>) -> Vec<InboxMessage> {
    let mut msgs = Vec::new();

    if let Some(inst) = instance {
        // Direct path lookup — skip directory scan entirely.
        let path = inbox_path_resolved(home, inst);
        collect_thread_messages(&path, thread_id, &mut msgs);
    } else {
        let inbox_dir = home.join("inbox");
        let entries = match std::fs::read_dir(&inbox_dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            collect_thread_messages(&path, thread_id, &mut msgs);
        }
    }

    msgs.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    msgs
}

fn collect_thread_messages(path: &Path, thread_id: &str, out: &mut Vec<InboxMessage>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    for line in content.lines() {
        if !line.contains(thread_id) {
            continue;
        }
        if let Ok(msg) = serde_json::from_str::<InboxMessage>(line) {
            if msg.thread_id.as_deref() == Some(thread_id) {
                out.push(msg);
            }
        }
    }
}

/// Look up a message by ID across all inbox files. Returns the message if found.
pub fn find_message(home: &Path, msg_id: &str) -> Option<InboxMessage> {
    let inbox_dir = home.join("inbox");
    let entries = std::fs::read_dir(&inbox_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let content = std::fs::read_to_string(&path).ok()?;
        for line in content.lines() {
            if let Ok(msg) = serde_json::from_str::<InboxMessage>(line) {
                if msg.id.as_deref() == Some(msg_id) {
                    return Some(msg);
                }
            }
        }
    }
    None
}

/// #982 B-narrow: scan `agent_name`'s inbox for a previously-drained
/// dispatch (`kind ∈ {query, task}`, `read_at.is_some()`) that shares
/// the given `correlation_id`. Used by `api::handlers::messaging` to
/// override codex ack-absorption when an inbound `kind=report|update`
/// is the reply to a blocking dispatch the recipient already consumed.
pub fn has_drained_blocker_for_correlation(
    home: &Path,
    agent_name: &str,
    correlation_id: &str,
) -> bool {
    let path = inbox_path_resolved(home, agent_name);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    content.lines().any(|line| {
        let Ok(msg) = serde_json::from_str::<InboxMessage>(line) else {
            return false;
        };
        msg.correlation_id.as_deref() == Some(correlation_id)
            && msg.read_at.is_some()
            && matches!(msg.kind.as_deref(), Some("query") | Some("task"))
    })
}

/// Read the agent's inbox JSONL and return `true` iff a message with
/// the given `msg_id` exists AND has `read_at` set.
pub(super) fn msg_already_drained_in_jsonl(home: &Path, agent_name: &str, msg_id: &str) -> bool {
    let path = inbox_path(home, agent_name);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    content.lines().any(|line| {
        let Ok(msg) = serde_json::from_str::<InboxMessage>(line) else {
            return false;
        };
        msg.id.as_deref() == Some(msg_id) && msg.read_at.is_some()
    })
}
