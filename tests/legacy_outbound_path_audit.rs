//! Sprint 22 P0 (Phase 5b hard-cut) — anti-bypass invariant tests.
//!
//! **Two enforcement layers**:
//!
//! 1. **Anti-bypass invariant** (Phase 5 spawn-rationale audit pattern): no
//!    production file outside `src/channel/telegram.rs` may call the legacy
//!    free fns `try_telegram_reply` / `try_telegram_reply_no_cleanup` /
//!    `try_telegram_react` / `try_telegram_edit`. All agent-callable
//!    outbound MUST route through `Channel::send_from_agent` (which calls
//!    `auth::gate_outbound_for_agent` per Sprint 22 P0 helper extraction).
//!
//! 2. **MCP tool surface sanitization** (reviewer-2 high signal): the four
//!    instance-mutating MCP handlers (`create_instance`, `update_instance`,
//!    `replace_instance`, `deploy_template`) MUST NOT accept an
//!    `outbound_capabilities` argument from agent callers. Operator-only
//!    grants happen via fleet.yaml on disk; agent-API grants would defeat
//!    the per-instance capability gate.
//!
//! **Anti-growth contract** (Phase 5 EXEMPTED_LEGACY_FILES pattern): the
//! `EXEMPTED_LEGACY_CALL_SITES` list is empty by intent. Adding entries
//! requires explicit dispatch scope — the legacy paths are scheduled for
//! true removal (not relegation) in Sprint 23.

use std::path::{Path, PathBuf};

