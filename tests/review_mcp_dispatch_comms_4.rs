//! Verification/reproduction test for the `mcp-dispatch-comms` review batch,
//! finding 4 (low / resource-leak).
//!
//! A plain self-dispatch with a `branch` leases + binds a worktree BEFORE
//! the API rejects the self-send, orphaning the worktree.
//!
//! The pinned FIX: an UNCONDITIONAL self-send rejection (`*sender == target`,
//! regardless of whether team-orchestrator resolution changed the target) must
//! fire BEFORE the `dispatch_auto_bind_lease_with_source_and_chain` lease — so
//! no worktree is leased for a dispatch the API will reject.
//!
//! ## W2.2 phase-split (#2710)
//! `handle_delegate_task` was split out of `src/mcp/handlers/comms.rs` into
//! named phase stages in `src/mcp/handlers/comms_delegate/mod.rs`. The invariant is
//! unchanged but now spans two functions: the unconditional `*sender == target`
//! reject lives in `resolve_delegate` (an early `return Err(...)`), and the lease
//! lives in `maybe_auto_bind_lease` (which calls
//! `dispatch_auto_bind_lease_with_source_and_chain`). The `handle_delegate_task`
//! orchestrator calls `resolve_delegate` BEFORE `maybe_auto_bind_lease`, so a
//! rejected self-dispatch early-returns and never reaches the lease.
//!
//! ## Detection — a `syn` AST walk (not a literal source `find()`)
//! The prior version string-sliced `fn handle_delegate_task` out of `comms.rs`;
//! the phase-split relocated the function and broke that scan (a false RED that
//! is not a behavior regression). Parsing the AST NORMALIZES fn boundaries,
//! visibility, generics, and relocation, so the invariant survives future phase
//! moves. Mirrors `tests/snapshot_failopen_invariant.rs` (#2612).
//!
//! RED if: the unconditional `*sender == target` reject is absent from
//! `resolve_delegate`, or the orchestrator calls the lease before `resolve_delegate`.

use std::path::PathBuf;
use syn::visit::{self, Visit};

fn parse_comms_delegate() -> syn::File {
    let p =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/mcp/handlers/comms_delegate/mod.rs");
    let src = std::fs::read_to_string(&p).expect("read src/mcp/handlers/comms_delegate/mod.rs");
    syn::parse_file(&src).expect("parse src/mcp/handlers/comms_delegate/mod.rs")
}

fn find_fn<'a>(file: &'a syn::File, name: &str) -> &'a syn::ItemFn {
    file.items
        .iter()
        .find_map(|it| match it {
            syn::Item::Fn(f) if f.sig.ident == name => Some(f),
            _ => None,
        })
        .unwrap_or_else(|| panic!("fn {name} not found in comms_delegate.rs"))
}

/// True iff `cond` is an UNCONDITIONAL `*sender == target` equality — a bare
/// `==` between a deref of `sender` and `target` — NOT a compound `&& ...`
/// (the qualified team-orchestrator loop guard `*sender == target && raw_target
/// != target`, which a plain self-dispatch slips through).
fn is_unconditional_self_eq(cond: &syn::Expr) -> bool {
    let syn::Expr::Binary(b) = cond else {
        return false;
    };
    if !matches!(b.op, syn::BinOp::Eq(_)) {
        return false;
    }
    let left_is_deref_sender = matches!(&*b.left, syn::Expr::Unary(u)
        if matches!(u.op, syn::UnOp::Deref(_))
        && matches!(&*u.expr, syn::Expr::Path(p) if p.path.is_ident("sender")));
    let right_is_target = matches!(&*b.right, syn::Expr::Path(p) if p.path.is_ident("target"));
    left_is_deref_sender && right_is_target
}

/// Finds an `if` whose condition is an unconditional `*sender == target`.
#[derive(Default)]
struct SelfRejectFinder {
    found: bool,
}
impl<'ast> Visit<'ast> for SelfRejectFinder {
    fn visit_expr_if(&mut self, i: &'ast syn::ExprIf) {
        if is_unconditional_self_eq(&i.cond) {
            self.found = true;
        }
        visit::visit_expr_if(self, i);
    }
}

/// Records, in source order, calls to the two phase functions we care about.
#[derive(Default)]
struct CallOrder {
    seq: Vec<String>,
}
impl<'ast> Visit<'ast> for CallOrder {
    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*c.func {
            if let Some(seg) = p.path.segments.last() {
                let name = seg.ident.to_string();
                if name == "resolve_delegate" || name == "maybe_auto_bind_lease" {
                    self.seq.push(name);
                }
            }
        }
        visit::visit_expr_call(self, c);
    }
}

#[test]
fn self_dispatch_rejected_before_auto_bind_lease_mcp_dispatch_comms() {
    let file = parse_comms_delegate();

    // (1) resolve_delegate carries an UNCONDITIONAL `*sender == target` reject.
    let resolve = find_fn(&file, "resolve_delegate");
    let mut reject = SelfRejectFinder::default();
    reject.visit_item_fn(resolve);
    assert!(
        reject.found,
        "resolve_delegate must contain an UNCONDITIONAL `*sender == target` self-send \
         rejection (NOT gated behind `raw_target != target`), so a plain self-dispatch \
         (raw_target == resolved == sender) is rejected before any lease. The only \
         `*sender == target` check is the qualified team-orchestrator loop guard, which \
         lets a plain self-dispatch slip through to the lease."
    );

    // (2) the orchestrator calls resolve_delegate BEFORE maybe_auto_bind_lease
    //     (which owns dispatch_auto_bind_lease_with_source_and_chain) — so a
    //     rejected self-dispatch early-returns and never leases a worktree.
    let orch = find_fn(&file, "handle_delegate_task");
    let mut order = CallOrder::default();
    order.visit_item_fn(orch);
    let resolve_at = order.seq.iter().position(|n| n == "resolve_delegate");
    let lease_at = order.seq.iter().position(|n| n == "maybe_auto_bind_lease");
    assert!(
        resolve_at.is_some(),
        "handle_delegate_task must call resolve_delegate (the self-reject phase)"
    );
    assert!(
        lease_at.is_some(),
        "handle_delegate_task must call maybe_auto_bind_lease (the auto-bind/lease phase)"
    );
    assert!(
        resolve_at < lease_at,
        "handle_delegate_task must call resolve_delegate (unconditional self-reject) BEFORE \
         maybe_auto_bind_lease (dispatch_auto_bind_lease_with_source_and_chain) — else a plain \
         self-dispatch leases + binds a worktree before the self-send is rejected (orphan worktree)."
    );
}
