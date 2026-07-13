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

pub(crate) const SCHEMA_VERSION: u32 = 2;

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
    /// task66: stable reviewer identity captured at dispatch. `None` marks a
    /// legacy assignment that cannot authorize a code-review receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_instance_id: Option<crate::types::InstanceId>,
    /// Exact full PR head assigned for review. Prefixes and bind-on-observation
    /// are forbidden; `None` is LegacyAssignment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_head: Option<String>,
    /// Explicit review slot within the task's review class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_slot: Option<crate::review_receipt::ReviewSlot>,
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
    /// Whether this active row carries every immutable field required to mint a
    /// task66 receipt. Anything else is a LegacyAssignment and must be
    /// explicitly re-dispatched; it is never upgraded by inference.
    pub(crate) fn is_receipt_capable(&self) -> bool {
        self.schema_version >= SCHEMA_VERSION
            && self.target_instance_id.is_some()
            && self
                .reviewed_head
                .as_deref()
                .is_some_and(crate::review_receipt::is_full_head)
            && self.review_slot.is_some()
            && !matches!(self.review_class, ReviewClass::Unresolved)
    }

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
            schema_version: 1,
            assignment_id: uuid::Uuid::new_v4(),
            pr_number,
            sender: sender.into(),
            task_id: task_id.into(),
            review_class,
            review_author,
            target_instance_id: None,
            reviewed_head: None,
            review_slot: None,
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

    /// Construct a receipt-capable assignment. Only the authoritative review
    /// dispatch uses this; the old constructor intentionally creates a legacy
    /// row so fixtures/imports cannot gain authority by inference.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_pending_typed(
        repo: impl Into<String>,
        branch: impl Into<String>,
        target: impl Into<String>,
        target_instance_id: crate::types::InstanceId,
        pr_number: u64,
        reviewed_head: impl Into<String>,
        review_slot: crate::review_receipt::ReviewSlot,
        sender: impl Into<String>,
        task_id: impl Into<String>,
        review_class: ReviewClass,
        review_author: ReviewAuthor,
        text: impl Into<String>,
        thread_id: Option<String>,
        parent_id: Option<String>,
        created_at: &str,
    ) -> Self {
        let mut record = Self::new_pending(
            repo,
            branch,
            target,
            pr_number,
            sender,
            task_id,
            review_class,
            review_author,
            text,
            thread_id,
            parent_id,
            created_at,
        );
        record.schema_version = SCHEMA_VERSION;
        record.target_instance_id = Some(target_instance_id);
        record.reviewed_head = Some(reviewed_head.into());
        record.review_slot = Some(review_slot);
        record
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
    let review_assignment = if record.is_receipt_capable() {
        match (
            record.target_instance_id,
            record.reviewed_head.clone(),
            record.review_slot,
        ) {
            (Some(target_instance_id), Some(reviewed_head), Some(slot)) => {
                Some(crate::review_receipt::ReviewAssignmentEnvelope {
                    assignment_id: record.assignment_id,
                    repo: record.repo.clone(),
                    pr_number: record.pr_number,
                    branch: record.branch.clone(),
                    task_id: record.task_id.clone(),
                    reviewed_head,
                    review_class: record.review_class,
                    slot,
                    target_instance_id,
                })
            }
            _ => None,
        }
    } else {
        None
    };
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
        review_assignment,
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
fn list_active_checked(
    home: &Path,
    repo: &str,
    branch: &str,
) -> anyhow::Result<Vec<ActiveAssignment>> {
    let dir = branch_dir(home, repo, branch);
    // B4 (codex m-…-322): a `read_dir` error must NOT be swallowed. A genuinely-
    // MISSING dir (`NotFound`) is a genuine ABSENCE (no assignments) ⇒ `Ok(empty)`;
    // any OTHER error means the dir EXISTS but is unreadable ⇒ propagate `Err` so the
    // merge-gate caller keeps the existing reserved set (never read as "no
    // reservation"). The old `read_dir(&dir).into_iter().flatten().flatten()` mapped
    // BOTH cases to empty — an exists-but-unreadable dir then silently DROPPED the
    // reservation and OPENed the gate.
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "unreadable assignment dir {}: {e}",
                dir.display()
            ))
        }
    };
    let mut out = Vec::new();
    for entry in rd {
        // A directory entry that fails to read mid-scan (the dir became unreadable)
        // propagates ⇒ fail closed, same as a corrupt record.
        let path = entry
            .map_err(|e| {
                anyhow::anyhow!("unreadable assignment dir entry in {}: {e}", dir.display())
            })?
            .path();
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

/// t-…-17 B4 (codex m-…-322): the merge-gate outcome of reading a branch's assignment
/// authority — a TRI-STATE distinction the lossy `list_active` cannot make. The A6
/// drain in [`crate::daemon::pr_state::record_ci_result`] uses THIS (not a lossy read)
/// so a SOLE corrupt record fails the gate closed instead of looking assignment-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchAuthority {
    /// The branch dir is MISSING (`read_dir` `NotFound`) OR exists with ZERO records —
    /// a genuine absence. The reserved derivation may proceed (an empty reserved set is
    /// then correct) and CLEAR `authority_unknown`.
    Absent,
    /// The branch dir exists with ≥1 VALID record AND no corrupt one. Take the
    /// assignment lock and derive under it.
    Active,
    /// The branch dir `read_dir` errored (non-`NotFound`) OR ANY record is
    /// corrupt/unreadable. The authority cannot be read reliably ⇒ FAIL CLOSED (keep
    /// the existing reserved set and SET `authority_unknown`); do NOT derive.
    Unreadable,
}

