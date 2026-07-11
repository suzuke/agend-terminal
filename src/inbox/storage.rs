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

/// #2299 reclaim-TTL: a `delivering` row (handed to the agent, not yet
/// confirmed `processed`) is reverted to `unread` for re-delivery once it has
/// been in-flight this long. The net under the explicit `inbox ack` (C) and
/// implicit next-drain ack (A): a turn that DIED after a message was drained
/// leaves it `delivering` forever; this bounds the silent-loss window. Matched
/// to `notification_dedup::IDEMPOTENCY_WINDOW_SECS` (10 min) — the same horizon
/// over which a delivered message's re-inject is suppressed — so reclaim and
/// dedup expire together (and reclaim also `forget`s the dedup entry to be
/// timing-independent). Trade-off: a message the agent DID process but never
/// acked nor re-drained within the window is re-delivered once (at-least-once);
/// P2 idempotency keys suppress that duplicate.
const RECLAIM_TTL_SECS: i64 = 600;

/// #2622: hard upper bound on how long the [`crate::snapshot::agent_is_busy`]
/// gate (see `reclaim_stale_delivering`) may hold a `delivering` row past
/// [`RECLAIM_TTL_SECS`] for a still-busy agent, before forcing reclaim
/// anyway. 6x the base TTL (1 hour) — long enough for a genuinely slow
/// turn (reading + analyzing + composing a long reply), short enough that a
/// wedged/stuck-reporting-busy agent doesn't zombie a row in `delivering`
/// indefinitely. `sweep_expired`'s 30-day unread-tier TTL is the final
/// backstop regardless (it keys on `read_at.is_none()`, matched whether or
/// not `delivering_at` is set).
const RECLAIM_BUSY_HARD_CAP_SECS: i64 = RECLAIM_TTL_SECS * 6;

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

/// #1902: id-based inbox path (pure — no fleet.yaml lookup, unlike
/// [`inbox_path_resolved`]). Mirrors `agent_ops::metadata_path_for_id`. For
/// teardown paths where the `InstanceId` is known directly and fleet.yaml has
/// already been removed (so the name→id resolver can't run) — e.g.
/// `full_delete_instance`, whose UUID inbox would otherwise leak silently.
pub(crate) fn inbox_path_for_id(home: &Path, id: &crate::types::InstanceId) -> PathBuf {
    home.join("inbox").join(format!("{}.jsonl", id.full()))
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
    let name_path = inbox_path(home, name);
    if id_path.exists() {
        // #2624: an id-direct write (bypassing this resolver — e.g. #1902's
        // teardown path) can leave BOTH files real and independent. Merge the
        // name file's rows in before short-circuiting to the id file, or they
        // become permanent orphans: unreachable to drain/ack/clear (which all
        // route through here), yet still visible to directory-scan readers
        // (get_thread/find_message/renudge) — an un-settleable re-nudge loop.
        // Skip when `id_path` is ITSELF a symlink (the classic opposite-
        // direction migration below) — merging then would create a
        // name→id→name symlink cycle.
        if !is_symlink(&id_path) {
            merge_dual_inbox_files_if_needed(&id_path, &name_path);
        }
        return id_path;
    }
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

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// #2624: merges an orphaned name-keyed inbox file's rows into the id-keyed
/// file, then atomically replaces the name file with a symlink to the id file
/// (Windows: removes it — this codebase's Windows migration already uses a
/// one-way `fs::copy` rather than symlinks, so there is no link to keep in
/// sync). Deduped by message `id`, falling back to exact-line dedup for
/// id-less legacy rows, so a row present in both files — or a re-run after a
/// crash mid-merge — is never duplicated.
///
/// Idempotent + crash-safe: dedup-by-id means a re-run after a crash between
/// the id-file write and the name-file swap recomputes to "nothing new to
/// append" and just retries the swap — never duplicates, never drops a row.
/// Runs under its own lock file, distinct from [`with_inbox_lock`]'s per-op
/// flock (which the caller acquires AFTER this function returns — see
/// `with_inbox_lock`), so the two can't deadlock each other.
fn merge_dual_inbox_files_if_needed(id_path: &Path, name_path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(name_path) else {
        return; // name file doesn't exist — nothing to merge
    };
    if meta.file_type().is_symlink() {
        return; // already migrated (#2624 direction)
    }

    let lock_path = id_path.with_extension("jsonl.merge.lock");
    let Ok(_lock) = crate::store::acquire_file_lock(&lock_path) else {
        return;
    };

    // Re-check under the lock: another thread/process may have completed the
    // swap between our unlocked stat above and acquiring this lock.
    let Ok(meta) = std::fs::symlink_metadata(name_path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }

    let Ok(name_content) = std::fs::read_to_string(name_path) else {
        return;
    };
    let id_content = std::fs::read_to_string(id_path).unwrap_or_default();

    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_lines = std::collections::HashSet::new();
    for line in id_content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        seen_lines.insert(line);
        if let Some(id) = extract_msg_id(line) {
            seen_ids.insert(id);
        }
    }

    let mut extra: Vec<&str> = Vec::new();
    for line in name_content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match extract_msg_id(line) {
            Some(id) => {
                if !seen_ids.insert(id) {
                    continue; // already in the id file — dedup, not a real addition
                }
            }
            None => {
                if !seen_lines.insert(line) {
                    continue; // exact-duplicate id-less legacy row
                }
            }
        }
        extra.push(line);
    }

    if !extra.is_empty() {
        let tmp = id_path.with_extension("jsonl.tmp");
        let result = (|| -> anyhow::Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            for line in id_content.lines().filter(|l| !l.trim().is_empty()) {
                writeln!(f, "{line}")?;
            }
            for line in &extra {
                writeln!(f, "{line}")?;
            }
            f.sync_all()?;
            std::fs::rename(&tmp, id_path)?;
            crate::store::fsync_parent_dir(id_path);
            Ok(())
        })();
        if let Err(e) = result {
            tracing::warn!(error = %e, "inbox #2624 dual-file merge write-back failed");
            return; // don't swap the name file if the merge write didn't land
        }
    }

    #[cfg(unix)]
    {
        let tmp_link = name_path.with_extension("jsonl.symlink_tmp");
        let _ = std::fs::remove_file(&tmp_link);
        if std::os::unix::fs::symlink(id_path, &tmp_link).is_ok() {
            let _ = std::fs::rename(&tmp_link, name_path);
        }
    }
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(name_path);
    }
}

/// Cheap `id`-only probe for [`merge_dual_inbox_files_if_needed`]'s dedup.
/// `id` is genuinely optional (`#[serde(default)]`, matching
/// [`InboxMessage::id`]) since legacy pre-`ensure_msg_id` rows lack it.
fn extract_msg_id(line: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct IdOnly {
        #[serde(default)]
        id: Option<String>,
    }
    serde_json::from_str::<IdOnly>(line).ok().and_then(|v| v.id)
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

/// Parse an inbox JSONL body into successfully-deserialized [`InboxMessage`]s,
/// skipping blank / unparseable / forward-schema rows. The READ-ONLY counterpart
/// to the read-modify-write rewriters (`drain` / `sweep_expired` / `clear_compact`
/// / `reclaim_stale_delivering` / `mark_ci_watch_superseded` / `enqueue`), which
/// instead preserve every raw line VERBATIM — those must NOT route through this
/// parse-and-skip helper or they would drop forward-schema rows (silent
/// data-loss). Lazy (no intermediate `Vec`) so `.any` / `.find` callers keep
/// their short-circuit and allocation profile. Takes already-read content (not a
/// path) because a path-taking iterator can't borrow its local read buffer
/// without an eager collect.
fn parse_inbox_messages(content: &str) -> impl Iterator<Item = InboxMessage> + '_ {
    content
        .lines()
        .filter_map(|line| serde_json::from_str::<InboxMessage>(line).ok())
}

/// AUDIT3-005: the single parse step for EVERY read-modify-write rewriter
/// (`drain` / `ack` / `settle_read_by_id` / `ack_by_correlation` / `clear_compact`
/// / `sweep_expired` / `reclaim_stale_delivering`). Returns `Some(msg)` only for a
/// row this daemon can process (parseable, schema ≤ current); the caller runs its
/// own logic on it. A row the caller must NOT process but MUST NOT DELETE — a
/// FORWARD-SCHEMA row (valid JSON a newer daemon wrote, downgrade-safe) OR an
/// UNPARSEABLE line (a torn enqueue / external corruption) — is pushed VERBATIM to
/// `preserved` (the caller re-emits it in its rewrite) and `None` returned.
/// Centralizing this closes the runtime-rewrite silent-loss window: pre-fix each
/// site's `Err(_) => continue` DROPPED the corrupt line on rewrite — before the
/// startup-only `recover_half_writes` could quarantine it — with no log. Routing
/// all sites through ONE helper (not per-site patches) prevents a missed/ drifted
/// site (the #27 lesson). Blank-line handling stays the caller's concern. WARNs so
/// the formerly-silent event is observable.
fn parse_or_preserve_line(line: &str, preserved: &mut Vec<String>) -> Option<InboxMessage> {
    match serde_json::from_str::<InboxMessage>(line) {
        Ok(msg) if msg.schema_version <= InboxMessage::CURRENT_VERSION => Some(msg),
        Ok(msg) => {
            // Forward-schema: unknown newer fields — re-emit intact, never downgrade-delete.
            tracing::warn!(
                found = msg.schema_version,
                supported = InboxMessage::CURRENT_VERSION,
                "inbox rewrite: preserving forward-schema row verbatim"
            );
            preserved.push(line.to_string());
            None
        }
        Err(e) => {
            // AUDIT3-005: a torn enqueue / externally-corrupt line. Preserve verbatim
            // so a runtime rewrite never destroys it before startup
            // `recover_half_writes` quarantines it to inbox.recovery/.
            tracing::warn!(
                error = %e,
                line_len = line.len(),
                "inbox rewrite: preserving unparseable line verbatim (AUDIT3-005; was silently dropped)"
            );
            preserved.push(line.to_string());
            None
        }
    }
}

