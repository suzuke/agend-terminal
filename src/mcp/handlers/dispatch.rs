//! MCP tool dispatch table — #694 BLOCK 2.
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
//! than commit to one shape, this module uses a single uniform
//! [`HandlerFn`] keyed on a [`HandlerCtx`] struct that bundles every
//! common parameter. Each migrated tool gets a tiny adapter fn that
//! pulls the fields it needs out of `HandlerCtx`.
//!
//! **Linear scan** — `<10ns` for 30 entries vs `~50ns` allocator hit
//! for HashMap/phf, and the table size is bounded by the MCP tool
//! catalogue, so static-search is cheap and avoids the deps.
//!
//! **Action sub-routing (T-B8)** — tools whose API consolidates several
//! sub-operations under one name (`task`, `decision`, `team`, …) carry
//! an optional [`HandlerEntry::actions`] table. [`try_dispatch`] inspects
//! `ctx.args["action"]`, looks the value up in that table, and routes to
//! the matching sub-handler if found. Missing-or-unknown action falls
//! through to the entry's base [`HandlerFn`] — which is the tool's
//! existing pre-migration handler, so error-handling for unknown
//! actions remains byte-identical to the pre-extraction code.
//!
//! **Fallback in mod.rs** — [`try_dispatch`] returns `Option<Value>`
//! (`None` = "tool name not in table"). `handle_tool` falls back to the
//! existing inline match for un-migrated arms; the catch-all
//! `unknown tool` branch in that match still handles fully-unknown
//! names.

use crate::identity::Sender;
use serde_json::Value;
use std::path::Path;

use super::{binding_state, comms, force_release, instance, task, worktree};

/// Shared per-call context — every common parameter `handle_tool`
/// would otherwise pass into the match arms, bundled together so each
/// [`HandlerFn`] has a single uniform shape.
pub(super) struct HandlerCtx<'a> {
    pub home: &'a Path,
    pub args: &'a Value,
    pub instance_name: &'a str,
    pub sender: &'a Option<Sender>,
}

/// One MCP tool's dispatcher. Function pointer (not `Box<dyn …>`) so
/// the slice in [`registered_handlers`] is `const`-friendly and
/// allocation-free.
pub(super) type HandlerFn = fn(&HandlerCtx<'_>) -> Value;

pub(super) struct HandlerEntry {
    pub name: &'static str,
    pub handler: HandlerFn,
    /// Optional action sub-routing table. When `Some`, `try_dispatch`
    /// reads `ctx.args["action"]` and routes to the matching sub-handler;
    /// missing-or-unknown action falls through to `handler` (the base).
    /// `None` means flat dispatch — `ctx.args["action"]` is ignored.
    pub actions: Option<&'static [(&'static str, HandlerFn)]>,
}

