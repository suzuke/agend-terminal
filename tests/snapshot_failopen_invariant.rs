//! [fugu §3.4] Snapshot fail-open invariant — every reader of the snapshot
//! projection must be a reviewed fail-open consumer.
//!
//! `workspace/fugu-0acdd8/agend-terminal-solutions.md` §3.4: `snapshot.json` is
//! a FAIL-OPEN projection of the in-memory `StateTracker`. It MAY be read by
//! deciders, but — per the corrected rule — a decision that reads it MUST
//! fail-open, be idempotent, and NEVER cause an IRREVERSIBLE action from a
//! stale or missing snapshot.
//!
//! ## Irreversible-vs-reversible boundary (what this invariant guards)
//!
//! IRREVERSIBLE (forbidden on a snapshot read): deleting/overwriting a
//! source-of-truth store (worktree, branch, task/inbox/decision file), or
//! fabricating an OUTBOUND send (a Telegram/Discord/PTY message that would not
//! exist under a correct snapshot).
//!
//! REVERSIBLE / acceptable (fail-open): a nudge, a warn, a log line, a
//! re-nudge, or delivering an ALREADY-AUTHORITATIVE inbox message whose TIMING
//! (not existence) the snapshot gates — the payload is owned by the inbox
//! source-of-truth and is bounded by `MAX_DEFER`, so a stale snapshot at worst
//! shifts delivery timing, never invents or drops a message.
//!
//! ## Detection — a `syn` AST walk (PR #2612 rework #2)
//!
//! The first two cuts scanned source text for literal call/import substrings.
//! Both were defeated: the reviewer (cross-vantage) injected real production
//! readers that string scanning missed — first alias/bare-call/fn-pointer
//! forms, then crate-level grouped imports `use crate::{snapshot as snap};`
//! and `use crate::{snapshot::{load}};`. Rust's `use` grammar (grouped,
//! nested, aliased, glob, arbitrary spacing, multi-line) cannot be enumerated
//! with literal substrings — that is a METHOD failure, not a patch gap.
//!
//! This cut parses each production file with `syn` and walks the AST, which
//! NORMALIZES every syntactic variant into the same tree. A file is a
//! "snapshot projection user" iff it contains, OUTSIDE `#[cfg(test)]`:
//!   - a `use` tree that imports from `crate::snapshot` (any form — path,
//!     name, rename, grouped, nested, glob), OR
//!   - an expression/type path with consecutive segments `crate :: snapshot`.
//!
//! Every such file must appear in [`AUDITED_FILES`] with a role.
//!
//! ## Completeness argument (name-resolution-free)
//!
//! To read the projection a file must, in ITS OWN text, either (a) import a
//! primitive/the module — caught by the use-tree walk regardless of grouping —
//! or (b) name it fully-qualified `crate::snapshot::…` — caught by the path
//! walk. No cross-file name resolution is needed: the reference is always
//! LOCAL to the reading file. The only two ways to reference the projection
//! WITHOUT the local token are closed by bans below, both currently at zero
//! violations:
//!   1. a `pub`/`pub(crate)` RE-EXPORT of `crate::snapshot` (would let a
//!      token-free downstream file read via the alias) — banned; the
//!      re-exporting file itself is caught, so the ban is enforceable there.
//!   2. aliasing the crate root, `use crate as c; c::snapshot::…` — banned.
//!
//! Root-anchoring on `crate::snapshot` excludes the unrelated, identically
//! named `crate::daemon::per_tick::snapshot` rotation module.
//!
//! Residual limits (accepted, mirroring `enqueue_drop_invariant`'s honest
//! limits): a read manufactured entirely inside a macro body defined in
//! another file, or via `build.rs`/`include!`, is out of scope for an
//! AST-of-source scan. None exist and such a form would be extraordinary.
//!
//! `scanner_catches_all_bypass_forms` below is the fence's fence: it proves
//! all six reviewer forms plus glob/multi-line are caught and the bans fire.
//!
//! ## The allowlist
//!
//! Adding a file to `AUDITED_FILES` REQUIRES review of the new user — for a
//! `reader`, a fail-open review; the list records that review, it does not
//! waive it. The behavioral companion
//! `snapshot_missing_fails_open_for_dispatch_deciders` (unit test in
//! `src/snapshot.rs`) proves the primitives return the conservative sentinel
//! on a missing/corrupt/old-format snapshot; it lives there because the
//! `snapshot` module is not on the curated `lib.rs` surface.