/// Iterate `home/inbox`'s `*.jsonl` entry paths (the directory-walk +
/// extension-filter shared by `sweep_expired` / `get_thread` / `find_message`).
/// A `read_dir` error yields an empty iteration (callers historically
/// early-`return`/skip on it). Yields only the raw path: each caller keeps its
/// own per-file choice — the read-only scanners (`get_thread` / `find_message`)
/// use the path directly, while `sweep_expired` re-derives the stem name and
/// re-resolves through [`with_inbox_lock`] (preserving the UUID→canonical
/// [`inbox_path_resolved`] redirect; operating on this raw path would drop it).
fn inbox_files(home: &Path) -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir(home.join("inbox"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
}

/// #2604: the agent names with an inbox file (the `*.jsonl` file stems under
/// `home/inbox`). The offline-unread watchdog iterates these to reach agents
/// that are NOT in the live registry (offline / never-existed) — the exact set
/// `poll_reminder` (registry-driven) can never see. A `read_dir` error yields an
/// empty iteration, same as [`inbox_files`].
pub fn inbox_agent_names(home: &Path) -> Vec<String> {
    inbox_files(home)
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
        .collect()
}

/// Enqueue a message — in-place flock'd append + fsync (O(1) JSONL append).
///
/// NOT crash-atomic: enqueue appends in place — no tmp+rename (only the
/// read-modify-write rewriters drain/sweep/clear/supersede use that). A crash
/// mid-write can leave a half-written trailing line. Read-ONLY paths skip an
/// unparseable line; the read-modify-write rewriters PRESERVE it verbatim
/// (AUDIT3-005 — all route through [`parse_or_preserve_line`], which re-emits a
/// forward-schema OR unparseable line instead of dropping it), so a RUNTIME
/// rewrite can never silently destroy it before [`recover_half_writes`]
/// quarantines it (to `inbox.recovery/`) at the next startup.
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

/// #t-84833-14 (R3 perf): the minimal projection of an [`InboxMessage`] that
/// decides post-#2299 *actionable-unread* membership. Deserializing a JSONL line
/// into this — rather than the full `InboxMessage` — skips the dominant per-row
/// deserialize cost (allocating the large `text` String + the ~25 other fields),
/// on the hot `send` path (~7–13× the file read), while serde still validates the
/// JSON.
///
/// ## Validity boundary MUST match `InboxMessage` (r6 #2350)
/// The count must be byte-identical to the prior `from_str::<InboxMessage>` loop
/// for EVERY line that can appear in the JSONL — and that includes forward-schema
/// rows, which `drain`/`ack`/`clear`/`reclaim` PRESERVE verbatim as raw lines
/// (storage.rs preserved_forward). A forward-schema row (or a stray `{}`) that
/// LACKS a field `InboxMessage` requires is a syntactically-valid JSON object that
/// the old loop REJECTS (`from_str::<InboxMessage>` → `Err` → skipped). So this
/// probe mirrors `InboxMessage`'s required-PRESENCE set — `from`, `text`, `kind`,
/// `timestamp` carry no `#[serde(default)]` there, so a row missing any of them
/// fails to deserialize here too, exactly as before. An earlier all-`Option`
/// probe accepted such rows and miscounted them as unread (the re-page gate) —
/// the bug r6 caught.
///
/// `text` (the big allocation we exist to avoid) uses [`serde::de::IgnoredAny`]:
/// its PRESENCE is still required, but its value is consumed without building a
/// `String`. `from`/`kind`/`timestamp` mirror `InboxMessage`'s declarations
/// exactly (tiny allocations). The one residual, intentional, r6-endorsed
/// deviation: `text` present as a NON-string value (`"text":1`) is accepted here
/// but rejected by `InboxMessage` — type-enforcing it would re-introduce the very
/// `String` allocation this optimization removes, and no producer (current or
/// forward-schema) ever emits a non-string `text`.
///
/// Equivalence is exhaustively pinned by `perf_r3_equiv` (proptest over BOTH
/// well-formed and adversarial valid-JSON-non-`InboxMessage` rows + state-coverage
/// via the real mutators + named boundary fixtures).
#[derive(serde::Deserialize)]
#[allow(dead_code)] // `from`/`text`/`kind` are required-presence validity markers
                    // (consumed by serde, never read); only the filter fields +
                    // `timestamp` are read.
struct UnreadProbe {
    // Required-presence markers mirroring `InboxMessage` (no `#[serde(default)]`),
    // so this probe rejects exactly the rows the full struct rejects.
    from: String,
    text: serde::de::IgnoredAny,
    kind: Option<String>,
    timestamp: String,
    // Filter fields — optional exactly as in `InboxMessage` (`#[serde(default)]`).
    #[serde(default)]
    read_at: Option<String>,
    #[serde(default)]
    delivering_at: Option<String>,
    #[serde(default)]
    superseded_by: Option<String>,
}

impl UnreadProbe {
    /// post-#2299 actionable-unread filter — identical to the `read_at.is_none()
    /// && delivering_at.is_none() && superseded_by.is_none()` predicate the
    /// full-struct loops used. `delivering` rows (in-flight) and `superseded`
    /// rows (silently retired by `drain`) are excluded so a healthy agent is not
    /// re-paged (see `unread_count` MED-3 / #2299 notes).
    fn is_unread(&self) -> bool {
        self.read_at.is_none() && self.delivering_at.is_none() && self.superseded_by.is_none()
    }
}

/// Count actionable-unread rows in inbox file `content` via the cheap
/// [`UnreadProbe`] deserialize. Shared spelling of the filter so the hot-path
/// counter and `unread_count` cannot drift.
fn count_unread_in_content(content: &str) -> usize {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| {
            serde_json::from_str::<UnreadProbe>(l)
                .map(|p| p.is_unread())
                .unwrap_or(false)
        })
        .count()
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
        // #t-84833-14 (R3 perf): count via the cheap `UnreadProbe` deserialize
        // instead of full `InboxMessage` (skips the big `text`/`from`/… allocs on
        // this hot `send` path). Same post-#2299 filter — superseded + delivering
        // rows excluded so a healthy agent isn't re-paged. Byte-identical result
        // to the prior full-struct count (pinned by `perf_r3_equiv`).
        let count = count_unread_in_content(&existing);
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
                // Authoritative match is `correlation_id` EQUALITY, not a
                // `text.contains` substring. ci-watch enqueues set
                // `correlation_id = "<repo>@<branch>"` (see `ci_watch::poller` /
                // `ci_watch::sweep`). A substring match supersedes a
                // prefix-colliding branch: a new `repo@feat/x` event would wrongly
                // supersede an unread `repo@feat/x-2` notice (its key CONTAINS
                // `repo@feat/x`), silently dropping feat/x-2's CI-ready signal. The
                // `line.contains` pre-filter above stays as a cheap screen (it
                // over-includes; this equality is the authoritative decision).
                if msg.read_at.is_none()
                    && msg.superseded_by.is_none()
                    && msg.kind.as_deref() == Some("ci-watch")
                    && msg.from == "system:ci"
                    && msg.correlation_id.as_deref() == Some(repo_branch_key)
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
            crate::store::fsync_parent_dir(path); // AUDIT2-015: durable rename
        }
        Ok(())
    });
}

/// Drain unread messages: mark them with `read_at` and write back.
/// Returns only the messages that were previously unread.
///
/// Soft-delete semantics: messages stay in the JSONL file with `read_at`
/// set; [`sweep_expired`] removes them later based on TTL rules.
/// #1940: byte budget for one drain's returned batch — kept under
/// `request_dedup::PER_ENTRY_CAP_BYTES` (64 KiB) so the response is always
/// dedup-cacheable (never `Oversized`). That is the whole fix: the bridge (#842)
/// retries a lost transport with the SAME `request_id`, and `request_dedup`
/// returns the cached response — but only if it was cacheable. An uncapped drain
/// that exceeded 64 KiB was cached as `Oversized`, so the retry got a
/// deterministic error and the (already `read_at`-set) content was lost.
pub(crate) const DRAIN_BATCH_BUDGET_BYTES: usize = 48 * 1024;

