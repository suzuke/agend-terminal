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
//! ## Detection — a two-layer fence (PR #2612 rework)
//!
//! The first cut of this test matched only the literal call form
//! `snapshot::load(` on a single line. The reviewer (cross-vantage, PR #2612)
//! correctly rejected it: four ordinary Rust forms read the same primitives yet
//! slip past a literal-call scan —
//!   1. `use crate::snapshot::load; ... load(home)`            (direct import)
//!   2. `use crate::snapshot::{agent_is_busy as busy}; ...`    (aliased import)
//!   3. `use crate::snapshot as snap; ... snap::agent_is_busy` (module alias)
//!   4. `let f = crate::snapshot::load; f(home)`               (fn pointer)
//!
//! A new reader in any of those forms would silently escape `ALLOWED_READERS`,
//! turning the anti-growth guard into a false guarantee.
//!
//! The fix makes the scan COMPLETE by combining two layers over production code
//! (`#[cfg(test)]` regions and comments excluded):
//!
//!   Layer 1 — import ban. Production code may reference the snapshot module
//!   only in FULLY-QUALIFIED form. The import shapes that could bring a read
//!   primitive into local scope under a name the read-scan can't see are
//!   banned outright: `use crate::snapshot::{…}` (grouped/aliased), `use
//!   crate::snapshot as …` (module alias), `use crate::snapshot;` (bare
//!   module). This kills forms 2 and 3 at the source.
//!
//!   Layer 2 — read-reference scan. Any production line naming a read primitive
//!   in fully-qualified form (`snapshot::load` / `snapshot::agent_state_of` /
//!   `snapshot::agent_is_busy`, WITHOUT requiring a trailing `(` so fn-pointers
//!   and single-item imports are caught too) marks its file a reader — which
//!   must appear in `ALLOWED_READERS`. This catches forms 1 and 4 and every
//!   current fully-qualified call.
//!
//! Given Layer 1, Layer 2's fully-qualified patterns are complete: there is no
//! way to read a primitive without either a fully-qualified reference (Layer 2)
//! or a banned import (Layer 1). `scanner_catches_all_bypass_forms` below is the
//! fence's fence — it proves all four forms are caught.
//!
//! (Detection relies on rustfmt-normalized spacing, e.g. `crate::snapshot::`
//! not `crate :: snapshot ::`; the repo's `cargo fmt --check` CI gate enforces
//! that normalization, so hand-spaced evasions cannot land.)
//!
//! ## The allowlist
//!
//! Adding a file to `ALLOWED_READERS` REQUIRES a fail-open review of the new
//! reader — the list records that the review happened; it does not waive it.
//! The behavioral companion `snapshot_missing_fails_open_for_dispatch_deciders`
//! (unit test in `src/snapshot.rs`) proves the primitives themselves return the
//! conservative sentinel on a missing / corrupt / old-format snapshot. It lives
//! there because the `snapshot` module is not on the curated `lib.rs` public
//! surface; exposing it would be a production change out of scope here.
//!
//! Inventory as of origin/main: 7 reader files (see `ALLOWED_READERS`). Writers
//! (`snapshot::save`) and type-only refs (`snapshot::FleetSnapshot`) are not
//! reads and are intentionally out of scope.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Fully-qualified read-primitive references. No trailing `(` — so a fn-pointer
/// (`let f = crate::snapshot::load;`) and a single-item import
/// (`use crate::snapshot::load;`) are caught, not just direct calls. The writer
/// `snapshot::save` and type refs `snapshot::FleetSnapshot` are deliberately
/// absent (not reads).
const READ_FN_REFS: &[&str] = &[
    "snapshot::load",
    "snapshot::agent_state_of",
    "snapshot::agent_is_busy",
];

/// Import shapes banned in production (Layer 1): each could bring a read
/// primitive into scope under a name Layer 2's fully-qualified scan cannot see.
/// Banning them forces every snapshot reference to stay fully-qualified.
const BANNED_IMPORTS: &[&str] = &[
    "use crate::snapshot::{",
    "use crate::snapshot as ",
    "use crate::snapshot;",
];

