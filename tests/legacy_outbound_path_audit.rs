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
    // Sprint 23 P1 r2 — normalize Windows backslash to forward-slash for
    // cross-platform EXEMPTED-list suffix-match. See PR #240 r2.
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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

/// Sprint 22 P0 reviewer-2 must-have + Sprint 22 P2a M1 fold-in — MCP
/// instance-mutating handlers AND their dispatch chain must NOT accept
/// `outbound_capabilities` from agent-API args. Operator grants happen
/// via fleet.yaml on disk; agent-API grants would defeat the per-instance
/// capability gate.
///
/// Sprint 22 P0 dev-reviewer M1 finding (NON-BLOCKING): the original
/// scope of this test only covered `src/mcp/handlers.rs`, missing the
/// `deploy_template` arm at `handlers.rs:1201` which dispatches to
/// `crate::deployments::deploy(...)`. Today's `src/deployments.rs` has
/// 0 production hits (manually verified Sprint 22 P0 review), but a
/// future regression there would not trip this test.
///
/// Sprint 22 P2a (this PR) extends the scan to cover the dispatch chain
/// from each instance-mutating MCP arm — closes the M1 invariant gap.
#[test]
fn mcp_handlers_do_not_accept_outbound_capabilities_arg() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    // Sprint 22 P2a M1 fold-in: extend scan to every file in the dispatch
    // chain from instance-mutating MCP handlers. New entries here as the
    // chain grows (e.g. future MCP handlers that dispatch into new
    // sub-modules with instance-config mutation surfaces).
    let scanned_files: &[&str] = &[
        "mcp/handlers/mod.rs", // top-level MCP dispatch (Sprint 22 P0 original scope)
        "deployments.rs",      // `deploy_template` arm dispatches here (Sprint 22 P0 M1 finding)
    ];

    let mut violations: Vec<String> = Vec::new();
    for rel_suffix in scanned_files {
        let path = src_root.join(rel_suffix);
        let Ok(content) = std::fs::read_to_string(&path) else {
            // File missing — don't fail the test (allows the dispatch
            // chain to evolve). The scanned_files list is a manual audit
            // surface; missing files surface via PR review of this test.
            continue;
        };

        // Cut off at #[cfg(test)] so unit-test fixtures don't trip.
        let cutoff_byte = content.find("#[cfg(test)]").unwrap_or(content.len());
        let prod = &content[..cutoff_byte];

        for (idx, line) in prod.lines().enumerate() {
            let trim = line.trim_start();
            if trim.starts_with("//") || trim.starts_with("///") {
                continue;
            }
            let lowered = line.to_ascii_lowercase();
            // Match: `args["outbound_capabilities"]` / `args.get("outbound_capabilities")`
            // / any direct extraction of the field from the MCP args bag
            // OR from a downstream handler's args param.
            if (lowered.contains("args[\"outbound_capabilities\"")
                || lowered.contains("args.get(\"outbound_capabilities\")"))
                && !lowered.contains("// allowed:")
            {
                violations.push(format!(
                    "  src/{}:{}: handler extracts `outbound_capabilities` from agent args — defeats per-instance capability gate\n      offending line: {}",
                    rel_suffix,
                    idx + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Sprint 22 P0 reviewer-2 must-have + P2a M1 fold-in violations — {} handler(s) accept `outbound_capabilities` from agent args across MCP dispatch chain.\n\nFix: remove the args extraction. Operator grants outbound capabilities ONLY via fleet.yaml on disk. Agent-API grants defeat the per-instance gate.\n\nIf this is intentional (operator-only caller), add `// allowed: <reason>` inline comment for documented exemption.\n\nIf a NEW dispatch-chain file is added (another sub-module that mutates instance config), extend the `scanned_files` list in this test.\n\nViolations:\n{}",
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