/// Drain unread messages: mark `read_at` and write back.
/// Uses atomic tmp+fsync+rename for crash safety.
///
/// #1940 (mark-read ≠ delivered — the #1888 class on the DELIVERY side): the MCP
/// response can be lost AFTER drain() has persisted `read_at`. The recovery is
/// the bridge's same-`request_id` retry + `request_dedup` cache, which is
/// already in place; the ONLY hole was that an oversized (>64 KiB) response was
/// cached as `Oversized` and could not be re-served. So (d): cap the returned
/// batch under the dedup per-entry cap, leaving the remainder UNREAD for the next
/// drain (a message is never split — at least one is always returned). A
/// per-agent `.draining` re-serve snapshot was evaluated and REJECTED: it cannot
/// distinguish a timeout-retry from a normal next poll without a client cursor,
/// so it either starves new messages (re-serve within a TTL) or double-delivers
/// (re-serve once / concurrent) — both break the inbox's exactly-once contract.
pub fn drain(home: &Path, name: &str) -> Vec<InboxMessage> {
    let path = inbox_path_resolved(home, name);

    if !path.exists() {
        return Vec::new();
    }

    // Phase 1 (locked): run a byte-capped drain.
    // Returns (messages_to_return, newly_delivered_subset).
    let (to_return, newly_delivered_msgs) = match with_inbox_lock(home, name, |path| {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return (Vec::new(), Vec::new()),
        };

        let now = chrono::Utc::now().to_rfc3339();
        let mut all_messages: Vec<InboxMessage> = Vec::new();
        // CR-2026-06-14: forward-schema rows we can't parse but must NOT delete
        // on rewrite — preserved as raw lines and re-emitted verbatim.
        let mut preserved_forward: Vec<String> = Vec::new();
        let mut batch: Vec<InboxMessage> = Vec::new(); // returned this drain
        let mut newly_delivered: Vec<InboxMessage> = Vec::new(); // #2299: just delivered (now `delivering`)
        let mut budget_used = 0usize;
        let mut budget_closed = false; // (d): once closed, remaining stay unread
        let mut changed = false;

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            // AUDIT3-005: parse-or-preserve — a forward-schema row (newer daemon) AND
            // an unparseable line (torn enqueue / corruption) are preserved verbatim
            // (re-emitted below), never silently dropped on the rewrite.
            let Some(mut msg) = parse_or_preserve_line(line, &mut preserved_forward) else {
                continue;
            };
            if msg.read_at.is_none() {
                if msg.superseded_by.is_some() {
                    // superseded obligations are retired (marked read) but never
                    // returned — unchanged from the pre-#1940 behavior. Covers an
                    // in-flight `delivering` row too (a newer message obsoleted it).
                    msg.read_at = Some(now.clone());
                    changed = true;
                    all_messages.push(msg);
                    continue;
                }
                if msg.delivering_at.is_some() {
                    // #2299 (A) implicit ack: a PRIOR `delivering` batch the agent
                    // already received. Its re-drain confirms consumption → mark
                    // PROCESSED (read_at), never return again (no double-deliver).
                    // A turn that died instead never reaches here; the reclaim-TTL
                    // sweep resets it to unread for re-delivery.
                    msg.read_at = Some(now.clone());
                    changed = true;
                    all_messages.push(msg);
                    continue;
                }
                if budget_closed {
                    // (d): budget already hit — leave this (and the rest) UNREAD.
                    all_messages.push(msg);
                    continue;
                }
                let sz = serde_json::to_string(&msg)
                    .map(|s| s.len())
                    .unwrap_or(line.len());
                // Always take ≥1 message (progress); otherwise only while the
                // running batch stays under budget. A message is never split.
                if !batch.is_empty() && budget_used + sz > DRAIN_BATCH_BUDGET_BYTES {
                    budget_closed = true;
                    all_messages.push(msg);
                    continue;
                }
                budget_used += sz;
                if auto_ack_on_drain_kind(&msg) {
                    // Fire-and-forget notifications have no blocked sender and no
                    // required reply. Return them to the agent once, but settle them
                    // immediately so daemon restarts cannot resurrect completed
                    // PR/CI/status chatter through the delivering reclaim path.
                    msg.read_at = Some(now.clone());
                } else {
                    // #2299: unread → DELIVERING (not processed). read_at stays None
                    // until the agent acks (explicit `inbox ack` / implicit next-drain)
                    // or the reclaim-TTL resets it. A turn dying after this drain leaves
                    // the row `delivering` → reclaimed → re-delivered (no silent loss).
                    msg.delivering_at = Some(now.clone());
                }
                changed = true;
                // #1888: a `ci-ready-for-action` handoff just transitioned to
                // DELIVERING on this drain (#2299: was "read" pre-3-state). Phase-1
                // this trace PROVED the read-state coupling; Phase-2 the watchdog
                // scans the `ci_handoff_track` sidecar instead, so this no longer
                // blinds anything — the trace stays as the delivery-vs-resolution
                // timeline marker. Unchanged in effect.
                if msg.kind.as_deref() == Some("ci-ready-for-action") {
                    let age_at_read_secs = chrono::DateTime::parse_from_rfc3339(&msg.timestamp)
                        .ok()
                        .map(|t| {
                            chrono::Utc::now()
                                .signed_duration_since(t.with_timezone(&chrono::Utc))
                                .num_seconds()
                        })
                        .unwrap_or(-1);
                    // info!-level so it lands in the production daemon.log (default
                    // filter is `agend_terminal=info`); rare — only a
                    // ci-ready-for-action message transitioning to delivering on a drain.
                    tracing::info!(
                        tag = "#1888-ciready-read",
                        agent = %name,
                        correlation = msg.correlation_id.as_deref().unwrap_or("<none>"),
                        age_at_read_secs,
                        "ci-ready-for-action handoff marked delivering on drain"
                    );
                }
                batch.push(msg.clone());
                newly_delivered.push(msg.clone());
                all_messages.push(msg);
            } else {
                all_messages.push(msg);
            }
        }

        if changed {
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
                // CR-2026-06-14: re-emit preserved forward-schema rows verbatim so
                // a downgrade never destroys a message a newer daemon wrote.
                for raw in &preserved_forward {
                    writeln!(f, "{raw}")?;
                }
                f.sync_all()?;
                std::fs::rename(&write_tmp, path)?;
                crate::store::fsync_parent_dir(path); // AUDIT2-015: durable rename
                Ok(())
            })();
            if let Err(e) = result {
                tracing::warn!(error = %e, "inbox drain write-back failed");
            }
        }

        (batch, newly_delivered)
    }) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(error = %e, "inbox drain lock failed");
            return Vec::new();
        }
    };

    // Phase 2 (unlocked): side effects only for messages newly read THIS drain
    // (empty on a snapshot re-serve — those already ran on the original drain).
    for msg in &newly_delivered_msgs {
        if let Some(ref id) = msg.id {
            crate::daemon::notification_dedup::global().mark_consumed(name, id);
        }
    }

    if let Some(channel_msg) = newly_delivered_msgs
        .iter()
        .rev()
        .find(|m| m.channel.is_some())
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

    // #1665/#2042 reply-ledger: arm the delivery-closure audit for EVERY user
    // channel message in this drain, in arrival order. `m.channel.is_some()`
    // is exactly the "[user:… via channel] inbound" eligibility gate. Inside
    // `arm`: a duplicate of the CURRENT obligation (same sender + normalized
    // content) group-joins it instead of superseding; a redelivery of an
    // already-SETTLED message opens no new obligation; genuinely new content
    // supersedes (the user moved on, never escalate the old turn).
    for m in newly_delivered_msgs.iter().filter(|m| m.channel.is_some()) {
        crate::reply_ledger::arm(
            home,
            name,
            *m.channel.as_ref().expect("checked"),
            m.id.clone(),
            m.thread_id.clone(),
            m.kind.clone(),
            Some(&m.from),
            Some(&m.text),
        );
    }

    to_return
}

// #1940: the pre-existing `.draining` snapshot READ/recovery path
// (`read_drain_file` + the `<name>.draining` existence check) was REMOVED here.
// It was zero-creator dead code (nothing wrote `.draining` since an earlier
// refactor dropped the creation half), and completing it into a real re-serve
// snapshot was evaluated and REJECTED for #1940: without a client cursor a
// snapshot cannot tell a timeout-retry from a normal next poll, so it either
// starves new messages (re-serve within a TTL — a rapid drain loop never
// advances) or double-delivers (re-serve once / concurrent drains), both of
// which break the inbox's exactly-once contract. The bridge's same-`request_id`
// retry + `request_dedup` cache already recovers a lost transport correctly;
// the (d) byte cap above is what keeps that recovery from being defeated by an
// `Oversized` response.

