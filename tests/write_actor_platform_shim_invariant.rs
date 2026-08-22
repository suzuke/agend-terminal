//! #3315 B3 PIN: `write_actor` may only be NAMED from a `unix`-gated item in
//! `src/agent/mod.rs`, and the write path's platform shim must keep both arms.
//!
//! `mod write_actor` is `#[cfg(unix)]` — it owns a raw PTY fd. The #3314 work
//! replaced the two-arm `try_actor_write` shim with a direct
//! `write_actor::write_guarded(..)` call inside `write_with_timeout_guarded`,
//! which is NOT gated. That does not exist on Windows: CI run 32560269942 failed
//! on the Windows `Check` job, and the stranded `#[cfg(not(unix))]` stub was
//! left behind unused. macOS/Linux developers never see either.
//!
//! Detection is a `syn` AST walk, not a grep: `src/agent/mod.rs` mentions
//! `write_actor` in six doc comments, and a string scan cannot tell a doc
//! mention from a call, nor see the `#[cfg]` that governs it. The walk carries
//! the enclosing item's gate down, so a call inside a `#[cfg(unix)] mod`/`fn`
//! is correctly accepted while a bare one is not.
//!
//! SCOPE BOUNDARY (deliberate, stated rather than implied): this pins
//! the two files that may name it. Other files naming `write_actor` (`agent/tests.rs`,
//! `daemon/lifecycle.rs`, …) inherit their gate from the `mod` declaration in
//! ANOTHER file, and this walk does not model cross-file cfg inheritance. The
//! Windows CI job remains the total gate; this is the fast local one for the
//! file where the write path actually lives.

use syn::visit::{self, Visit};

/// Every file that may legitimately name `write_actor`. The shim was re-homed
/// out of `mod.rs` when that file hit its anti-monolith ceiling, so the scan
/// follows it — a pin that watched only the old location would have gone quietly
/// vacuous the moment the code moved.
const TARGETS: &[&str] = &["src/agent/mod.rs", "src/agent/actor_write.rs"];
/// The file that must carry BOTH platform arms of the shim.
const SHIM_FILE: &str = "src/agent/actor_write.rs";
const SHIM: &str = "try_actor_write_guarded";

/// Does this `cfg` predicate IMPLY `unix`? Conservative by construction:
/// `all(..)` needs one unix-implying arm, `any(..)` needs every arm to imply
/// it, and `not(..)` never implies it (`not(windows)` is not `unix`).
fn meta_implies_unix(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(p) => p.is_ident("unix"),
        syn::Meta::List(list) => {
            let Ok(nested) = list.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            ) else {
                return false;
            };
            if list.path.is_ident("all") {
                nested.iter().any(meta_implies_unix)
            } else if list.path.is_ident("any") {
                !nested.is_empty() && nested.iter().all(meta_implies_unix)
            } else {
                false
            }
        }
        syn::Meta::NameValue(_) => false,
    }
}

fn attrs_imply_unix(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Meta>()
                .is_ok_and(|m| meta_implies_unix(&m))
    })
}

/// Walks one item, carrying the gate down through nested items and through
/// STATEMENT-level `#[cfg(unix)]` — `spawn_agent_with_capture_home` gates its
/// `write_actor::register` call on the statement, not on the fn, and treating
/// that as ungated is a false positive the first draft of this pin produced.
///
/// Conservative in the loud direction: an attribute-carrying expression form
/// not listed in `expr_attrs` reads as UNGATED, so the failure mode is a visible
/// false positive to be fixed here, never a silent pass.
struct Scanner<'a> {
    gated: bool,
    context: &'a str,
    hits: Vec<String>,
}

impl<'ast> Visit<'ast> for Scanner<'_> {
    fn visit_item(&mut self, i: &'ast syn::Item) {
        let prev = self.gated;
        self.gated |= attrs_imply_unix(item_attrs(i));
        visit::visit_item(self, i);
        self.gated = prev;
    }

    fn visit_stmt(&mut self, s: &'ast syn::Stmt) {
        let prev = self.gated;
        self.gated |= attrs_imply_unix(stmt_attrs(s));
        visit::visit_stmt(self, s);
        self.gated = prev;
    }

    fn visit_expr(&mut self, e: &'ast syn::Expr) {
        let prev = self.gated;
        self.gated |= attrs_imply_unix(expr_attrs(e));
        visit::visit_expr(self, e);
        self.gated = prev;
    }

    /// `write_actor::x`, `crate::agent::write_actor::x` and `self::write_actor::x`
    /// all count. Doc comments are attributes, not paths, so they never match —
    /// which is the whole reason this is an AST walk and not a grep.
    fn visit_path(&mut self, p: &'ast syn::Path) {
        if !self.gated
            && p.segments.len() >= 2
            && p.segments.iter().any(|seg| seg.ident == "write_actor")
        {
            self.hits.push(self.context.to_string());
        }
        visit::visit_path(self, p);
    }
}

fn stmt_attrs(s: &syn::Stmt) -> &[syn::Attribute] {
    match s {
        syn::Stmt::Local(l) => &l.attrs,
        syn::Stmt::Item(i) => item_attrs(i),
        syn::Stmt::Expr(e, _) => expr_attrs(e),
        syn::Stmt::Macro(m) => &m.attrs,
    }
}