/// Every production file allowed to READ the snapshot projection, each with the
/// reviewed reason its snapshot-driven action is REVERSIBLE / fail-open.
/// Suffix-matched against the `src/`-relative path.
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

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

/// Layer 1: a banned import form (grouped/aliased/bare-module snapshot import).
fn is_banned_import(line: &str) -> bool {
    BANNED_IMPORTS.iter().any(|b| line.contains(b))
}

/// Layer 2: a fully-qualified reference to a snapshot READ primitive.
fn is_read_ref(line: &str) -> bool {
    READ_FN_REFS.iter().any(|r| line.contains(r))
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

    let mut banned: Vec<String> = Vec::new();
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
            if test_lines.contains(&lineno) || is_comment(line) {
                continue;
            }
            // Layer 1 (applies to ALL files, allowlisted or not): the bypass-
            // enabling import forms are forbidden outright.
            if is_banned_import(line) {
                banned.push(format!("  {rel}:{lineno}: {}", line.trim()));
                continue;
            }
            // Layer 2: a fully-qualified read reference marks a reader file.
            if !is_read_ref(line) {
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
        banned.is_empty(),
        "Banned snapshot import form(s) in production (Layer 1).\n\n\
         Reference the snapshot module only in FULLY-QUALIFIED form \
         (`crate::snapshot::<fn>(...)`). Grouped/aliased/bare-module imports \
         (`use crate::snapshot::{{…}}`, `use crate::snapshot as …`, \
         `use crate::snapshot;`) can bring a read primitive into scope under a \
         name the reader-scan cannot see, defeating the fail-open allowlist.\n\n\
         Offending line(s):\n{}",
        banned.join("\n")
    );

    assert!(
        unlisted.is_empty(),
        "New snapshot reader(s) outside the fail-open ALLOWLIST (Layer 2).\n\n\
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

/// The fence's fence: prove the two-layer scan catches every read form the
/// reviewer (PR #2612) showed the literal-call scan missed, and does NOT flag
/// benign lines. Each `case` is classified by the same predicates the main test
/// uses; "caught" = Layer 1 (banned import) OR Layer 2 (read ref) on a
/// non-comment line.
#[test]
fn scanner_catches_all_bypass_forms() {
    fn caught(line: &str) -> bool {
        !is_comment(line) && (is_banned_import(line) || is_read_ref(line))
    }

    // Every bypass form the reviewer flagged, plus the baseline call. Each must
    // be caught on at least one of its lines.
    let must_catch: &[(&str, &[&str])] = &[
        ("baseline call", &["let _ = crate::snapshot::load(home);"]),
        (
            "form 1: direct import + bare call",
            &["use crate::snapshot::load;", "    let _ = load(home);"],
        ),
        (
            "form 2: grouped/aliased import",
            &[
                "use crate::snapshot::{agent_is_busy as busy};",
                "    let _ = busy(home, name);",
            ],
        ),
        (
            "form 3: module alias",
            &[
                "use crate::snapshot as snap;",
                "    let _ = snap::agent_is_busy(home, name);",
            ],
        ),
        (
            "form 4: fn pointer",
            &["let f = crate::snapshot::load;", "    let _ = f(home);"],
        ),
        (
            "bare module import + call",
            &["use crate::snapshot;", "    let _ = snapshot::load(home);"],
        ),
    ];
    for (name, lines) in must_catch {
        assert!(
            lines.iter().any(|l| caught(l)),
            "bypass form NOT caught by the fence: {name}"
        );
    }

    // Negative controls: benign lines must NOT be flagged.
    let must_not_catch: &[&str] = &[
        "    let snapshotter = build_snapshotter();", // unrelated identifier
        "// see crate::snapshot::load for the fail-open contract", // comment
        "    let s: Option<&crate::snapshot::FleetSnapshot> = None;", // type ref, not a read
        "        crate::snapshot::save(home, &snaps);", // writer, not a read
    ];
    for line in must_not_catch {
        assert!(!caught(line), "benign line wrongly flagged: {line:?}");
    }
}
