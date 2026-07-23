//! Exec reachability invariant (decision d-20260706152615194350-1).
//!
//! `exec_real_git` and `exec_with_conflict_guidance` are the shim's terminal
//! passthrough capability — the ONLY two functions that hand the caller's argv
//! to real git. The whole security value of the shim rests on every path to
//! them passing through the policy surface (`should_bypass` / the no-agent
//! early exit / `classify` + the `Action` dispatch). Today that is auditable
//! with a single-file grep because `main.rs` has zero `pub` items; this test
//! makes the audit MACHINE-CHECKED so it survives future refactors:
//!
//! 1. Every reference to the two exec fns anywhere under `src/` is found by
//!    walking the syn AST — call sites, bare path mentions (fn pointers),
//!    `use`-tree imports/renames (`use crate::exec_real_git as x`), macro
//!    token streams, and shadow re-definitions. Literal-string scanning is
//!    deliberately NOT used for code (it misses alias/import evasions); the
//!    only string check is on macro token streams, where it is conservative
//!    (any mention inside a macro fails — fail-closed).
//! 2. Each ALLOWED occurrence is whitelisted below keyed by
//!    (file, enclosing fn, structural context) — never line numbers — and the
//!    context is derived from the AST (enclosing `match` arm variant + guard,
//!    or enclosing `if` condition), so the classification is verified, not
//!    decorative: moving a call out of its dispatch arm changes its context
//!    string and fails the test.
//! 3. The comparison is an exact multiset in BOTH directions: a new/relocated
//!    call site fails, and a REMOVED call site also fails until the whitelist
//!    is consciously updated under review.
//!
//! Scope boundary (deliberate): integration tests under `tests/` exec the
//! built BINARY and cannot link these private fns; `agentic-git-core` cannot
//! reach them either. Internal `Command::new(git)` query sites (fixed argv,
//! e.g. `current_branch_of`) are a different, lower-risk class and are not
//! covered here.
//!
//! To change the exec surface legitimately: update `expected_whitelist()` in
//! the same PR and say why in the PR description — that is the review hook
//! this invariant exists to force.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};

const EXEC_FNS: &[&str] = &["exec_real_git", "exec_with_conflict_guidance"];

/// Structural context of an occurrence, derived from AST ancestry.
#[derive(Clone)]
enum Frame {
    /// Inside a `match` arm: variant name(s) of the pattern + whether guarded.
    Arm { variant: String, guarded: bool },
    /// Inside an `if` body: coarse descriptor of the condition's called fns.
    If { desc: String },
}

struct Scanner<'a> {
    file: &'a str,
    fn_stack: Vec<String>,
    frames: Vec<Frame>,
    /// Canonical occurrence descriptor strings (the whitelist currency).
    out: Vec<String>,
}

impl<'a> Scanner<'a> {
    fn new(file: &'a str) -> Self {
        Scanner {
            file,
            fn_stack: Vec::new(),
            frames: Vec::new(),
            out: Vec::new(),
        }
    }

    fn enclosing_fn(&self) -> &str {
        self.fn_stack
            .last()
            .map(String::as_str)
            .unwrap_or("<top-level>")
    }

    fn context_desc(&self) -> String {
        match self.frames.last() {
            Some(Frame::Arm { variant, guarded }) => {
                let g = if *guarded { "guarded" } else { "unguarded" };
                format!("dispatch-arm {variant} ({g})")
            }
            Some(Frame::If { desc }) => desc.clone(),
            None => "no-context".to_string(),
        }
    }

    fn record(&mut self, kind: &str, name: &str, ctx: Option<String>) {
        let mut s = format!("{} :: {} :: {kind} {name}", self.file, self.enclosing_fn());
        if let Some(ctx) = ctx {
            s.push_str(&format!(" @ {ctx}"));
        }
        self.out.push(s);
    }
}

/// Last path-segment idents of every fn/method called inside an expression —
/// used to give `if` conditions a stable, content-derived descriptor.
struct CondScan {
    called: Vec<String>,
}

impl<'ast> Visit<'ast> for CondScan {
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*node.func {
            if let Some(seg) = p.path.segments.last() {
                self.called.push(seg.ident.to_string());
            }
        }
        visit::visit_expr_call(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.called.push(node.method.to_string());
        visit::visit_expr_method_call(self, node);
    }
}

fn if_condition_desc(cond: &syn::Expr) -> String {
    let mut scan = CondScan { called: Vec::new() };
    scan.visit_expr(cond);
    if scan.called.iter().any(|c| c == "should_bypass") {
        "if should_bypass".to_string()
    } else if scan.called.iter().any(|c| c == "is_empty") {
        "if is_empty".to_string()
    } else if scan.called.iter().any(|c| c == "shim_depth") {
        "if shim_depth".to_string()
    } else {
        "if other".to_string()
    }
}

