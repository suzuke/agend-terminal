//! PR-D6/F1 entry-point PIN: BOTH deployment paths must fire the retired-flag
//! boot warn (`worktree_cleanup::warn_if_prune_live_retired`).
//!
//! The daemon and the owned-app process are mutually exclusive per process
//! (`main.rs` dispatch: `Start` → `daemon::run`/`run_core`; `App` → `app::run`
//! → `run_app`). The LIVE fleet daemon actually runs the app-mode path, so a
//! warn wired ONLY into `run_core` is dead in production. This is exactly the
//! `#1720/#685` silent-dead-in-app class the codebase keeps re-hitting, so the
//! wiring is pinned by an invariant rather than left to review vigilance.
//!
//! Detection is a `syn` AST walk (mirrors `snapshot_failopen_invariant.rs`):
//! parse each file, locate the target fn by name, and assert its body contains
//! a call path whose final segment is `warn_if_prune_live_retired`. Robust to
//! formatting, `crate::`/bare call form, and reordering — a substring grep over
//! the whole file would pass even if the call sat in an unrelated fn.

use std::path::Path;
use syn::visit::{self, Visit};

/// Walks a function body looking for a path ending in the target ident.
struct CallFinder<'a> {
    target: &'a str,
    found: bool,
}

impl<'ast> Visit<'ast> for CallFinder<'_> {
    fn visit_path(&mut self, p: &'ast syn::Path) {
        if p.segments
            .last()
            .is_some_and(|seg| seg.ident == self.target)
        {
            self.found = true;
        }
        visit::visit_path(self, p);
    }
}

/// Finds the free fn `fn_name` in the file and reports whether its body calls
/// `call_ident`. Descends into nested `mod` blocks so a fn inside `mod tests`
/// (not our case here, but keeps the walk total) is still reachable.
struct FnBodyScanner<'a> {
    fn_name: &'a str,
    call_ident: &'a str,
    fn_seen: bool,
    call_in_fn: bool,
}

impl<'ast> Visit<'ast> for FnBodyScanner<'_> {
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        if i.sig.ident == self.fn_name {
            self.fn_seen = true;
            let mut finder = CallFinder {
                target: self.call_ident,
                found: false,
            };
            finder.visit_block(&i.block);
            if finder.found {
                self.call_in_fn = true;
            }
        }
        visit::visit_item_fn(self, i);
    }
}

fn scan_fn_body_for_call(rel_path: &str, fn_name: &str, call_ident: &str) -> (bool, bool) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let full = root.join(rel_path);
    let src =
        std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("read {}: {e}", full.display()));
    let file = syn::parse_file(&src).unwrap_or_else(|e| panic!("parse {}: {e}", full.display()));
    let mut scanner = FnBodyScanner {
        fn_name,
        call_ident,
        fn_seen: false,
        call_in_fn: false,
    };
    scanner.visit_file(&file);
    (scanner.fn_seen, scanner.call_in_fn)
}

/// PR-D6/F1: both `run_core` (daemon) and `run_app` (owned app — the path the
/// LIVE fleet daemon actually runs) must call the retired-flag boot warn.
#[test]
fn both_startup_paths_warn_prune_live_retired_d6() {
    // #2453 Slice 2: run_app delegates the boot warn to `app_boot_preflight`;
    // pin BOTH the delegation edge and the warn itself so the transitive
    // guarantee cannot silently break at either hop.
    for (path, func, call) in [
        (
            "src/daemon/mod.rs",
            "run_core",
            "warn_if_prune_live_retired",
        ),
        ("src/app/mod.rs", "run_app", "app_boot_preflight"),
        (
            "src/app/mod.rs",
            "app_boot_preflight",
            "warn_if_prune_live_retired",
        ),
    ] {
        let (fn_seen, call_in_fn) = scan_fn_body_for_call(path, func, call);
        assert!(fn_seen, "expected to find `fn {func}` in {path}");
        assert!(
            call_in_fn,
            "`fn {func}` in {path} must call `{call}` — the \
             retired-flag boot warn is dead on that deployment path otherwise \
             (daemon `run_core` and owned-app `run_app` are mutually exclusive \
             per process, so each boot fires exactly one)"
        );
    }
}