/// #2299 explicit ack (C): confirm `delivering` rows as `processed` (stamp
/// `read_at`). Called by the `inbox action=ack` MCP path after an agent has
/// HANDLED what it drained. This is the primary, unambiguous confirm signal —
/// the implicit next-drain ack (A) and the reclaim-TTL (the net) only cover
/// agents that don't (or can't) ack.
///
/// `msg_id`: `Some(id)` acks exactly that message; `None` acks EVERY currently
/// `delivering` row for the caller (the "I've processed this whole batch" path).
/// Only `delivering` rows (`delivering_at` set, `read_at` None) transition —
/// an already-processed row is an idempotent no-op, and a plain `unread` row is
/// left untouched (acking a never-delivered message would be a silent drop).
/// Returns the count of rows newly transitioned to `processed`.
pub fn ack(home: &Path, name: &str, msg_id: Option<&str>) -> usize {
    let path = inbox_path_resolved(home, name);
    if !path.exists() {
        return 0;
    }
    with_inbox_lock(home, name, |path| {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let now = chrono::Utc::now().to_rfc3339();
        let mut all: Vec<InboxMessage> = Vec::new();
        let mut preserved_forward: Vec<String> = Vec::new();
        let mut acked = 0usize;
        let mut changed = false;
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            // AUDIT3-005: forward-schema + unparseable lines preserved verbatim.
            let Some(mut msg) = parse_or_preserve_line(line, &mut preserved_forward) else {
                continue;
            };
            // Only an in-flight `delivering` row is ackable. Match on id when given.
            let is_target = msg_id.is_none_or(|id| msg.id.as_deref() == Some(id));
            if is_target && msg.read_at.is_none() && msg.delivering_at.is_some() {
                msg.read_at = Some(now.clone());
                acked += 1;
                changed = true;
            }
            all.push(msg);
        }
        if changed {
            let tmp = path.with_extension("jsonl.tmp");
            let r = (|| -> anyhow::Result<()> {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&tmp)?;
                for m in &all {
                    writeln!(f, "{}", serde_json::to_string(m)?)?;
                }
                for raw in &preserved_forward {
                    writeln!(f, "{raw}")?;
                }
                f.sync_all()?;
                std::fs::rename(&tmp, path)?;
                crate::store::fsync_parent_dir(path); // AUDIT2-015: durable rename
                Ok(())
            })();
            if let Err(e) = r {
                tracing::warn!(error = %e, "inbox ack write-back failed");
                return 0;
            }
        }
        acked
    })
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "inbox ack lock failed");
        0
    })
}

/// #2622 PR-2: settle the single row `msg_id` to `read` regardless of its
/// current state (`unread` OR `delivering`) — the discharge path uses this so a
/// discharged channel message stops redelivering (unlike [`ack`], which only
/// transitions a `delivering` row; a discharged obligation's row is typically
/// `unread`). Idempotent (an already-`read` row is a no-op). Returns whether a
/// row was newly marked read. Same flock + atomic tmp+rename + forward-schema
/// preservation as [`ack`].
pub fn settle_read_by_id(home: &Path, name: &str, msg_id: &str) -> bool {
    let path = inbox_path_resolved(home, name);
    if !path.exists() {
        return false;
    }
    with_inbox_lock(home, name, |path| {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let now = chrono::Utc::now().to_rfc3339();
        let mut all: Vec<InboxMessage> = Vec::new();
        let mut preserved_forward: Vec<String> = Vec::new();
        let mut settled = false;
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            // AUDIT3-005: forward-schema + unparseable lines preserved verbatim.
            let Some(mut msg) = parse_or_preserve_line(line, &mut preserved_forward) else {
                continue;
            };
            if msg.id.as_deref() == Some(msg_id) && msg.read_at.is_none() {
                msg.read_at = Some(now.clone());
                settled = true;
            }
            all.push(msg);
        }
        if settled {
            let tmp = path.with_extension("jsonl.tmp");
            let r = (|| -> anyhow::Result<()> {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&tmp)?;
                for m in &all {
                    writeln!(f, "{}", serde_json::to_string(m)?)?;
                }
                for raw in &preserved_forward {
                    writeln!(f, "{raw}")?;
                }
                f.sync_all()?;
                std::fs::rename(&tmp, path)?;
                crate::store::fsync_parent_dir(path);
                Ok(())
            })();
            if let Err(e) = r {
                tracing::warn!(error = %e, "inbox settle_read_by_id write-back failed");
                return false;
            }
        }
        settled
    })
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "inbox settle_read_by_id lock failed");
        false
    })
}

/// Test-only: force one row's `delivering_at` to an explicit rfc3339 timestamp so
/// a test can age a `delivering` row past [`RECLAIM_TTL_SECS`] deterministically
/// (no wall-clock sleep) and then exercise the real [`reclaim_stale_delivering`]
/// path. Rewrites via raw JSON so forward-schema fields survive.
#[cfg(test)]
pub(crate) fn set_row_delivering_at_for_test(
    home: &Path,
    name: &str,
    msg_id: &str,
    ts_rfc3339: &str,
) {
    let path = inbox_path_resolved(home, name);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let mut out = String::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(mut v) => {
                if v.get("id").and_then(|x| x.as_str()) == Some(msg_id) {
                    v["delivering_at"] = serde_json::Value::String(ts_rfc3339.to_string());
                }
                out.push_str(&serde_json::to_string(&v).unwrap_or_else(|_| line.to_string()));
            }
            Err(_) => out.push_str(line),
        }
        out.push('\n');
    }
    let _ = std::fs::write(&path, out);
}

/// Session-reset settle: stamp `read_at` on ALL `delivering` rows for `name`,
/// transitioning them to `processed` so [`reclaim_stale_delivering`] will not
/// revert them to `unread` for re-delivery.
///
/// Called by the daemon when an agent's session is **intentionally reset** —
/// `restart_instance mode=fresh` — where the agent's context is known to be
/// lost. The old session already drained these messages (they are
/// `delivering`), so treating them as "delivered and processed" is correct:
/// re-injecting them into a fresh, context-less session would cause the
/// stale-resend pattern (agend-customization#159).
///
/// This does NOT break #2299's at-least-once guarantee for crashes: an
/// unintentional interruption (OOM, kill -9, backend crash) never reaches this
/// code path — only the explicit `restart mode=fresh` handler calls it — so
/// `reclaim_stale_delivering` still recovers those.
///
/// `restart_instance mode=resume` deliberately does NOT call this: the resumed
/// session retains context and the implicit next-drain ack (A) handles it.
pub fn settle_delivering_for_session_reset(home: &Path, name: &str) -> usize {
    let settled = ack(home, name, None);
    if settled > 0 {
        tracing::info!(
            tag = "#2299-session-settle",
            agent = %name,
            count = settled,
            "session-reset: settled delivering rows to processed (preventing stale re-inject)"
        );
    }
    settled
}

/// Scoped ack: settle DELIVERING rows for `name` where `task_id` matches
/// `correlation_id`. Used by the `send(kind=report, ack_inbox=true)` flow to
/// settle exactly the dispatch message(s) that originated the task being
/// reported on — not every delivering message in the inbox.
///
/// This is a filtered variant of [`ack`]: same JSONL read-modify-write
/// pattern, but only transitions rows where `msg.task_id ==
/// Some(correlation_id)` AND the row is in `delivering` state.
///
/// Returns the count of rows newly transitioned to `processed`.
pub fn ack_by_correlation(home: &Path, name: &str, correlation_id: &str) -> usize {
    let path = inbox_path_resolved(home, name);
    if !path.exists() {
        return 0;
    }
    with_inbox_lock(home, name, |path| {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let now = chrono::Utc::now().to_rfc3339();
        let mut all: Vec<InboxMessage> = Vec::new();
        let mut preserved_forward: Vec<String> = Vec::new();
        let mut acked = 0usize;
        let mut changed = false;
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            // AUDIT3-005: forward-schema + unparseable lines preserved verbatim.
            let Some(mut msg) = parse_or_preserve_line(line, &mut preserved_forward) else {
                continue;
            };
            // Only ack delivering rows whose task_id matches the correlation_id.
            let task_matches = msg
                .task_id
                .as_deref()
                .is_some_and(|tid| tid == correlation_id);
            if task_matches && msg.read_at.is_none() && msg.delivering_at.is_some() {
                msg.read_at = Some(now.clone());
                acked += 1;
                changed = true;
            }
            all.push(msg);
        }
        if changed {
            let tmp = path.with_extension("jsonl.tmp");
            let r = (|| -> anyhow::Result<()> {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&tmp)?;
                for m in &all {
                    writeln!(f, "{}", serde_json::to_string(m)?)?;
                }
                for raw in &preserved_forward {
                    writeln!(f, "{raw}")?;
                }
                f.sync_all()?;
                std::fs::rename(&tmp, path)?;
                crate::store::fsync_parent_dir(path); // AUDIT2-015: durable rename
                Ok(())
            })();
            if let Err(e) = r {
                tracing::warn!(error = %e, "inbox ack_by_correlation write-back failed");
                return 0;
            }
        }
        if acked > 0 {
            tracing::info!(
                agent = %name,
                %correlation_id,
                count = acked,
                "send+ack_inbox: settled delivering rows by correlation_id"
            );
        }
        acked
    })
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "inbox ack_by_correlation lock failed");
        0
    })
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
        // CR-2026-06-14: forward-schema rows preserved as raw lines (see drain()).
        let mut preserved_forward: Vec<String> = Vec::new();
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
            // AUDIT3-005: forward-schema + unparseable lines preserved verbatim.
            let Some(mut msg) = parse_or_preserve_line(line, &mut preserved_forward) else {
                continue;
            };
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
                // CR-2026-06-14: re-emit preserved forward-schema rows verbatim.
                for raw in &preserved_forward {
                    writeln!(f, "{raw}")?;
                }
                f.sync_all()?;
                std::fs::rename(&write_tmp, path)?;
                crate::store::fsync_parent_dir(path); // AUDIT2-015: durable rename
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

