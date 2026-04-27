//! Sprint 25 P0 — anti-bypass invariant: MCP channel ops route through
//! daemon API only.
//!
//! **Enforcement**: no production file in `src/mcp/` may call
//! `crate::channel::active_channel()` directly for channel operations.
//! All channel ops MUST route through `proxy_channel_op` (which
//! short-circuits in-process when running inside daemon, or relays via
//! daemon API when MCP subprocess).
//!
//! **Scope**: ALL `src/**/*.rs` — the most conservative scope per
//! operator decision (Sprint 25 P0 design question #4).
//!
//! **EXEMPTED_CALLERS**: the `proxy_channel_op` helper in
//! `mcp/handlers.rs` is the ONLY MCP-side code allowed to call
//! `active_channel()`, and only inside the `is_running_inside_daemon_process()`
//! guard. All other MCP code must use `proxy_channel_op`.

use std::path::{Path, PathBuf};

/// Files in `src/mcp/` that are allowed to contain `active_channel()`.
/// The proxy helper's in-process short-circuit is the only legitimate
/// call site — it's guarded by `is_running_inside_daemon_process()`.
const EXEMPTED_MCP_FILES: &[&str] = &[
    // proxy_channel_op helper's in-process short-circuit
    "mcp/handlers.rs",
];

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

fn rel_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_mcp_file(rel: &str) -> bool {
    rel.starts_with("mcp/")
}

fn is_exempted(rel: &str) -> bool {
    EXEMPTED_MCP_FILES.iter().any(|s| rel.ends_with(s))
}

/// Sprint 25 P0 anti-bypass — no MCP file may call `active_channel()`
/// directly except the exempted proxy helper. This enforces the
/// single-path architecture: all MCP channel ops route through
/// `proxy_channel_op` → daemon API.
#[test]
fn mcp_files_do_not_call_active_channel_directly() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&src_root, &mut files);

    let mut violations: Vec<String> = Vec::new();

    for path in &files {
        let rel = rel_path(path, &src_root);
        if !is_mcp_file(&rel) || is_exempted(&rel) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        // Only scan production code (cut off at #[cfg(test)]).
        let cutoff = content.find("#[cfg(test)]").unwrap_or(content.len());
        let prod = &content[..cutoff];
        for (idx, line) in prod.lines().enumerate() {
            let trim = line.trim_start();
            if trim.starts_with("//") || trim.starts_with("///") || trim.starts_with("//!") {
                continue;
            }
            if line.contains("active_channel()") {
                violations.push(format!(
                    "  src/{}:{}: direct active_channel() call in MCP code\n      line: {}",
                    rel,
                    idx + 1,
                    trim
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Sprint 25 P0 anti-bypass invariant: {} MCP file(s) call active_channel() directly.\n\n\
         Fix: use proxy_channel_op() which routes through the daemon API \
         (cross-process) or short-circuits in-process when running inside daemon.\n\n\
         Do NOT add to EXEMPTED_MCP_FILES without explicit scope decision.\n\n\
         Violations:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// Verify the exempted file (`mcp/handlers.rs`) only calls
/// `active_channel()` inside the `is_running_inside_daemon_process()`
/// guard — not as a bare call.
#[test]
fn exempted_mcp_handler_active_channel_is_guarded() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let path = src_root.join("mcp/handlers.rs");
    let content = std::fs::read_to_string(&path).expect("mcp/handlers.rs must exist");

    // Find the proxy_channel_op function body.
    let fn_start = content
        .find("fn proxy_channel_op(")
        .expect("proxy_channel_op must exist");
    // Scan forward to find the function's closing brace (heuristic: next
    // top-level `fn ` or `pub fn `).
    let rest = &content[fn_start..];
    let fn_end = rest[1..]
        .find("\nfn ")
        .or_else(|| rest[1..].find("\npub fn "))
        .map(|e| fn_start + 1 + e)
        .unwrap_or(content.len());
    let fn_body = &content[fn_start..fn_end];

    assert!(
        fn_body.contains("is_running_inside_daemon_process()"),
        "proxy_channel_op must check is_running_inside_daemon_process() before calling active_channel()"
    );

    // Count active_channel() calls in the entire production section of
    // mcp/handlers.rs (before #[cfg(test)]). Should be exactly 1 (inside
    // proxy_channel_op's in-process short-circuit).
    let cutoff = content.find("#[cfg(test)]").unwrap_or(content.len());
    let prod = &content[..cutoff];
    let count = prod
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("///") && l.contains("active_channel()")
        })
        .count();
    assert_eq!(
        count, 1,
        "mcp/handlers.rs production code should have exactly 1 active_channel() call \
         (inside proxy_channel_op's in-process guard), found {count}"
    );
}

/// Verify that `proxy_channel_op` daemon API handler exists and calls
/// `send_from_agent` — the shared gate that enforces
/// `outbound_capabilities`.
#[test]
fn daemon_proxy_handler_uses_send_from_agent() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let path = src_root.join("api/handlers/channel_op.rs");
    let content = std::fs::read_to_string(&path).expect("channel_op.rs must exist");

    assert!(
        content.contains("send_from_agent"),
        "daemon proxy_channel_op handler must dispatch through Channel::send_from_agent \
         (which calls gate_outbound_for_agent for the per-instance capability gate)"
    );
    assert!(
        content.contains("active_channel()"),
        "daemon proxy_channel_op handler must look up active_channel() \
         (it runs inside daemon process where ACTIVE_CHANNEL is registered)"
    );
}