/// t-…-17 B4 (codex m-…-322): probe `(repo,branch)`'s assignment authority for the
/// merge-gate drain. NotFound dir OR zero records ⇒ [`BranchAuthority::Absent`]; a dir
/// with ≥1 valid record and no corrupt one ⇒ [`BranchAuthority::Active`]; a
/// non-NotFound `read_dir` error OR ANY corrupt record ⇒ [`BranchAuthority::Unreadable`]
/// (fail closed). Unlike a lossy read, a corrupt record is NEVER conflated with
/// absence. Lock-free (mirrors the enumeration in [`list_active`]); creates no dir.
pub(crate) fn probe_branch_authority(home: &Path, repo: &str, branch: &str) -> BranchAuthority {
    let dir = branch_dir(home, repo, branch);
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BranchAuthority::Absent,
        Err(_) => return BranchAuthority::Unreadable,
    };
    let mut saw_record = false;
    for entry in rd {
        let path = match entry {
            Ok(e) => e.path(),
            // The dir became unreadable mid-scan ⇒ fail closed.
            Err(_) => return BranchAuthority::Unreadable,
        };
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("markers.json") {
            continue;
        }
        match read_record(&path) {
            Ok(Some(_)) => saw_record = true,
            // A concurrent remove raced us — genuine absence of THIS record; keep scanning.
            Ok(None) => {}
            // A corrupt record ⇒ authority unreadable, whatever else the dir holds.
            Err(_) => return BranchAuthority::Unreadable,
        }
    }
    if saw_record {
        BranchAuthority::Active
    } else {
        BranchAuthority::Absent
    }
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