/// Files allowed to call `try_telegram_*` legacy free fns. Empty by intent
/// — the implementation is in `src/channel/telegram.rs` and wraps its own
/// internal callers (Channel::send_from_agent dispatcher + ux_event sink
/// dispatcher). Any other file calling these is a bypass attempt.
///
/// Adding entries here requires explicit dispatch scope per
/// d-20260427042738203707-13. Sprint 23 will remove the fns entirely
/// after the 2-stage transition window closes.
const EXEMPTED_LEGACY_CALL_SITES: &[&str] = &[
    // Telegram impl itself + its internal subscriber dispatcher.
    "channel/telegram.rs",
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
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn is_exempted(rel_path: &str) -> bool {
    EXEMPTED_LEGACY_CALL_SITES
        .iter()
        .any(|suffix| rel_path.ends_with(suffix))
}

fn is_legacy_call_line(line: &str) -> bool {
    let trim = line.trim_start();
    if trim.starts_with("//") || trim.starts_with("///") || trim.starts_with("//!") {
        return false;
    }
    // Match call patterns. The fn names are unique enough that substring
    // match doesn't false-positive on unrelated identifiers.
    line.contains("try_telegram_reply(")
        || line.contains("try_telegram_reply_no_cleanup(")
        || line.contains("try_telegram_react(")
        || line.contains("try_telegram_edit(")
}

/// Sprint 22 P0 anti-bypass — no production file outside `telegram.rs`
/// may call the legacy free fns. All agent-callable outbound routes
/// through `Channel::send_from_agent`.
#[test]
fn legacy_try_telegram_fns_unreachable_outside_telegram_impl() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations: Vec<String> = Vec::new();

    for path in rust_files_in_src() {
        let rel = rel_path_str(&path, &src_root);
        if is_exempted(&rel) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Cut off at first `#[cfg(test)]` so test fixtures can reference
        // these fns under cfg(test) without tripping the invariant.
        let cutoff_byte = content.find("#[cfg(test)]").unwrap_or(content.len());
        let prod_section = &content[..cutoff_byte];
        for (idx, line) in prod_section.lines().enumerate() {
            if !is_legacy_call_line(line) {
                continue;
            }
            violations.push(format!(
                "  {}:{}: legacy call outside Channel::send_from_agent dispatch\n      offending line: {}",
                rel,
                idx + 1,
                line.trim()
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "Sprint 22 P0 Phase 5b hard-cut anti-bypass invariant violations — {} call site(s) reach legacy try_telegram_* fns outside the telegram.rs implementation.\n\nFix: route through `Channel::send_from_agent` (which calls `auth::gate_outbound_for_agent` for the per-instance capability gate). Direct calls bypass the per-agent outbound_capabilities gate.\n\nDo NOT add to EXEMPTED_LEGACY_CALL_SITES without explicit dispatch scope per d-20260427042738203707-13.\n\nViolations:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// Sprint 22 P0 reviewer-2 must-have — MCP instance-mutating handlers
/// must NOT accept `outbound_capabilities` from agent-API args. Operator
/// grants happen via fleet.yaml on disk; agent-API grants would defeat
/// the per-instance capability gate.
///
/// Grep-based check: scan `src/mcp/handlers.rs` for any reference to
/// `args["outbound_capabilities"]` or `args.get("outbound_capabilities")`
/// inside the four instance-mutating handler arms. Today: 0 hits expected.
/// Future regression (someone wires through the arg): test fails-loud.
#[test]
fn mcp_handlers_do_not_accept_outbound_capabilities_arg() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let handlers_path = src_root.join("mcp/handlers.rs");
    let content = std::fs::read_to_string(&handlers_path).expect("src/mcp/handlers.rs must exist");

    // Cut off at #[cfg(test)] so unit-test fixtures don't trip.
    let cutoff_byte = content.find("#[cfg(test)]").unwrap_or(content.len());
    let prod = &content[..cutoff_byte];

    let mut violations: Vec<String> = Vec::new();
    for (idx, line) in prod.lines().enumerate() {
        let trim = line.trim_start();
        if trim.starts_with("//") || trim.starts_with("///") {
            continue;
        }
        let lowered = line.to_ascii_lowercase();
        // Match: `args["outbound_capabilities"]` / `args.get("outbound_capabilities")`
        // / any direct extraction of the field from the MCP args bag.
        if (lowered.contains("args[\"outbound_capabilities\"")
            || lowered.contains("args.get(\"outbound_capabilities\")"))
            && !lowered.contains("// allowed:")
        {
            violations.push(format!(
                "  src/mcp/handlers.rs:{}: MCP handler extracts `outbound_capabilities` from agent args — defeats per-instance capability gate\n      offending line: {}",
                idx + 1,
                line.trim()
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "Sprint 22 P0 reviewer-2 must-have violations — {} MCP handler(s) accept `outbound_capabilities` from agent args.\n\nFix: remove the args extraction. Operator grants outbound capabilities ONLY via fleet.yaml on disk. Agent-API grants defeat the per-instance gate.\n\nIf this is intentional (operator-only caller), add `// allowed: <reason>` inline comment for documented exemption.\n\nViolations:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// Sanity check: `Channel::send_from_agent` impl in telegram.rs must call
/// the shared `gate_outbound_for_agent` helper. If this test fails, the
/// impl reverted to inline gate logic and future Discord/Slack adapters
/// won't inherit the centralisation guarantee.
#[test]
fn telegram_send_from_agent_uses_shared_gate_helper() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let path = src_root.join("channel/telegram.rs");
    let content = std::fs::read_to_string(&path).expect("telegram.rs must exist");

    // Find the send_from_agent impl block + check it calls gate_outbound_for_agent.
    let impl_start = content
        .find("fn send_from_agent(")
        .expect("send_from_agent impl must exist");
    // Bounded scan — 80 lines forward should cover the helper call.
    let body_end = content[impl_start..]
        .find("\n    }\n")
        .map(|e| impl_start + e)
        .unwrap_or(content.len());
    let body = &content[impl_start..body_end];

    assert!(
        body.contains("gate_outbound_for_agent"),
        "telegram.rs send_from_agent must call `auth::gate_outbound_for_agent` shared helper (Sprint 22 P0). Inline gate logic reverts the centralisation that future Discord/Slack adapters depend on."
    );
}
