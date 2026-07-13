//! t-…-17 C8: the durable, crash-safe reviewer-assignment AUTHORITY STORE.
//!
//! This is the source-of-truth for an ACTIVE reviewer assignment, keyed
//! `(repo, branch, target)` and GENERATION-BOUND at dispatch by a MANDATORY
//! `pr_number` (never `Option`, never bound-on-observation — plan §2 / B18/B19).
//! The store owns the durable `ActiveAssignment` record plus a per-`(repo,branch)`
//! `TerminalMarkers` set (the EXACT set of terminal pr_numbers, RETAINED with NO
//! compaction/GC — B20/I19). Evidence (verdict/ack classification) is DERIVED
//! elsewhere; this file only persists authority + delivery bookkeeping and never
//! seeds a `PrState`.
//!
//! ## Durable substrate — mirrors [`crate::daemon::ci_handoff_track`]
//! Same three disciplines as the #1963 CI-handoff store, adapted to a per-branch
//! (not per-key) lock because revoke / transfer / terminal-tombstone must mutate
//! MULTIPLE targets on one branch atomically:
//!   - one `assignment.lock` sidecar per `(repo,branch)` — EVERY mutating op holds
//!     it, so all targets on a branch serialize (a same-process re-lock would
//!     deadlock on `flock`, so ops never nest — each is a top-level lock scope);
//!   - atomic whole-record write (`<key>.json.tmp` → `rename`) so the lock-free
//!     `list_active` / `get` never observe a half-written record;
//!   - CAS on `assignment_id` ([`remove_if_assignment_matches`]) — a stale op
//!     carrying an OLD `assignment_id` can NEVER mutate a record recreated with a
//!     NEW one (ABA safety — B9/I14).
//!
//! ## Delivery outbox (rows carry a `delivery_nonce`)
//! A row is a durable inbox message carrying the record's current `delivery_nonce`
//! (via [`crate::inbox::storage::enqueue`] — a flock'd append, NO self-IPC). Dedup
//! is by the CURRENT nonce: a crash between enqueue and the row→Persisted CAS
//! leaves an actionable row already carrying the nonce, so recovery detects the
//! nonce-hit and marks Persisted WITHOUT a duplicate append (I10). Repair is
//! APPEND-ONLY: it mints a NEW nonce, enqueues a fresh row, and SUPERSEDES the
//! stale row by the OLD nonce — it NEVER resets `read_at` in place (I12).

use crate::daemon::pr_state::{MergeState, PrState, ReservedAssignment, ReviewClass};
use crate::mcp::handlers::comms_gates::ReviewAuthor;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) const SCHEMA_VERSION: u32 = 1;

/// The SINGLE internal re-nudge/repair lease. FIXED — there is NO runtime config
/// (plan §2 / I12). A record's `next_nudge_at` advances by exactly this on each
/// repair, so two ticks inside one interval produce at most one repair.
const FIXED_INTERVAL: Duration = Duration::from_secs(60);

/// Outbox row lifecycle. `Pending` = the record is persisted but no durable inbox
/// row is confirmed yet (crash-before-enqueue window); `Persisted` = a row
/// carrying the current nonce is durably enqueued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum RowState {
    Pending,
    Persisted,
}

/// The terminal state of a PR generation — the kind stored in [`TerminalMarkers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum TerminalKind {
    Merged,
    Closed,
}

/// The durable authority record for one active reviewer assignment.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ActiveAssignment {
    pub schema_version: u32,
    /// Stable CAS identity — a stale op carrying an old id can never mutate a
    /// record recreated with a new id (B9/I14).
    pub assignment_id: uuid::Uuid,
    /// MANDATORY generation binding (B18/B19). The SOLE terminal-match key. Never
    /// zero (rejected at [`persist`]) and never `Option`.
    pub pr_number: u64,
    // ── authority (set at dispatch, immutable thereafter) ──
    pub sender: String,
    pub task_id: String,
    pub review_class: ReviewClass,
    pub review_author: ReviewAuthor,
    pub created_at: String,
    // ── engagement (authenticated correlated ack; a LATER slice sets these) ──
    #[serde(default)]
    pub acked_at: Option<String>,
    #[serde(default)]
    pub acked_by: Option<String>,
    // ── delivery bookkeeping ──
    pub text: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Rotates on repair (A4). Distinct from `assignment_id`.
    pub delivery_nonce: String,
    pub row: RowState,
    /// FIXED-interval repair lease (RFC3339). Repair is gated on `now >=`.
    pub next_nudge_at: String,
    // ── key components (self-describing so `list_active` reconstructs keys) ──
    pub repo: String,
    pub branch: String,
    pub target: String,
}

impl ActiveAssignment {
    /// Construct a fresh PENDING record: mint `assignment_id` + `delivery_nonce`,
    /// `row = Pending`, `acked = ∅`, and `next_nudge_at = created_at` (immediately
    /// eligible for the first nudge/repair). `created_at` is CLOCK-INJECTED so
    /// tests are deterministic (no unmockable global clock in the tested logic).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_pending(
        repo: impl Into<String>,
        branch: impl Into<String>,
        target: impl Into<String>,
        pr_number: u64,
        sender: impl Into<String>,
        task_id: impl Into<String>,
        review_class: ReviewClass,
        review_author: ReviewAuthor,
        text: impl Into<String>,
        thread_id: Option<String>,
        parent_id: Option<String>,
        created_at: &str,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            assignment_id: uuid::Uuid::new_v4(),
            pr_number,
            sender: sender.into(),
            task_id: task_id.into(),
            review_class,
            review_author,
            created_at: created_at.to_string(),
            acked_at: None,
            acked_by: None,
            text: text.into(),
            thread_id,
            parent_id,
            delivery_nonce: uuid::Uuid::new_v4().to_string(),
            row: RowState::Pending,
            next_nudge_at: created_at.to_string(),
            repo: repo.into(),
            branch: branch.into(),
            target: target.into(),
        }
    }
}

/// One retained terminal marker.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TerminalMarker {
    pub pr_number: u64,
    pub kind: TerminalKind,
}

/// The EXACT, RETAINED set of terminal pr_numbers for one `(repo,branch)`. NO
/// compaction/GC — a stale old-generation replay ALWAYS dies on reconcile
/// regardless of how many newer generations elapsed (B20/I19).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct TerminalMarkers {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub markers: Vec<TerminalMarker>,
}

impl TerminalMarkers {
    pub(crate) fn contains(&self, pr_number: u64) -> bool {
        self.markers.iter().any(|m| m.pr_number == pr_number)
    }
}

// ─────────────────────────── paths ───────────────────────────

fn base_dir(home: &Path) -> PathBuf {
    home.join("reviewer-assignments")
}

/// One directory per `(repo,branch)`. Sanitized parts stay for operator
/// readability; the [`key_hash`] suffix guarantees injectivity (a lossy sanitize
/// can otherwise collapse distinct keys to the same name — the #1969 lesson).
fn branch_dir(home: &Path, repo: &str, branch: &str) -> PathBuf {
    base_dir(home).join(format!(
        "{}--{}--{}",
        sanitize_component(repo),
        sanitize_component(branch),
        key_hash(&[repo, branch])
    ))
}

/// One record file per target within the branch dir.
fn record_file(home: &Path, repo: &str, branch: &str, target: &str) -> PathBuf {
    branch_dir(home, repo, branch).join(format!(
        "{}--{}.json",
        sanitize_component(target),
        key_hash(&[repo, branch, target])
    ))
}

/// The retained terminal-markers file for the branch. Named so it can never be a
/// record file (records always carry a `--<hash>.json` suffix), so `list_active`
/// excludes it by exact name.
fn markers_file(home: &Path, repo: &str, branch: &str) -> PathBuf {
    branch_dir(home, repo, branch).join("markers.json")
}

/// The per-`(repo,branch)` assignment lock sidecar. A `.lock` (not `.json`) so it
/// is excluded from `list_active`; a SEPARATE file from any record so the atomic
/// tmp→rename of a record can't invalidate a flock held on it.
fn branch_lock_path(home: &Path, repo: &str, branch: &str) -> PathBuf {
    branch_dir(home, repo, branch).join("assignment.lock")
}

/// Test-only path accessors: cross-module regression tests (scanner / pr_state)
/// need the internal on-disk layout to inject a corrupt record, a corrupt markers
/// file, or an un-acquirable branch lock. `#[cfg(test)]` — zero production surface.
#[cfg(test)]
pub(crate) fn record_path_for_test(home: &Path, repo: &str, branch: &str, target: &str) -> PathBuf {
    record_file(home, repo, branch, target)
}
#[cfg(test)]
pub(crate) fn branch_lock_path_for_test(home: &Path, repo: &str, branch: &str) -> PathBuf {
    branch_lock_path(home, repo, branch)
}
#[cfg(test)]
pub(crate) fn markers_path_for_test(home: &Path, repo: &str, branch: &str) -> PathBuf {
    markers_file(home, repo, branch)
}

