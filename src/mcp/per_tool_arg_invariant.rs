//! #2648 follow-up — per-(tool, arg) MCP handler arg-read invariant.
//!
//! The sibling `tools::tests::mcp_handler_arg_reads_are_declared_or_allowlisted`
//! (#1505) reconciles handler `args["k"]` reads against the GLOBAL UNION of
//! every tool's declared schema keys. That union has a blind spot, proven live
//! by #2648: `handle_reply` read `args["message_id"]` which no `reply` schema
//! declared, yet the union passed because the *inbox* tool's schema declares
//! `message_id` — the whole reply-by-id capability was unreachable from the
//! `reply` tool's advertised schema, and no invariant caught it (reviewer5 did,
//! by hand).
//!
//! This module adds **entry-handler per-tool attribution** (Design A, lead-
//! vetted): a read that occurs DIRECTLY in a tool's registered entry-handler
//! function must be declared in THAT tool's own `inputSchema` or that tool's
//! per-tool allowlist. Reads in called helpers (shared or not) are left to the
//! existing global-union check — so genuinely-shared helpers
//! (`lift_message`, `instance_state::spawn`, the `comms_gates`) keep zero false
//! positives, and the existing global allowlist (`summary`/`question`/`kind`/
//! `done_source`, all in helpers) is untouched.
//!
//! ## Why AST, not line-scan (#2612 lesson)
//!
//! The mapping is recovered by parsing three layers with `syn` (already a
//! normal dependency), because a literal-string scan cannot reliably attribute
//! a read to a tool:
//!   1. `registry.rs` — `ToolEntry { name, definition: def_X, handler:
//!      dispatch_X }` ⇒ tool name → dispatch fn.
//!   2. `dispatch.rs` — `adapter!(dispatch_X, shape, mod::handle_Y)` /
//!      `action_adapter!(dispatch_X, "tool", ["a" => mod::handle_Y, shape; …])`
//!      / hand-written `fn dispatch_X` ⇒ dispatch fn → entry-handler fn(s).
//!   3. handler files — each entry-handler `fn` body, walked for `args["k"]` /
//!      `args.get("k")` reads (AST index / method-call exprs, robust to reflow).
//!
//! ## Scope + residual limits (deliberate, honest)
//!
//! - Only reads through a parameter literally named `args` are attributed —
//!   identical to the sibling #1505 union check's `args`-only scope. The
//!   hand-written `dispatch_*` routers read `ctx.args["…"]` (field access), so
//!   their inline reads are out of scope for BOTH checks; the routed `handle_*`
//!   entry handlers take a bare `args: &Value` and ARE covered (the #2648
//!   surface). If a future hand-written dispatcher reads bare `args`, it is
//!   covered too (its self-entry is included).
//! - Completeness is fail-LOUD, not silent: an unresolved registry→dispatch
//!   mapping panics (`build_tool_entries`), and a registry parse that
//!   under-counts trips the `tools.len() >= 25` floor — a drift can never
//!   silently shrink coverage (the #2612 trap).
//! - A read in a helper CALLED by an entry handler is left to the union check
//!   (Design A, lead-vetted): tightening those needs a transitive call graph
//!   whose trait/closure edges are imprecise — deferred until a helper-shaped
//!   bug actually appears.

#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use syn::visit::Visit;

