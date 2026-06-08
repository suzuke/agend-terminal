use std::io::Write;
use std::path::{Path, PathBuf};

use super::message::{InboxMessage, MessageStatus};

// ── #inbox-gc retention bounds (decision d-20260607081209372642-1, part b) ──
//
// Root cause of the unbounded-looking inbox files: read (drained) messages were
// retained for 7 DAYS, so a high-throughput agent accumulates 1000s of read
// rows within that window. Two complementary bounds replace the single 7d TTL:
//
// 1. A shorter read TTL for the bulk (`update`/`report`/`ci`/`poll` …), and
// 2. A per-inbox SIZE CAP on retained read rows — the robust bound a TTL alone
//    can't provide (a burst inside ANY window still blows past the cap).
//
// EXEMPTION: drained `query`/`task` rows are "blockers" — they are read by
// `has_drained_blocker_for_correlation` (ack-absorption / reply-routing, see
// storage.rs `has_drained_blocker_for_correlation`) for the full task
// turnaround, which has no finite upper bound (overnight / multi-day tasks).
// They keep the original 7d window AND are exempt from the size cap so the
// audit path never regresses. Unread rows (obligations) keep the 30d window.

/// Read (drained) NON-blocker messages expire this many hours after their
/// timestamp. Lowered from 7 days — these are the high-volume `update`/`report`/
/// `ci`/`poll` rows that flood the file.
const READ_TTL_HOURS: i64 = 48;

/// Read (drained) BLOCKER rows (`kind` ∈ {query, task}) keep this longer window
/// so `has_drained_blocker_for_correlation` can still see a consumed dispatch
/// when its reply arrives late. Unchanged from the legacy read TTL.
const READ_TTL_BLOCKER_DAYS: i64 = 7;

/// Unread (obligation) messages expire this many days after their timestamp.
/// Unchanged — unread rows are work the agent hasn't acknowledged.
const UNREAD_TTL_DAYS: i64 = 30;

/// Per-inbox cap on retained read NON-blocker rows (most-recent-N kept,
/// oldest beyond N dropped regardless of age). The hard bound against a burst.
const READ_ROW_CAP: usize = 300;

/// A drained row that the ack-absorption / reply-routing audit
/// (`has_drained_blocker_for_correlation`) depends on: `read_at` set AND
/// `kind` ∈ {query, task}. Such rows are exempt from the short read TTL and
/// from the size cap.
fn is_blocker_row(msg: &InboxMessage) -> bool {
    msg.read_at.is_some() && matches!(msg.kind.as_deref(), Some("query") | Some("task"))
}

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
        // #1665 reply-ledger: arm the delivery-closure audit for this user
        // channel message. `m.channel.is_some()` above is exactly the
        // "[user:… via channel] inbound" eligibility gate. Arming overwrites
        // any prior in-flight turn (supersede — the user moved on, never warn).
        crate::reply_ledger::arm(
            name,
            *channel_msg.channel.as_ref().expect("checked"),
            channel_msg.id.clone(),
            channel_msg.thread_id.clone(),
            channel_msg.kind.clone(),
        );
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

/// One bounded line in a [`ClearCompactResult`] — a COMPACT projection of an
/// inbox message (never the full [`InboxMessage`], so a clear can never
/// reintroduce the multi-megabyte blowup that a full-message drain could).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClearSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub from: String,
    /// Single-line, sanitised, ≤[`CLEAR_PREVIEW_CHARS`] preview of the body.
    pub preview: String,
    pub marked_read: bool,
    /// Why this message was kept unread (obligations) or cleared with a note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Result of [`clear_compact`] — a quiet, trust-preserving inbox clear.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClearCompactResult {
    /// Non-obligation messages whose `read_at` was set this call.
    pub cleared_count: usize,
    /// Obligation messages deliberately left UNREAD (still need attention).
    pub kept_unread_count: usize,
    /// Bounded sample of CLEARED messages (capped at [`CLEAR_SUMMARY_CAP`]).
    pub summaries: Vec<ClearSummary>,
    /// How many cleared summaries were omitted past the cap.
    pub summaries_omitted: usize,
    /// EVERY kept-unread obligation — NEVER capped (the trust guarantee:
    /// clearing must never hide a query you owe a reply to or an open task).
    pub requires_response: Vec<ClearSummary>,
}

/// Max chars in a [`ClearSummary::preview`] (single line).
const CLEAR_PREVIEW_CHARS: usize = 60;
/// Cap on [`ClearCompactResult::summaries`] (cleared sample). `requires_response`
/// is intentionally NOT capped.
const CLEAR_SUMMARY_CAP: usize = 200;