/// Map an arbitrary key part to a filename-safe, human-readable component. Lossy
/// (collisions possible) — [`key_hash`] restores injectivity.
fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 8-hex sha256 of the NUL-joined RAW key parts — an injective per-key
/// disambiguator (NUL can't appear in a repo slug / branch / agent name). sha2 +
/// hex are already deps (mirrors [`crate::daemon::ci_handoff_track`]).
fn key_hash(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            h.update([0u8]);
        }
        h.update(p.as_bytes());
    }
    hex::encode(&h.finalize()[..4])
}

// ─────────────────────────── durable io ───────────────────────────

/// Acquire the per-`(repo,branch)` assignment lock (creating the branch dir
/// first). EVERY mutating op holds this for its whole body.
fn lock_branch(
    home: &Path,
    repo: &str,
    branch: &str,
) -> anyhow::Result<crate::store::FileFlockGuard> {
    std::fs::create_dir_all(branch_dir(home, repo, branch))?;
    crate::store::acquire_file_lock(&branch_lock_path(home, repo, branch))
}

/// Atomic whole-file write (`<name>.tmp` → `rename`). The caller holds the branch
/// lock, so the fixed `.tmp` name can't collide; the `.tmp` extension keeps a
/// crash leftover out of `list_active`.
fn atomic_write_json<T: serde::Serialize>(path: &Path, val: &T) -> std::io::Result<()> {
    let mut tmp_name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    // B4(c): NEVER write an empty/partial file — a serialization failure must
    // propagate, not silently truncate the record/markers to `[]`/`{}`.
    let bytes = serde_json::to_vec(val)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    crate::store::fsync_parent_dir(path);
    Ok(())
}

/// Read one authority record. Three-state (B4): `Ok(None)` = genuinely ABSENT
/// (NotFound), `Ok(Some)` = present + parsed, `Err` = the file EXISTS but is
/// unreadable/corrupt. A corrupt record is NEVER conflated with absent — merge-gate
/// callers (reserved derivation) MUST fail closed on `Err` (keep the reservation),
/// and destructive callers (revoke/tombstone/repair/persist-overwrite) MUST refuse
/// to act. Lossy callers that legitimately skip both absent + corrupt use
/// `read_record(..).ok().flatten()`.
fn read_record(path: &Path) -> anyhow::Result<Option<ActiveAssignment>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let r = serde_json::from_slice::<ActiveAssignment>(&bytes).map_err(|e| {
                anyhow::anyhow!("corrupt assignment record {}: {e}", path.display())
            })?;
            Ok(Some(r))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!(
            "unreadable assignment record {}: {e}",
            path.display()
        )),
    }
}

/// Read the retained terminal-markers set. Like [`read_record`], corruption is
/// SURFACED (B4): `Ok(default-empty)` only for a genuinely ABSENT file; a present
/// but unparseable file is `Err` (never silently emptied — an empty read would let
/// `record_terminal` overwrite/lose the retained set and let the A10a tombstoning
/// pass proceed as if there were zero terminals).
fn read_markers(path: &Path) -> anyhow::Result<TerminalMarkers> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<TerminalMarkers>(&bytes)
            .map_err(|e| anyhow::anyhow!("corrupt terminal markers {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TerminalMarkers::default()),
        Err(e) => Err(anyhow::anyhow!(
            "unreadable terminal markers {}: {e}",
            path.display()
        )),
    }
}

/// CAS delete: remove the record at `path` ONLY if its on-disk `assignment_id`
/// still equals `expect` — the ABA guard (B9/I14). The caller holds the branch
/// lock, so the re-read + remove are atomic w.r.t. any other op on the branch.
/// B4: a CORRUPT record cannot confirm the CAS match ⇒ do NOT remove (fail closed).
fn remove_if_assignment_matches(path: &Path, expect: uuid::Uuid) -> bool {
    match read_record(path) {
        Ok(Some(r)) if r.assignment_id == expect => std::fs::remove_file(path).is_ok(),
        _ => false,
    }
}

// ─────────────────────────── time helpers (clock-injected) ───────────────────────────

fn parse_ts(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// `now >= threshold` over RFC3339 timestamps. Fail-CLOSED: an unparseable input
/// yields `false` (do NOT repair — keeps the lease bounded).
fn now_ge(now: &str, threshold: &str) -> bool {
    match (parse_ts(now), parse_ts(threshold)) {
        (Some(n), Some(t)) => n >= t,
        _ => false,
    }
}

/// `now + FIXED_INTERVAL` as RFC3339 (the next lease). Degrades to `now` verbatim
/// if `now` is unparseable (never happens with clock-injected callers).
fn add_interval(now: &str) -> String {
    parse_ts(now)
        .and_then(|n| {
            chrono::Duration::from_std(FIXED_INTERVAL)
                .ok()
                .and_then(|d| n.checked_add_signed(d))
        })
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| now.to_string())
}

// ─────────────────────────── message builders ───────────────────────────

/// Rebuild the actionable delivery message PURELY from the stored record — no
/// bind/create, no external lookup (T11). Carries the record's current
/// `delivery_nonce` so [`crate::inbox::storage::nonce_present_actionable`] and the
/// supersede path can key on it.
fn build_delivery_message(record: &ActiveAssignment, now: &str) -> crate::inbox::InboxMessage {
    crate::inbox::InboxMessage {
        from: record.sender.clone(),
        text: record.text.clone(),
        kind: Some("review-assignment".to_string()),
        timestamp: now.to_string(),
        thread_id: record.thread_id.clone(),
        parent_id: record.parent_id.clone(),
        task_id: Some(record.task_id.clone()),
        correlation_id: Some(record.task_id.clone()),
        pr_number: Some(record.pr_number),
        delivery_nonce: Some(record.delivery_nonce.clone()),
        ..Default::default()
    }
}

/// A durable revocation notice — enqueued only when the retracted row had already
/// been READ (I21), so the reviewer learns a seen assignment was pulled.
fn build_revocation_notice(record: &ActiveAssignment, now: &str) -> crate::inbox::InboxMessage {
    crate::inbox::InboxMessage {
        from: record.sender.clone(),
        text: format!(
            "Reviewer assignment for PR #{} ({}@{}) has been revoked.",
            record.pr_number, record.repo, record.branch
        ),
        kind: Some("review-assignment-revoked".to_string()),
        timestamp: now.to_string(),
        task_id: Some(record.task_id.clone()),
        correlation_id: Some(record.task_id.clone()),
        pr_number: Some(record.pr_number),
        ..Default::default()
    }
}

// ─────────────────────────── read helpers ───────────────────────────

/// All active records for `(repo,branch)`. Lock-free (atomic writes make torn
/// reads impossible); skips the markers file, lock/tmp sidecars, and unparseable
/// records.
pub(crate) fn list_active(home: &Path, repo: &str, branch: &str) -> Vec<ActiveAssignment> {
    let dir = branch_dir(home, repo, branch);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("markers.json") {
            continue;
        }
        // Lossy: skip both absent + corrupt (this feeds ack-scan / reconcile-loop /
        // tombstone enumeration, none of which is the merge gate; the gate uses the
        // corruption-aware [`list_active_checked`]).
        if let Some(r) = read_record(&path).ok().flatten() {
            out.push(r);
        }
    }
    out
}

/// Like [`list_active`] but FAILS CLOSED on a corrupt/unreadable record: any `Err`
/// from [`read_record`] propagates instead of silently dropping the record. The
/// SOLE reader for the merge-gate-affecting reserved derivation (B4) — a corrupt
/// record must NEVER be read as "no reservation".
fn list_active_checked(home: &Path, repo: &str, branch: &str) -> anyhow::Result<Vec<ActiveAssignment>> {
    let dir = branch_dir(home, repo, branch);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("markers.json") {
            continue;
        }
        // `Err` (corrupt) propagates ⇒ caller keeps the existing reserved set.
        // `Ok(None)` (a concurrent remove raced us) is a genuine absence — skip it.
        if let Some(r) = read_record(&path)? {
            out.push(r);
        }
    }
    Ok(out)
}

/// The record for one `(repo,branch,target)`, if any.
// t-…-17: a public single-record accessor. Production reconcile/drain/dispatch read
// the whole branch via `list_active`; this point-lookup is exercised by the store
// unit tests and reserved for a future single-assignment query — no production caller
// yet (slice-4 wires list/persist/enqueue/repair/terminal/revoke, not point-get).
#[allow(dead_code)]
pub(crate) fn get(home: &Path, repo: &str, branch: &str, target: &str) -> Option<ActiveAssignment> {
    read_record(&record_file(home, repo, branch, target))
        .ok()
        .flatten()
}

/// The retained terminal-marker set for `(repo,branch)`. Inspection-only accessor:
/// a corrupt/absent file yields an empty set. Production terminal paths
/// ([`record_terminal`] / [`tombstone_terminal_matches`]) read markers through the
/// corruption-aware [`read_markers`] instead so they can fail closed on corruption.
pub(crate) fn terminal_markers(home: &Path, repo: &str, branch: &str) -> TerminalMarkers {
    read_markers(&markers_file(home, repo, branch)).unwrap_or_default()
}