/// One registry row: `ToolEntry { name, definition, handler }`.
#[derive(Debug, Clone)]
struct ToolReg {
    name: String,
    dispatch_fn: String,
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn parse_file(path: &Path) -> syn::File {
    let src =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    syn::parse_file(&src).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Last identifier of a path expression (`super::tools::def_reply` → `def_reply`).
fn path_last_ident(path: &syn::Path) -> Option<String> {
    path.segments.last().map(|s| s.ident.to_string())
}

// ── Layer 1: registry.rs → [(tool name, dispatch fn)] ────────────────────────

struct RegistryVisitor {
    tools: Vec<ToolReg>,
}

impl<'ast> Visit<'ast> for RegistryVisitor {
    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        if path_last_ident(&node.path).as_deref() == Some("ToolEntry") {
            let mut name = None;
            let mut dispatch_fn = None;
            for field in &node.fields {
                let key = match &field.member {
                    syn::Member::Named(id) => id.to_string(),
                    syn::Member::Unnamed(_) => continue,
                };
                match (key.as_str(), &field.expr) {
                    ("name", syn::Expr::Lit(l)) => {
                        if let syn::Lit::Str(s) = &l.lit {
                            name = Some(s.value());
                        }
                    }
                    ("handler", syn::Expr::Path(p)) => {
                        dispatch_fn = path_last_ident(&p.path);
                    }
                    _ => {}
                }
            }
            if let (Some(name), Some(dispatch_fn)) = (name, dispatch_fn) {
                self.tools.push(ToolReg { name, dispatch_fn });
            }
        }
        syn::visit::visit_expr_struct(self, node);
    }
}

fn parse_registry() -> Vec<ToolReg> {
    let file = parse_file(&manifest_dir().join("src/mcp/registry.rs"));
    let mut v = RegistryVisitor { tools: Vec::new() };
    v.visit_file(&file);
    v.tools
}

// ── Layer 2: dispatch.rs → dispatch fn → [entry-handler fn] ───────────────────

/// Extract the identifier runs from a macro invocation's **stringified** token
/// stream, skipping string-literal contents (so an action label like `"watch"`
/// is not mistaken for an ident). `proc_macro2` is not a direct dependency, and
/// the `adapter!` / `action_adapter!` grammar is author-controlled and invoked
/// in ONE file with a consistent style — so a bounded ident scan of the token
/// string is safe here (unlike the #2612 case of enumerating arbitrary-Rust
/// syntax variants; the registry rows and handler bodies stay full-AST below).
fn tokens_to_idents(tokens: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut prev_backslash = false;
    for c in tokens.chars() {
        if in_str {
            if c == '"' && !prev_backslash {
                in_str = false;
            }
            prev_backslash = c == '\\' && !prev_backslash;
            continue;
        }
        if c == '"' {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            in_str = true;
            continue;
        }
        if c.is_ascii_alphanumeric() || c == '_' {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// For `adapter!(dispatch_X, shape, mod::path::handle_Y)`: first ident is the
/// dispatch fn, the LAST ident is the handler (shape idents like `hai` sit
/// between and are dropped by taking first + last). For a single-handler adapter
/// this is exact.
fn parse_adapter_single(idents: &[String]) -> Option<(String, Vec<String>)> {
    if idents.len() < 2 {
        return None;
    }
    let dispatch_fn = idents.first()?.clone();
    let handler = idents.last()?.clone();
    Some((dispatch_fn, vec![handler]))
}

/// For `action_adapter!(dispatch_X, "tool", ["a" => mod::handle_A, shape; …])`:
/// the flattened idents are `dispatch_X, <handler-and-shape idents…>`. Each
/// action contributes `handle_A, shape`. We can't cleanly separate handler from
/// shape by position alone, so we keep every ident that starts with `handle_`
/// as a handler (shape idents are `hai/ha/hais/has/h/a`, none of which start
/// with `handle_`). The leading ident is the dispatch fn.
fn parse_action_adapter(idents: &[String]) -> Option<(String, Vec<String>)> {
    let dispatch_fn = idents.first()?.clone();
    let handlers: Vec<String> = idents
        .iter()
        .skip(1)
        .filter(|id| id.starts_with("handle_"))
        .cloned()
        .collect();
    Some((dispatch_fn, handlers))
}

/// Called-fn idents inside a hand-written `fn dispatch_X` body, restricted to
/// `handle_*` names (the routed entry handlers). The dispatch fn itself is also
/// returned as an "entry" so its own inline `args[...]` reads are attributed.
struct CallVisitor {
    handlers: Vec<String>,
}
impl<'ast> Visit<'ast> for CallVisitor {
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*node.func {
            if let Some(id) = path_last_ident(&p.path) {
                if id.starts_with("handle_") {
                    self.handlers.push(id);
                }
            }
        }
        syn::visit::visit_expr_call(self, node);
    }
}

struct DispatchVisitor {
    /// dispatch fn → entry-handler fns (plus the dispatch fn itself for
    /// hand-written routers, so their inline reads are attributed).
    map: BTreeMap<String, Vec<String>>,
}

impl<'ast> Visit<'ast> for DispatchVisitor {
    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        let mac_name = path_last_ident(&node.mac.path);
        let idents = tokens_to_idents(&node.mac.tokens.to_string());
        let parsed = match mac_name.as_deref() {
            Some("adapter") => parse_adapter_single(&idents),
            Some("action_adapter") => parse_action_adapter(&idents),
            _ => None,
        };
        if let Some((dispatch_fn, handlers)) = parsed {
            self.map.entry(dispatch_fn).or_default().extend(handlers);
        }
        syn::visit::visit_item_macro(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let name = node.sig.ident.to_string();
        if name.starts_with("dispatch_") {
            let mut cv = CallVisitor {
                handlers: Vec::new(),
            };
            cv.visit_block(&node.block);
            // The dispatch fn itself is an "entry" so its inline reads count.
            let mut entries = vec![name.clone()];
            entries.extend(cv.handlers);
            self.map.entry(name).or_default().extend(entries);
        }
        syn::visit::visit_item_fn(self, node);
    }
}

fn parse_dispatch() -> BTreeMap<String, Vec<String>> {
    let file = parse_file(&manifest_dir().join("src/mcp/handlers/dispatch.rs"));
    let mut v = DispatchVisitor {
        map: BTreeMap::new(),
    };
    v.visit_file(&file);
    for handlers in v.map.values_mut() {
        handlers.sort();
        handlers.dedup();
    }
    v.map
}

// ── Layer 3: handler files → fn name → [arg keys read directly in body] ───────

/// Extract the string key from `args["k"]` (Index on the `args` path) or
/// `args.get("k")` (MethodCall on the `args` path).
struct ArgReadVisitor {
    keys: BTreeSet<String>,
}
fn expr_is_args_ident(expr: &syn::Expr) -> bool {
    matches!(expr, syn::Expr::Path(p) if p.path.is_ident("args"))
}
fn lit_str_value(expr: &syn::Expr) -> Option<String> {
    if let syn::Expr::Lit(l) = expr {
        if let syn::Lit::Str(s) = &l.lit {
            return Some(s.value());
        }
    }
    None
}
impl<'ast> Visit<'ast> for ArgReadVisitor {
    fn visit_expr_index(&mut self, node: &'ast syn::ExprIndex) {
        if expr_is_args_ident(&node.expr) {
            if let Some(k) = lit_str_value(&node.index) {
                self.keys.insert(k);
            }
        }
        syn::visit::visit_expr_index(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if node.method == "get" && expr_is_args_ident(&node.receiver) {
            if let Some(first) = node.args.first() {
                if let Some(k) = lit_str_value(first) {
                    self.keys.insert(k);
                }
            }
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

/// fn name → (defining file, arg keys read directly in that fn body).
struct FnReadVisitor {
    map: BTreeMap<String, Vec<(String, BTreeSet<String>)>>,
    file: String,
}
impl<'ast> Visit<'ast> for FnReadVisitor {
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.record(&node.sig.ident.to_string(), &node.block);
        syn::visit::visit_item_fn(self, node);
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.record(&node.sig.ident.to_string(), &node.block);
        syn::visit::visit_impl_item_fn(self, node);
    }
}
impl FnReadVisitor {
    fn record(&mut self, name: &str, block: &syn::Block) {
        let mut av = ArgReadVisitor {
            keys: BTreeSet::new(),
        };
        av.visit_block(block);
        if !av.keys.is_empty() {
            self.map
                .entry(name.to_string())
                .or_default()
                .push((self.file.clone(), av.keys));
        }
    }
}

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

fn handler_files() -> Vec<PathBuf> {
    let manifest = manifest_dir();
    let mut files = Vec::new();
    for dir in ["src/mcp", "src/tasks"] {
        collect_rs_files(&manifest.join(dir), &mut files);
    }
    for file in [
        "src/schedules.rs",
        "src/teams.rs",
        "src/decisions.rs",
        "src/deployments.rs",
    ] {
        files.push(manifest.join(file));
    }
    files
}

fn parse_fn_reads() -> BTreeMap<String, Vec<(String, BTreeSet<String>)>> {
    let mut v = FnReadVisitor {
        map: BTreeMap::new(),
        file: String::new(),
    };
    for f in handler_files() {
        let fname = f.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if fname.ends_with("tests.rs") || fname.ends_with("test.rs") {
            continue;
        }
        v.file = f
            .strip_prefix(manifest_dir())
            .unwrap_or(&f)
            .to_string_lossy()
            .to_string();
        v.visit_file(&parse_file(&f));
    }
    v.map
}

// ── Per-tool declared schema keys (ground truth from tool_definitions) ────────

fn declared_props_by_tool() -> BTreeMap<String, BTreeSet<String>> {
    let defs = crate::mcp::tools::tool_definitions();
    let mut out = BTreeMap::new();
    for tool in defs["tools"].as_array().expect("tools array") {
        let name = tool["name"].as_str().unwrap_or_default().to_string();
        let mut props = BTreeSet::new();
        if let Some(obj) = tool["inputSchema"]["properties"].as_object() {
            props.extend(obj.keys().cloned());
        }
        out.insert(name, props);
    }
    out
}

// ── Attribution + the check (pure, so the counterexample is synthetic) ────────

/// (tool name, entry-handler fn) pairs. A handler registered by N tools appears
/// N times — so a multi-owner handler's reads are checked against EVERY owning
/// tool's schema (lead-vetted rule: any owner's call path reaches the read).
fn build_tool_entries(
    tools: &[ToolReg],
    dispatch: &BTreeMap<String, Vec<String>>,
) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for t in tools {
        let Some(entries) = dispatch.get(&t.dispatch_fn) else {
            // A registered tool whose dispatch fn we could not resolve is itself
            // a coverage hole — surface it loudly rather than silently skipping.
            panic!(
                "tool `{}` dispatch fn `{}` not found in dispatch.rs — the \
                 registry↔dispatch mapping drifted; the invariant cannot \
                 attribute this tool's reads",
                t.name, t.dispatch_fn
            );
        };
        for h in entries {
            pairs.push((t.name.clone(), h.clone()));
        }
    }
    pairs
}

/// A single per-(tool, arg) violation: `tool`'s entry handler `handler`
/// (defined in `file`) reads `key`, which is neither in `tool`'s own schema nor
/// its per-tool allowlist.
#[derive(Debug, PartialEq, Eq)]
struct Violation {
    tool: String,
    handler: String,
    file: String,
    key: String,
}

/// The pure check: for every (tool, entry-handler) pair, every arg key the
/// handler reads directly must be in that tool's declared props OR the per-tool
/// allowlist `(tool, key)`. Pure over its inputs so it is unit-testable with
/// synthetic data (the #2648 counterexample) without mutating real source.
fn find_violations(
    tool_entries: &[(String, String)],
    fn_reads: &BTreeMap<String, Vec<(String, BTreeSet<String>)>>,
    declared: &BTreeMap<String, BTreeSet<String>>,
    allow: &BTreeSet<(String, String)>,
) -> Vec<Violation> {
    let empty = BTreeSet::new();
    let mut out = Vec::new();
    for (tool, handler) in tool_entries {
        let Some(defs) = fn_reads.get(handler) else {
            continue; // handler has no direct args read (or is a macro-gen dispatch fn)
        };
        let props = declared.get(tool).unwrap_or(&empty);
        for (file, keys) in defs {
            for k in keys {
                if !props.contains(k) && !allow.contains(&(tool.clone(), k.clone())) {
                    out.push(Violation {
                        tool: tool.clone(),
                        handler: handler.clone(),
                        file: file.clone(),
                        key: k.clone(),
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| (&a.tool, &a.handler, &a.key).cmp(&(&b.tool, &b.handler, &b.key)));
    out
}

/// Per-tool allowlist — a handler read that is NOT an agent-facing schema param
/// for THAT tool, with its reason. Distinct from the sibling #1505 union test's
/// GLOBAL allowlist: these are scoped to `(tool, key)` so borrowing another
/// tool's declaration can never satisfy them (the #2648 blind spot this whole
/// module closes). Each entry is a latent read the union check hid; convert to a
/// schema declaration instead if the param becomes a primary agent surface.
const PER_TOOL_ALLOWLIST: &[(&str, &str, &str)] = &[
    // `ci action=watch` persists task_id as a back-link on the watch sidecar
    // (watch.rs:163). It is PRIMARILY dispatch-injected by the auto-watch hook
    // (dispatch_hook/auto_watch.rs:39, from dispatch_auto_bind_lease); the manual
    // pass-through the comment mentions is a secondary affordance, not the
    // primary agent surface. Declaring it in def_ci (discoverability) would pull
    // in the #1933 consumption table — deferred to follow-up task
    // t-20260705161926295621-30532-2 ①; allow-listed here.
    (
        "ci",
        "task_id",
        "dispatch-injected watch back-link (auto_watch.rs:39); secondary manual pass-through; schema-declaration deferred to t-20260705161926295621-30532-2 ①",
    ),
    // `ci action=unwatch` reads `instance` as an OPTIONAL caller-identity override
    // for selective subscription removal (watch.rs:261), falling back to the
    // validated sender (`instance_name`). Caller-identity, not a primary tool
    // param — and whether agent A may drop agent B's subscription is an authority
    // question deferred to follow-up task t-20260705161926295621-30532-2 ②;
    // allow-listed here rather than advertised.
    (
        "ci",
        "instance",
        "optional caller-identity override for selective unwatch (watch.rs:261); falls back to validated sender; isolation/authority review deferred to t-20260705161926295621-30532-2 ②",
    ),
];

fn per_tool_allow_set() -> BTreeSet<(String, String)> {
    PER_TOOL_ALLOWLIST
        .iter()
        .map(|(t, k, _)| (t.to_string(), k.to_string()))
        .collect()
}

// ── The invariant ────────────────────────────────────────────────────────────

/// #2648 follow-up — per-(tool, arg): every arg key read DIRECTLY in a tool's
/// registered entry-handler fn must be declared in THAT tool's own schema or its
/// per-tool allowlist. Closes the union-check blind spot (a read satisfied only
/// by ANOTHER tool's schema) that let #2648's `handle_reply` → `message_id` ship
/// unreachable. Reads in called helpers keep the sibling #1505 global-union
/// check (unchanged); this only tightens the entry-handler surface.
#[test]
fn per_tool_entry_handler_arg_reads_declared_in_own_schema_2648() {
    let tools = parse_registry();
    assert!(
        tools.len() >= 25,
        "registry parse under-counted tools ({} < 25) — the syn ToolEntry \
         extraction likely drifted; refusing to run a hollow check",
        tools.len()
    );
    let dispatch = parse_dispatch();
    let tool_entries = build_tool_entries(&tools, &dispatch);
    let fn_reads = parse_fn_reads();
    let declared = declared_props_by_tool();
    let allow = per_tool_allow_set();

    let violations = find_violations(&tool_entries, &fn_reads, &declared, &allow);
    let rendered: Vec<String> = violations
        .iter()
        .map(|v| {
            format!(
                "tool `{}` entry `{}` ({}) reads `{}` — declared by NO {} schema \
                 and not in its per-tool allowlist",
                v.tool, v.handler, v.file, v.key, v.tool
            )
        })
        .collect();
    assert!(
        violations.is_empty(),
        "#2648: an MCP entry handler reads an arg its OWN tool's schema does not \
         declare (a read another tool's schema hid under the union check). Either \
         declare it in that tool's `inputSchema.properties` (if agent-facing) or \
         add a `(tool, key, reason)` PER_TOOL_ALLOWLIST entry (if internal):\n{}",
        rendered.join("\n")
    );
}

/// #2648 lineage guard: the exact bug shape — `reply`'s `handle_reply` reads
/// `message_id` — must now be declared in `reply`'s OWN schema (the PR-3 fix),
/// not merely somewhere in the union.
#[test]
fn reply_message_id_is_declared_in_replys_own_schema_2648() {
    let declared = declared_props_by_tool();
    let reply = declared
        .get("reply")
        .expect("reply tool present in tool_definitions");
    assert!(
        reply.contains("message_id"),
        "reply's own schema must declare `message_id` (the #2648 fix); got {reply:?}"
    );
}

/// Counterexample proving the NEW check is strictly tighter than the OLD union:
/// synthesize the #2648 shape — tool A's entry handler reads `k`, only tool B's
/// schema declares `k`. The per-tool check MUST flag it; a union check MUST NOT.
#[test]
fn per_tool_check_catches_cross_tool_borrow_that_union_would_miss() {
    // tool A entry `handle_a` reads `k`; tool B (only) declares `k`.
    let tool_entries = vec![("tool_a".to_string(), "handle_a".to_string())];
    let mut a_reads = BTreeSet::new();
    a_reads.insert("k".to_string());
    let mut fn_reads = BTreeMap::new();
    fn_reads.insert("handle_a".to_string(), vec![("a.rs".to_string(), a_reads)]);

    let mut declared = BTreeMap::new();
    declared.insert("tool_a".to_string(), BTreeSet::new()); // A declares nothing
    let mut b_props = BTreeSet::new();
    b_props.insert("k".to_string());
    declared.insert("tool_b".to_string(), b_props); // only B declares `k`

    let empty_allow = BTreeSet::new();

    // NEW per-tool check: flags it (A reads `k`, A doesn't declare it).
    let v = find_violations(&tool_entries, &fn_reads, &declared, &empty_allow);
    assert_eq!(
        v.len(),
        1,
        "per-tool check must flag the cross-tool borrow: {v:?}"
    );
    assert_eq!(v[0].tool, "tool_a");
    assert_eq!(v[0].key, "k");

    // OLD union check: `k` IS in the union (B declares it) → would NOT flag.
    let union: BTreeSet<&String> = declared.values().flatten().collect();
    assert!(
        union.contains(&"k".to_string()),
        "sanity: the union DOES contain `k` (so the old check passed it) — this \
         is exactly the blind spot the per-tool check closes"
    );

    // And the per-tool allowlist can legitimately silence it (the internal case).
    let allow: BTreeSet<(String, String)> = [("tool_a".to_string(), "k".to_string())]
        .into_iter()
        .collect();
    let v2 = find_violations(&tool_entries, &fn_reads, &declared, &allow);
    assert!(
        v2.is_empty(),
        "a per-tool allowlist entry must silence it: {v2:?}"
    );
}

// ── Diagnostic dump (kept, ignored) ──────────────────────────────────────────

/// Human-readable attribution dump — multi-owner handlers, dup fn-names, and
/// all per-tool violations. Not an assertion; run explicitly when auditing.
#[test]
#[ignore = "diagnostic dump; run with --ignored --nocapture"]
fn dump_per_tool_entry_handler_arg_reads() {
    let tools = parse_registry();
    let dispatch = parse_dispatch();
    let fn_reads = parse_fn_reads();
    let declared = declared_props_by_tool();
    let allow = per_tool_allow_set();

    let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let tool_entries = build_tool_entries(&tools, &dispatch);
    for (tool, h) in &tool_entries {
        owners.entry(h.clone()).or_default().insert(tool.clone());
    }
    let multi: Vec<_> = owners.iter().filter(|(_, o)| o.len() > 1).collect();
    println!(
        "\n=== tools: {} | multi-owner handlers: {} ===",
        tools.len(),
        multi.len()
    );
    for (h, o) in &multi {
        println!("  {h} ← {o:?}");
    }
    let violations = find_violations(&tool_entries, &fn_reads, &declared, &allow);
    println!("=== violations (after allowlist): {} ===", violations.len());
    for v in &violations {
        println!(
            "  tool `{}` `{}` ({}) reads `{}`",
            v.tool, v.handler, v.file, v.key
        );
    }
    println!("=== END DUMP ===");
}
