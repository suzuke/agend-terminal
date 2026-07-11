//! #2454 source invariant: the MCP `health` write handlers `handle_report_health`
//! and `handle_clear_blocked_reason` must not contain `crate::api::call` — the
//! blocked-reason write was migrated off the self-IPC loopback to the in-process
//! `agent_ops` service. A `syn` AST walk scoped to the two named functions (the
//! file legitimately keeps other `api::call` sites); matches `call` exactly so
//! the cross-process `api::call_at` and `api::method::…` do not trip. Mirrors
//! `tests/review_mcp_dispatch_comms_4.rs`. RED if either handler calls `api::call`.

use std::path::PathBuf;
use syn::visit::{self, Visit};

fn parse_instance_metadata() -> syn::File {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/mcp/handlers/instance_metadata.rs");
    let src = std::fs::read_to_string(&p).expect("read src/mcp/handlers/instance_metadata.rs");
    syn::parse_file(&src).expect("parse src/mcp/handlers/instance_metadata.rs")
}

fn find_fn<'a>(file: &'a syn::File, name: &str) -> &'a syn::ItemFn {
    file.items
        .iter()
        .find_map(|it| match it {
            syn::Item::Fn(f) if f.sig.ident == name => Some(f),
            _ => None,
        })
        .unwrap_or_else(|| panic!("fn {name} not found in instance_metadata.rs"))
}

/// Finds any path whose consecutive segments are `api :: call` — i.e.
/// `crate::api::call` / `api::call`. Deliberately matches `call` EXACTLY so the
/// legitimate cross-process `api::call_at` (a different segment) does not trip,
/// and `crate::api::method::…` (segments `api`,`method`,…) does not either.
#[derive(Default)]
struct ApiCallFinder {
    found: bool,
}
impl<'ast> Visit<'ast> for ApiCallFinder {
    fn visit_path(&mut self, p: &'ast syn::Path) {
        let segs: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
        if segs.windows(2).any(|w| w[0] == "api" && w[1] == "call") {
            self.found = true;
        }
        visit::visit_path(self, p);
    }
}

#[test]
fn health_mcp_write_handlers_have_no_api_call_self_ipc_2454() {
    let file = parse_instance_metadata();
    for name in ["handle_report_health", "handle_clear_blocked_reason"] {
        let f = find_fn(&file, name);
        let mut finder = ApiCallFinder::default();
        finder.visit_item_fn(f);
        assert!(
            !finder.found,
            "#2454: `{name}` must NOT call `crate::api::call` — the blocked-reason write path was \
             migrated off the MCP→API self-IPC loopback to the in-process \
             `crate::agent_ops::{{set,clear}}_blocked_reason` service (called via the forwarded \
             RuntimeContext). A `crate::api::call` here reintroduces the self-IPC deadlock/backstop \
             exposure this slice removed."
        );
    }
}