/// Strict store-wide lookup for a receipt's generation token. Missing,
/// unreadable/corrupt, duplicated, terminal, revoked, and superseded assignments
/// all fail closed. Unlike the lossy reconcile scans, one corrupt row aborts the
/// lookup rather than being treated as absence.
pub(crate) fn lookup_by_assignment_id_strict(
    home: &Path,
    assignment_id: uuid::Uuid,
) -> anyhow::Result<ActiveAssignment> {
    let base = base_dir(home);
    let dirs = match std::fs::read_dir(&base) {
        Ok(dirs) => dirs,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("active assignment not found")
        }
        Err(e) => anyhow::bail!("unreadable assignment store {}: {e}", base.display()),
    };
    let mut found: Option<ActiveAssignment> = None;
    for dir in dirs {
        let dir = dir.map_err(|e| anyhow::anyhow!("unreadable assignment directory: {e}"))?;
        if !dir.path().is_dir() {
            continue;
        }
        let rows = std::fs::read_dir(dir.path()).map_err(|e| {
            anyhow::anyhow!("unreadable assignment branch {}: {e}", dir.path().display())
        })?;
        for row in rows {
            let path = row
                .map_err(|e| anyhow::anyhow!("unreadable assignment row: {e}"))?
                .path();
            if path.extension().and_then(|e| e.to_str()) != Some("json")
                || path.file_name().and_then(|n| n.to_str()) == Some("markers.json")
            {
                continue;
            }
            let Some(record) = read_record(&path)? else {
                continue;
            };
            if record.assignment_id != assignment_id {
                continue;
            }
            if found.is_some() {
                anyhow::bail!("assignment id is ambiguous (duplicate active rows)");
            }
            found = Some(record);
        }
    }
    let record = found.ok_or_else(|| anyhow::anyhow!("active assignment not found"))?;
    let markers = read_markers(&markers_file(home, &record.repo, &record.branch))?;
    if markers.contains(record.pr_number) {
        anyhow::bail!("assignment generation is terminal");
    }
    Ok(record)
}

/// Strict pre-cutover census of every active LegacyAssignment. A corrupt or
/// unreadable row aborts the census instead of producing a misleading empty
/// inventory. Terminal generations are excluded; they cannot authorize a
/// receipt and are handled by the retained terminal-marker reconciler.
pub(crate) fn legacy_active_assignments_strict(
    home: &Path,
) -> anyhow::Result<Vec<ActiveAssignment>> {
    let base = base_dir(home);
    let dirs = match std::fs::read_dir(&base) {
        Ok(dirs) => dirs,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => anyhow::bail!("unreadable assignment store {}: {e}", base.display()),
    };
    let mut legacy = Vec::new();
    for dir in dirs {
        let dir = dir.map_err(|e| anyhow::anyhow!("unreadable assignment directory: {e}"))?;
        if !dir.path().is_dir() {
            continue;
        }
        let rows = std::fs::read_dir(dir.path()).map_err(|e| {
            anyhow::anyhow!("unreadable assignment branch {}: {e}", dir.path().display())
        })?;
        for row in rows {
            let path = row
                .map_err(|e| anyhow::anyhow!("unreadable assignment row: {e}"))?
                .path();
            if path.extension().and_then(|e| e.to_str()) != Some("json")
                || path.file_name().and_then(|n| n.to_str()) == Some("markers.json")
            {
                continue;
            }
            let Some(record) = read_record(&path)? else {
                continue;
            };
            let markers = read_markers(&markers_file(home, &record.repo, &record.branch))?;
            if !markers.contains(record.pr_number) && !record.is_receipt_capable() {
                legacy.push(record);
            }
        }
    }
    legacy.sort_by(|a, b| {
        (&a.repo, &a.branch, a.pr_number, &a.target, a.assignment_id).cmp(&(
            &b.repo,
            &b.branch,
            b.pr_number,
            &b.target,
            b.assignment_id,
        ))
    });
    Ok(legacy)
}