/// #2524 P6-r2 (#2537): does `msg` represent a ci-fail whose `(head_sha, job_name)`
/// signature is already discharged (`send.triaged`, PR-1)?
///
/// Silent-loss defense (§3.21 — this predicate can ONLY suppress, it is consumed
/// by callers that only ever narrow "worthy of nudge" to "not worthy", never widen
/// it): every extraction step fails OPEN — `false` = "not confirmed discharged,
/// treat as a normal live obligation":
/// - wrong `kind` (only `"ci-watch"` rows are ci-fail-shaped at all),
/// - no `Detail: <job>` line in the body ([`extract_ci_fail_job`] — a ci-pass/
///   ci-ended body has none, or the format has drifted),
/// - `correlation_id` isn't a parseable `repo@branch` pair,
/// - no on-disk ci-watch file for that `repo@branch`, or it has no `head_sha` yet,
/// - the ledger has no entry for `(head_sha, job)` — including a genuinely
///   different job, or the SAME job at an OLDER head (the watch's `head_sha` is
///   always the CURRENT head, so a discharge recorded against a since-superseded
///   head naturally stops matching — free head-invalidation, per PR-1's spike).
///
/// Only an EXPLICIT ledger hit for the watch's CURRENT head returns `true`.
fn is_discharged_ci_fail(home: &Path, msg: &InboxMessage) -> bool {
    if msg.kind.as_deref() != Some("ci-watch") {
        return false;
    }
    let Some(job) = extract_ci_fail_job(&msg.text) else {
        return false;
    };
    let Some((repo, branch)) = msg
        .correlation_id
        .as_deref()
        .and_then(|c| c.split_once('@'))
    else {
        return false;
    };
    let watch_path = crate::daemon::ci_watch::ci_watches_dir(home)
        .join(crate::daemon::ci_watch::watch_filename(repo, branch));
    let Some(head_sha) = std::fs::read_to_string(&watch_path)
        .ok()
        .and_then(|c| serde_json::from_str::<crate::daemon::ci_watch::WatchState>(&c).ok())
        .and_then(|w| w.head_sha)
    else {
        return false;
    };
    let Some(entry) = crate::daemon::discharge_ledger::lookup_discharge(home, &head_sha, &job)
    else {
        return false;
    };
    // Audit trail (dispatching task's explicit ask): every absorption is logged,
    // even if the SAME row is re-evaluated on a later tick/sweep — the audit log
    // is append-only history, not a dedup ledger (the discharge ledger already is).
    crate::event_log::log(
        home,
        "discharge_absorbed",
        &msg.from,
        &format!(
            "head={head_sha} job={job} discharged_by={} discharged_at={}",
            entry.discharged_by, entry.discharged_at
        ),
    );
    true
}

/// Pull the job name out of a ci-fail body's `Detail: <job>` line — the daemon's
/// own `build_inbox_body` format (`daemon/ci_watch/poller.rs`), the SAME material
/// `send.triaged.job` reports when an agent discharges it. `None` when the line
/// is absent (a ci-pass/ci-ended body has none) or empty.
fn extract_ci_fail_job(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("Detail: "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
        // #t-84833-14 (R3 perf): same `UnreadProbe` cheap deserialize as the
        // hot-path counter (skips big `text`/`from` allocs). The filter is
        // unchanged:
        // MED-3: a superseded-but-undrained row is NOT actionable unread —
        // `drain` silently consumes it (stamps `read_at`, never surfaces it).
        // Counting it here inflated the unread count, so a busy branch whose
        // CI SHA churns (each `mark_ci_watch_superseded` leaves the prior row
        // superseded + unread until the next drain) tripped
        // `inbox_stuck_watchdog` into false-paging a healthy agent. Match
        // drain's actionable-unread definition.
        // #2299: a `delivering` row (`delivering_at` set, `read_at` None) is
        // in-flight — already delivered to the agent, not actionable-unread.
        // Counting it would re-page a healthy agent mid-turn (and the
        // reclaim-TTL sweep, not the watchdog, owns re-delivery if it stalls).
        if let Ok(probe) = serde_json::from_str::<UnreadProbe>(line) {
            if probe.is_unread() {
                count += 1;
                // `oldest` over the unread rows — identical to the prior
                // `msg.timestamp` parse (`timestamp` is a required field, so a
                // parsed row always has it; an unparseable value leaves `oldest`
                // untouched, as before).
                if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&probe.timestamp) {
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

/// #2524 P6-r2 (#2537): [`unread_count`], but a ci-fail row already discharged
/// (`(head_sha, job)` — [`is_discharged_ci_fail`]) doesn't count. Byte-identical
/// to `unread_count` for every row that isn't a discharged ci-fail — same TTL/
/// delivering/superseded filter, same `oldest` computation.
///
/// This is the fix for `collect_poll_reminders`'s literal observed duplicate-nudge:
/// `should_notify_and_record` only compares whether the unread COUNT changed. A
/// second `[ci-fail]` for the SAME `(head, job)` an agent already triaged is a
/// genuinely NEW file row (0→1, a real count change) even though the daemon and
/// the agent both already know it's the same failure — `unread_count` alone has no
/// way to see that. This function is the discharge-aware count `collect_poll_reminders`
/// needs instead.
pub fn unread_count_after_discharge(
    home: &Path,
    name: &str,
) -> (usize, Option<chrono::DateTime<chrono::Utc>>) {
    let path = inbox_path_resolved(home, name);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return (0, None),
    };
    let mut count = 0usize;
    let mut oldest: Option<chrono::DateTime<chrono::Utc>> = None;
    for line in content.lines() {
        let Ok(probe) = serde_json::from_str::<UnreadProbe>(line) else {
            continue;
        };
        if !probe.is_unread() {
            continue;
        }
        // Only a ci-watch row needs the full parse (signature extraction) — every
        // other kind is counted exactly as `unread_count` does, cheap-probe-only.
        if probe.kind.as_deref() == Some("ci-watch") {
            if let Ok(full) = serde_json::from_str::<InboxMessage>(line) {
                if is_discharged_ci_fail(home, &full) {
                    continue; // absorbed — does not count, does not set `oldest`
                }
            }
        }
        count += 1;
        if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&probe.timestamp) {
            let ts_utc = ts.with_timezone(&chrono::Utc);
            if oldest.is_none_or(|t| t > ts_utc) {
                oldest = Some(ts_utc);
            }
        }
    }
    (count, oldest)
}

/// Which UNREAD messages are real OBLIGATIONS that MUST keep nagging — `Some(reason)`
/// = an unhandled obligation (a sender is blocked / a task is open), `None` = safe to
/// drop from attention. The SINGLE source of truth shared by `inbox action=clear`'s
/// KEEP-set ([`clear_compact`]) and the reclaim re-nudge gate ([`reclaim_renudge_worthy`],
/// in [`reclaim_stale_delivering`]), so the two can never drift
/// (decision d-20260607081209372642-1).
///
/// `query` → always an obligation (the sender is blocked on a reply). `task` → an
/// obligation unless the board proves it terminal (Done/Cancelled). EVERYTHING ELSE
/// (report / update / ci-watch / poll / a plain `kind=None` message) → `None`: a
/// fire-and-forget delivery with no one waiting, so it must NOT be re-paged periodically.
/// When task proof is uncertain we KEEP (failure mode = noise, never hidden work).
pub fn obligation_reason(home: &Path, msg: &InboxMessage) -> Option<String> {
    match msg.kind.as_deref() {
        Some("query") => Some("unanswered query".to_string()),
        Some("task") => {
            let tid = msg.task_id.as_deref().or(msg.correlation_id.as_deref());
            match tid {
                Some(id) => match crate::tasks::load_by_id(home, id) {
                    Some(t)
                        if matches!(
                            t.status,
                            crate::task_events::TaskStatus::Done
                                | crate::task_events::TaskStatus::Cancelled
                        ) =>
                    {
                        None
                    }
                    Some(t) => Some(format!("task {id} not terminal (status={})", t.status)),
                    None => Some(format!("task {id} not on board — kept")),
                },
                None => Some("task without id — kept".to_string()),
            }
        }
        _ => None,
    }
}

/// #2604: per-agent split of ACTIONABLE-UNREAD messages into real OBLIGATIONS
/// (query / open task, via [`obligation_reason`]) vs the raw unread total, plus
/// the timestamp of the OLDEST obligation (the escalation watermark).
///
/// The offline-target escalation watchdog
/// ([`crate::daemon::per_tick::offline_unread_alert`]) keys the operator P0 on
/// the OBLIGATION watermark, not raw unread: a fire-and-forget report/update
/// piling up for an offline agent is not lost work (post-#2636 those kinds are
/// classified non-obligation), so it must never trip an Error-severity page.
/// `raw_unread_total` is carried only as context for the alert body.
///
/// LOCK-DISCIPLINE (mirrors [`unread_count`] + the [`obligation_reason`] warning):
/// reads the inbox file WITHOUT [`with_inbox_lock`] and calls [`obligation_reason`]
/// (task-board IO for `kind=task`). Safe ONLY because the per-tick caller runs it
/// unlocked — calling it inside an inbox flock is the #1617 fleet-stall class.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UnreadObligationSummary {
    /// Actionable-unread rows that are real obligations (query / open task).
    pub obligation_count: usize,
    /// Timestamp of the oldest obligation row — `None` when `obligation_count == 0`.
    pub oldest_obligation: Option<chrono::DateTime<chrono::Utc>>,
    /// All actionable-unread rows (obligations + fire-and-forget), context only.
    pub raw_unread_total: usize,
}

pub fn unread_obligation_summary(home: &Path, name: &str) -> UnreadObligationSummary {
    let path = inbox_path_resolved(home, name);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return UnreadObligationSummary::default(),
    };
    let mut summary = UnreadObligationSummary::default();
    for line in content.lines() {
        let msg: InboxMessage = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(_) => continue,
        };
        // Actionable-unread filter — the SAME predicate as `UnreadProbe::is_unread`
        // (storage.rs, post-#2299): a delivering (in-flight) / superseded (silently
        // retired by drain) / read row is not actionable unread.
        if msg.read_at.is_some() || msg.delivering_at.is_some() || msg.superseded_by.is_some() {
            continue;
        }
        summary.raw_unread_total += 1;
        if obligation_reason(home, &msg).is_some() {
            summary.obligation_count += 1;
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&msg.timestamp) {
                let ts_utc = ts.with_timezone(&chrono::Utc);
                if summary.oldest_obligation.is_none_or(|t| t > ts_utc) {
                    summary.oldest_obligation = Some(ts_utc);
                }
            }
        }
    }
    summary
}