/// Variant name(s) of a match-arm pattern: `Action::ChdirPass(w)` → "ChdirPass",
/// or-patterns joined with "|", anything unrecognized → "?" (which can never
/// match a whitelist entry — fail-closed).
fn pattern_variant(pat: &syn::Pat) -> String {
    match pat {
        syn::Pat::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_else(|| "?".to_string()),
        syn::Pat::TupleStruct(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_else(|| "?".to_string()),
        syn::Pat::Struct(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_else(|| "?".to_string()),
        syn::Pat::Or(p) => {
            let parts: Vec<String> = p.cases.iter().map(pattern_variant).collect();
            parts.join("|")
        }
        syn::Pat::Wild(_) => "_".to_string(),
        syn::Pat::Ident(p) => p.ident.to_string(),
        _ => "?".to_string(),
    }
}

fn is_exec_ident(ident: &syn::Ident) -> bool {
    EXEC_FNS.iter().any(|f| ident == f)
}

fn path_names_exec(path: &syn::Path) -> Option<String> {
    path.segments
        .iter()
        .find(|seg| is_exec_ident(&seg.ident))
        .map(|seg| seg.ident.to_string())
}

impl<'ast> Visit<'ast> for Scanner<'_> {
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let name = node.sig.ident.to_string();
        if is_exec_ident(&node.sig.ident) {
            self.record("def", &name, None);
        }
        self.fn_stack.push(name);
        // A fn body starts a fresh context: enclosing arm/if frames of an
        // OUTER fn must not leak into a nested fn's classification.
        let saved = std::mem::take(&mut self.frames);
        visit::visit_item_fn(self, node);
        self.frames = saved;
        self.fn_stack.pop();
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let name = node.sig.ident.to_string();
        if is_exec_ident(&node.sig.ident) {
            self.record("def", &name, None);
        }
        self.fn_stack.push(name);
        let saved = std::mem::take(&mut self.frames);
        visit::visit_impl_item_fn(self, node);
        self.frames = saved;
        self.fn_stack.pop();
    }

    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        // The condition itself is scanned WITHOUT the new frame (a banned call
        // in a condition would be genuinely weird — let it classify under the
        // outer context and fail the whitelist).
        self.visit_expr(&node.cond);
        self.frames.push(Frame::If {
            desc: if_condition_desc(&node.cond),
        });
        self.visit_block(&node.then_branch);
        self.frames.pop();
        if let Some((_, else_expr)) = &node.else_branch {
            self.visit_expr(else_expr);
        }
    }

    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        self.frames.push(Frame::Arm {
            variant: pattern_variant(&node.pat),
            guarded: node.guard.is_some(),
        });
        visit::visit_arm(self, node);
        self.frames.pop();
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*node.func {
            if let Some(name) = path_names_exec(&p.path) {
                self.record("call", &name, Some(self.context_desc()));
                // Skip the callee path (already recorded as a call, must not
                // double-report as a bare path mention); still scan the args.
                for arg in &node.args {
                    self.visit_expr(arg);
                }
                return;
            }
        }
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        // Non-call mention: fn pointer harvest (`let f = exec_real_git;`),
        // passing as a value, etc. Never legitimate.
        if let Some(name) = path_names_exec(&node.path) {
            self.record("path-mention", &name, Some(self.context_desc()));
        }
        visit::visit_expr_path(self, node);
    }

    fn visit_use_tree(&mut self, node: &'ast syn::UseTree) {
        // Import or alias of the exec fns (`use crate::exec_real_git as rg`).
        // Never legitimate anywhere — main.rs is the crate root and needs no
        // import of its own items.
        match node {
            syn::UseTree::Path(p) if is_exec_ident(&p.ident) => {
                self.record("use-mention", &p.ident.to_string(), None);
            }
            syn::UseTree::Name(n) if is_exec_ident(&n.ident) => {
                self.record("use-mention", &n.ident.to_string(), None);
            }
            // Both directions: aliasing the exec fn away, or naming
            // something ELSE to an exec-fn name (a confusion vector).
            syn::UseTree::Rename(r) if is_exec_ident(&r.ident) || is_exec_ident(&r.rename) => {
                self.record("use-mention", &r.ident.to_string(), None);
            }
            _ => {}
        }
        visit::visit_use_tree(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        // syn cannot see through macro expansion; a token-stream string scan
        // is the conservative fail-closed fallback for this one gap.
        let tokens = node.tokens.to_string();
        for f in EXEC_FNS {
            if tokens.contains(f) {
                self.record("macro-mention", f, None);
            }
        }
        visit::visit_macro(self, node);
    }
}