/// The retained terminal-marker set for `(repo,branch)`. Inspection-only accessor:
/// a corrupt/absent file yields an empty set. Production terminal paths
/// ([`record_terminal`] / [`tombstone_terminal_matches`]) read markers through the
/// corruption-aware [`read_markers`] instead so they can fail closed on corruption —
/// so this convenience accessor is now only referenced by tests (`#[cfg(test)]`).
#[cfg(test)]
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
    let mut new_record = ActiveAssignment::new_pending(
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
    if old.target_instance_id.is_some() || old.reviewed_head.is_some() || old.review_slot.is_some()
    {
        let target_instance_id = crate::fleet::resolve_uuid(home, new_target).ok_or_else(|| {
            anyhow::anyhow!("assignment_authority::transfer: new target has no stable InstanceId")
        })?;
        new_record.schema_version = SCHEMA_VERSION;
        new_record.target_instance_id = Some(target_instance_id);
        new_record.reviewed_head = old.reviewed_head;
        new_record.review_slot = old.review_slot;
    }
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

/// t-…-17 B4 (codex m-…-378): the SINGLE `authority_unknown` transition, applied
/// IDENTICALLY by BOTH the A6 drain ([`crate::daemon::pr_state::record_ci_result`])
/// and the A10b reconciler ([`redrive_reserved`]). codex REJECTED the split where the
/// reconciler could neither SET the flag on a freshly-unreadable authority NOR CLEAR it
/// after a transient corruption/lock-failure was repaired — a repaired branch stayed
/// STUCK merge-closed until an unrelated CI event drove `record_ci_result`. Extracting
/// the transition into one helper called from both paths makes re-divergence impossible.
///
/// Given the tri-state `authority` probe, whether the assignment lock is held
/// (`lock_acquired`), and a `derive` that re-derives the reserved set under the caller's
/// lock discipline:
///   - `Absent`             ⇒ derive lock-free; `Ok` ⇒ replace reserved + CLEAR; `Err`
///     (raced to unreadable) ⇒ keep + SET.
///   - `Active` + lock held  ⇒ derive under the lock; `Ok` ⇒ replace + CLEAR; `Err`
///     (co-resident corrupt record) ⇒ keep + SET.
///   - `Active` w/o the lock ⇒ keep + SET (a lock-free derive could read a torn set that
///     wrongly CLEARS the gate).
///   - `Unreadable`          ⇒ keep + SET; do NOT derive.
///
/// Net: SET on corrupt/unreadable/required-lock-failure, CLEARED only on a successful
/// locked-or-absent derive. A reservation is NEVER dropped on corruption (fail closed).
pub(crate) fn apply_authority_transition(
    state: &mut PrState,
    repo: &str,
    branch: &str,
    authority: BranchAuthority,
    lock_acquired: bool,
    derive: impl FnOnce(&mut PrState) -> anyhow::Result<Vec<ReservedAssignment>>,
) {
    match authority {
        // Genuine absence — an empty reserved set is correct; derive lock-free
        // (nothing to race). On Ok, adopt it and CLEAR the flag (authority is
        // readable). A dir that flipped to unreadable between probe and derive
        // KEEPS the existing set and fails closed.
        BranchAuthority::Absent => match derive(state) {
            Ok(v) => {
                state.reserved_assignments = v;
                state.authority_unknown = false;
            }
            Err(e) => {
                state.authority_unknown = true;
                tracing::error!(
                    repo = %repo, branch = %branch, error = %e,
                    "t-…-17 B4: reserved derivation failed on an absent-probed branch (raced to unreadable) — keeping existing reserved set + authority_unknown SET (fail closed)"
                );
            }
        },
        // Active + lock held — derive under the OUTER lock (consistent w.r.t. a
        // concurrent revoke/transfer). Ok ⇒ adopt + CLEAR; Err (a co-resident
        // corrupt record) ⇒ keep existing set + SET (never drop on corruption).
        BranchAuthority::Active if lock_acquired => match derive(state) {
            Ok(v) => {
                state.reserved_assignments = v;
                state.authority_unknown = false;
            }
            Err(e) => {
                state.authority_unknown = true;
                tracing::error!(
                    repo = %repo, branch = %branch, error = %e,
                    "t-…-17 B4: reserved derivation skipped — corrupt authority record; keeping existing reserved set + authority_unknown SET (fail closed, gate stays closed)"
                );
            }
        },
        // Active but the required lock could NOT be acquired — a lock-free
        // derive could read a torn set that CLEARS the gate. Keep the existing
        // set + SET (fail closed); the per-tick reconciler re-derives under the
        // lock later.
        BranchAuthority::Active => {
            state.authority_unknown = true;
            tracing::warn!(
                repo = %repo, branch = %branch,
                "t-…-17 B4(d): reserved derivation skipped fail-closed — assignment lock not acquired while active assignments exist; keeping existing reserved set + authority_unknown SET (reconciler re-derives under the lock)"
            );
        }
        // The authority is UNREADABLE (a corrupt/unreadable record or an
        // unreadable branch dir) — do NOT derive (can't read reliably). Keep the
        // existing set + SET (fail closed). THIS is the sole-corrupt-record path
        // the lossy `has_active` used to mis-read as assignment-free.
        BranchAuthority::Unreadable => {
            state.authority_unknown = true;
            tracing::error!(
                repo = %repo, branch = %branch,
                "t-…-17 B4: reserved derivation skipped — branch authority UNREADABLE (corrupt/unreadable record or dir); keeping existing reserved set + authority_unknown SET (fail closed, gate stays closed)"
            );
        }
    }
    // t-…-17 B4 (codex m-…-479): recompute the CACHED derived `merge_state` from the
    // authority we JUST mutated (reserved_assignments / authority_unknown). This is the
    // FINAL step of the shared transition, so BOTH callers — the `record_ci_result` A6
    // drain AND `redrive_reserved` — get it and cannot diverge. Why it is required: the
    // reducer (`apply(Event::CiObserved)`) derived `merge_state` BEFORE this transition
    // ran, and `redrive_reserved` never derived it at all — so a late ACTIVE reservation
    // or a freshly-unreadable authority left a STALE `MergeReady`, and the scanner emits
    // `[pr-ready-for-merge]` on that cached value → the fail-closed merge gate is
    // bypassed. Mirror the reducer's derivation EXACTLY (reuse `is_merge_ready`, do NOT
    // hand-roll a divergent gate). Guard STRICTLY: only a NONTERMINAL `merge_state` is
    // re-derived — a terminal `Merged` / `ClosedUnmerged` is sticky and NEVER resurrected
    // (mirrors the reducer's terminal guard). This is pure state derivation under the
    // caller's pr_state flock: it emits/wakes nothing and changes no lock order. Being in
    // the shared helper makes the recompute bidirectional (a revoked reservation / repaired
    // authority re-derives back to `MergeReady` when otherwise ready — not a one-way latch).
    if !matches!(
        state.merge_state,
        MergeState::Merged { .. } | MergeState::ClosedUnmerged { .. }
    ) {
        state.merge_state = if crate::daemon::pr_state::is_merge_ready(state) {
            MergeState::MergeReady
        } else {
            MergeState::NotReady
        };
    }
}

/// A10b — re-derive `reserved_assignments` on the LIVE `PrState` for `(repo,branch)`
/// (declarative, convergent) AND apply the SAME `authority_unknown` set/clear the A6
/// drain does, via the shared [`apply_authority_transition`]. Lock order is the mandated
/// assignment-lock OUTER → pr_state-flock INNER (I11/I15): probe lock-free, take the
/// branch lock ONLY when `Active`, then [`crate::daemon::pr_state::with_pr_state`] takes
/// the pr_state flock. On an assignment-lock FAILURE only the pr_state flock is taken —
/// setting the fail-closed flag needs just that, and no OUTER lock is acquired after the
/// INNER, so there is no lock-order inversion. A missing pr_state file is a no-op.
/// Best-effort (a lock/save failure is swallowed — the next tick re-derives).
///
/// t-…-17 B4 (codex m-…-378): this MIRRORS `record_ci_result`'s drain exactly — before,
/// this path set reserved only (never touching `authority_unknown`), so a transient
/// unreadable/lock-failure that SET the flag stayed STUCK-TRUE after repair (the gate
/// never reopened without a CI event) and a freshly-unreadable authority never SET it on
/// a stale-false state. A corrupt-record derivation failure still KEEPS the existing
/// reserved set (fail closed) — the reservation is never dropped on corruption.
pub(crate) fn redrive_reserved(home: &Path, repo: &str, branch: &str) {
    let authority = probe_branch_authority(home, repo, branch);
    // Acquire the OUTER assignment lock ONLY when Active (mirrors `record_ci_result`):
    // `Absent` derives lock-free, `Unreadable` is not derived at all, and an `Active`
    // branch whose lock cannot be acquired is handled fail-closed by the transition.
    let _lock = if matches!(authority, BranchAuthority::Active) {
        lock_branch_for_drain(home, repo, branch)
    } else {
        None
    };
    let lock_acquired = _lock.is_some();
    let _ = crate::daemon::pr_state::with_pr_state(home, repo, branch, |ps| {
        apply_authority_transition(ps, repo, branch, authority, lock_acquired, |state| {
            derive_reserved_for_prstate(home, repo, branch, state)
        });
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

// ─────────────────────────── C7: 3-state evidence classifier ───────────────────────────

/// The 3-state evidence classification for one active assignment.
/// DERIVED, never stored. `SatisfiedExactHead` ⇒ NOT reserved / no nudge; the other
/// two ⇒ reserved; only `Unengaged` is eligible for nudge/repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssignmentEvidence {
    /// `record.target` is VERIFIED at the CURRENT head.
    SatisfiedExactHead,
    /// `record.target` REJECTED / UNVERIFIED at the CURRENT head.
    EngagedUnsatisfied,
    /// None of the above (incl. no `PrState`/head yet, and not acked).
    Unengaged,
}

/// C7 — classify one assignment's code-review evidence. Only a typed receipt
/// retained by the PR-state funnel may satisfy or engage the assignment. Legacy
/// collapsed verdicts, reviewer names, reviewed_head display data, report text,
/// and the old generic correlated ACK are never code-review authority.
pub(crate) fn classify_assignment(
    record: &ActiveAssignment,
    prstate: Option<&crate::daemon::pr_state::PrState>,
) -> AssignmentEvidence {
    if let (Some(ps), Some(target_instance_id), Some(reviewed_head), Some(slot)) = (
        prstate,
        record.target_instance_id,
        record.reviewed_head.as_deref(),
        record.review_slot,
    ) {
        if let Some(receipt) = ps.validated_review_receipts.iter().find(|receipt| {
            receipt.assignment_id == record.assignment_id
                && receipt.reviewer_instance_id == target_instance_id
                && receipt.reviewer_name == record.target
                && receipt.repo == record.repo
                && receipt.pr_number == record.pr_number
                && receipt.branch == record.branch
                && receipt.task_id == record.task_id
                && receipt.reviewed_head == reviewed_head
                && receipt.reviewed_head == ps.head_sha
                && receipt.review_class == record.review_class
                && receipt.slot == slot
                && receipt.matches_state(ps)
        }) {
            return match receipt.verdict {
                crate::review_receipt::ReviewVerdict::Verified => {
                    AssignmentEvidence::SatisfiedExactHead
                }
                crate::review_receipt::ReviewVerdict::Rejected
                | crate::review_receipt::ReviewVerdict::Unverified => {
                    AssignmentEvidence::EngagedUnsatisfied
                }
            };
        }
    }
    AssignmentEvidence::Unengaged
}

// ─────────────────────────── C9: authenticated correlated ACK ───────────────────────────

/// The outcome of an authenticated correlated ACK ([`ack`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
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
#[cfg(test)]
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
#[cfg(test)]
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

    /// A legacy schema row cannot look receipt-capable merely because partial
    /// rollout fields happen to be present. It must be re-dispatched before the
    /// reviewer receives a typed assignment envelope.
    #[test]
    fn legacy_schema_with_typed_fields_does_not_emit_assignment_envelope_2760() {
        let mut record = mk_record("o/r", "feat/legacy", "reviewer", 7, "2026-07-13T00:00:00Z");
        record.target_instance_id = Some(crate::types::InstanceId::new());
        record.reviewed_head = Some("a".repeat(40));
        record.review_slot = Some(crate::review_receipt::ReviewSlot::Primary);

        assert!(build_delivery_message(&record, "2026-07-13T00:00:01Z")
            .review_assignment
            .is_none());

        record.schema_version = SCHEMA_VERSION;
        assert!(build_delivery_message(&record, "2026-07-13T00:00:01Z")
            .review_assignment
            .is_some());
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

    fn typed_record(head: &str) -> ActiveAssignment {
        let mut record = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:00:00Z");
        record.schema_version = SCHEMA_VERSION;
        record.target_instance_id = Some(crate::types::InstanceId::new());
        record.reviewed_head = Some(head.to_string());
        record.review_slot = Some(crate::review_receipt::ReviewSlot::Primary);
        record
    }

    fn prstate_with_receipt(
        record: &ActiveAssignment,
        head: &str,
        verdict: Option<crate::review_receipt::ReviewVerdict>,
    ) -> PrState {
        let mut ps =
            crate::daemon::pr_state::new_for_branch("o/r", "feat/x", head, ReviewClass::Dual);
        ps.pr_number = 42;
        if let Some(verdict) = verdict {
            ps.validated_review_receipts
                .push(crate::review_receipt::ReviewReceiptSummary {
                    receipt_id: "review-receipt:m-test".into(),
                    source_id: "m-test".into(),
                    evidence_digest: "a".repeat(64),
                    assignment_id: record.assignment_id,
                    reviewer_instance_id: record.target_instance_id.unwrap(),
                    reviewer_name: record.target.clone(),
                    repo: record.repo.clone(),
                    pr_number: record.pr_number,
                    branch: record.branch.clone(),
                    task_id: record.task_id.clone(),
                    reviewed_head: record.reviewed_head.clone().unwrap(),
                    review_class: record.review_class,
                    slot: record.review_slot.unwrap(),
                    verdict,
                });
        }
        ps
    }

    /// task66: only exact typed receipt evidence classifies an assignment. The
    /// collapsed legacy VerdictState and generic correlated ACK remain inert.
    #[test]
    fn c7_classify_only_typed_exact_receipt_evidence() {
        let head = "a".repeat(40);
        let rec = typed_record(&head);

        let ps = prstate_with_receipt(
            &rec,
            &head,
            Some(crate::review_receipt::ReviewVerdict::Verified),
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::SatisfiedExactHead,
            "exact typed VERIFIED receipt satisfies its assignment"
        );

        let mut wrong_identity = ps.clone();
        wrong_identity.validated_review_receipts[0].reviewer_instance_id =
            crate::types::InstanceId::new();
        assert_eq!(
            classify_assignment(&rec, Some(&wrong_identity)),
            AssignmentEvidence::Unengaged,
            "another stable reviewer identity cannot satisfy the assignment"
        );

        let advanced = "b".repeat(40);
        let ps = prstate_with_receipt(
            &rec,
            &advanced,
            Some(crate::review_receipt::ReviewVerdict::Verified),
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::Unengaged,
            "a stale typed receipt does not satisfy after head advance"
        );

        let ps = prstate_with_receipt(
            &rec,
            &head,
            Some(crate::review_receipt::ReviewVerdict::Rejected),
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::EngagedUnsatisfied,
            "exact typed REJECTED receipt is engaged-unsatisfied"
        );

        let ps = prstate_with_receipt(
            &rec,
            &head,
            Some(crate::review_receipt::ReviewVerdict::Unverified),
        );
        assert_eq!(
            classify_assignment(&rec, Some(&ps)),
            AssignmentEvidence::EngagedUnsatisfied,
            "exact typed UNVERIFIED receipt is engaged-unsatisfied"
        );

        let mut legacy = prstate_with_receipt(&rec, &head, None);
        legacy.verdict_state = VerdictState::Verified {
            reviewers: vec![("reviewer".into(), head.clone())],
        };
        assert_eq!(
            classify_assignment(&rec, Some(&legacy)),
            AssignmentEvidence::Unengaged,
            "legacy collapsed verdict state is display-only"
        );

        let mut acked = rec.clone();
        acked.acked_at = Some("2026-07-13T00:00:05Z".into());
        assert_eq!(
            classify_assignment(&acked, Some(&prstate_with_receipt(&acked, &head, None))),
            AssignmentEvidence::Unengaged,
            "generic correlated ACK is not code-review evidence"
        );

        assert_eq!(
            classify_assignment(&rec, None),
            AssignmentEvidence::Unengaged,
            "no PR state means no receipt evidence"
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
        // Legacy ACK persists for backwards-compatible audit display, but it no
        // longer counts as code-review evidence.
        assert_eq!(
            classify_assignment(&got, None),
            AssignmentEvidence::Unengaged,
            "acked record remains unengaged without a typed receipt"
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

    /// task66/RED 11: the receipt-generation lookup is strict across the active
    /// store. Missing IDs, duplicate generations, and even an unrelated corrupt
    /// co-resident row all fail closed instead of degrading to "not found".
    #[test]
    fn strict_assignment_id_lookup_rejects_missing_duplicate_and_corrupt_2760() {
        let missing_home = tmp_home("strict-id-missing");
        assert!(lookup_by_assignment_id_strict(&missing_home, uuid::Uuid::new_v4()).is_err());

        let duplicate_home = tmp_home("strict-id-duplicate");
        let a = mk_record("o/r", "feat/a", "reviewer-a", 42, "2026-07-14T00:00:00Z");
        let mut b = mk_record("o/r", "feat/b", "reviewer-b", 43, "2026-07-14T00:00:00Z");
        b.assignment_id = a.assignment_id;
        persist(&duplicate_home, &a).unwrap();
        persist(&duplicate_home, &b).unwrap();
        let duplicate = lookup_by_assignment_id_strict(&duplicate_home, a.assignment_id)
            .expect_err("duplicate generation must reject");
        assert!(duplicate.to_string().contains("ambiguous"));

        let corrupt_home = tmp_home("strict-id-corrupt");
        let valid = mk_record("o/r", "feat/x", "reviewer", 42, "2026-07-14T00:00:00Z");
        persist(&corrupt_home, &valid).unwrap();
        std::fs::write(
            record_file(&corrupt_home, "o/r", "feat/x", "corrupt-row"),
            b"{not-json",
        )
        .unwrap();
        let corrupt = lookup_by_assignment_id_strict(&corrupt_home, valid.assignment_id)
            .expect_err("corrupt store must reject even when the target row parses");
        assert!(corrupt.to_string().contains("corrupt assignment record"));

        std::fs::remove_dir_all(missing_home).ok();
        std::fs::remove_dir_all(duplicate_home).ok();
        std::fs::remove_dir_all(corrupt_home).ok();
    }

    #[test]
    fn cutover_census_enumerates_only_active_receipt_incapable_rows_2760() {
        let home = tmp_home("legacy-census");
        let legacy = mk_record(
            "o/r",
            "feat/legacy",
            "reviewer-a",
            41,
            "2026-07-14T00:00:00Z",
        );
        let mut partial = mk_record(
            "o/r",
            "feat/partial",
            "reviewer-b",
            42,
            "2026-07-14T00:00:00Z",
        );
        partial.schema_version = SCHEMA_VERSION;
        partial.target_instance_id = Some(crate::types::InstanceId::new());
        let typed = ActiveAssignment::new_pending_typed(
            "o/r",
            "feat/typed",
            "reviewer-c",
            crate::types::InstanceId::new(),
            43,
            "a".repeat(40),
            crate::review_receipt::ReviewSlot::Primary,
            "lead",
            "t-typed",
            ReviewClass::Single,
            ReviewAuthor::External("octocat".into()),
            "review",
            None,
            None,
            "2026-07-14T00:00:00Z",
        );
        persist(&home, &legacy).unwrap();
        persist(&home, &partial).unwrap();
        persist(&home, &typed).unwrap();

        let rows = legacy_active_assignments_strict(&home).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.assignment_id).collect::<Vec<_>>(),
            vec![legacy.assignment_id, partial.assignment_id],
            "schema-1 and partially upgraded rows require audited re-dispatch; an exact typed row does not"
        );
        std::fs::remove_dir_all(home).ok();
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
        assert_ne!(
            r1.assignment_id, r2.assignment_id,
            "R2 is a distinct record"
        );
        persist(&home, &r2).unwrap();

        // R1's orphaned actionable row MUST be retired (superseded ⇒ not actionable).
        assert!(
            !crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &r1.delivery_nonce),
            "R1's prior actionable row is superseded on re-persist (not orphaned)"
        );
        // R2 is the stored record.
        assert_eq!(
            get(&home, "o/r", "feat/x", "reviewer")
                .unwrap()
                .assignment_id,
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