/// #t-…61487: is a reverted (stale-`delivering` → unread) row worth RE-ARMING the
/// idle agent's poll-reminder for? `true` for a real obligation (unanswered query /
/// open task, via [`obligation_reason`]) OR an UNKNOWN kind (forward-compat: re-arm
/// conservatively rather than silently drop a kind we don't recognise). `false` for a
/// delivered NON-obligation we DO recognise (report / update / ci-watch / poll / a
/// plain `kind=None`) — those were the ~2h drained-report re-page noise this fixes.
///
/// ⚠ LOCK-DISCIPLINE: this calls [`obligation_reason`], which does task-board IO
/// (`tasks::load_by_id`) for `kind=task`. It MUST run UNLOCKED — in Phase 2 of
/// [`reclaim_stale_delivering`], after the `with_inbox_lock` closure closes — never
/// inside it (holding the per-agent inbox flock across a blocking board read is the
/// #1617 fleet-stall class).
///
/// #2524 P6-r2 (#2537): [`is_discharged_ci_fail`] is checked FIRST and can only
/// force `false` (never override an existing `false` back to `true`) — a
/// discharged ci-fail is never worth re-arming regardless of what
/// `obligation_reason`/`kind_is_unknown` would otherwise say. In practice this is
/// a defense-in-depth wire, not a behavior change TODAY: `ci-watch` is already in
/// `known_fire_and_forget_kind`, so `obligation_reason` already returns `None` and
/// `kind_is_unknown` already returns `false` for it — this function already
/// returns `false` for every ci-watch row before this change. Wired anyway per the
/// dispatching task, so a future kind-taxonomy change can't silently reopen the
/// gap this closes.
fn reclaim_renudge_worthy(home: &Path, msg: &InboxMessage) -> bool {
    if is_discharged_ci_fail(home, msg) {
        return false;
    }
    obligation_reason(home, msg).is_some() || kind_is_unknown(msg)
}

/// A `kind` outside the set the daemon is KNOWN to emit. [`obligation_reason`] maps
/// every recognised non-obligation kind to `None`, so without this helper an
/// unrecognised future kind would be indistinguishable from a non-obligation and
/// silently dropped from the reclaim re-nudge. The known set mirrors the kinds the
/// daemon emits today: `query` / `task` (obligations) plus the
/// [`known_fire_and_forget_kind`] set, plus a plain `kind=None`. Anything else
/// → `true` (unknown → conservatively re-arm; failure mode = noise, never silent
/// loss).
fn kind_is_unknown(msg: &InboxMessage) -> bool {
    match msg.kind.as_deref() {
        None => false, // a plain message is a recognised non-obligation
        Some("query" | "task") => false,
        Some(_) => !known_fire_and_forget_kind(msg),
    }
}

/// Recognised fire-and-forget *kinds* have no outstanding obligation once they
/// were delivered once. Reclaim uses this for rows left in `delivering`;
/// otherwise old `report`/`update`/`pr-merged` rows can loop forever through
/// poll-reminder (#2482 / ghost pr-merged).
///
/// Plain `kind=None` rows intentionally stay outside this set: #2299's core
/// anti-silent-loss guarantee still redelivers generic messages after a dead turn.
fn known_fire_and_forget_kind(msg: &InboxMessage) -> bool {
    matches!(
        msg.kind.as_deref(),
        Some(
            "report"
                | "update"
                | "ci-watch"
                | "ci-watch-stalled"
                | "ci-watch-resumed"
                | "poll"
                | "pr-merged"
                // #2412 follow-up (kind-taxonomy audit): the rest of the
                // pr-state FYI class (already fire-and-forget in
                // `auto_ack_on_drain_kind` since #2506, but missing here —
                // an inconsistency with no live impact today since
                // `auto_ack_on_drain_kind` settles them before reclaim ever
                // sees a stale `delivering` row of these kinds, but a
                // legacy/older-daemon-written row could still reach this
                // check) plus the two dispatch_idle notification subtypes
                // (one-shot-by-design at the source — team_nudge's
                // `nudge_sent_at` / dispatch_idle's `long_running_escalated`
                // latch never re-fire the same notice — see
                // `auto_ack_on_drain_kind` below for the primary fix).
                | "pr-closed-unmerged"
                | "pr-ready-for-merge"
                | "review-verdict"
                | "dispatch_idle_long_running"
                // #78445-2: the quota-wedge escalation subtype — same one-shot daemon
                // FYI shape as dispatch_idle_long_running (its source-side
                // `quota_escalated` latch never re-fires the same notice). MUST be
                // registered or reclaim classifies it unknown → renudge re-delivers it,
                // defeating the source one-shot (reviewer4 #2678 F1).
                | "dispatch_idle_quota_wedged"
                | "dispatch_idle_nudge"
                // #2622 PR-2: the operator-facing notice emitted when an agent
                // self-discharges a channel-reply obligation. Pure FYI — no
                // reply owed, no actor blocked (same fire-and-forget shape as
                // dispatch_idle_long_running). Classified fire-and-forget FROM
                // BIRTH so this "an obligation was closed" notice can never
                // itself become a nagging un-dischargeable obligation and
                // regenerate the loop it reports on (the fb2461 lesson).
                | "channel-reply-discharged"
                // #35896-11 ④: a `ci-ready-for-action` handoff left in `delivering`
                // (the reviewer drained but hasn't acted) must NOT be reverted to
                // unread by reclaim — that reopens a SECOND, uncoordinated poll-
                // reminder stream for the same event on top of the ci_handoff_track
                // renudge watchdog (the single intended ci-ready renudge, which is
                // decoupled from inbox read-state per #1888 and NOT affected by this
                // settle). Terminally settling the reclaimed row leaves the watchdog
                // as the one renudge source. NOTE: ci-ready is deliberately kept OUT
                // of `auto_ack_on_drain_kind` — it must survive the FIRST drain as
                // `delivering` so the reviewer sees it; only a STALE past-cap
                // delivering row is settled here.
                | "ci-ready-for-action"
        )
    )
}