fn scan_source(file_label: &str, source: &str) -> Vec<String> {
    let ast = syn::parse_file(source)
        .unwrap_or_else(|e| panic!("exec-invariant: failed to parse {file_label}: {e}"));
    let mut scanner = Scanner::new(file_label);
    scanner.visit_file(&ast);
    scanner.out
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("exec-invariant: cannot read {}: {e}", dir.display()))
    {
        let path = entry.expect("exec-invariant: dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn scan_crate_src() -> Vec<String> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    assert!(
        !files.is_empty(),
        "exec-invariant: no .rs files found under {} — the scan basis is gone",
        src.display()
    );
    files.sort();
    let mut out = Vec::new();
    for path in &files {
        let label = path
            .strip_prefix(&src)
            .expect("exec-invariant: path under src/")
            .to_string_lossy()
            .replace('\\', "/");
        let source = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("exec-invariant: cannot read {}: {e}", path.display()));
        out.extend(scan_source(&label, &source));
    }
    out
}

/// THE whitelist. Keys are the canonical occurrence descriptors produced by
/// `Scanner`; values are exact expected counts. Every entry names WHY it is
/// legitimate. Any diff in either direction fails `exec_reachability_invariant`.
fn expected_whitelist() -> BTreeMap<String, usize> {
    let entries = [
        // The two capability definitions themselves — owned by the exec
        // module since the #30 split (mechanical move, same bodies; the
        // shim_main call sites below stay in lib.rs).
        "exec.rs :: <top-level> :: def exec_real_git",
        "exec.rs :: <top-level> :: def exec_with_conflict_guidance",
        // Bypass early path: `if should_bypass()` — audited/deny-checked
        // above the exec (§7 3-layer bypass; #2158 audit; #2234 deny).
        "lib.rs :: shim_main :: call exec_real_git @ if should_bypass",
        // No-agent early path: `if agent.is_empty() || home.is_empty()` —
        // non-agent caller passthrough (#2234 defect#2 instrumentation above).
        "lib.rs :: shim_main :: call exec_real_git @ if is_empty",
        // #34: nested submodule--helper at depth > 0 — pass through without
        // classify/snapshot/audit (the depth-0 shim already routed the parent).
        "lib.rs :: shim_main :: call exec_real_git @ if shim_depth",
        // Action dispatch arms — the ONLY post-classify exec points.
        "lib.rs :: shim_main :: call exec_real_git @ dispatch-arm Passthrough (unguarded)",
        "lib.rs :: shim_main :: call exec_real_git @ dispatch-arm ChdirPass (unguarded)",
        "lib.rs :: shim_main :: call exec_real_git @ dispatch-arm CleanupAndChdirPushPass (unguarded)",
        // Conflict-capable ChdirPass arm (`if is_conflict_capable(subcommand)`)
        // routes through the guidance wrapper instead.
        "lib.rs :: shim_main :: call exec_with_conflict_guidance @ dispatch-arm ChdirPass (guarded)",
    ];
    let mut map = BTreeMap::new();
    for e in entries {
        *map.entry(e.to_string()).or_insert(0) += 1;
    }
    map
}

fn diff_against_whitelist(occurrences: &[String]) -> Result<(), String> {
    let mut actual: BTreeMap<String, usize> = BTreeMap::new();
    for o in occurrences {
        *actual.entry(o.clone()).or_insert(0) += 1;
    }
    let expected = expected_whitelist();
    let mut violations = Vec::new();
    for (occ, &n) in &actual {
        let allowed = expected.get(occ).copied().unwrap_or(0);
        if n > allowed {
            violations.push(format!("  UNEXPECTED (x{}): {occ}", n - allowed));
        }
    }
    for (occ, &n) in &expected {
        let have = actual.get(occ).copied().unwrap_or(0);
        if have < n {
            violations.push(format!("  MISSING    (x{}): {occ}", n - have));
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "exec reachability invariant violated:\n{}\n\
             The exec fns ({}) are the shim's terminal passthrough capability.\n\
             If this change to the exec surface is intentional, update\n\
             `expected_whitelist()` in tests/exec_reachability_invariant.rs in the\n\
             SAME PR and justify it — that review hook is the point of this test.\n\
             (decision d-20260706152615194350-1)",
            violations.join("\n"),
            EXEC_FNS.join(", ")
        ))
    }
}