// ─────────────────────────── operations ───────────────────────────

/// A1 — persist a fresh record (`row = Pending`). Writes ONLY the record (never a
/// `PrState` or an inbox row — I9). Rejects `pr_number == 0` as a caller error
/// (defense-in-depth; the dispatch gate already enforces nonzero).
pub(crate) fn persist(home: &Path, record: &ActiveAssignment) -> anyhow::Result<()> {
    if record.pr_number == 0 {
        anyhow::bail!("assignment_authority::persist: pr_number must be nonzero (generation-bound at dispatch)");
    }
    let _lock = lock_branch(home, &record.repo, &record.branch)?;
    let path = record_file(home, &record.repo, &record.branch, &record.target);
    // B1 — atomic revoke-and-replace under the one branch lock: if a record ALREADY
    // exists at this key with a DIFFERENT assignment_id, RETIRE its outbox row BEFORE
    // overwriting it, so the old actionable delivery_nonce is not orphaned. A corrupt
    // existing record FAILS CLOSED (bail) — never blindly overwrite/lose it (B4). A
    // same-id re-persist is idempotent (no self-supersede).
    if let Some(old) = read_record(&path)? {
        if old.assignment_id != record.assignment_id {
            let successor = format!("superseded-{}", record.assignment_id);
            let outcome = crate::inbox::storage::supersede_by_nonce(
                home,
                &record.target,
                &old.delivery_nonce,
                &successor,
            );
            // Mirror the A8 revoke path: a row the reviewer had already READ is not
            // retracted by the supersede alone — surface a durable revocation notice
            // for the retired assignment (I21). Clock-injected via the new record's
            // `created_at` (persist takes no separate `now`).
            if outcome.was_read {
                crate::inbox::storage::enqueue(
                    home,
                    &record.target,
                    build_revocation_notice(&old, &record.created_at),
                )?;
            }
        }
    }
    atomic_write_json(&path, record)?;
    Ok(())
}

/// A2 — durable enqueue. If the record's CURRENT nonce is already actionable in
/// the target's inbox (crash between enqueue and mark), just CAS `row → Persisted`
/// (no duplicate). Otherwise enqueue a fresh actionable row carrying the nonce,
/// then CAS `row → Persisted` keyed on `assignment_id`. Duplicate rows are
/// forbidden; the nonce-hit makes recovery idempotent (I10).
pub(crate) fn durable_enqueue(
    home: &Path,
    repo: &str,
    branch: &str,
    target: &str,
    now: &str,
) -> anyhow::Result<()> {
    let _lock = lock_branch(home, repo, branch)?;
    let path = record_file(home, repo, branch, target);
    let record = match read_record(&path) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()), // nothing to enqueue (revoked/tombstoned)
        Err(e) => {
            // B4: a corrupt record is NOT absent — surface + skip the enqueue rather
            // than silently treating it as gone. The reconciler retries next tick.
            tracing::error!(repo, branch, target, error = %e,
                "t-…-17 B4: durable_enqueue skipped — corrupt authority record (fail closed)");
            return Ok(());
        }
    };
    if record.row == RowState::Persisted {
        return Ok(()); // idempotent
    }
    // Dedup by the CURRENT nonce: an actionable row already carrying it means the
    // enqueue landed but the mark crashed — do NOT append a duplicate (I10).
    if !crate::inbox::storage::nonce_present_actionable(home, target, &record.delivery_nonce) {
        crate::inbox::storage::enqueue(home, target, build_delivery_message(&record, now))?;
    }
    // CAS row → Persisted on assignment_id (re-read under the same lock). A corrupt
    // re-read ⇒ skip the mark (fail closed; retried next tick).
    if let Ok(Some(mut cur)) = read_record(&path) {
        if cur.assignment_id == record.assignment_id {
            cur.row = RowState::Persisted;
            atomic_write_json(&path, &cur)?;
        }
    }
    Ok(())
}

/// A4 — APPEND-ONLY row repair, gated on `now >= next_nudge_at` (FIXED-interval
/// lease). Mints a NEW nonce, enqueues a fresh actionable row, SUPERSEDES the
/// stale row by the OLD nonce, and advances `delivery_nonce` + `next_nudge_at`
/// (CAS on `assignment_id`). NEVER resets `read_at`. Returns whether a repair
/// fired.
pub(crate) fn repair_row(
    home: &Path,
    repo: &str,
    branch: &str,
    target: &str,
    now: &str,
) -> anyhow::Result<bool> {
    let _lock = lock_branch(home, repo, branch)?;
    let path = record_file(home, repo, branch, target);
    let record = match read_record(&path) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(false),
        Err(e) => {
            // B4: corrupt record ⇒ surface + skip the destructive rotate/supersede.
            tracing::error!(repo, branch, target, error = %e,
                "t-…-17 B4: repair_row skipped — corrupt authority record (fail closed)");
            return Ok(false);
        }
    };
    // FIXED-interval lease: repair only when the lease is due (bounded).
    if !now_ge(now, &record.next_nudge_at) {
        return Ok(false);
    }
    // B2 (A4 = Unengaged ∧ NON-ACTIONABLE ∧ lease-due): a still-actionable (unread,
    // un-superseded) current row is HEALTHY — the reviewer simply hasn't read it yet.
    // Do NOT rotate/supersede a healthy delivery (that would be a spurious re-nudge
    // and would orphan an unread row); just advance the lease under the CAS so the
    // next check is a full interval away. ONLY a NON-actionable current row
    // (read-and-ignored, or lost) proceeds to the append-only repair below.
    if crate::inbox::storage::nonce_present_actionable(home, target, &record.delivery_nonce) {
        if let Ok(Some(mut cur)) = read_record(&path) {
            if cur.assignment_id == record.assignment_id {
                cur.next_nudge_at = add_interval(now);
                atomic_write_json(&path, &cur)?;
            }
        }
        return Ok(false);
    }
    let old_nonce = record.delivery_nonce.clone();
    let new_nonce = uuid::Uuid::new_v4().to_string();

    // 1) Enqueue a FRESH actionable row carrying the new nonce (append-only). The
    //    successor id is pre-stamped so it can name the supersede relationship.
    let mut fresh = record.clone();
    fresh.delivery_nonce = new_nonce.clone();
    let mut msg = build_delivery_message(&fresh, now);
    let successor = format!("m-asgn-{new_nonce}");
    msg.id = Some(successor.clone());
    crate::inbox::storage::enqueue(home, target, msg)?;

    // 2) SUPERSEDE the stale row by the OLD nonce (never resets read_at).
    crate::inbox::storage::supersede_by_nonce(home, target, &old_nonce, &successor);

    // 3) Advance nonce + lease under the assignment_id CAS (corrupt re-read ⇒ skip).
    if let Ok(Some(mut cur)) = read_record(&path) {
        if cur.assignment_id == record.assignment_id {
            cur.delivery_nonce = new_nonce;
            cur.next_nudge_at = add_interval(now);
            cur.row = RowState::Persisted;
            atomic_write_json(&path, &cur)?;
        }
    }
    Ok(true)
}

/// A8 — revoke. CAS-remove the record by `assignment_id`, supersede the current
/// inbox row by its nonce, and — if that row had already been READ — enqueue a
/// durable revocation notice (I21). Other targets on the branch are untouched.
/// Returns whether a record was removed.
pub(crate) fn revoke(
    home: &Path,
    repo: &str,
    branch: &str,
    target: &str,
    now: &str,
) -> anyhow::Result<bool> {
    let _lock = lock_branch(home, repo, branch)?;
    revoke_under_lock(home, repo, branch, target, now)
}

/// The revoke body WITHOUT acquiring the branch lock — the caller MUST already
/// hold it. Shared by [`revoke`] (which locks) and [`transfer`] (which revokes
/// the old target and persists the new one under ONE lock so a same-process
/// re-lock cannot deadlock on `flock`).
fn revoke_under_lock(
    home: &Path,
    repo: &str,
    branch: &str,
    target: &str,
    now: &str,
) -> anyhow::Result<bool> {
    let path = record_file(home, repo, branch, target);
    let record = match read_record(&path) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(false),
        Err(e) => {
            // B4: corrupt record ⇒ surface + do NOT act (cannot derive the nonce to
            // supersede, and remove_if_assignment_matches would refuse anyway).
            tracing::error!(repo, branch, target, error = %e,
                "t-…-17 B4: revoke skipped — corrupt authority record (fail closed)");
            return Ok(false);
        }
    };
    let removed = remove_if_assignment_matches(&path, record.assignment_id);
    let successor = format!("revoked-{}", record.assignment_id);
    let outcome =
        crate::inbox::storage::supersede_by_nonce(home, target, &record.delivery_nonce, &successor);
    // A row the reviewer had already READ is not retracted by the supersede alone
    // (it is already non-actionable) — surface an explicit revocation notice (I21).
    if outcome.was_read {
        crate::inbox::storage::enqueue(home, target, build_revocation_notice(&record, now))?;
    }
    Ok(removed)
}