/// Daemon-originated notification kinds that are safe to settle on first drain.
/// This is narrower than [`known_fire_and_forget_kind`]: peer `report`/`update`
/// messages keep #2299's delivering/ack behavior on first drain, while pure
/// daemon notifications avoid the restart/reclaim loop entirely.
fn auto_ack_on_drain_kind(msg: &InboxMessage) -> bool {
    matches!(
        msg.kind.as_deref(),
        Some(
            "ci-watch"
                | "ci-watch-stalled"
                | "ci-watch-resumed"
                | "poll"
                // #2506: the rest of the pr-state FYI class. `pr-merged` was the
                // only one #2493 listed; its siblings (terminal `pr-closed-unmerged`,
                // plus `pr-ready-for-merge` / `review-verdict`) are the same
                // fire-and-forget shape — once drained there is nothing to
                // re-deliver — and were nagging poll-reminder for merged PRs.
                | "pr-merged"
                | "pr-closed-unmerged"
                | "pr-ready-for-merge"
                | "review-verdict"
                // #2412 follow-up: primary fix for the live 58-minute
                // poll-reminder loop sample — both kinds are one-shot
                // daemon-generated FYI with no recipient action expected
                // (dispatch_idle_long_running: "Long run EXPECTED -> no
                // action"; dispatch_idle_nudge: "No action needed if
                // you're mid-task"). Settling on first drain, same as the
                // ci-watch/pr-state pure-daemon-notification precedent
                // above, means the row never enters `delivering` limbo
                // long enough to race the reclaim-TTL in the first place.
                | "dispatch_idle_long_running"
                // #78445-2: the quota-wedge subtype — same one-shot daemon FYI; settle
                // on first drain like its `dispatch_idle_long_running` sibling.
                | "dispatch_idle_quota_wedged"
                | "dispatch_idle_nudge"
                // #2622 PR-2: the self-discharge operator notice — settle on
                // first drain (same pure-daemon-notification shape). Paired
                // with the `known_fire_and_forget_kind` entry above so the
                // notice is inert on BOTH the drain-settle and reclaim paths.
                | "channel-reply-discharged"
        )
    )
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
    let now = chrono::Utc::now();
    for path in inbox_files(home) {
        // Extract agent name from filename (e.g. "agent1.jsonl" → "agent1") and
        // re-resolve through `with_inbox_lock` — NOT the raw yielded `path`,
        // which would bypass the UUID→canonical `inbox_path_resolved` redirect.
        let Some(agent_name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let _ = with_inbox_lock(home, agent_name, |path| {
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
            // AUDIT3-005: forward-schema + unparseable lines to re-emit verbatim (never swept).
            let mut preserved: Vec<String> = Vec::new();
            let mut changed = false;
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                // AUDIT3-005: a corrupt line has no parseable TTL — preserve it
                // verbatim, never sweep it away.
                let Some(msg) = parse_or_preserve_line(line, &mut preserved) else {
                    continue;
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
                // AUDIT3-005: only delete the file when NOTHING survives — a preserved
                // (forward-schema / unparseable) line must keep the file alive.
                if kept.is_empty() && preserved.is_empty() {
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
                        // AUDIT3-005: re-emit forward-schema + unparseable lines verbatim.
                        for raw in &preserved {
                            writeln!(f, "{raw}")?;
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

/// #2299 reclaim-TTL sweep: revert every `delivering` row (in-flight,
/// `delivering_at` set, `read_at` None) older than [`RECLAIM_TTL_SECS`] back to
/// `unread` (clear `delivering_at`) so the normal notification path re-delivers
/// it. This is the net under explicit ack (C) + implicit next-drain ack (A):
/// the only thing that recovers a message whose recipient turn DIED after the
/// drain but never confirmed.
///
/// Mirrors [`sweep_expired`]'s shape — directory scan, per-inbox
/// [`with_inbox_lock`], atomic tmp+rename write-back. For each reverted row it
/// also `forget`s the `notification_dedup` entry (flagged `consumed` at the
/// original delivering drain) so the re-inject isn't suppressed for the rest of
/// the dedup window. Recognised non-obligation *kinds* are terminally settled
/// instead of reverted (#2482). Runs from `per_tick::inbox_maintenance` (60-tick).
pub fn reclaim_stale_delivering(home: &Path) {
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
        let agent_name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        // #2622: for the production-default UUID-keyed inbox path, `agent_name`
        // (the file stem) IS the UUID, not the human name `agent_is_busy` looks
        // up by (`AgentSnapshot.name`). Resolve once per file (not per row) so
        // the busy gate actually engages against a UUID-keyed file; a
        // legacy/unresolvable stem falls back to using it as-is (the name-keyed
        // topology's stem already IS the human name).
        let busy_check_name = crate::fleet::resolve_name_by_uuid(home, &agent_name)
            .unwrap_or_else(|| agent_name.clone());
        // Phase 1 (locked): revert stale delivering rows, collect the reverted
        // MESSAGES (not just ids) so Phase 2 can classify them for the re-nudge gate.
        // #t-…61487: the classifier (`reclaim_renudge_worthy` → `obligation_reason`)
        // does task-board IO, so it MUST run in Phase 2 (unlocked) — NEVER here, under
        // `with_inbox_lock` (#1617 stall class).
        let reverted: Vec<InboxMessage> = with_inbox_lock(home, &agent_name, |path| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let mut all: Vec<InboxMessage> = Vec::new();
            let mut preserved_forward: Vec<String> = Vec::new();
            let mut reverted: Vec<InboxMessage> = Vec::new();
            let mut settled_non_obligations = 0usize;
            let mut changed = false;
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                // AUDIT3-005: forward-schema + unparseable lines preserved verbatim.
                let Some(mut msg) = parse_or_preserve_line(line, &mut preserved_forward) else {
                    continue;
                };
                // A `delivering` row = read_at None AND delivering_at Some.
                if msg.read_at.is_none() {
                    if let Some(ref since) = msg.delivering_at {
                        let elapsed_secs = chrono::DateTime::parse_from_rfc3339(since)
                            .map(|t| {
                                now.signed_duration_since(t.with_timezone(&chrono::Utc))
                                    .num_seconds()
                            })
                            .ok();
                        let stale = elapsed_secs.is_none_or(|s| s > RECLAIM_TTL_SECS);
                        if stale {
                            // #2622: a genuinely still-working agent can legitimately
                            // take longer than RECLAIM_TTL_SECS to read + analyze +
                            // reply to a long message (observed trigger: long-article/
                            // paper analysis requests). Reclaiming out from under it
                            // loses the race the agent's own eventual `ack` needs
                            // (`ack()` only transitions a row CURRENTLY `delivering`),
                            // which is exactly the #2622 redelivery loop. `agent_is_busy`
                            // is a fail-open snapshot read — a missing/corrupt snapshot
                            // means not-busy, so reclaim proceeds exactly as before this
                            // change (see AUDITED_FILES in
                            // tests/snapshot_failopen_invariant.rs). Bounded by
                            // `RECLAIM_BUSY_HARD_CAP_SECS` so a permanently-busy
                            // illusion (wedged agent) can never zombie a row in
                            // `delivering` forever; `sweep_expired`'s 30-day unread-tier
                            // TTL (keyed on `read_at.is_none()`, matched regardless of
                            // `delivering_at`) is the final backstop either way.
                            let busy_past_cap =
                                elapsed_secs.is_none_or(|s| s > RECLAIM_BUSY_HARD_CAP_SECS);
                            let busy = crate::snapshot::agent_is_busy(home, &busy_check_name);
                            if busy && !busy_past_cap {
                                // Still legitimately working — leave `delivering`,
                                // re-check next sweep.
                            } else {
                                if busy {
                                    tracing::warn!(
                                        agent = %agent_name,
                                        msg_id = ?msg.id,
                                        elapsed_secs = ?elapsed_secs,
                                        "reclaim: agent still reports busy past the hard \
                                         cap — forcing reclaim anyway (possible wedge)"
                                    );
                                }
                                if known_fire_and_forget_kind(&msg) {
                                    // #2482: fire-and-forget rows were already delivered
                                    // once and have no actor blocked on them. Reverting
                                    // them to unread lets poll-reminder/drain resurrect
                                    // old completed PR/CI/status chatter forever.
                                    // Terminally settle instead.
                                    msg.read_at = Some(now.to_rfc3339());
                                    settled_non_obligations += 1;
                                } else {
                                    // Collect the full reverted row: Phase 2 reads `id`
                                    // (dedup forget) + `kind`/`task_id`/`correlation_id`
                                    // (re-nudge gate).
                                    msg.delivering_at = None; // → unread (re-deliverable)
                                    reverted.push(msg.clone());
                                }
                                changed = true;
                            }
                        }
                    }
                }
                all.push(msg);
            }
            if changed {
                let tmp = path.with_extension("jsonl.tmp");
                let r = (|| -> anyhow::Result<()> {
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(&tmp)?;
                    for m in &all {
                        writeln!(f, "{}", serde_json::to_string(m)?)?;
                    }
                    for raw in &preserved_forward {
                        writeln!(f, "{raw}")?;
                    }
                    f.sync_all()?;
                    std::fs::rename(&tmp, path)?;
                    Ok(())
                })();
                if let Err(e) = r {
                    tracing::warn!(error = %e, "inbox reclaim write-back failed");
                    return Vec::new();
                }
                if !reverted.is_empty() {
                    tracing::info!(
                        tag = "#2299-reclaim",
                        agent = %agent_name,
                        count = reverted.len(),
                        "reverted stale delivering rows to unread for re-delivery"
                    );
                }
                if settled_non_obligations > 0 {
                    tracing::info!(
                        tag = "#2482-reclaim-settle",
                        agent = %agent_name,
                        count = settled_non_obligations,
                        "settled stale delivering non-obligation rows to processed"
                    );
                }
            }
            reverted
        })
        .unwrap_or_default();

        // Phase 2 (unlocked): drop each reverted row's dedup entry so the
        // daemon's re-inject of the now-unread message isn't suppressed.
        for m in &reverted {
            if let Some(id) = &m.id {
                crate::daemon::notification_dedup::global().forget(&agent_name, id);
            }
        }
        // #t-98760-9 (#2299 regression): also reset this agent's poll-reminder
        // count-dedup. The loop above clears the per-MESSAGE inject dedup, but the
        // poll-reminder ledger keys on the unread COUNT and still holds the
        // pre-drain N. Reverting back to N then reads as "no change" and the
        // idle-agent nudge is withheld until the next count change or the 10-min
        // reclaim TTL — defeating reclaim's "no silent message loss" promise.
        // Recording 0 on a 0-count poll pass would not suffice (a pass may never
        // observe the count==0 window between drain and reclaim); re-arming here
        // is deterministic.
        //
        // #t-…61487: re-arm ONLY if a reverted row is re-nudge-worthy — a real
        // obligation (unanswered query / open task) or an UNKNOWN kind. A delivered
        // NON-obligation we recognise (report / update / ci-watch / poll / plain)
        // must NOT re-arm: that was the ~2h drained-report re-page noise (the
        // pre-#t-…61487 code re-armed unconditionally). The #2299 promise above
        // still holds for obligations. `reclaim_renudge_worthy` does task-board IO
        // (`obligation_reason`) → it runs HERE (unlocked), never under
        // `with_inbox_lock` (#1617 stall class).
        //
        // #t-…61487 (pre-existing #2299-class residual): the poll-reminder ledger
        // (`should_notify_and_record`/`remove_agent`) is keyed by the HUMAN NAME
        // (`collect_poll_reminders` uses `handle.name`), never the raw file stem.
        // For a UUID-keyed inbox `agent_name` IS the UUID — passing it here no-ops
        // against the name-keyed ledger and the re-arm silently fails to fire.
        // Reuse `busy_check_name` (already resolved once per file, above) so this
        // matches the same human name the ledger is keyed by; a legacy/unresolvable
        // stem falls back to itself exactly as `busy_check_name` already does.
        if reverted.iter().any(|m| reclaim_renudge_worthy(home, m)) {
            crate::daemon::poll_reminder::remove_agent(&busy_check_name);
        }
    }
}

/// Look up a message by ID in a specific agent's inbox file.
/// If `instance` is provided, only that agent's inbox is searched.
pub fn describe_message(home: &Path, msg_id: &str, instance: &str) -> MessageStatus {
    let path = inbox_path_resolved(home, instance);
    // A missing/unreadable file → empty content → no match → `NotFound`
    // fall-through (same result as the prior explicit exists/Err guards).
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let now = chrono::Utc::now();
    for msg in parse_inbox_messages(&content) {
        if msg.id.as_deref() != Some(msg_id) {
            continue;
        }
        if let Some(ref read_at) = msg.read_at {
            return MessageStatus::ReadAt(read_at.clone(), msg.delivery_mode.clone());
        }
        // #2299: a delivered-but-unconfirmed (delivering) row — report it as
        // Delivering (not Unread/NotFound) so a delivery audit sees it WAS
        // delivered and does not re-send. Reported regardless of age (a
        // delivering row is short-lived: the reclaim-TTL reverts it to unread).
        if msg.delivering_at.is_some() {
            return MessageStatus::Delivering {
                delivery_mode: msg.delivery_mode.clone(),
                correlation_id: msg.correlation_id.clone(),
            };
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
        for path in inbox_files(home) {
            collect_thread_messages(&path, thread_id, &mut msgs);
        }
        // CR-2026-06-14: a migrated inbox is present under TWO directory entries —
        // `<name>.jsonl` and `<uuid>.jsonl` — holding the SAME messages, so the
        // cross-inbox scan double-counts every thread message. The two entries are
        // a symlink on Unix but a real `fs::copy` duplicate on Windows
        // (`inbox_path_resolved` migration), so a symlink-type skip only fixes
        // Unix. Dedup by message `id` instead — portable across both: message ids
        // are globally unique (`ensure_msg_id`), so the same id appearing in both
        // files is the migration duplicate, while a thread legitimately spanning
        // several agents' inboxes carries distinct ids and is preserved. Id-less
        // (legacy, pre-`ensure_msg_id`) rows are kept as-is (can't dedup safely).
        let mut seen_ids = std::collections::HashSet::new();
        msgs.retain(|m| match &m.id {
            Some(id) => seen_ids.insert(id.clone()),
            None => true,
        });
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
    // CR-2026-06-14: an unreadable file is skipped (yields no messages) so one
    // bad file can't hide a message living in a LATER inbox.
    for path in inbox_files(home) {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        for msg in parse_inbox_messages(&content) {
            if msg.id.as_deref() == Some(msg_id) {
                return Some(msg);
            }
        }
    }
    None
}

/// #982 B-narrow: scan `agent_name`'s inbox for a delivered blocking
/// dispatch (`kind ∈ {query, task}`) that shares the given `correlation_id`.
/// Used by `api::handlers::messaging` to override codex ack-absorption when an
/// inbound `kind=report|update` is the reply to a blocking dispatch the
/// recipient already received.
///
/// #2299: "delivered" = `read_at` set (processed) OR `delivering_at` set
/// (in-flight) — sibling-consistent with [`msg_already_drained_in_jsonl`]. A
/// `delivering` query/task has already been handed to the agent and it is
/// actively processing it, so a reply on that correlation should reach it
/// (override absorption) just as a fully-drained one does. Safe either way:
/// the reply is always enqueued; this only governs whether it ALSO wakes now.
pub fn has_drained_blocker_for_correlation(
    home: &Path,
    agent_name: &str,
    correlation_id: &str,
) -> bool {
    let path = inbox_path_resolved(home, agent_name);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    for msg in parse_inbox_messages(&content) {
        if msg.correlation_id.as_deref() == Some(correlation_id)
            && (msg.read_at.is_some() || msg.delivering_at.is_some())
            && matches!(msg.kind.as_deref(), Some("query") | Some("task"))
        {
            return true;
        }
    }
    false
}

/// Read the agent's inbox JSONL and return `true` iff a message with
/// the given `msg_id` exists AND has already been delivered — `read_at`
/// set (processed) OR `delivering_at` set (#2299 in-flight).
///
/// #2299: a `delivering` row has been handed to the agent once; treating it
/// as "already drained" keeps this #911 re-inject dedup from re-pushing an
/// in-flight message. A daemon re-inject of a `delivering` row would make the
/// agent re-drain it, and `drain`'s implicit-ack step would confirm-and-drop it
/// (premature `read_at`) — silent loss. Controlled re-delivery instead happens
/// only after the reclaim-TTL reverts it to plain `unread` (`delivering_at`
/// cleared), at which point this returns `false` again and re-inject resumes.
pub(super) fn msg_already_drained_in_jsonl(home: &Path, agent_name: &str, msg_id: &str) -> bool {
    // H12 (CR-2026-06-14): read the RESOLVED (UUID-when-id-native) path — the same
    // one `drain` writes `read_at` to. The old `inbox_path` (raw name) path does
    // not exist for an id-native instance, so this #911 JSONL dedup fallback read
    // a nonexistent file and returned `false` unconditionally — a permanent no-op
    // that let an already-drained message be re-injected after a daemon restart
    // (when the in-memory `OnceLock` ledger is gone).
    let path = inbox_path_resolved(home, agent_name);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    for msg in parse_inbox_messages(&content) {
        if msg.id.as_deref() == Some(msg_id)
            && (msg.read_at.is_some() || msg.delivering_at.is_some())
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod review_repro_inbox_notify;

// #t-84833-14 (R3 perf): equivalence proof for the `UnreadProbe` count refactor.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod perf_r3_equiv;

// #t-84833-14 (R3 perf): manual #[ignore]d bench (not CI — no criterion).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod perf_r3_bench;

// #2524 P6-r2 (#2537): discharge-ledger consumption tests for the two
// chokepoints (reclaim_renudge_worthy + unread_count_after_discharge).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod p6_discharge_consume;
