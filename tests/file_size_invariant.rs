//! File size invariant — prevents MCP handler monolith regrowth.
//!
//! Sprint 26 PR-C: after splitting src/mcp/handlers.rs (3223 LOC) into
//! sub-modules, this test enforces that no single file in the handlers
//! directory exceeds 500 LOC. Prevents the split-then-regrow pattern
//! observed in prior commit 386b98d.

use std::path::Path;

const HANDLERS_DIR: &str = "src/mcp/handlers";
const MAX_LOC: usize = 500;

#[test]
fn mcp_handler_files_under_500_loc() {
    let dir = Path::new(HANDLERS_DIR);
    assert!(
        dir.is_dir(),
        "src/mcp/handlers must be a directory (not a single file)"
    );

    let mut violations = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read handlers dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().map(|e| e == "rs").unwrap_or(false) {
            let content = std::fs::read_to_string(&path).expect("read file");
            let loc = content.lines().count();
            if loc > MAX_LOC {
                violations.push(format!(
                    "{}: {} LOC (max {})",
                    path.display(),
                    loc,
                    MAX_LOC
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "MCP handler files exceed {} LOC limit:\n{}",
        MAX_LOC,
        violations.join("\n")
    );
}