use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};

/// Every production file that references the `crate::snapshot` projection, with
/// its role. `reader` entries carry the reviewed reason a stale/missing
/// snapshot drives only a REVERSIBLE action; `writer`/`type-user` produce or
/// type the projection and do not read it for a decision. Suffix-matched
/// against the `src/`-relative path.
const AUDITED_FILES: &[(&str, &str, &str)] = &[
    (
        "src/daemon/dispatch_idle/mod.rs",
        "reader",
        "idle silence gate; missing -> target_is_working=false -> still FIRES the nudge (fail-open by design, #1516). Reversible: a nudge.",
    ),
    (
        "src/inbox/notify.rs",
        "reader",
        "inject defer/drain TIMING gate; missing -> not-busy -> inject now (bounded by MAX_DEFER). Payload authoritative from the inbox source-of-truth; the snapshot gates timing, not existence.",
    ),
    (
        "src/daemon/handoff_timeout_watchdog.rs",
        "reader",
        "re-nudge gate + telemetry; missing -> not-busy -> re-nudge. Reversible: a nudge.",
    ),
    (
        "src/reply_ledger.rs",
        "reader",
        "sweep gate; missing -> not-busy -> emit_warn + NudgeAgent. Reversible: a warn/nudge, never a delete.",
    ),
    (
        "src/daemon/mod.rs",
        "reader",
        "startup diagnostic only: logs 'previous snapshot found' via tracing::info; missing -> skip the log. No action.",
    ),
    (
        "src/api/handlers/query.rs",
        "reader",
        "read-only: builds the status query response; missing -> empty {agents:[], timestamp:null}. No mutation.",
    ),
    (
        "src/bugreport.rs",
        "reader",
        "read-only: renders the snapshot section of a bug report; missing -> section empty. No mutation.",
    ),
    (
        "src/daemon/per_tick/snapshot.rs",
        "writer",
        "the per-tick rotation handler: PRODUCES the projection via crate::snapshot::save. Not a reader; no decision reads snapshot here.",
    ),
];

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_rs_files(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

fn rel_str(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// True if any attribute is `#[cfg(test)]` (or a `cfg(...)` mentioning `test`).
fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && matches!(&a.meta, syn::Meta::List(l) if l.tokens.to_string().contains("test"))
    })
}

/// Given a use-subtree positioned immediately after the `crate` root, does it
/// bring the `snapshot` projection module (or something under it) into scope?
fn after_crate_is_snapshot(t: &syn::UseTree) -> bool {
    match t {
        syn::UseTree::Path(p) => p.ident == "snapshot",
        syn::UseTree::Name(n) => n.ident == "snapshot",
        syn::UseTree::Rename(r) => r.ident == "snapshot",
        syn::UseTree::Group(g) => g.items.iter().any(after_crate_is_snapshot),
        // `use crate::*;` glob-imports the crate root, which includes
        // `snapshot`; treat conservatively as a hit.
        syn::UseTree::Glob(_) => true,
    }
}

/// True if this use tree imports from `crate::snapshot` in any form.
fn use_tree_hits_projection(t: &syn::UseTree) -> bool {
    match t {
        syn::UseTree::Path(p) if p.ident == "crate" => after_crate_is_snapshot(&p.tree),
        // A leading group, e.g. `use {crate::snapshot, ...};`.
        syn::UseTree::Group(g) => g.items.iter().any(use_tree_hits_projection),
        _ => false,
    }
}

/// True if this use tree aliases the crate root, e.g. `use crate as c;` — a
/// token-free escape hatch (`c::snapshot::…`) that the ban forbids.
fn use_tree_aliases_crate_root(t: &syn::UseTree) -> bool {
    match t {
        syn::UseTree::Rename(r) => r.ident == "crate",
        syn::UseTree::Group(g) => g.items.iter().any(use_tree_aliases_crate_root),
        _ => false,
    }
}