/// TEARDOWN hygiene — REVOKE every active reviewer assignment whose TARGET is the
/// deleted instance `name`, across all branches. LOAD-BEARING once the store is live:
/// a deleted reviewer that still held an active record would keep the per-tick
/// reconciler deriving a `reserved_assignments` entry for a GHOST, holding
/// [`crate::daemon::pr_state::is_merge_ready`] CLOSED forever — the same
/// per-instance-residual class the ci_watch / pr_state subscriber cleanups close at
/// teardown. Best-effort; returns the number revoked. `now` is clock-injected. Each
/// [`revoke`] takes the branch lock top-level (never nested — the `list_active` read
/// is lock-free), so there is no re-lock deadlock.
pub(crate) fn revoke_all_for_target(home: &Path, name: &str, now: &str) -> usize {
    let mut revoked = 0;
    for (repo, branch) in active_branches(home) {
        for record in list_active(home, &repo, &branch) {
            if record.target == name && revoke(home, &repo, &branch, name, now).unwrap_or(false) {
                revoked += 1;
            }
        }
    }
    revoked
}

/// A9 — transfer old→new atomically under ONE branch lock: {revoke old} + {persist
/// a fresh record for new with a NEW `assignment_id` + nonce, SAME `pr_number`,
/// authority carried over}. Other targets untouched.
// t-…-17: reviewer REASSIGNMENT primitive (A9). Slice-4 wires dispatch (A1), terminal
// (A7), reconcile (A2/A3/A4/A10) and the teardown REVOKE (A8) — but an operator
// reviewer-REASSIGNMENT command (the sole caller of A9) is a distinct future feature,
// so this has no production caller in this slice. Tested by `t33_transfer_atomic_*`.
#[allow(dead_code)]
pub(crate) fn transfer(
    home: &Path,
    repo: &str,
    branch: &str,
    old_target: &str,
    new_target: &str,
    now: &str,
) -> anyhow::Result<()> {
    let _lock = lock_branch(home, repo, branch)?;
    // B4: `?` propagates a corrupt-record Err (fail closed — never transfer off a
    // record we cannot read); `ok_or_else` handles a genuinely absent old target.
    let old = read_record(&record_file(home, repo, branch, old_target))?.ok_or_else(|| {
        anyhow::anyhow!("assignment_authority::transfer: no active assignment for old target")
    })?;
    // {revoke old} under the held lock.
    revoke_under_lock(home, repo, branch, old_target, now)?;
    // {persist new} — fresh assignment_id + nonce, SAME pr_number, authority
    // carried over. Written directly (persist would re-lock the same branch).
    let new_record = ActiveAssignment::new_pending(
        repo,
        branch,
        new_target,
        old.pr_number,
        old.sender,
        old.task_id,
        old.review_class,
        old.review_author,
        old.text,
        old.thread_id,
        old.parent_id,
        now,
    );
    atomic_write_json(&record_file(home, repo, branch, new_target), &new_record)?;
    Ok(())
}

/// A7 (store half) — record a terminal PR generation: add `pr_number` to the
/// RETAINED [`TerminalMarkers`] set (NO compaction — B20/I19) and CAS-tombstone
/// ONLY records whose stored `pr_number == pr_number` (NEVER a different
/// generation — B18/B19/I18). Returns the number of records tombstoned. (The
/// `record_ci_result` call-site wiring is a LATER slice.)
pub(crate) fn record_terminal(
    home: &Path,
    repo: &str,
    branch: &str,
    pr_number: u64,
    kind: TerminalKind,
) -> anyhow::Result<usize> {
    let _lock = lock_branch(home, repo, branch)?;
    // 1) Add the marker to the RETAINED set (idempotent; NO compaction — I19). B4:
    //    `?` propagates a corrupt-markers Err — a silent default-empty read here
    //    would OVERWRITE the retained set with just this marker on the write below,
    //    losing every prior terminal generation. Fail closed instead.
    let mpath = markers_file(home, repo, branch);
    let mut markers = read_markers(&mpath)?;
    if !markers.contains(pr_number) {
        markers.schema_version = SCHEMA_VERSION;
        markers.markers.push(TerminalMarker { pr_number, kind });
        atomic_write_json(&mpath, &markers)?;
    }
    // 2) CAS-tombstone ONLY records whose stored pr_number matches — NEVER a
    //    different generation (B18/B19/I18).
    let mut tombstoned = 0;
    for record in list_active(home, repo, branch) {
        if record.pr_number == pr_number {
            let path = record_file(home, repo, branch, &record.target);
            if remove_if_assignment_matches(&path, record.assignment_id) {
                tombstoned += 1;
            }
        }
    }
    Ok(tombstoned)
}

// ─────────────────────────── C10/C12: live-wiring helpers ───────────────────────────

/// Map a `PrState`'s terminal `merge_state` to its [`TerminalKind`], or `None` when
/// the PR is not (yet) terminal. Used by the scanner's terminal wire (A7) to record
/// the retained marker at the moment a generation goes terminal.
pub(crate) fn terminal_kind_of(state: &PrState) -> Option<TerminalKind> {
    match state.merge_state {
        MergeState::Merged { .. } => Some(TerminalKind::Merged),
        MergeState::ClosedUnmerged { .. } => Some(TerminalKind::Closed),
        MergeState::NotReady | MergeState::MergeReady => None,
    }
}

/// A10a store-half — TERMINAL RESTART-REPAIR. CAS-tombstone every active record of
/// `(repo,branch)` whose stored `pr_number` is already in the RETAINED
/// [`TerminalMarkers`] set (the A7 crash-gap / old-generation-replay backstop —
/// I18/I19). Takes the branch lock internally; NEVER re-locks (call it top-level,
/// never while already holding the branch lock). Returns the number tombstoned.
pub(crate) fn tombstone_terminal_matches(home: &Path, repo: &str, branch: &str) -> usize {
    let Ok(_lock) = lock_branch(home, repo, branch) else {
        return 0;
    };
    // B4: a corrupt markers file is NOT "zero terminals" — SKIP the pass (surface +
    // return) rather than act on an unreliable empty set. Acting as if there are no
    // terminals would fail to tombstone a replayed old-generation record (B20).
    let markers = match read_markers(&markers_file(home, repo, branch)) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(repo, branch, error = %e,
                "t-…-17 B4: A10a tombstone pass skipped — corrupt terminal markers (fail closed)");
            return 0;
        }
    };
    if markers.markers.is_empty() {
        return 0;
    }
    let mut tombstoned = 0;
    for record in list_active(home, repo, branch) {
        if markers.contains(record.pr_number) {
            let path = record_file(home, repo, branch, &record.target);
            if remove_if_assignment_matches(&path, record.assignment_id) {
                tombstoned += 1;
            }
        }
    }
    tombstoned
}

/// A6/A10b — DERIVE the RESERVED set for `prstate` from the active assignment
/// records of its `(repo,branch)`: every active record whose stored `pr_number`
/// equals `prstate.pr_number` AND whose evidence is NOT `SatisfiedExactHead`
/// (plan §1/I16). FULL-TYPED (`target`/`review_author`/`assignment_id`), generation-
/// matched, Satisfied-excluded. Reads records LOCK-FREE ([`list_active_checked`]), so
/// it is safe to call while the caller already holds the branch lock (the A6 drain
/// does). B4: returns `Err` if ANY record on the branch is corrupt/unreadable — the
/// merge-gate caller MUST then KEEP the existing reserved set (a corrupt record must
/// NEVER silently drop a reservation and OPEN the reserved gate).
pub(crate) fn derive_reserved_for_prstate(
    home: &Path,
    repo: &str,
    branch: &str,
    prstate: &PrState,
) -> anyhow::Result<Vec<ReservedAssignment>> {
    Ok(list_active_checked(home, repo, branch)?
        .into_iter()
        .filter(|r| r.pr_number == prstate.pr_number)
        .filter(|r| classify_assignment(r, Some(prstate)) != AssignmentEvidence::SatisfiedExactHead)
        .map(|r| ReservedAssignment {
            target: r.target,
            review_author: r.review_author,
            assignment_id: r.assignment_id,
        })
        .collect())
}

/// A10b — re-derive `reserved_assignments` on the LIVE `PrState` for `(repo,branch)`
/// (declarative, convergent). Lock order is the mandated assignment-lock OUTER →
/// pr_state-flock INNER (I11/I15): the branch lock is acquired FIRST, then
/// [`crate::daemon::pr_state::with_pr_state`] takes the pr_state flock. A missing
/// pr_state file is a no-op. Best-effort (a lock/save failure is swallowed — the
/// next tick re-derives). B4: a corrupt-record derivation failure KEEPS the existing
/// reserved set (fail closed) — the reservation is never dropped on corruption.
pub(crate) fn redrive_reserved(home: &Path, repo: &str, branch: &str) {
    let Ok(_lock) = lock_branch(home, repo, branch) else {
        return;
    };
    let _ = crate::daemon::pr_state::with_pr_state(home, repo, branch, |ps| {
        match derive_reserved_for_prstate(home, repo, branch, ps) {
            Ok(v) => ps.reserved_assignments = v,
            Err(e) => tracing::error!(repo, branch, error = %e,
                "t-…-17 B4: reserved re-derive skipped — corrupt authority record; keeping existing reserved set (fail closed, gate stays closed)"),
        }
    });
}

