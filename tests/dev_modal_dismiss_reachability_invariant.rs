//! #3547 guard: the PTY read loop must stay reachable in EVERY transport mode.
//!
//! The dev-channel startup modal is the single point on the fresh-restart
//! self-recovery chain. Nothing else can answer it: the modal blocks Claude
//! before it starts its MCP servers, one of which is the channel bridge that
//! publishes the locator the self-kick waits on. So the only thing that can
//! clear it is the daemon reading the PTY and pressing Enter.
//!
//! Today `spawn_agent` starts `pty_read_loop` unconditionally, so a
//! ChannelBridge generation is still scanned even though it never uses the PTY
//! for delivery. That is load-bearing, and it is exactly the kind of thing a
//! later "we do not need the PTY in structured mode" optimisation removes
//! without noticing: doing so would turn #3547 from an intermittent
//! dismiss-miss into a permanent deadlock for every bridge-mode restart.
//!
//! This pins it with an AST walk rather than a text scan, so reformatting,
//! renaming the local binding, or moving the call cannot quietly disarm it.

use syn::visit::Visit;

const TARGET: &str = "src/agent/mod.rs";
/// The read loop's entry point. Renaming it fails the anchor assertion below
/// rather than silently making this invariant vacuous.
const READ_LOOP_FN: &str = "pty_read_loop";
/// Identifiers that mark a conditional as transport-mode-dependent.
const TRANSPORT_IDENTS: &[&str] = &["TransportMode", "LegacyPty", "ChannelBridge", "StructuredTransport"];

#[derive(Default)]
struct IdentScan {
    idents: Vec<String>,
}

impl<'ast> Visit<'ast> for IdentScan {
    fn visit_ident(&mut self, node: &'ast proc_macro2::Ident) {
        self.idents.push(node.to_string());
    }
}

fn idents_of<T>(node: &T) -> Vec<String>
where
    for<'a> IdentScan: VisitOne<'a, T>,
{
    let mut scan = IdentScan::default();
    scan.visit_one(node);
    scan.idents
}

/// Tiny shim so one helper can walk either an expression or a block.
trait VisitOne<'a, T> {
    fn visit_one(&mut self, node: &'a T);
}
impl<'a> VisitOne<'a, syn::Expr> for IdentScan {
    fn visit_one(&mut self, node: &'a syn::Expr) {
        self.visit_expr(node);
    }
}
impl<'a> VisitOne<'a, syn::Block> for IdentScan {
    fn visit_one(&mut self, node: &'a syn::Block) {
        self.visit_block(node);
    }
}
impl<'a> VisitOne<'a, syn::Pat> for IdentScan {
    fn visit_one(&mut self, node: &'a syn::Pat) {
        self.visit_pat(node);
    }
}

fn mentions_transport(idents: &[String]) -> bool {
    idents
        .iter()
        .any(|i| TRANSPORT_IDENTS.iter().any(|t| i == t))
}

fn starts_read_loop(idents: &[String]) -> bool {
    idents.iter().any(|i| i == READ_LOOP_FN)
}

#[derive(Default)]
struct Audit {
    violations: Vec<String>,
    read_loop_sites: usize,
}

impl<'ast> Visit<'ast> for Audit {
    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        if mentions_transport(&idents_of(&*node.cond)) {
            if starts_read_loop(&idents_of(&node.then_branch)) {
                self.violations
                    .push(format!("`if` on transport mode gates {READ_LOOP_FN}"));
            }
            if let Some((_, else_branch)) = node.else_branch.as_ref() {
                if starts_read_loop(&idents_of(&**else_branch)) {
                    self.violations
                        .push(format!("`else` of a transport-mode `if` gates {READ_LOOP_FN}"));
                }
            }
        }
        syn::visit::visit_expr_if(self, node);
    }

    fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
        // The discriminating name usually lives in the ARM PATTERNS
        // (`TransportMode::LegacyPty => ...`), not in the scrutinee, which is
        // typically a lower-case binding. Checking only the scrutinee is the
        // hole the counter-example test below caught.
        let scrutinee_is_transport = mentions_transport(&idents_of(&*node.expr));
        for arm in &node.arms {
            let arm_is_transport =
                scrutinee_is_transport || mentions_transport(&idents_of(&arm.pat));
            if arm_is_transport && starts_read_loop(&idents_of(&*arm.body)) {
                self.violations
                    .push(format!("a transport-mode `match` arm gates {READ_LOOP_FN}"));
            }
        }
        syn::visit::visit_expr_match(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*node.func {
            if path
                .path
                .segments
                .last()
                .is_some_and(|s| s.ident == READ_LOOP_FN)
            {
                self.read_loop_sites += 1;
            }
        }
        syn::visit::visit_expr_call(self, node);
    }
}

fn audit(source: &str) -> Audit {
    let file = syn::parse_file(source).expect("target must parse as Rust");
    let mut audit = Audit::default();
    audit.visit_file(&file);
    audit
}

#[test]
fn pty_read_loop_is_never_gated_on_transport_mode_3547() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(TARGET);
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let result = audit(&source);

    // Fail-loud anchor: if the entry point is renamed or moved out of this file
    // the check above would pass vacuously, which is worse than failing.
    assert!(
        result.read_loop_sites > 0,
        "{TARGET} no longer calls `{READ_LOOP_FN}` — this invariant has gone \
         vacuous. Point it at the new entry point instead of deleting it."
    );
    assert!(
        result.violations.is_empty(),
        "#3547: the PTY read loop is the only thing that can answer the \
         dev-channel startup modal, and the modal blocks the MCP server whose \
         locator the self-kick waits on. Gating it on transport mode makes that \
         deadlock permanent for bridge-mode restarts. Found: {:?}",
        result.violations
    );
}

/// The counter-example: the audit must actually fire on the shape it forbids.
/// Without this, a walk that silently matches nothing would look identical to a
/// clean tree.
#[test]
fn the_audit_rejects_a_transport_gated_read_loop() {
    let bad = r#"
        fn spawn_agent() {
            if mode == TransportMode::LegacyPty {
                pty_read_loop(&mut reader, &ctx, capture);
            }
        }
    "#;
    let result = audit(bad);
    assert_eq!(
        result.violations.len(),
        1,
        "the audit must flag a read loop nested under a transport-mode `if`"
    );

    let bad_match = r#"
        fn spawn_agent() {
            match transport_mode {
                TransportMode::LegacyPty => pty_read_loop(&mut reader, &ctx, capture),
                _ => {}
            }
        }
    "#;
    assert_eq!(
        audit(bad_match).violations.len(),
        1,
        "the audit must flag a read loop nested under a transport-mode `match` arm"
    );

    let good = r#"
        fn spawn_agent() {
            pty_read_loop(&mut reader, &ctx, capture);
            if mode == TransportMode::LegacyPty { deliver_via_pty(); }
        }
    "#;
    let clean = audit(good);
    assert!(clean.violations.is_empty(), "the healthy shape must pass");
    assert_eq!(clean.read_loop_sites, 1, "the anchor must still count the call");
}
