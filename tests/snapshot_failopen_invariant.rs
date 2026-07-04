//! [fugu §3.4] Snapshot fail-open invariant — every reader of the snapshot
//! projection must be a reviewed fail-open consumer.
//!
//! `workspace/fugu-0acdd8/agend-terminal-solutions.md` §3.4: `snapshot.json` is
//! a FAIL-OPEN projection of the in-memory `StateTracker`. It MAY be read by
//! deciders, but — per the corrected rule — a decision that reads it MUST
//! fail-open, be idempotent, and NEVER cause an IRREVERSIBLE action from a
//! stale or missing snapshot.
//!
//! ## Irreversible-vs-reversible boundary (what this invariant guards)
//!
//! IRREVERSIBLE (forbidden on a snapshot read):
//!   - deleting or overwriting a source-of-truth store (worktree, branch,
//!     task / inbox / decision file), or
//!   - fabricating an OUTBOUND send (a Telegram / Discord / PTY message that
//!     would not exist under a correct snapshot).
//!
//! REVERSIBLE / acceptable (fail-open):
//!   - a nudge, a warn, a log line, a re-nudge, or
//!   - delivering an ALREADY-AUTHORITATIVE inbox message whose TIMING (not
//!     existence) the snapshot gates — the payload is owned by the inbox
//!     source-of-truth and is bounded by the `MAX_DEFER` cap, so a stale
//!     snapshot at worst shifts delivery timing, never invents or drops a
//!     message.
//!
//! ## What this test does
//!
//! It enumerates every PRODUCTION call site of a snapshot READ primitive
//! (`snapshot::load`, `snapshot::agent_state_of`, `snapshot::agent_is_busy`;
//! the writer `snapshot::save` is intentionally out of scope) and asserts each
//! lives in a file on [`ALLOWED_READERS`], whose entry records the reviewed
//! reason the read's action is reversible. A NEW reader in an unlisted file
//! fails this test until its author adds an entry — forcing a fail-open review
//! at the moment a new snapshot dependency is introduced. This is the same
//! anti-growth contract as `task_events_invariant` / `spawn_rationale_audit`.
//!
//! The "never causes an irreversible action" property is therefore enforced by
//! three layers: (1) this allowlist forces review when a reader is added,
//! (2) each entry records the reviewed reversibility rationale, and (3) the
//! behavioral companion below proves the read primitives fail open.
//!
//! Behavioral companion: `snapshot_missing_fails_open_for_dispatch_deciders`
//! (unit test in `src/snapshot.rs`) proves the read primitives return the
//! conservative sentinel on a missing / corrupt / old-format snapshot. It lives
//! there rather than here because the `snapshot` module is not on the curated
//! `lib.rs` public surface — exposing it would be a production change out of
//! scope for this test-only PR.
//!
//! Inventory as of origin/main: 7 files, 10 read sites (see `ALLOWED_READERS`).
//! `#[cfg(test)]` reads (notify / per_tick::snapshot / app) are sliced out.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Snapshot READ primitives. Qualified `snapshot::` form only — the writer
/// `snapshot::save` and the module's own bare internal calls are excluded.
const READ_PATTERNS: &[&str] = &[
    "snapshot::load(",
    "snapshot::agent_state_of(",
    "snapshot::agent_is_busy(",
];