/// Acquire the per-`(repo,branch)` assignment lock as the OUTER lock of the A6 drain
/// in `record_ci_result` (which then takes the pr_state flock INNER). Returns the
/// guard so the caller holds it across `with_pr_state_or_create`; `None` on failure
/// (the drain proceeds best-effort — the reconciler re-derives). NEVER call while
/// already holding this branch's lock (same-process re-lock deadlocks on `flock`).
pub(crate) fn lock_branch_for_drain(
    home: &Path,
    repo: &str,
    branch: &str,
) -> Option<crate::store::FileFlockGuard> {
    lock_branch(home, repo, branch).ok()
}

/// True iff `(repo,branch)` has at least one active record. Lock-free (mirrors
/// [`list_active`]). Lets the A6 drain skip acquiring the branch lock — and creating
/// an empty branch dir — for a branch with no reviewer assignments.
pub(crate) fn has_active(home: &Path, repo: &str, branch: &str) -> bool {
    !list_active(home, repo, branch).is_empty()
}

/// Enumerate every `(repo,branch)` with at least one active record — the reconciler's
/// bounded work set. Derived from the self-describing record files (the branch-dir
/// NAME is lossy/hashed and cannot be reversed); a markers-only dir (all records
/// tombstoned) yields nothing, which is correct (it needs no reconciler action).
/// Lock-free.
pub(crate) fn active_branches(home: &Path) -> Vec<(String, String)> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for branch_entry in std::fs::read_dir(base_dir(home))
        .into_iter()
        .flatten()
        .flatten()
    {
        let dir = branch_entry.path();
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some("markers.json") {
                continue;
            }
            if let Some(r) = read_record(&path).ok().flatten() {
                if seen.insert((r.repo.clone(), r.branch.clone())) {
                    out.push((r.repo, r.branch));
                }
                break; // one record identifies the branch
            }
        }
    }
    out
}

// ─────────────────────────── C7: 4-state evidence classifier ───────────────────────────

/// The FROZEN 4-state evidence classification for one active assignment (plan §1).
/// DERIVED, never stored. `SatisfiedExactHead` ⇒ NOT reserved / no nudge; the other
/// three ⇒ reserved; only `Unengaged` is eligible for nudge/repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssignmentEvidence {
    /// `record.target` is VERIFIED at the CURRENT head.
    SatisfiedExactHead,
    /// `record.target` REJECTED / UNVERIFIED at the CURRENT head.
    EngagedUnsatisfied,
    /// Authenticated correlated ACK received, but no current-head verdict yet.
    EngagedPending,
    /// None of the above (incl. no `PrState`/head yet, and not acked).
    Unengaged,
}

/// C7 — classify one assignment's evidence (plan §1, EXACT). PURE: reads ONLY the
/// record's `target`/`acked_at` and the passed-in `PrState`'s current-head
/// `VerdictState` (the `prstate` is the one for `record.pr_number`; its `head_sha`
/// is the current head). NEVER reads `read_at`/`delivering_at`/report text. A
/// DIFFERENT reviewer's verdict NEVER satisfies/engages THIS record.
pub(crate) fn classify_assignment(
    record: &ActiveAssignment,
    prstate: Option<&crate::daemon::pr_state::PrState>,
) -> AssignmentEvidence {
    use crate::daemon::pr_state::VerdictState;
    // A verdict counts ONLY when it names THIS record's target AND was rendered at
    // the CURRENT head (`prstate.head_sha`). `record_verdict` already drops stale
    // Verified entries on head-advance, but the head match is asserted defensively
    // here so a stale entry can never satisfy/engage (plan §1).
    if let Some(ps) = prstate {
        match &ps.verdict_state {
            VerdictState::Verified { reviewers } => {
                if reviewers
                    .iter()
                    .any(|(name, head)| name == &record.target && head == &ps.head_sha)
                {
                    return AssignmentEvidence::SatisfiedExactHead;
                }
            }
            VerdictState::Rejected {
                reviewer,
                reviewed_head,
                ..
            }
            | VerdictState::Unverified {
                reviewer,
                reviewed_head,
            } => {
                if reviewer == &record.target && reviewed_head == &ps.head_sha {
                    return AssignmentEvidence::EngagedUnsatisfied;
                }
            }
            VerdictState::None | VerdictState::Pending => {}
        }
    }
    // No current-head verdict for the target. An authenticated correlated ACK means
    // the reviewer engaged but hasn't rendered a verdict yet ⇒ EngagedPending
    // (TRUE stop — no nudge). Otherwise ⇒ Unengaged (the only nudge-eligible state).
    if record.acked_at.is_some() {
        return AssignmentEvidence::EngagedPending;
    }
    AssignmentEvidence::Unengaged
}

// ─────────────────────────── C9: authenticated correlated ACK ───────────────────────────

/// The outcome of an authenticated correlated ACK ([`ack`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AckOutcome {
    /// EXACTLY ONE active assignment matched `(target,task_id)` — its `acked_at` is
    /// now set (or was already set: re-ack is an idempotent no-op success).
    Acked,
    /// >1 active assignment matched — FAIL CLOSED, NOTHING set (I20).
    Ambiguous,
    /// No active assignment matched — no-op.
    NoMatch,
}

/// A5 / C9 — authenticated correlated ACK. Resolves the ACTIVE assignment(s) whose
/// `target == sender_target` AND `task_id == task_id` STORE-WIDE, then:
///   - EXACTLY ONE ⇒ set `acked_at`/`acked_by` under THAT record's per-branch
///     assignment-lock, CAS on `assignment_id`; re-ack is an idempotent success;
///   - >1 ⇒ FAIL CLOSED: set NOTHING, return [`AckOutcome::Ambiguous`] (I20);
///   - 0 ⇒ [`AckOutcome::NoMatch`] (no-op).
///
/// Takes ONLY the assignment-lock — NEVER the inbox or pr_state lock (no inversion).
pub(crate) fn ack(home: &Path, sender_target: &str, task_id: &str, now: &str) -> AckOutcome {
    let mut matches = list_active_by_target_task(home, sender_target, task_id);
    // >1 ⇒ FAIL CLOSED: touch NOTHING (no lock taken) — I20.
    if matches.len() > 1 {
        return AckOutcome::Ambiguous;
    }
    let Some(rec) = matches.pop() else {
        return AckOutcome::NoMatch;
    };
    // Set acked under THAT record's per-branch assignment-lock ONLY. Re-read + CAS
    // on `assignment_id` so a concurrent revoke/transfer between the lock-free scan
    // and here can never resurrect a stale record (ABA — B9/I14).
    let Ok(_lock) = lock_branch(home, &rec.repo, &rec.branch) else {
        return AckOutcome::NoMatch;
    };
    let path = record_file(home, &rec.repo, &rec.branch, &rec.target);
    // B4: absent OR corrupt ⇒ NoMatch (never ack a record we cannot read).
    let Ok(Some(mut cur)) = read_record(&path) else {
        return AckOutcome::NoMatch;
    };
    if cur.assignment_id != rec.assignment_id {
        return AckOutcome::NoMatch;
    }
    // Idempotent: an already-acked record is a no-op SUCCESS (never overwrites the
    // original timestamp) — a single atomic write only on the first ack.
    if cur.acked_at.is_none() {
        cur.acked_at = Some(now.to_string());
        cur.acked_by = Some(sender_target.to_string());
        if atomic_write_json(&path, &cur).is_err() {
            return AckOutcome::NoMatch;
        }
    }
    AckOutcome::Acked
}