/// True if a path names consecutive segments `crate :: snapshot` (the
/// projection), excluding the identically named `…::per_tick::snapshot`.
fn path_is_crate_snapshot(p: &syn::Path) -> bool {
    let idents: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
    idents
        .windows(2)
        .any(|w| w[0] == "crate" && w[1] == "snapshot")
}

#[derive(Default)]
struct Scan {
    refs_projection: bool,
    reexports_projection: bool,
    aliases_crate_root: bool,
}

impl<'ast> Visit<'ast> for Scan {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        if has_cfg_test(&i.attrs) {
            return;
        }
        visit::visit_item_mod(self, i);
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        if has_cfg_test(&i.attrs) {
            return;
        }
        visit::visit_item_fn(self, i);
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if has_cfg_test(&i.attrs) {
            return;
        }
        visit::visit_item_impl(self, i);
    }
    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        if has_cfg_test(&i.attrs) {
            return;
        }
        if use_tree_aliases_crate_root(&i.tree) {
            self.aliases_crate_root = true;
        }
        if use_tree_hits_projection(&i.tree) {
            self.refs_projection = true;
            // pub / pub(crate) / pub(super) etc. re-export reaches other files.
            if !matches!(i.vis, syn::Visibility::Inherited) {
                self.reexports_projection = true;
            }
        }
        visit::visit_item_use(self, i);
    }
    fn visit_path(&mut self, p: &'ast syn::Path) {
        if path_is_crate_snapshot(p) {
            self.refs_projection = true;
        }
        visit::visit_path(self, p);
    }
}

fn scan_file(content: &str) -> Result<Scan, syn::Error> {
    let ast = syn::parse_file(content)?;
    let mut scan = Scan::default();
    scan.visit_file(&ast);
    Ok(scan)
}

#[test]
fn snapshot_stale_never_causes_irreversible_action() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = root.join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    files.sort();

    let mut parse_failures: Vec<String> = Vec::new();
    let mut banned: Vec<String> = Vec::new();
    let mut unaudited: Vec<String> = Vec::new();
    let mut matched: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for f in &files {
        let rel = rel_str(f, root);
        // The definition module itself is not a consumer.
        if rel == "src/snapshot.rs" {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(f) else {
            continue;
        };
        let scan = match scan_file(&content) {
            Ok(s) => s,
            // A file we cannot parse could hide a reader — surface it, never
            // silently skip.
            Err(e) => {
                parse_failures.push(format!("  {rel}: {e}"));
                continue;
            }
        };
        if scan.reexports_projection {
            banned.push(format!(
                "  {rel}: pub/pub(crate) re-export of crate::snapshot"
            ));
        }
        if scan.aliases_crate_root {
            banned.push(format!("  {rel}: crate-root alias `use crate as …`"));
        }
        if scan.refs_projection {
            match AUDITED_FILES
                .iter()
                .find(|(suffix, _, _)| rel.ends_with(suffix))
            {
                Some((suffix, _, _)) => {
                    matched.insert(*suffix);
                }
                None => unaudited.push(format!("  {rel}")),
            }
        }
    }

    assert!(
        parse_failures.is_empty(),
        "Could not parse these source files with syn (a reader could hide in \
         one). Fix the parse or narrow the walk:\n{}",
        parse_failures.join("\n")
    );

    assert!(
        banned.is_empty(),
        "Forbidden snapshot reference form(s) in production.\n\n\
         A `pub`/`pub(crate)` re-export of crate::snapshot, or a crate-root \
         alias, lets a downstream file read the projection WITHOUT any local \
         `snapshot` reference — defeating the audited-files completeness \
         argument. Reference the projection only in fully-qualified, \
         non-re-exported form.\n\n{}",
        banned.join("\n")
    );

    assert!(
        unaudited.is_empty(),
        "Production file(s) reference crate::snapshot but are not in \
         AUDITED_FILES.\n\n\
         Any reader of the snapshot projection must fail-open: a stale/missing \
         snapshot may cause only a REVERSIBLE action (nudge/warn/log/timing), \
         never an irreversible one. Add each file to AUDITED_FILES in this test \
         with a role (reader/writer/type-user) and — for a reader — a one-line \
         reversibility rationale, only after confirming it fails open.\n\n{}",
        unaudited.join("\n")
    );

    let stale: Vec<&str> = AUDITED_FILES
        .iter()
        .map(|(s, _, _)| *s)
        .filter(|s| !matched.contains(s))
        .collect();
    assert!(
        stale.is_empty(),
        "Stale AUDITED_FILES entr(y/ies) — no crate::snapshot reference found \
         in: {stale:?}. Remove the entry to keep the inventory honest.",
    );
}