/// Every production file allowed to read the snapshot projection, each with the
/// reviewed reason its snapshot-driven action is REVERSIBLE / fail-open.
/// Suffix-matched against the `src/`-relative path.
///
/// Adding an entry REQUIRES a fail-open review of the new reader — this list
/// records that the review happened; it does not waive it. The bar for every
/// entry: "a stale / missing snapshot here causes only a reversible action."
const ALLOWED_READERS: &[(&str, &str)] = &[
    (
        "src/daemon/dispatch_idle/mod.rs",
        "idle silence gate; missing -> target_is_working=false -> still FIRES the nudge (fail-open by design, #1516). Reversible: a nudge.",
    ),
    (
        "src/inbox/notify.rs",
        "inject defer/drain TIMING gate; missing -> not-busy -> inject now (bounded by MAX_DEFER). Payload is authoritative from the inbox source-of-truth; the snapshot gates timing, not existence.",
    ),
    (
        "src/daemon/handoff_timeout_watchdog.rs",
        "re-nudge gate + telemetry; missing -> not-busy -> re-nudge. Reversible: a nudge.",
    ),
    (
        "src/reply_ledger.rs",
        "sweep gate; missing -> not-busy -> emit_warn + NudgeAgent. Reversible: a warn/nudge, never a delete.",
    ),
    (
        "src/daemon/mod.rs",
        "startup diagnostic only: logs 'previous snapshot found' via tracing::info; missing -> skip the log. No action at all.",
    ),
    (
        "src/api/handlers/query.rs",
        "read-only: builds the status query response; missing -> empty {agents:[], timestamp:null}. No mutation.",
    ),
    (
        "src/bugreport.rs",
        "read-only: renders the snapshot section of a bug report; missing -> section shows nothing. No mutation.",
    ),
];

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs_files(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

/// 1-based line numbers inside `#[cfg(test)] mod` regions (brace-depth tracked),
/// so in-file test modules are sliced out. Mirrors the proven helper in
/// `tests/enqueue_drop_invariant.rs::test_region_lines`.
fn test_region_lines(content: &str) -> HashSet<usize> {
    let mut out = HashSet::new();
    let mut depth: i32 = 0;
    let mut pending_cfg_test = false;
    let mut region_depth: Option<i32> = None;
    for (i, line) in content.lines().enumerate() {
        let t = line.trim();
        if region_depth.is_none() {
            if t.starts_with("#[cfg(test)]") {
                pending_cfg_test = true;
            } else if pending_cfg_test && t.contains("mod ") && line.contains('{') {
                region_depth = Some(depth);
                pending_cfg_test = false;
            }
        }
        if region_depth.is_some_and(|d| depth >= d) {
            out.insert(i + 1);
        }
        let next = depth + line.matches('{').count() as i32 - line.matches('}').count() as i32;
        if let Some(d) = region_depth {
            if next <= d {
                region_depth = None;
            }
        }
        depth = next;
    }
    out
}

fn rel_str(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn snapshot_stale_never_causes_irreversible_action() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = root.join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    files.sort();

    let mut unlisted: Vec<String> = Vec::new();
    let mut matched: HashSet<&str> = HashSet::new();

    for f in &files {
        let rel = rel_str(f, root);
        // The definition module itself is not a consumer.
        if rel == "src/snapshot.rs" {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(f) else {
            continue;
        };
        let test_lines = test_region_lines(&content);
        for (idx, line) in content.lines().enumerate() {
            let lineno = idx + 1;
            if test_lines.contains(&lineno) {
                continue;
            }
            if line.trim_start().starts_with("//") {
                continue;
            }
            if !READ_PATTERNS.iter().any(|p| line.contains(p)) {
                continue;
            }
            match ALLOWED_READERS
                .iter()
                .find(|(suffix, _)| rel.ends_with(suffix))
            {
                Some((suffix, _)) => {
                    matched.insert(*suffix);
                }
                None => unlisted.push(format!("  {rel}:{lineno}: {}", line.trim())),
            }
        }
    }

    assert!(
        unlisted.is_empty(),
        "New snapshot reader(s) outside the fail-open ALLOWLIST.\n\n\
         Every reader of the snapshot projection must fail-open: a stale/missing \
         snapshot may cause only a REVERSIBLE action (nudge/warn/log/timing), never \
         an irreversible one (delete/overwrite/fabricated outbound send). Add the \
         site to ALLOWED_READERS in this file WITH a one-line reversibility \
         rationale — and only after confirming the read actually fails open.\n\n\
         Unlisted read site(s):\n{}",
        unlisted.join("\n")
    );

    let stale: Vec<&str> = ALLOWED_READERS
        .iter()
        .map(|(s, _)| *s)
        .filter(|s| !matched.contains(s))
        .collect();
    assert!(
        stale.is_empty(),
        "Stale ALLOWED_READERS entr(y/ies) — no snapshot read found in: {stale:?}.\n\
         Remove the entry (the reader was deleted/moved) to keep the inventory honest.",
    );
}