/// Store-wide scan for [`ack`]: every ACTIVE record across ALL `(repo,branch)` whose
/// `target`/`task_id` match. Lock-free — atomic whole-record writes make torn reads
/// impossible. Cheap on the common path: an empty store means the base dir does not
/// exist, so `read_dir` yields nothing and no record files are opened.
fn list_active_by_target_task(home: &Path, target: &str, task_id: &str) -> Vec<ActiveAssignment> {
    let mut out = Vec::new();
    for branch_entry in std::fs::read_dir(base_dir(home))
        .into_iter()
        .flatten()
        .flatten()
    {
        let dir = branch_entry.path();
        if !dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some("markers.json") {
                continue;
            }
            if let Some(r) = read_record(&path).ok().flatten() {
                if r.target == target && r.task_id == task_id {
                    out.push(r);
                }
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("agend-asgn-{}-{}-{}", std::process::id(), tag, id));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn mk_record(
        repo: &str,
        branch: &str,
        target: &str,
        pr: u64,
        created_at: &str,
    ) -> ActiveAssignment {
        ActiveAssignment::new_pending(
            repo,
            branch,
            target,
            pr,
            "lead",
            "t-orig-1",
            ReviewClass::Dual,
            ReviewAuthor::External("octocat".into()),
            "Please review PR",
            Some("thr-1".into()),
            Some("par-1".into()),
            created_at,
        )
    }

    /// Read all inbox rows for `target` (any state), for row-level assertions.
    fn inbox_rows(home: &Path, target: &str) -> Vec<crate::inbox::InboxMessage> {
        let path = crate::inbox::storage::inbox_path_resolved(home, target);
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<crate::inbox::InboxMessage>(l).ok())
            .collect()
    }

    fn rows_with_nonce(home: &Path, target: &str, nonce: &str) -> Vec<crate::inbox::InboxMessage> {
        inbox_rows(home, target)
            .into_iter()
            .filter(|m| m.delivery_nonce.as_deref() == Some(nonce))
            .collect()
    }

    /// Simulate the reviewer having READ the row carrying `nonce` (set `read_at`).
    fn mark_row_read(home: &Path, target: &str, nonce: &str, ts: &str) {
        let path = crate::inbox::storage::inbox_path_resolved(home, target);
        let content = std::fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut msg: crate::inbox::InboxMessage = serde_json::from_str(line).unwrap();
            if msg.delivery_nonce.as_deref() == Some(nonce) {
                msg.read_at = Some(ts.to_string());
            }
            out.push_str(&serde_json::to_string(&msg).unwrap());
            out.push('\n');
        }
        std::fs::write(&path, out).unwrap();
    }

    /// T11: a full-payload record round-trips through persist→get, and
    /// durable_enqueue REBUILDS the delivery message purely from the stored record
    /// (no bind/create) — every payload field is carried onto the inbox row.
    #[test]
    fn t11_record_roundtrip_and_message_rebuild() {
        let home = tmp_home("t11");
        let rec = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();

        let got = get(&home, "o/r", "feat/x", "reviewer").expect("record persisted");
        assert_eq!(got.assignment_id, rec.assignment_id);
        assert_eq!(got.pr_number, 42);
        assert_eq!(got.sender, "lead");
        assert_eq!(got.task_id, "t-orig-1");
        assert_eq!(got.review_class, ReviewClass::Dual);
        assert_eq!(got.review_author, ReviewAuthor::External("octocat".into()));
        assert_eq!(got.text, "Please review PR");
        assert_eq!(got.thread_id.as_deref(), Some("thr-1"));
        assert_eq!(got.parent_id.as_deref(), Some("par-1"));
        assert_eq!(got.delivery_nonce, rec.delivery_nonce);
        assert_eq!(got.row, RowState::Pending);

        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        let rows = rows_with_nonce(&home, "reviewer", &rec.delivery_nonce);
        assert_eq!(rows.len(), 1, "exactly one row carries the nonce");
        let row = &rows[0];
        // Rebuilt PURELY from the record's fields.
        assert_eq!(row.text, "Please review PR");
        assert_eq!(row.pr_number, Some(42));
        assert_eq!(row.thread_id.as_deref(), Some("thr-1"));
        assert_eq!(row.parent_id.as_deref(), Some("par-1"));
        assert_eq!(row.task_id.as_deref(), Some("t-orig-1"));
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &rec.delivery_nonce),
            "the rebuilt row is actionable"
        );
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer").unwrap().row,
            RowState::Persisted
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A1 defense-in-depth: persisting a zero `pr_number` is a caller error and
    /// writes NOTHING (no unbound generation can exist — B18/B19).
    #[test]
    fn persist_rejects_zero_pr_number() {
        let home = tmp_home("zero-pr");
        let rec = mk_record("o/r", "feat/x", "reviewer", 0, "2026-07-13T00:00:00Z");
        assert!(persist(&home, &rec).is_err(), "zero pr_number rejected");
        assert!(
            get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "no record written on rejection"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T12: crash BEFORE enqueue — `row = Pending`, nonce NOT in inbox. Recovery
    /// enqueues a row carrying the SAME nonce and marks Persisted.
    #[test]
    fn t12_crash_before_enqueue_reenqueues_same_nonce() {
        let home = tmp_home("t12");
        let rec = mk_record("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();
        assert!(
            !crate::inbox::storage::nonce_present_actionable(
                &home,
                "reviewer",
                &rec.delivery_nonce
            ),
            "no row before enqueue (crash window)"
        );

        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        assert_eq!(
            rows_with_nonce(&home, "reviewer", &rec.delivery_nonce).len(),
            1,
            "recovery enqueued exactly one row with the same nonce"
        );
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer").unwrap().row,
            RowState::Persisted
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T13: enqueue succeeded but the row→Persisted mark crashed — the nonce IS
    /// actionable in the inbox, `row` still `Pending`. Recovery detects the
    /// nonce-hit and marks Persisted with NO duplicate row.
    #[test]
    fn t13_enqueue_ok_mark_fail_nonce_hit_no_duplicate() {
        let home = tmp_home("t13");
        let rec = mk_record("o/r", "feat/x", "reviewer", 9, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();
        // Simulate "enqueue succeeded, mark crashed": a row already carries the nonce.
        crate::inbox::storage::enqueue(
            &home,
            "reviewer",
            build_delivery_message(&rec, "2026-07-13T00:00:01Z"),
        )
        .unwrap();
        assert_eq!(
            rows_with_nonce(&home, "reviewer", &rec.delivery_nonce).len(),
            1
        );

        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        assert_eq!(
            rows_with_nonce(&home, "reviewer", &rec.delivery_nonce).len(),
            1,
            "nonce-hit ⇒ NO duplicate row appended"
        );
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer").unwrap().row,
            RowState::Persisted,
            "row marked Persisted on the nonce-hit"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T26 (B9 ABA): `assignment_id` is the CAS identity. A record revoked (id X)
    /// then recreated at the SAME key with a NEW id (Y) must reject a stale op
    /// carrying X — it does NOT mutate Y.
    #[test]
    fn t26_aba_assignment_id_is_cas_identity() {
        let home = tmp_home("t26");
        let r1 = mk_record("o/r", "feat/x", "reviewer", 5, "2026-07-13T00:00:00Z");
        persist(&home, &r1).unwrap();
        let x = get(&home, "o/r", "feat/x", "reviewer")
            .unwrap()
            .assignment_id;

        // Revoke (removes id X), then recreate the SAME key with a fresh id Y.
        revoke(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:10Z").unwrap();
        let r2 = mk_record("o/r", "feat/x", "reviewer", 5, "2026-07-13T00:01:00Z");
        persist(&home, &r2).unwrap();
        let y = get(&home, "o/r", "feat/x", "reviewer")
            .unwrap()
            .assignment_id;
        assert_ne!(x, y, "recreated record has a fresh assignment_id");

        // A STALE op carrying X must be rejected (does not touch Y).
        let path = record_file(&home, "o/r", "feat/x", "reviewer");
        assert!(
            !remove_if_assignment_matches(&path, x),
            "stale assignment_id X must NOT mutate the recreated record"
        );
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer")
                .unwrap()
                .assignment_id,
            y,
            "record with id Y survives the stale X op"
        );
        // The current id Y still CASes successfully.
        assert!(remove_if_assignment_matches(&path, y));
        assert!(get(&home, "o/r", "feat/x", "reviewer").is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    /// T32 (B7/I21): revoke supersedes the current inbox row by its nonce
    /// (is_actionable ⇒ false). Case A (unread): no notice. Case B (already read):
    /// a durable revocation notice is enqueued.
    #[test]
    fn t32_revoke_supersedes_row_and_notice_only_if_read() {
        // Case A: unread row → superseded, NO revocation notice.
        let home = tmp_home("t32a");
        let rec = mk_record("o/r", "feat/x", "reviewer", 11, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        assert!(crate::inbox::storage::nonce_present_actionable(
            &home,
            "reviewer",
            &rec.delivery_nonce
        ));
        assert!(revoke(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:10Z").unwrap());
        assert!(
            get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "record removed"
        );
        assert!(
            !crate::inbox::storage::nonce_present_actionable(
                &home,
                "reviewer",
                &rec.delivery_nonce
            ),
            "row superseded ⇒ not actionable"
        );
        assert_eq!(
            inbox_rows(&home, "reviewer")
                .iter()
                .filter(|m| m.kind.as_deref() == Some("review-assignment-revoked"))
                .count(),
            0,
            "an unread revoke needs no revocation notice"
        );
        std::fs::remove_dir_all(&home).ok();

        // Case B: row already READ → superseded AND a revocation notice enqueued.
        let home = tmp_home("t32b");
        let rec = mk_record("o/r", "feat/x", "reviewer", 11, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        mark_row_read(
            &home,
            "reviewer",
            &rec.delivery_nonce,
            "2026-07-13T00:00:07Z",
        );
        assert!(revoke(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:10Z").unwrap());
        assert_eq!(
            inbox_rows(&home, "reviewer")
                .iter()
                .filter(|m| m.kind.as_deref() == Some("review-assignment-revoked"))
                .count(),
            1,
            "a revoke of an already-read assignment enqueues a revocation notice"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T33: transfer old→new is atomic — old record removed + its row superseded,
    /// the new record has a FRESH assignment_id + nonce (same pr_number), and a
    /// co-target on the same branch is UNTOUCHED.
    #[test]
    fn t33_transfer_atomic_other_targets_untouched() {
        let home = tmp_home("t33");
        let a = mk_record("o/r", "feat/x", "rev-a", 21, "2026-07-13T00:00:00Z");
        let c = mk_record("o/r", "feat/x", "rev-c", 21, "2026-07-13T00:00:00Z");
        persist(&home, &a).unwrap();
        persist(&home, &c).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "rev-a", "2026-07-13T00:00:05Z").unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "rev-c", "2026-07-13T00:00:05Z").unwrap();

        transfer(
            &home,
            "o/r",
            "feat/x",
            "rev-a",
            "rev-b",
            "2026-07-13T00:00:10Z",
        )
        .unwrap();

        // Old gone + its row superseded.
        assert!(
            get(&home, "o/r", "feat/x", "rev-a").is_none(),
            "old removed"
        );
        assert!(
            !crate::inbox::storage::nonce_present_actionable(&home, "rev-a", &a.delivery_nonce),
            "old row superseded"
        );
        // New present, fresh identity, same pr_number.
        let b = get(&home, "o/r", "feat/x", "rev-b").expect("new target persisted");
        assert_eq!(b.pr_number, 21, "same pr_number carried over");
        assert_ne!(b.assignment_id, a.assignment_id, "fresh assignment_id");
        assert_ne!(b.delivery_nonce, a.delivery_nonce, "fresh nonce");
        assert_eq!(b.row, RowState::Pending, "new record starts Pending");
        // Co-target untouched.
        let c_after = get(&home, "o/r", "feat/x", "rev-c").expect("co-target survives");
        assert_eq!(c_after.assignment_id, c.assignment_id);
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "rev-c", &c.delivery_nonce),
            "co-target's row still actionable"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// B18/B19/I18: a terminal marker tombstones ONLY records whose stored
    /// pr_number matches; a DIFFERENT-generation record on the same branch
    /// SURVIVES (no force-bind by branch, no unbound window).
    #[test]
    fn terminal_tombstones_only_matching_pr_number() {
        let home = tmp_home("terminal-pr");
        let g = mk_record("o/r", "feat/x", "rev-g", 30, "2026-07-13T00:00:00Z");
        let gp = mk_record("o/r", "feat/x", "rev-gp", 31, "2026-07-13T00:00:00Z");
        persist(&home, &g).unwrap();
        persist(&home, &gp).unwrap();

        let tombstoned = record_terminal(&home, "o/r", "feat/x", 30, TerminalKind::Merged).unwrap();
        assert_eq!(tombstoned, 1, "only the pr_number==30 record is tombstoned");
        assert!(
            get(&home, "o/r", "feat/x", "rev-g").is_none(),
            "the terminal generation's record is removed"
        );
        let survivor = get(&home, "o/r", "feat/x", "rev-gp").expect("different pr survives");
        assert_eq!(survivor.pr_number, 31);
        assert!(
            terminal_markers(&home, "o/r", "feat/x").contains(30),
            "the terminal pr_number is recorded in the retained marker set"
        );
        assert!(
            !terminal_markers(&home, "o/r", "feat/x").contains(31),
            "the surviving generation is NOT marked terminal"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// B20/I19: terminal markers are RETAINED (no compaction/GC). After many
    /// generations terminate, EVERY marker is still present; re-recording the same
    /// pr_number does not duplicate it.
    #[test]
    fn markers_retained_no_compaction() {
        let home = tmp_home("markers-retained");
        for pr in 100u64..110 {
            record_terminal(&home, "o/r", "feat/x", pr, TerminalKind::Closed).unwrap();
        }
        let markers = terminal_markers(&home, "o/r", "feat/x");
        for pr in 100u64..110 {
            assert!(markers.contains(pr), "marker {pr} retained (no compaction)");
        }
        assert_eq!(markers.markers.len(), 10, "all ten generations retained");

        // Re-recording an existing terminal pr_number does not duplicate the marker.
        record_terminal(&home, "o/r", "feat/x", 105, TerminalKind::Closed).unwrap();
        assert_eq!(
            terminal_markers(&home, "o/r", "feat/x").markers.len(),
            10,
            "re-record is idempotent (no duplicate marker)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A4/I12: append-only repair rotates the nonce, supersedes the OLD row, keeps
    /// the new row actionable, and respects the FIXED-interval bound — a repair
    /// before `next_nudge_at` is a no-op; two repairs inside one interval fire at
    /// most once. `read_at` is never reset.
    #[test]
    fn repair_append_only_fixed_interval() {
        let home = tmp_home("repair");
        let rec = mk_record("o/r", "feat/x", "reviewer", 55, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        let n1 = rec.delivery_nonce.clone();
        // B2: repair only re-nudges a NON-actionable row. Mark the delivered row READ
        // (seen but not acted) — the state the FIXED-interval re-nudge exists for.
        mark_row_read(&home, "reviewer", &n1, "2026-07-13T00:00:00Z");

        // Before next_nudge_at (== created_at) → NOT eligible.
        assert!(
            !repair_row(&home, "o/r", "feat/x", "reviewer", "2026-07-12T23:59:59Z").unwrap(),
            "repair before the lease is a no-op (bounded)"
        );

        // At/after next_nudge_at, with the row seen (non-actionable) → repairs once.
        assert!(
            repair_row(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:01Z").unwrap(),
            "repair fires once the lease is due and the row is non-actionable"
        );
        let after = get(&home, "o/r", "feat/x", "reviewer").unwrap();
        let n2 = after.delivery_nonce.clone();
        assert_ne!(n1, n2, "repair rotates the nonce");
        assert_eq!(
            after.next_nudge_at,
            add_interval("2026-07-13T00:00:01Z"),
            "next_nudge_at advanced by exactly FIXED_INTERVAL"
        );
        // OLD row superseded (not actionable); NEW row actionable.
        assert!(
            !crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &n1),
            "stale row superseded by the old nonce"
        );
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &n2),
            "fresh row carries the new nonce and is actionable"
        );
        // Both nonces still have exactly one row each (append-only, no in-place reset).
        assert_eq!(rows_with_nonce(&home, "reviewer", &n1).len(), 1);
        assert_eq!(rows_with_nonce(&home, "reviewer", &n2).len(), 1);

        // A second repair within the same interval must NOT fire.
        assert!(
            !repair_row(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:30Z").unwrap(),
            "≤ 1 repair per FIXED_INTERVAL"
        );
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer")
                .unwrap()
                .delivery_nonce,
            n2,
            "no rotation on the throttled second repair"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ─────────────────── C7: 4-state evidence classifier ───────────────────

    use crate::daemon::pr_state::{PrState, VerdictState};

    /// A fresh `PrState` at `head` carrying `verdict` (the production constructor,
    /// so the shape never drifts from the real default).
    fn prstate_with(head: &str, verdict: VerdictState) -> PrState {
        let mut ps =
            crate::daemon::pr_state::new_for_branch("o/r", "feat/x", head, ReviewClass::Dual);
        ps.verdict_state = verdict;
        ps
    }

    /// C7 (T24/T25): the FROZEN 4-state classifier (plan §1). Target VERIFIED@head ⇒
    /// Satisfied; a DIFFERENT reviewer's verdict NEVER satisfies/engages this record;
    /// a head-advanced (stale) verdict ⇒ NOT Satisfied; acked-no-verdict ⇒
    /// EngagedPending; nothing ⇒ Unengaged; target REJECTED/UNVERIFIED@head ⇒
    /// EngagedUnsatisfied.
    #[test]
    fn c7_classify_four_state_evidence() {
        let rec = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:00:00Z");

        // target VERIFIED @ current head ⇒ Satisfied.
        let ps = prstate_with(
            "sha-1",
            VerdictState::Verified {
                reviewers: vec![("reviewer".into(), "sha-1".into())],
            },
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::SatisfiedExactHead,
            "target VERIFIED@head ⇒ Satisfied"
        );

        // A DIFFERENT reviewer VERIFIED @ head does NOT satisfy this record.
        let ps = prstate_with(
            "sha-1",
            VerdictState::Verified {
                reviewers: vec![("other".into(), "sha-1".into())],
            },
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::Unengaged,
            "a different reviewer's VERIFIED never satisfies/engages this record"
        );

        // Head advanced AFTER the target's VERIFIED (stale) ⇒ NOT Satisfied.
        let ps = prstate_with(
            "sha-2",
            VerdictState::Verified {
                reviewers: vec![("reviewer".into(), "sha-1".into())],
            },
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::Unengaged,
            "a stale (head-advanced) verdict does NOT satisfy"
        );

        // target REJECTED @ head ⇒ EngagedUnsatisfied.
        let ps = prstate_with(
            "sha-1",
            VerdictState::Rejected {
                reviewer: "reviewer".into(),
                reviewed_head: "sha-1".into(),
                reason: None,
            },
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::EngagedUnsatisfied,
            "target REJECTED@head ⇒ EngagedUnsatisfied"
        );

        // target UNVERIFIED @ head ⇒ EngagedUnsatisfied.
        let ps = prstate_with(
            "sha-1",
            VerdictState::Unverified {
                reviewer: "reviewer".into(),
                reviewed_head: "sha-1".into(),
            },
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::EngagedUnsatisfied,
            "target UNVERIFIED@head ⇒ EngagedUnsatisfied"
        );

        // A DIFFERENT reviewer REJECTED @ head does NOT engage this record.
        let ps = prstate_with(
            "sha-1",
            VerdictState::Rejected {
                reviewer: "other".into(),
                reviewed_head: "sha-1".into(),
                reason: None,
            },
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::Unengaged,
            "a different reviewer's REJECTED never engages this record"
        );

        // acked, no current-head verdict ⇒ EngagedPending.
        let mut acked = rec.clone();
        acked.acked_at = Some("2026-07-13T00:00:05Z".into());
        assert_eq!(
            classify_assignment(&acked, Some(&prstate_with("sha-1", VerdictState::None))),
            AssignmentEvidence::EngagedPending,
            "acked + no verdict ⇒ EngagedPending"
        );

        // nothing (no PrState, not acked) ⇒ Unengaged.
        assert_eq!(
            classify_assignment(&rec, None),
            AssignmentEvidence::Unengaged,
            "no PrState + not acked ⇒ Unengaged"
        );
    }

    // ─────────────────── C9: authenticated correlated ACK ───────────────────

    /// C9 (T23): exactly-one match ⇒ `acked_at`/`acked_by` set as a SINGLE atomic
    /// write that PERSISTS across a re-read; a re-ack is an idempotent no-op success
    /// (never overwrites the original timestamp); a non-matching (target,task_id) is
    /// a no-op.
    #[test]
    fn c9_ack_exactly_one_idempotent_atomic() {
        let home = tmp_home("c9-ack");
        // mk_record ⇒ target "reviewer", task_id "t-orig-1".
        let rec = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:00:00Z");
        persist(&home, &rec).unwrap();

        assert_eq!(
            ack(&home, "reviewer", "t-orig-1", "2026-07-13T00:00:05Z"),
            AckOutcome::Acked,
            "exactly one (target,task_id) match ⇒ Acked"
        );
        let got = get(&home, "o/r", "feat/x", "reviewer").unwrap();
        assert_eq!(got.acked_at.as_deref(), Some("2026-07-13T00:00:05Z"));
        assert_eq!(got.acked_by.as_deref(), Some("reviewer"));
        // Persists across re-read; classify now sees EngagedPending (no verdict).
        assert_eq!(
            classify_assignment(&got, Some(&prstate_with("sha-1", VerdictState::None))),
            AssignmentEvidence::EngagedPending,
            "acked record classifies EngagedPending"
        );

        // Re-ack is an idempotent success — the original timestamp is NOT overwritten.
        assert_eq!(
            ack(&home, "reviewer", "t-orig-1", "2026-07-13T01:00:00Z"),
            AckOutcome::Acked,
            "re-ack is an idempotent success"
        );
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer")
                .unwrap()
                .acked_at
                .as_deref(),
            Some("2026-07-13T00:00:05Z"),
            "re-ack must NOT overwrite the original acked_at"
        );

        // Non-matching sender or task_id ⇒ no-op.
        assert_eq!(
            ack(&home, "someone-else", "t-orig-1", "2026-07-13T02:00:00Z"),
            AckOutcome::NoMatch,
            "a non-target sender never acks"
        );
        assert_eq!(
            ack(&home, "reviewer", "t-nope", "2026-07-13T02:00:00Z"),
            AckOutcome::NoMatch,
            "a non-matching task_id never acks"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// C9 (T15 fail-closed): TWO active assignments with the SAME (target,task_id)
    /// on DIFFERENT branches ⇒ ack is AMBIGUOUS and sets NEITHER. Once the ambiguity
    /// is resolved (one revoked) the remaining single match acks.
    #[test]
    fn c9_ack_ambiguity_fails_closed() {
        let home = tmp_home("c9-ambig");
        let a = mk_record("o/r", "feat/a", "reviewer", 42, "2026-07-13T00:00:00Z");
        let b = mk_record("o/r", "feat/b", "reviewer", 43, "2026-07-13T00:00:00Z");
        persist(&home, &a).unwrap();
        persist(&home, &b).unwrap();

        assert_eq!(
            ack(&home, "reviewer", "t-orig-1", "2026-07-13T00:00:05Z"),
            AckOutcome::Ambiguous,
            ">1 (target,task_id) match ⇒ FAIL CLOSED"
        );
        assert!(
            get(&home, "o/r", "feat/a", "reviewer")
                .unwrap()
                .acked_at
                .is_none(),
            "ambiguous ack sets NOTHING on branch a"
        );
        assert!(
            get(&home, "o/r", "feat/b", "reviewer")
                .unwrap()
                .acked_at
                .is_none(),
            "ambiguous ack sets NOTHING on branch b"
        );

        // Resolve the ambiguity: revoke one ⇒ the remaining single match acks.
        revoke(&home, "o/r", "feat/b", "reviewer", "2026-07-13T00:00:06Z").unwrap();
        assert_eq!(
            ack(&home, "reviewer", "t-orig-1", "2026-07-13T00:00:07Z"),
            AckOutcome::Acked,
            "once unambiguous, the single match acks"
        );
        assert_eq!(
            get(&home, "o/r", "feat/a", "reviewer")
                .unwrap()
                .acked_at
                .as_deref(),
            Some("2026-07-13T00:00:07Z"),
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// B1 — persisting a NEW record (different `assignment_id`) at a key that ALREADY
    /// holds a record must RETIRE the prior record's actionable inbox row under the
    /// same branch lock (atomic revoke-and-replace). Otherwise the old actionable
    /// `delivery_nonce` row is ORPHANED forever — a stale actionable assignment the
    /// reviewer can still act on after it was superseded.
    #[test]
    fn b1_persist_supersedes_orphaned_prior_row() {
        let home = tmp_home("b1");
        let r1 = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:00:00Z");
        persist(&home, &r1).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &r1.delivery_nonce),
            "R1's row is actionable after dispatch"
        );

        // A NEW dispatch at the SAME (repo,branch,target) with a DIFFERENT id.
        let r2 = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:01:00Z");
        assert_ne!(r1.assignment_id, r2.assignment_id, "R2 is a distinct record");
        persist(&home, &r2).unwrap();

        // R1's orphaned actionable row MUST be retired (superseded ⇒ not actionable).
        assert!(
            !crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &r1.delivery_nonce),
            "R1's prior actionable row is superseded on re-persist (not orphaned)"
        );
        // R2 is the stored record.
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer").unwrap().assignment_id,
            r2.assignment_id,
            "R2 is the stored authority record"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// B1 — re-persisting the SAME record (same `assignment_id`) is idempotent and
    /// must NOT retire its own still-actionable row.
    #[test]
    fn b1_persist_same_id_is_idempotent_no_supersede() {
        let home = tmp_home("b1-idem");
        let r1 = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:00:00Z");
        persist(&home, &r1).unwrap();
        durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:05Z").unwrap();
        // Re-persist the identical record (same assignment_id + nonce).
        persist(&home, &r1).unwrap();
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &r1.delivery_nonce),
            "same-id re-persist keeps its own actionable row (idempotent, no self-supersede)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// B4(b) — a CORRUPT markers file must NOT be silently read as an EMPTY
    /// (zero-terminals) set. `record_terminal` reads the RETAINED marker set to
    /// append to; if a corrupt file read as default-empty, the subsequent write
    /// would OVERWRITE the retained set with just the new marker, LOSING every
    /// prior terminal generation (old-generation replays would then no longer die
    /// — B20). Fail closed: `record_terminal` must surface the corruption (Err) and
    /// NOT overwrite. Pre-fix: `read_markers` `.unwrap_or_default()` empties on
    /// corrupt, so `record_terminal` returns `Ok` and clobbers the retained set.
    #[test]
    fn b4_corrupt_markers_fails_closed_not_silent_empty() {
        let home = tmp_home("b4-markers");
        // Retain two terminal generations first.
        record_terminal(&home, "o/r", "feat/x", 100, TerminalKind::Merged).unwrap();
        record_terminal(&home, "o/r", "feat/x", 101, TerminalKind::Closed).unwrap();
        // Corrupt the markers file on disk.
        let mpath = markers_file(&home, "o/r", "feat/x");
        std::fs::write(&mpath, b"{ this is not valid markers json").unwrap();

        // A subsequent record_terminal must FAIL CLOSED (not silently treat the
        // corrupt file as zero terminals and overwrite the retained set).
        let res = record_terminal(&home, "o/r", "feat/x", 102, TerminalKind::Merged);
        assert!(
            res.is_err(),
            "record_terminal must fail closed on a corrupt markers file, not overwrite the retained set"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