/// The fence's fence: prove the AST walk catches every read form the reviewer
/// showed the string scans missed (and the crate-level grouped/nested/glob/
/// multi-line variants), that the bans fire, and that benign non-projection
/// references are not flagged.
#[test]
fn scanner_catches_all_bypass_forms() {
    let caught = |src: &str| -> bool {
        let s = scan_file(src).expect("parse snippet");
        s.refs_projection || s.reexports_projection || s.aliases_crate_root
    };

    // Every read form must be caught. Forms 1-4 are the first review's; forms
    // 5-6 are the crate-level grouped imports the second review injected;
    // 7-8 harden glob and multi-line grouping.
    let read_forms: &[(&str, &str)] = &[
        ("baseline call", "fn f(h: &std::path::Path) { let _ = crate::snapshot::load(h); }"),
        ("form1 direct import", "use crate::snapshot::load;\nfn f(h: &std::path::Path){ let _ = load(h); }"),
        ("form2 grouped alias", "use crate::snapshot::{agent_is_busy as busy};\nfn f(h:&std::path::Path){ let _=busy(h,\"a\"); }"),
        ("form3 module alias", "use crate::snapshot as snap;\nfn f(h:&std::path::Path){ let _=snap::agent_is_busy(h,\"a\"); }"),
        ("form4 fn pointer", "fn f(h:&std::path::Path){ let g = crate::snapshot::load; let _=g(h); }"),
        ("form5 crate-grouped alias", "use crate::{snapshot as snap};\nfn f(h:&std::path::Path){ let _=snap::load(h); }"),
        ("form6 crate-grouped nested", "use crate::{snapshot::{load}};\nfn f(h:&std::path::Path){ let _=load(h); }"),
        ("form7 crate glob", "use crate::*;\nfn f(h:&std::path::Path){ let _=snapshot::load(h); }"),
        ("form8 multiline group", "use crate::{\n    snapshot::agent_is_busy,\n    fmt_util,\n};\nfn f(h:&std::path::Path){ let _=agent_is_busy(h,\"a\"); }"),
    ];
    for (name, src) in read_forms {
        assert!(caught(src), "read form NOT caught by the AST fence: {name}");
    }

    // Token-free escape hatches must be caught as bans.
    let ban_forms: &[(&str, &str)] = &[
        ("pub re-export fn", "pub use crate::snapshot::load;"),
        (
            "pub(crate) re-export module",
            "pub(crate) use crate::snapshot as snap;",
        ),
        (
            "crate-root alias",
            "use crate as c;\nfn f(h:&std::path::Path){ let _=c::snapshot::load(h); }",
        ),
    ];
    for (name, src) in ban_forms {
        let s = scan_file(src).expect("parse snippet");
        assert!(
            s.reexports_projection || s.aliases_crate_root,
            "escape-hatch form not banned: {name}"
        );
    }

    // Benign references must NOT be flagged: the identically named per_tick
    // rotation module, an unrelated identifier, a foreign Snapshot type, and a
    // comment (syn drops comments entirely).
    let benign: &[(&str, &str)] = &[
        (
            "per_tick::snapshot module",
            "use crate::daemon::per_tick::snapshot::SnapshotRotationHandler;",
        ),
        (
            "per_tick bare re-export",
            "pub(crate) use snapshot::SnapshotRotationHandler;",
        ),
        (
            "unrelated identifier",
            "fn f(){ let snapshot_count = 3; let _ = snapshot_count; }",
        ),
        (
            "foreign Snapshot type",
            "fn f(x: other_crate::Snapshot){ let _ = x; }",
        ),
        (
            "comment mention only",
            "// reads crate::snapshot::load elsewhere\nfn f(){}",
        ),
    ];
    for (name, src) in benign {
        assert!(!caught(src), "benign reference wrongly flagged: {name}");
    }
}
