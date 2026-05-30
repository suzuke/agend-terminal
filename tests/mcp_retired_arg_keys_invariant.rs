//! #1502 invariant (Part A — retired-name denylist): no MCP handler may read a
//! tool argument under a RETIRED key name. When a tool's schema renames a
//! parameter (e.g. #1461 `target_instance` → `instance`, #1484 `source_repo` →
//! `repository_path`), the handler's `args["..."]` read must be renamed in
//! lockstep. The failure class this closes is silent schema↔handler drift: the
//! schema advertises the new name, the agent sends the new name, but a handler
//! (often in a shared helper fn, like #1484's `force_release` reading
//! `args["source_repo"]`) still reads the OLD name and silently gets `null` —
//! no compile error, no test failure, just a feature that quietly stops working.
//!
//! Scope (per the #1502 approach sketch, scope option iii): this is the
//! zero-false-positive, zero-gap half. It scans for EXACT retired arg-read
//! patterns only — `args["KEY"]` / `args.get("KEY")` — so it never matches
//! binding-file value reads (`v["source_repo"]`, a legitimate binding.json
//! field in a different namespace) or json! response/construction literals
//! (`json!({"source": ...})`). The complementary "every read has a schema
//! declaration" half (Invariant B + the 8 real schema gaps it surfaces) ships
//! in a separate follow-up PR.
//!
//! RED proof: re-introduce `let _ = args["source_repo"].as_str();` in any
//! `src/mcp/**` non-comment line → this test fails naming the file:line and the
//! current replacement key.

use std::path::{Path, PathBuf};

/// Retired arg-key name → the current key that replaced it. Each was verified to
/// have ZERO live `args[...]` reads at the time of writing; this test keeps them
/// dead. Add a row whenever a tool-arg parameter is renamed.
const RETIRED_ARG_KEYS: &[(&str, &str)] = &[
    ("target_instance", "instance / instances"), // #1461 send unification
    ("source_repo", "repository_path"),          // #1484 cross-tool standard name
    ("source", "repository_path"),               // checkout legacy source arg
    ("repo_slug", "repository"),                 // GitHub slug standard name
];

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs_files(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

#[test]
fn mcp_handlers_must_not_read_retired_arg_keys() {
    // The args-reading surface is the MCP handler/tool layer.
    let mcp_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/mcp");
    let mut files = Vec::new();
    collect_rs_files(&mcp_dir, &mut files);
    assert!(!files.is_empty(), "no .rs files found under src/mcp/");

    let mut violations = Vec::new();
    for f in &files {
        let Ok(content) = std::fs::read_to_string(f) else {
            continue;
        };
        for (idx, line) in content.lines().enumerate() {
            // Skip comment/doc lines that merely mention a retired key (e.g. a
            // migration note documenting the rename).
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with("*") {
                continue;
            }
            for (old, new) in RETIRED_ARG_KEYS {
                // Match ONLY the arg-READ patterns — not response/binding-value
                // literals. The closing `"]` / `")` makes `args["source"]`
                // distinct from `args["source_repo"]` (no substring overlap).
                let index_form = format!("args[\"{old}\"]");
                let get_form = format!("args.get(\"{old}\")");
                if line.contains(&index_form) || line.contains(&get_form) {
                    violations.push(format!(
                        "{}:{}: reads retired arg key `{old}` (use `{new}`): {}",
                        f.display(),
                        idx + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "#1502: MCP handler reads a RETIRED tool-arg key — rename the `args[...]` \
         read to match the current schema (silent schema↔handler drift; the agent \
         sends the new name and this read silently gets null):\n{}",
        violations.join("\n")
    );
}