fn expr_attrs(e: &syn::Expr) -> &[syn::Attribute] {
    match e {
        syn::Expr::Block(x) => &x.attrs,
        syn::Expr::Call(x) => &x.attrs,
        syn::Expr::ForLoop(x) => &x.attrs,
        syn::Expr::If(x) => &x.attrs,
        syn::Expr::Let(x) => &x.attrs,
        syn::Expr::Loop(x) => &x.attrs,
        syn::Expr::Macro(x) => &x.attrs,
        syn::Expr::Match(x) => &x.attrs,
        syn::Expr::MethodCall(x) => &x.attrs,
        syn::Expr::Unsafe(x) => &x.attrs,
        syn::Expr::While(x) => &x.attrs,
        _ => &[],
    }
}

fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(i) => &i.attrs,
        syn::Item::Enum(i) => &i.attrs,
        syn::Item::ExternCrate(i) => &i.attrs,
        syn::Item::Fn(i) => &i.attrs,
        syn::Item::ForeignMod(i) => &i.attrs,
        syn::Item::Impl(i) => &i.attrs,
        syn::Item::Macro(i) => &i.attrs,
        syn::Item::Mod(i) => &i.attrs,
        syn::Item::Static(i) => &i.attrs,
        syn::Item::Struct(i) => &i.attrs,
        syn::Item::Trait(i) => &i.attrs,
        syn::Item::TraitAlias(i) => &i.attrs,
        syn::Item::Type(i) => &i.attrs,
        syn::Item::Union(i) => &i.attrs,
        syn::Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn describe(item: &syn::Item) -> String {
    match item {
        syn::Item::Fn(i) => format!("fn {}", i.sig.ident),
        syn::Item::Mod(i) => format!("mod {}", i.ident),
        syn::Item::Struct(i) => format!("struct {}", i.ident),
        syn::Item::Impl(_) => "impl block".to_string(),
        syn::Item::Const(i) => format!("const {}", i.ident),
        syn::Item::Static(i) => format!("static {}", i.ident),
        syn::Item::Use(_) => "use".to_string(),
        _ => "item".to_string(),
    }
}

/// Collects ungated `write_actor` namings, labelled by the top-level item they
/// sit in so a failure names the place to fix.
fn collect_ungated(items: &[syn::Item], out: &mut Vec<String>) {
    for item in items {
        // A bare `mod write_actor;` declaration is itself a naming, and it has
        // no path node for the walk to see.
        if let syn::Item::Mod(m) = item {
            if m.content.is_none()
                && m.ident == "write_actor"
                && !attrs_imply_unix(item_attrs(item))
            {
                out.push("mod write_actor; (declaration)".to_string());
                continue;
            }
        }
        let label = describe(item);
        let mut scanner = Scanner {
            gated: false,
            context: &label,
            hits: Vec::new(),
        };
        scanner.visit_item(item);
        out.extend(scanner.hits.into_iter().take(1));
    }
}

fn parse_file(rel: &str) -> syn::File {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    syn::parse_file(&src).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

#[test]
fn write_actor_is_never_named_outside_a_unix_gate_3315() {
    let mut ungated = Vec::new();
    for target in TARGETS {
        let file = parse_file(target);
        let mut hits = Vec::new();
        collect_ungated(&file.items, &mut hits);
        ungated.extend(hits.into_iter().map(|hit| format!("{target}: {hit}")));
    }
    assert!(
        ungated.is_empty(),
        "#3315 B3: `mod write_actor` is #[cfg(unix)], so naming it from an item that also \
         compiles on Windows is a build break the local (macOS/Linux) gates cannot see. \
         Route the call through the `{SHIM}` cfg-pair instead. Ungated: {ungated:?}"
    );
}

#[test]
fn the_actor_write_shim_keeps_both_platform_arms_3315() {
    let file = parse_file(SHIM_FILE);
    let mut unix_arm = 0usize;
    let mut non_unix_arm = 0usize;
    let mut arities = Vec::new();
    for item in &file.items {
        let syn::Item::Fn(f) = item else { continue };
        if f.sig.ident != SHIM {
            continue;
        }
        arities.push(f.sig.inputs.len());
        if attrs_imply_unix(&f.attrs) {
            unix_arm += 1;
        } else if f.attrs.iter().any(|a| {
            a.path().is_ident("cfg")
                && a.parse_args::<syn::Meta>().is_ok_and(|m| {
                    matches!(&m, syn::Meta::List(l) if l.path.is_ident("not")
                        && l.parse_args::<syn::Meta>().is_ok_and(|inner| inner.path().is_ident("unix")))
                })
        }) {
            non_unix_arm += 1;
        }
    }
    assert_eq!(
        (unix_arm, non_unix_arm),
        (1, 1),
        "#3315 B3: `{SHIM}` must exist exactly once per platform arm — one #[cfg(unix)] \
         delegating to write_actor and one #[cfg(not(unix))] returning None. Deleting the \
         non-Unix arm compiles here and breaks the Windows job. Found {unix_arm} unix / \
         {non_unix_arm} non-unix in {SHIM_FILE}"
    );
    assert!(
        arities.windows(2).all(|w| w[0] == w[1]),
        "#3315 B3: both `{SHIM}` arms must take the same arguments, else the ungated call \
         site cannot type-check on both platforms. Arities: {arities:?}"
    );
}
