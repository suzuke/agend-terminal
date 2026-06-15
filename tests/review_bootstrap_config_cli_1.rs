//! Repro guard for: "Corrupt MCP config destroyed when backup copy fails —
//! `backing up` warn is a lie (data loss)" (src/mcp_config.rs upsert_mcp_servers).
//!
//! When the existing per-workspace config fails to parse, `upsert_mcp_servers`
//! logs "backing up and starting fresh", then runs a single best-effort
//! `let _ = std::fs::copy(path, &backup)` whose result is DISCARDED, and
//! unconditionally `atomic_write`s a fresh `json!({})` over the original. If the
//! copy fails (disk full, permission, read error) the user's ONLY copy of the
//! shared settings file is destroyed while the log claims a backup was made.
//!
//! This is the exact failure class `store.rs::backup_corrupt_file` was written
//! to fix (its doc: "The prior `let _ = std::fs::copy(...)` swallowed the
//! failure, making the 'backing up' warn a LIE and letting the next save destroy
//! the only copy"). The robust fix was never propagated here.
//!
//! METHOD: static_invariant (source-scan, mirrors tests/core_mutex_invariant.rs).
//! Forcing `std::fs::copy` to fail-but-`atomic_write`-succeed on the SAME path /
//! directory is not deterministic or portable, so we assert at the source level
//! that the swallow pattern is GONE and a confirmed/gated backup is in place.
//!
//! RED now: `let _ = std::fs::copy(path, &backup)` is present in
//! src/mcp_config.rs. GREEN after fix: the swallow is replaced by a robust
//! rename-first backup whose failure gates the subsequent `atomic_write`
//! (e.g. reuse `crate::store::*backup_corrupt_file*` or a checked-result copy).

use std::path::PathBuf;

#[test]
fn upsert_mcp_servers_must_not_swallow_corrupt_backup_copy_bootstrap_config_cli() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/mcp_config.rs");
    let text = std::fs::read_to_string(&path).expect("read src/mcp_config.rs");

    // The swallow pattern: a discarded best-effort copy whose failure cannot
    // gate the destructive atomic_write that follows. Matched ignoring inner
    // whitespace so a trivial reflow doesn't hide the bug.
    let mut violation = None;
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            continue; // skip comment/doc lines mentioning the pattern
        }
        let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        // `let_=std::fs::copy(path,&backup)` — the discarded-result corrupt backup.
        if compact.contains("let_=std::fs::copy(path") {
            violation = Some(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
            break;
        }
    }

    assert!(
        violation.is_none(),
        "mcp_config corrupt-backup swallow still present — a failed `std::fs::copy` \
         cannot gate the subsequent `atomic_write`, so a copy failure destroys the \
         user's ONLY copy of the shared settings file while the warn claims a backup \
         was made. Reuse store.rs's robust pattern (rename-first, gate the write on a \
         confirmed backup). Offending line:\n{}",
        violation.unwrap_or_default()
    );
}