/// Collapse a message body to a single sanitised preview line of ≤N chars.
fn preview_line(text: &str, max_chars: usize) -> String {
    let collapsed: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let normalised = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalised.chars().count() > max_chars {
        let truncated: String = normalised.chars().take(max_chars).collect();
        format!("{truncated}…")
    } else {
        normalised
    }
}

fn clear_summary_of(msg: &InboxMessage, marked_read: bool, reason: Option<String>) -> ClearSummary {
    ClearSummary {
        id: msg.id.clone(),
        kind: msg.kind.clone(),
        from: msg.from.clone(),
        preview: preview_line(&msg.text, CLEAR_PREVIEW_CHARS),
        marked_read,
        reason,
    }
}

/// Quiet, trust-preserving inbox clear (#inbox-gc part a).
///
/// Sibling of [`drain`]: same `with_inbox_lock` + tmp+fsync+rename write-back,
/// but it sets `read_at` SELECTIVELY (only non-obligation messages) and returns
/// COMPACT structs instead of full [`InboxMessage`]s.
///
/// `obligation`: returns `Some(reason)` when a message MUST stay unread (an
/// unanswered query, an open task, anything the caller can't prove is settled —
/// failure mode is noise, never hidden work) and `None` when it is safe to clear
/// (`update`/`report`/CI/poll/superseded/ambient). The storage layer is policy-
/// free; the caller (which can read the task board) supplies the predicate.
///
/// TRUST: `read_at` here means "non-obligation cleared from attention", NOT
/// "obligation accepted". Unlike [`drain`], this does NOT arm the reply-ledger
/// nor touch `heartbeat_pair` — clearing historical channel backlog must never
/// fabricate a "must-reply" turn. It DOES consume the notification dedup for
/// cleared rows (they're no longer pending). Never deletes rows (that's
/// [`sweep_expired`]'s job); only mutates `read_at`.
pub fn clear_compact(
    home: &Path,
    name: &str,
    obligation: impl Fn(&InboxMessage) -> Option<String>,
) -> ClearCompactResult {
    let path = inbox_path_resolved(home, name);
    if !path.exists() {
        return ClearCompactResult {
            cleared_count: 0,
            kept_unread_count: 0,
            summaries: Vec::new(),
            summaries_omitted: 0,
            requires_response: Vec::new(),
        };
    }

    // Phase 1 (locked): read, selectively mark read_at, write back. Collect the
    // ids of newly-cleared rows for the phase-2 dedup consume.
    struct Phase1 {
        cleared_count: usize,
        kept_unread_count: usize,
        summaries: Vec<ClearSummary>,
        summaries_omitted: usize,
        requires_response: Vec<ClearSummary>,
        cleared_ids: Vec<String>,
    }
    let result = with_inbox_lock(home, name, |path| {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let now = chrono::Utc::now().to_rfc3339();
        let mut out: Vec<InboxMessage> = Vec::new();
        let mut p = Phase1 {
            cleared_count: 0,
            kept_unread_count: 0,
            summaries: Vec::new(),
            summaries_omitted: 0,
            requires_response: Vec::new(),
            cleared_ids: Vec::new(),
        };
        let mut changed = false;

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
            // Already-read rows are untouched (and not re-summarised).
            if msg.read_at.is_some() {
                out.push(msg);
                continue;
            }
            // Superseded rows are always safe to clear (mirror drain()).
            let obligation_reason = if msg.superseded_by.is_some() {
                None
            } else {
                obligation(&msg)
            };
            match obligation_reason {
                Some(reason) => {
                    // Obligation → keep UNREAD, surface in requires_response.
                    p.kept_unread_count += 1;
                    p.requires_response
                        .push(clear_summary_of(&msg, false, Some(reason)));
                    out.push(msg);
                }
                None => {
                    let reason = msg.superseded_by.as_ref().map(|_| "superseded".to_string());
                    if p.summaries.len() < CLEAR_SUMMARY_CAP {
                        p.summaries.push(clear_summary_of(&msg, true, reason));
                    } else {
                        p.summaries_omitted += 1;
                    }
                    if let Some(ref id) = msg.id {
                        p.cleared_ids.push(id.clone());
                    }
                    msg.read_at = Some(now.clone());
                    p.cleared_count += 1;
                    changed = true;
                    out.push(msg);
                }
            }
        }

        if changed {
            let write_tmp = path.with_extension("jsonl.tmp");
            let r = (|| -> anyhow::Result<()> {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&write_tmp)?;
                for m in &out {
                    writeln!(f, "{}", serde_json::to_string(m)?)?;
                }
                f.sync_all()?;
                std::fs::rename(&write_tmp, path)?;
                Ok(())
            })();
            if let Err(e) = r {
                tracing::warn!(error = %e, "inbox clear_compact write-back failed");
            }
        }
        p
    });

    let p = match result {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "inbox clear_compact lock failed");
            return ClearCompactResult {
                cleared_count: 0,
                kept_unread_count: 0,
                summaries: Vec::new(),
                summaries_omitted: 0,
                requires_response: Vec::new(),
            };
        }
    };

    // Phase 2 (unlocked): consume notification dedup for cleared rows so they
    // don't re-nudge. Deliberately NONE of drain()'s channel side effects (no
    // reply-ledger arming, no turn-state touch) — see the TRUST note above.
    for id in &p.cleared_ids {
        crate::daemon::notification_dedup::global().mark_consumed(name, id);
    }

    ClearCompactResult {
        cleared_count: p.cleared_count,
        kept_unread_count: p.kept_unread_count,
        summaries: p.summaries,
        summaries_omitted: p.summaries_omitted,
        requires_response: p.requires_response,
    }
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

