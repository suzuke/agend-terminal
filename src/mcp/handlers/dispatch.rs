//! MCP tool dispatch table — first cut of #694 BLOCK 2.
//!
//! `handle_tool` (in `mod.rs`) historically routed 30+ MCP tools through
//! a 143-line `match` literal. This module introduces a linear-scan
//! dispatch table so tools can register their handlers as data instead
//! of as match arms. Adding a tool becomes "append an entry"; un-
//! migrated tools fall through to the (shrinking) inline match.
//!
//! **Signature design** — the 30+ arms have at least four distinct
//! handler shapes (`(home, args, instance)`, `(home, args)`,
//! `(home, args, sender)`, `(home, args, instance, sender)`). Rather
//! than commit to one shape now and migrate only the arms that fit,
//! this module uses a single uniform [`HandlerFn`] keyed on a
//! [`HandlerCtx`] struct that bundles every common parameter. Each
//! migrated tool gets a tiny adapter fn that pulls the fields it needs
//! out of `HandlerCtx`. The same table will accommodate `&sender`-style
//! arms in T-B8+ without changing the type.
//!
//! **Linear scan** — `<10ns` for 30 entries vs `~50ns` allocator hit
//! for HashMap/phf, and the table size is bounded by the MCP tool
//! catalogue, so static-search is cheap and avoids the deps. Per
//! T-B7 dispatch contract: "Linear scan over 30 entries is fine."
//!
//! **Fallback in mod.rs** — [`try_dispatch`] returns `Option<Value>`
//! (`None` = "tool name not in table"). `handle_tool` falls back to
//! the existing inline match for un-migrated arms; the catch-all
//! `unknown tool` branch in that match still handles fully-unknown
//! names. This keeps the migration as a pure subtraction from the
//! inline match in `mod.rs`, no inline-match relocation.

use crate::identity::Sender;
use serde_json::Value;
use std::path::Path;

use super::instance;

/// Shared per-call context — every common parameter `handle_tool`
/// would otherwise pass into the match arms, bundled together so each
/// [`HandlerFn`] has a single uniform shape.
pub(super) struct HandlerCtx<'a> {
    pub home: &'a Path,
    pub args: &'a Value,
    pub instance_name: &'a str,
    #[allow(dead_code)] // used by T-B8+ migrations (`&sender`-style arms)
    pub sender: &'a Option<Sender>,
}