/// The invariant itself: the real `src/` tree matches the whitelist exactly.
#[test]
fn exec_reachability_invariant() {
    if let Err(report) = diff_against_whitelist(&scan_crate_src()) {
        panic!("{report}");
    }
}

// ── Counter-example (injection) tests ────────────────────────────────────────
// Each proves the scanner DETECTS a specific evasion — i.e. that the invariant
// fails loudly on a "buggy" tree, not just passes on the good one.

fn real_main_rs() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("exec-invariant: cannot read {}: {e}", path.display()))
}

/// A bare new call point appended to the REAL main.rs must be detected.
#[test]
fn injected_bare_call_in_main_rs_is_detected() {
    let injected = format!(
        "{}\nfn sneaky_escape(args: &[String]) {{ exec_real_git(args, None); }}\n",
        real_main_rs()
    );
    let err = diff_against_whitelist(&scan_source("lib.rs", &injected))
        .expect_err("injected bare call point must violate the invariant");
    assert!(
        err.contains("lib.rs :: sneaky_escape :: call exec_real_git"),
        "violation report must name the injected call site, got:\n{err}"
    );
}

/// An alias import in a sibling module (`use crate::exec_real_git as rg`)
/// hides the capability name at the call site — the use-tree walk must catch
/// the rename itself.
#[test]
fn alias_import_in_sibling_module_is_detected() {
    let src = "use crate::exec_real_git as rg;\n\
               pub fn helper(args: &[String]) { rg(args, None); }\n";
    let err = diff_against_whitelist(&scan_source("cli.rs", src))
        .expect_err("use-rename alias must violate the invariant");
    assert!(
        err.contains("cli.rs :: <top-level> :: use-mention exec_real_git"),
        "violation report must flag the alias import, got:\n{err}"
    );
}

/// A fully-qualified call from another module (no import to catch) must be
/// detected at the call site itself.
#[test]
fn qualified_call_from_sibling_module_is_detected() {
    let src = "pub fn helper(args: &[String]) { crate::exec_real_git(args, None); }\n";
    let err = diff_against_whitelist(&scan_source("snapshot.rs", src))
        .expect_err("crate-qualified call must violate the invariant");
    assert!(
        err.contains("snapshot.rs :: helper :: call exec_real_git"),
        "violation report must name the qualified call, got:\n{err}"
    );
}

/// Harvesting the fn as a value (`let f = exec_real_git;`) launders the
/// capability without a direct call expression — must be detected.
#[test]
fn fn_pointer_harvest_is_detected() {
    let src = "pub fn helper() { let _f = exec_real_git; }\n";
    let err = diff_against_whitelist(&scan_source("cli.rs", src))
        .expect_err("fn-pointer mention must violate the invariant");
    assert!(
        err.contains("cli.rs :: helper :: path-mention exec_real_git"),
        "violation report must flag the fn-pointer harvest, got:\n{err}"
    );
}

/// A call smuggled through a macro body is invisible to AST call analysis —
/// the conservative token scan must catch it.
#[test]
fn macro_wrapped_call_is_detected() {
    let src = "macro_rules! smuggle { ($a:expr) => { exec_real_git($a, None) }; }\n\
               pub fn helper(args: &[String]) { smuggle!(args); }\n";
    let err = diff_against_whitelist(&scan_source("cli.rs", src))
        .expect_err("macro-wrapped call must violate the invariant");
    assert!(
        err.contains("macro-mention exec_real_git"),
        "violation report must flag the macro mention, got:\n{err}"
    );
}

/// A shadow re-definition of the exec fn in another file is a confusion
/// vector (review reads `exec_real_git(...)` and assumes the real one) —
/// must be detected as an unexpected def.
#[test]
fn shadow_definition_in_sibling_module_is_detected() {
    let src = "fn exec_real_git(_args: &[String], _chdir: Option<&str>) {}\n";
    let err = diff_against_whitelist(&scan_source("cli.rs", src))
        .expect_err("shadow definition must violate the invariant");
    assert!(
        err.contains("cli.rs :: <top-level> :: def exec_real_git"),
        "violation report must flag the shadow def, got:\n{err}"
    );
}

/// Deleting a whitelisted call site must ALSO fail (missing direction), so the
/// whitelist can never silently rot ahead of the code.
#[test]
fn removed_call_site_is_detected() {
    // Scan only a stub — every whitelisted occurrence is "missing".
    let err = diff_against_whitelist(&scan_source("lib.rs", "fn main() {}\n"))
        .expect_err("empty tree must report missing whitelist entries");
    assert!(
        err.contains("MISSING"),
        "violation report must flag missing entries, got:\n{err}"
    );
}