/// Sweep expired messages from all inbox files (#inbox-gc part b).
///
/// Two-pass per inbox, both serialised under [`with_inbox_lock`]:
/// 1. **TTL pass** — drop by age, with three tiers:
///    - unread (`read_at.is_none()`): age > [`UNREAD_TTL_DAYS`]
///    - read blocker (`is_blocker_row`): age > [`READ_TTL_BLOCKER_DAYS`]
///    - read non-blocker: age > [`READ_TTL_HOURS`]
/// 2. **Size-cap pass** — among the TTL survivors, keep at most
///    [`READ_ROW_CAP`] read NON-blocker rows (most-recent by timestamp);
///    drop the oldest beyond the cap. Unread + blocker rows are never counted
///    nor dropped here (obligations / ack-absorption audit window).
///
/// File line order is preserved for survivors.
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

            // Pass 1 (TTL): retain non-expired lines, recording for each kept
            // line its timestamp + whether it's a read non-blocker (the only
            // tier the size cap touches).
            struct Kept {
                line: String,
                ts: chrono::DateTime<chrono::Utc>,
                read_non_blocker: bool,
            }
            let mut kept: Vec<Kept> = Vec::new();
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
                let blocker = is_blocker_row(&msg);
                let expired = match &msg.read_at {
                    None => age > chrono::Duration::days(UNREAD_TTL_DAYS),
                    Some(_) if blocker => age > chrono::Duration::days(READ_TTL_BLOCKER_DAYS),
                    Some(_) => age > chrono::Duration::hours(READ_TTL_HOURS),
                };
                if expired {
                    changed = true;
                } else {
                    kept.push(Kept {
                        line: line.to_string(),
                        ts,
                        read_non_blocker: msg.read_at.is_some() && !blocker,
                    });
                }
            }

            // Pass 2 (size cap): if read non-blocker survivors exceed the cap,
            // drop the oldest beyond the most-recent READ_ROW_CAP. Find the
            // cutoff timestamp by descending sort of just those rows' timestamps.
            let read_count = kept.iter().filter(|k| k.read_non_blocker).count();
            if read_count > READ_ROW_CAP {
                let mut ts_desc: Vec<chrono::DateTime<chrono::Utc>> = kept
                    .iter()
                    .filter(|k| k.read_non_blocker)
                    .map(|k| k.ts)
                    .collect();
                ts_desc.sort_unstable_by(|a, b| b.cmp(a));
                let cutoff = ts_desc[READ_ROW_CAP - 1];
                // Keep read non-blockers strictly newer than cutoff, plus exactly
                // enough at-the-cutoff rows to total READ_ROW_CAP (ties broken by
                // file order, deterministic). Everything else (unread/blocker) is
                // always retained.
                let mut at_cutoff_budget =
                    READ_ROW_CAP - ts_desc.iter().filter(|t| **t > cutoff).count();
                let before = kept.len();
                kept.retain(|k| {
                    if !k.read_non_blocker {
                        return true;
                    }
                    if k.ts > cutoff {
                        return true;
                    }
                    if k.ts == cutoff && at_cutoff_budget > 0 {
                        at_cutoff_budget -= 1;
                        return true;
                    }
                    false
                });
                if kept.len() != before {
                    changed = true;
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
                        for k in &kept {
                            writeln!(f, "{}", k.line)?;
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
        // #bughunt-r2 #3: a live, not-yet-read message. Previously returned
        // NotFound (indistinguishable from "no such id") — breaking delivery
        // audit of an un-drained message. Report it as Unread with its
        // delivery_mode + correlation_id for correlation tracking.
        return MessageStatus::Unread {
            delivery_mode: msg.delivery_mode.clone(),
            correlation_id: msg.correlation_id.clone(),
        };
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