/// One MCP tool's dispatcher. Function pointer (not `Box<dyn …>`) so
/// the slice in [`registered_handlers`] is `const`-friendly and
/// allocation-free.
pub(super) type HandlerFn = fn(&HandlerCtx<'_>) -> Value;

pub(super) struct HandlerEntry {
    pub name: &'static str,
    pub handler: HandlerFn,
}

/// Look the `tool` name up in the dispatch table. Returns `Some(value)`
/// on hit; returns `None` if the tool isn't registered — the caller
/// falls back to the inline `match` in `mod.rs` for un-migrated arms.
pub(super) fn try_dispatch(tool: &str, ctx: &HandlerCtx<'_>) -> Option<Value> {
    for entry in registered_handlers() {
        if entry.name == tool {
            return Some((entry.handler)(ctx));
        }
    }
    None
}

// ---------------------------------------------------------------------
// Adapter fns — one per migrated tool. Each pulls the fields its
// underlying handler needs out of `HandlerCtx` and forwards the call.
// ---------------------------------------------------------------------

// Shape 1 — takes `instance_name`.

fn dispatch_list_instances(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_list_instances(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_create_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_create_instance(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_set_description(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_set_description(ctx.home, ctx.args, ctx.instance_name)
}

// Shape 2 — ignores `instance_name`.

fn dispatch_interrupt(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_interrupt(ctx.home, ctx.args)
}

fn dispatch_delete_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_delete_instance(ctx.home, ctx.args)
}

fn dispatch_start_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_start_instance(ctx.home, ctx.args)
}

fn dispatch_replace_instance(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_replace_instance(ctx.home, ctx.args)
}

fn dispatch_move_pane(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_move_pane(ctx.home, ctx.args)
}

/// Registered tool dispatchers. **Adding a tool**: write its adapter
/// above, append a [`HandlerEntry`] here. **Removing**: delete both
/// halves. Order doesn't affect correctness (linear-scan match by
/// `name`), but keeping similar tools clustered helps grep.
pub(super) fn registered_handlers() -> &'static [HandlerEntry] {
    &[
        // Instance lifecycle — shape 1
        HandlerEntry {
            name: "list_instances",
            handler: dispatch_list_instances,
        },
        HandlerEntry {
            name: "create_instance",
            handler: dispatch_create_instance,
        },
        HandlerEntry {
            name: "set_description",
            handler: dispatch_set_description,
        },
        // Instance lifecycle — shape 2
        HandlerEntry {
            name: "interrupt",
            handler: dispatch_interrupt,
        },
        HandlerEntry {
            name: "delete_instance",
            handler: dispatch_delete_instance,
        },
        HandlerEntry {
            name: "start_instance",
            handler: dispatch_start_instance,
        },
        HandlerEntry {
            name: "replace_instance",
            handler: dispatch_replace_instance,
        },
        HandlerEntry {
            name: "move_pane",
            handler: dispatch_move_pane,
        },
    ]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx_for<'a>(home: &'a Path, args: &'a Value, instance: &'a str) -> HandlerCtx<'a> {
        // Sender field is only read by T-B8+ adapters; static None is
        // fine for this test scaffold.
        static EMPTY_SENDER: Option<Sender> = None;
        HandlerCtx {
            home,
            args,
            instance_name: instance,
            sender: &EMPTY_SENDER,
        }
    }

    /// Unknown tool name → `None`, so `handle_tool` falls back to the
    /// inline match (which has its own `unknown tool` catch-all).
    #[test]
    fn try_dispatch_returns_none_for_unregistered_tool() {
        let home = std::env::temp_dir();
        let args = json!({});
        let ctx = ctx_for(&home, &args, "");
        assert!(try_dispatch("definitely_not_a_real_tool", &ctx).is_none());
    }

    /// Registered tool name → `Some(_)` — proves the table actually
    /// routes through to the adapter. Content of the Value depends on
    /// the underlying handler; we only assert it's not `None`.
    #[test]
    fn try_dispatch_returns_some_for_registered_tool() {
        let home = std::env::temp_dir();
        let args = json!({});
        let ctx = ctx_for(&home, &args, "");
        // `list_instances` is registered as of T-B7 first cut.
        assert!(try_dispatch("list_instances", &ctx).is_some());
    }

    /// Regression guard: pin the expected set of registered tool names
    /// for T-B7. Future PRs that migrate more arms will update this
    /// list; an accidental rename / removal trips the test.
    #[test]
    fn registered_handler_names_pin() {
        let names: Vec<&'static str> = registered_handlers().iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            vec![
                "list_instances",
                "create_instance",
                "set_description",
                "interrupt",
                "delete_instance",
                "start_instance",
                "replace_instance",
                "move_pane",
            ]
        );
        assert_eq!(registered_handlers().len(), 8);
    }

    /// Coverage test: every tool name advertised by
    /// [`crate::mcp::tools::tool_definitions`] must be routed somewhere
    /// — either by the dispatch table (`dispatch.rs`) or by the
    /// fallback inline `match` (`mod.rs`). Catches the bug class
    /// "tool added to the catalogue but routing forgotten".
    ///
    /// Implemented as a **static source-grep** rather than calling
    /// `handle_tool` per name: a handler call would invoke real
    /// side-effectful code paths (filesystem, channel, daemon API)
    /// against the dev's environment. The source-grep is fast,
    /// deterministic, and has no side effects. False-positive risk
    /// (a tool name appearing in a doc comment but no real routing)
    /// is low because match arms and `HandlerEntry { name: "..." }`
    /// are the only places where a quoted bare tool name typically
    /// appears in these files.
    #[test]
    fn every_advertised_tool_is_routed_somewhere() {
        let defs = crate::mcp::tools::tool_definitions();
        // `tool_definitions()` returns `{"tools": [...]}`.
        let arr = defs
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tool_definitions() should return {tools: [...]}");
        let names: Vec<&str> = arr
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        let mod_rs = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/mcp/handlers/mod.rs"
        ))
        .expect("read mod.rs");
        let dispatch_rs = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/mcp/handlers/dispatch.rs"
        ))
        .expect("read dispatch.rs");
        let mut missing: Vec<&str> = Vec::new();
        for name in &names {
            let quoted = format!("\"{name}\"");
            if !mod_rs.contains(&quoted) && !dispatch_rs.contains(&quoted) {
                missing.push(name);
            }
        }
        assert!(
            missing.is_empty(),
            "tools advertised by tool_definitions() but not routed in mod.rs or dispatch.rs: {missing:?}"
        );
    }
}
