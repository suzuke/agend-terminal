//! Sprint 23 P0 — heartbeat-pair atomicity invariant test.
//!
//! Mirrors PR #233 dual-coverage anti-rollback pattern (source-grep guard
//! plus behavioural test) applied to F6 lock-around-pair (Sprint 20
//! DAEMON.md F6). Extends Sprint 22 P0 anti-growth contract pattern.
//!
//! ## Two enforcement layers
//!
//! 1. **Source-grep guard**: every `save_metadata` / `save_metadata_batch`
//!    call site that writes `"last_heartbeat"` OR `"waiting_on_since"`
//!    MUST be paired with a `heartbeat_pair::update_with` (or
//!    `heartbeat_pair::pair_for(...).lock()`) call within the preceding
//!    10 lines. Pre-pair writes that skip the in-memory update would
//!    re-introduce the F6 race window — caught here.
//!
//! 2. **`EXEMPTED_LEGACY_FILES` anti-growth contract**: no entries by
//!    intent. Adding entries requires explicit dispatch scope per
//!    Sprint 23 P0 dispatch. Sprint 22 P0 pattern transfer.

use std::path::{Path, PathBuf};

/// Files exempted from the pair-update invariant (legacy / bootstrap /
/// test-fixture sites). Empty by intent — new entries forbidden without
/// explicit dispatch scope per Sprint 23 P0 anti-growth contract.
const EXEMPTED_LEGACY_FILES: &[&str] = &[
    // No exemptions today.
];

fn rust_files_in_src() -> Vec<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    walk(&src, &mut out);
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

fn rel_path_str(path: &Path, root: &Path) -> String {
    // Sprint 23 P1 r2 — normalize Windows backslash to forward-slash for
    // cross-platform EXEMPTED-list / inline `ends_with("daemon/heartbeat_pair.rs")`
    // suffix-match. See PR #240 r2.
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_exempted(rel_path: &str) -> bool {
    EXEMPTED_LEGACY_FILES
        .iter()
        .any(|suffix| rel_path.ends_with(suffix))
}

/// Sprint 23 P0 source-grep guard: every save_metadata write of a
/// pair-relevant field must have a heartbeat_pair update within the
/// preceding 10 lines.
#[test]
fn heartbeat_pair_writes_paired_with_in_memory_update() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations: Vec<String> = Vec::new();

    for path in rust_files_in_src() {
        let rel = rel_path_str(&path, &src_root);
        if is_exempted(&rel) {
            continue;
        }
        // The pair module itself + the lock-ordering doc references its
        // own primitives — exempt by definition.
        if rel.ends_with("daemon/heartbeat_pair.rs") {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Cut off at #[cfg(test)] so test-fixture writes don't trip.
        let cutoff_byte = content.find("#[cfg(test)]").unwrap_or(content.len());
        let prod = &content[..cutoff_byte];
        let lines: Vec<&str> = prod.lines().collect();

        for (idx, line) in lines.iter().enumerate() {
            let trim = line.trim_start();
            if trim.starts_with("//") || trim.starts_with("///") || trim.starts_with("//!") {
                continue;
            }
            // Match: write of pair-relevant field via save_metadata or
            // save_metadata_batch. Quoted "last_heartbeat" or
            // "waiting_on_since" appearing on a line that ALSO contains
            // a save_metadata call is the regression pattern.
            //
            // Note: tuples in save_metadata_batch like
            // `("waiting_on", json!(null))` may span multiple lines. We
            // accept a narrow heuristic: the QUOTED token + presence of
            // `save_metadata` in the preceding 5 lines.
            let writes_pair_field =
                line.contains("\"last_heartbeat\"") || line.contains("\"waiting_on_since\"");
            if !writes_pair_field {
                continue;
            }
            let look_back_start = idx.saturating_sub(5);
            let preceding = lines[look_back_start..=idx].join("\n");
            if !preceding.contains("save_metadata") {
                // Field name appears but not in a save_metadata context
                // — likely a struct field decl or doc string. Skip.
                continue;
            }

            // Found a pair-field save_metadata write. Look back 10 lines
            // for the in-memory pair update.
            let pair_look_back_start = idx.saturating_sub(10);
            let pair_window = lines[pair_look_back_start..=idx].join("\n");
            let has_pair_update = pair_window.contains("heartbeat_pair::update_with")
                || pair_window.contains("heartbeat_pair::pair_for")
                || pair_window.contains("heartbeat_pair::snapshot_for");
            if !has_pair_update {
                violations.push(format!(
                    "  {}:{}: save_metadata write of pair-relevant field without preceding \
                     heartbeat_pair update — re-introduces F6 race window\n      offending line: {}",
                    rel,
                    idx + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Sprint 23 P0 F6 lock-around-pair invariant violations — {} write site(s) skip the \
         in-memory pair update.\n\nFix: add `crate::daemon::heartbeat_pair::update_with(name, |p| {{ ... }})` \
         (or equivalent) before the save_metadata call. The in-memory pair update + disk persist \
         pair must remain symmetric so supervisor's snapshot-read sees consistent state.\n\n\
         Do NOT add to EXEMPTED_LEGACY_FILES without explicit dispatch scope per Sprint 23 P0.\n\n\
         Violations:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// Sprint 23 P0 sanity test: the pair module exists and exposes the
/// expected public API. If this test fails, F6 was reverted at module
/// level — the source-grep guard above wouldn't fire because there's
/// nothing to grep against.
#[test]
fn heartbeat_pair_module_exposes_required_api() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let path = src_root.join("daemon/heartbeat_pair.rs");
    let content = std::fs::read_to_string(&path).expect("daemon/heartbeat_pair.rs must exist");
    assert!(
        content.contains("pub fn pair_for("),
        "pair_for(name) must be public — this is the lock acquisition entry point"
    );
    assert!(
        content.contains("pub fn snapshot_for("),
        "snapshot_for(name) must be public — readers depend on it"
    );
    assert!(
        content.contains("pub fn update_with<"),
        "update_with(name, f) must be public — writers depend on it"
    );
    assert!(
        content.contains("pub fn now_ms()"),
        "now_ms() must be public — common utility for callers updating the timestamp"
    );
}
