//! #1504 L1 invariant: `agent/mod.rs` must not parse PATH with a hardcoded `:`
//! separator. Windows PATH is `;`-separated and entries carry drive-colons
//! (`C:\…`), so `.split(':')` shreds it → `which_in("git")` fails → the daemon
//! never injects `AGEND_REAL_GIT` → the shim resolves git to itself → recursive
//! spawn storm (the #1504 root cause). Use `std::env::split_paths`.
//!
//! Pairs with the runtime fix; this is the deterministic, cross-platform RED
//! that fails CI if the hardcoded split ever returns (comment mentions of the
//! pattern are skipped).

#[test]
fn agent_mod_no_hardcoded_path_colon_split_1504() {
    let src = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/agent/mod.rs"),
    )
    .expect("read src/agent/mod.rs");

    let mut violations = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let t = line.trim_start();
        // Skip comment/doc lines that merely mention the pattern.
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        if line.contains(".split(':')") {
            violations.push(format!("{}: {}", i + 1, line.trim()));
        }
    }

    assert!(
        violations.is_empty(),
        "#1504: agent/mod.rs parses PATH with hardcoded `.split(':')` — use \
         std::env::split_paths (Windows PATH is `;`-separated with drive-colons):\n{}",
        violations.join("\n")
    );
}