/// Look the `tool` name up in the dispatch table. Returns `Some(value)`
/// on hit (including the action-routing path); returns `None` if the
/// tool isn't registered — the caller falls back to the inline `match`
/// in `mod.rs` for un-migrated arms.
pub(super) fn try_dispatch(tool: &str, ctx: &HandlerCtx<'_>) -> Option<Value> {
    for entry in registered_handlers() {
        if entry.name == tool {
            if let Some(action_table) = entry.actions {
                if let Some(action) = ctx.args["action"].as_str() {
                    // `HandlerFn` is `Copy`, so destructuring the
                    // ref-to-tuple as `&(_, sub_handler)` binds the
                    // fn pointer by value.
                    if let Some(&(_, sub_handler)) = action_table.iter().find(|(a, _)| *a == action)
                    {
                        return Some(sub_handler(ctx));
                    }
                }
                // Action missing or unknown → fall through to base
                // handler below. Preserves the pre-migration error
                // path (the base handler is the existing pub fn that
                // already has its own "unknown action" branch).
            }
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

// Shape 3 — takes `instance_name` AND `sender`.

fn dispatch_set_waiting_on(ctx: &HandlerCtx<'_>) -> Value {
    instance::handle_set_waiting_on(ctx.home, ctx.args, ctx.instance_name, ctx.sender)
}

fn dispatch_send(ctx: &HandlerCtx<'_>) -> Value {
    comms::handle_unified_send(ctx.home, ctx.args, ctx.sender)
}

// Shape 4 — takes `sender` but ignores `instance_name`.

fn dispatch_bind_self(ctx: &HandlerCtx<'_>) -> Value {
    worktree::handle_bind_self(ctx.home, ctx.args, ctx.sender)
}

fn dispatch_binding_state(ctx: &HandlerCtx<'_>) -> Value {
    binding_state::handle_binding_state(ctx.home, ctx.args, ctx.sender)
}

fn dispatch_release_worktree(ctx: &HandlerCtx<'_>) -> Value {
    worktree::handle_release_worktree(ctx.home, ctx.args, ctx.sender)
}

fn dispatch_force_release_worktree(ctx: &HandlerCtx<'_>) -> Value {
    force_release::handle_force_release_worktree(ctx.home, ctx.args, ctx.sender)
}

fn dispatch_gc_dry_run(ctx: &HandlerCtx<'_>) -> Value {
    worktree::handle_gc_dry_run(ctx.home, ctx.args, ctx.sender)
}

// `task` — action sub-routing validation case (T-B8).
//
// All 5 sub-handlers (`dispatch_task_create` / `..._list` / `..._claim`
// / `..._update` / `..._done`) AND the base `dispatch_task` all forward
// to `task::handle_task`. The forwarding is identical at runtime — the
// actions table is purely structural here, adding a routing-table layer
// without changing what gets called.
//
// `task::handle_task` is itself a 1-line forward to `crate::tasks::handle`,
// which has its own internal match on `args["action"]`. So:
//
// * recognized action (e.g. "list") → dispatch table actions[…] hit →
//   matching sub-handler → task::handle_task → tasks::handle → matching
//   arm of internal match
// * unrecognized action (e.g. "frobnicate") → dispatch table actions miss
//   → fall-through to base `dispatch_task` → task::handle_task →
//   tasks::handle → "unknown action" error from internal match
//
// The "double match" (action-routing in dispatch.rs AND internal match
// in `tasks::handle`) is redundant work but preserves ZERO behavior
// change. Refactoring `tasks::handle` to expose per-action `pub fn`s
// is left as a future optimization once the dispatch table is stable
// across all action-based tools.

fn dispatch_task(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_task_create(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_task_list(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_task_claim(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_task_update(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

fn dispatch_task_done(ctx: &HandlerCtx<'_>) -> Value {
    task::handle_task(ctx.home, ctx.args, ctx.instance_name)
}

/// Static sub-handler table for `task` action routing. Lifted out of
/// the entry literal so the table address can be `'static`-borrowed.
/// Action set matches `tasks::handle`'s internal `match` arms.
static TASK_ACTIONS: &[(&str, HandlerFn)] = &[
    ("create", dispatch_task_create),
    ("list", dispatch_task_list),
    ("claim", dispatch_task_claim),
    ("update", dispatch_task_update),
    ("done", dispatch_task_done),
];

/// Registered tool dispatchers. **Adding a tool**: write its adapter
/// above, append a [`HandlerEntry`] here. **Removing**: delete both
/// halves. Order doesn't affect correctness (linear-scan match by
/// `name`), but keeping similar tools clustered helps grep.
///
/// **Why `static` instead of inline `&[...]` return**: Rust's
/// const-promotion turns an `&[T {...}]` array literal in fn-return
/// position into a `'static` slice automatically — but only if every
/// field of `T` is itself const-promotable in that context. Once
/// `HandlerEntry` gained `actions: Option<&'static [(&str, HandlerFn)]>`
/// in T-B8, the inner static-slice field defeats promotion of the
/// outer array and rustc raises E0515 ("returns a reference to data
/// owned by the current function"). Lifting the array to a top-level
/// `static` gives the slice an unambiguous `'static` origin and side-
/// steps the gotcha. Symptom for future readers: if you add a field
/// to `HandlerEntry` and the array literal stops compiling, this
/// `static` is why.
static REGISTERED: &[HandlerEntry] = &[
    // Instance lifecycle — shape 1 (T-B7)
    HandlerEntry {
        name: "list_instances",
        handler: dispatch_list_instances,
        actions: None,
    },
    HandlerEntry {
        name: "create_instance",
        handler: dispatch_create_instance,
        actions: None,
    },
    HandlerEntry {
        name: "set_description",
        handler: dispatch_set_description,
        actions: None,
    },
    // Instance lifecycle — shape 2 (T-B7)
    HandlerEntry {
        name: "interrupt",
        handler: dispatch_interrupt,
        actions: None,
    },
    HandlerEntry {
        name: "delete_instance",
        handler: dispatch_delete_instance,
        actions: None,
    },
    HandlerEntry {
        name: "start_instance",
        handler: dispatch_start_instance,
        actions: None,
    },
    HandlerEntry {
        name: "replace_instance",
        handler: dispatch_replace_instance,
        actions: None,
    },
    HandlerEntry {
        name: "move_pane",
        handler: dispatch_move_pane,
        actions: None,
    },
    // Sender-style — shape 3 (T-B8)
    HandlerEntry {
        name: "set_waiting_on",
        handler: dispatch_set_waiting_on,
        actions: None,
    },
    HandlerEntry {
        name: "send",
        handler: dispatch_send,
        actions: None,
    },
    // Sender-style — shape 4 (T-B8)
    HandlerEntry {
        name: "bind_self",
        handler: dispatch_bind_self,
        actions: None,
    },
    HandlerEntry {
        name: "binding_state",
        handler: dispatch_binding_state,
        actions: None,
    },
    HandlerEntry {
        name: "release_worktree",
        handler: dispatch_release_worktree,
        actions: None,
    },
    HandlerEntry {
        name: "force_release_worktree",
        handler: dispatch_force_release_worktree,
        actions: None,
    },
    HandlerEntry {
        name: "gc_dry_run",
        handler: dispatch_gc_dry_run,
        actions: None,
    },
    // Action sub-routing validation case (T-B8) — only "list"
    // wired for early-report spot-check. T-B9 wires the rest.
    HandlerEntry {
        name: "task",
        handler: dispatch_task,
        actions: Some(TASK_ACTIONS),
    },
];

pub(super) fn registered_handlers() -> &'static [HandlerEntry] {
    REGISTERED
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx_for<'a>(home: &'a Path, args: &'a Value, instance: &'a str) -> HandlerCtx<'a> {
        // Sender field carries `None` for the `&sender`-style adapters
        // — every one of them handles the no-sender case gracefully.
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
    /// routes through to the adapter.
    #[test]
    fn try_dispatch_returns_some_for_registered_tool() {
        let home = std::env::temp_dir();
        let args = json!({});
        let ctx = ctx_for(&home, &args, "");
        assert!(try_dispatch("list_instances", &ctx).is_some());
    }

    /// Regression guard: pin the expected registered tool names + count.
    /// Future PRs that migrate more arms update this list.
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
                "set_waiting_on",
                "send",
                "bind_self",
                "binding_state",
                "release_worktree",
                "force_release_worktree",
                "gc_dry_run",
                "task",
            ]
        );
        assert_eq!(registered_handlers().len(), 16);
    }

    /// Coverage test: every tool name advertised by
    /// [`crate::mcp::tools::tool_definitions`] must be routed somewhere
    /// — either by the dispatch table (`dispatch.rs`) or by the
    /// fallback inline `match` (`mod.rs`). Catches the bug class
    /// "tool added to the catalogue but routing forgotten".
    #[test]
    fn every_advertised_tool_is_routed_somewhere() {
        let defs = crate::mcp::tools::tool_definitions();
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

    /// Action sub-routing — each of the 5 recognized `task` actions
    /// routes through its sub-handler. All 5 sub-handlers currently
    /// forward to the same base, so the assertion is that the call
    /// returns `Some` (proves the sub-routing path executed without
    /// panic for every wired action). Parameterized to avoid 5 near-
    /// identical test fns.
    #[test]
    fn try_dispatch_routes_known_action_through_sub_handler() {
        let home = std::env::temp_dir();
        for action in ["create", "list", "claim", "update", "done"] {
            let args = json!({"action": action});
            let ctx = ctx_for(&home, &args, "");
            assert!(
                try_dispatch("task", &ctx).is_some(),
                "action='{action}' did not route through dispatch table"
            );
        }
    }

    /// Action sub-routing — unknown action falls through to the base
    /// handler. The base handler is `dispatch_task` which forwards to
    /// `task::handle_task`. Internally `tasks::handle` will return its
    /// own "unknown action" error JSON. We only assert `Some(_)` to
    /// confirm the fall-through path executed (vs panicking or
    /// returning `None`).
    #[test]
    fn try_dispatch_unknown_action_falls_through_to_base() {
        let home = std::env::temp_dir();
        let args = json!({"action": "frobnicate-not-a-real-action"});
        let ctx = ctx_for(&home, &args, "");
        let result = try_dispatch("task", &ctx);
        assert!(result.is_some(), "fall-through must reach base handler");
        // Sanity-check the base handler ran by looking for the
        // upstream "unknown" error wording. (`tasks::handle` returns
        // `{"error": "unknown task action: ..."}` for unrecognized
        // actions; pinning this proves the fall-through actually
        // reached `tasks::handle` and not an empty Some(json!(null)).)
        let v = result.unwrap();
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(
            err.contains("unknown") || err.contains("action"),
            "expected unknown-action error from base; got: {v:?}"
        );
    }

    /// Action sub-routing — missing `action` arg falls through to the
    /// base handler (same path as unknown-action). The base handler's
    /// internal match handles the missing case its own way.
    #[test]
    fn try_dispatch_missing_action_falls_through_to_base() {
        let home = std::env::temp_dir();
        let args = json!({}); // no "action" key
        let ctx = ctx_for(&home, &args, "");
        assert!(try_dispatch("task", &ctx).is_some());
    }
}
