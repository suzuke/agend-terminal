//! Snapshot fail-open invariant — every reader of the snapshot
//! projection must be a reviewed fail-open consumer.
//!
//! Per `docs/SOURCE-OF-TRUTH.md`, `snapshot.json` is
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
//! ## Detection — a `syn` AST walk (PR #2612 rework #3)
//!
//! Earlier cuts scanned source text for literal substrings and were defeated by
//! ordinary Rust the reviewer injected: alias/bare-call/fn-pointer, then
//! crate-level grouped imports. Rust's `use` grammar cannot be enumerated with
//! literal substrings — that is a METHOD failure. This cut parses each file
//! with `syn`, which NORMALIZES every syntactic variant into the same tree.
//!
//! A file is a "snapshot projection user" iff, OUTSIDE `#[cfg(test)]`, it has a
//! use-tree or an expression/type path that names the `crate::snapshot`
//! projection module under any of the FOUR Rust module-path roots:
//!   - `crate` — PRECISE: `snapshot` must sit directly under `crate`, which
//!     excludes the identically named `crate::…::per_tick::snapshot` rotation
//!     module.
//!   - `super` / `self` — CONSERVATIVE: any `snapshot` path segment counts;
//!     module depth is NOT resolved (wide is stable), so a false hit becomes a
//!     benign audited entry. (A glob such as `use super::*;` is NOT matched —
//!     any real read it enables is a bare `snapshot::X` path caught below.)
//!   - bare `snapshot` root — CONSERVATIVE: reachable-as-projection only from
//!     the crate root (`main.rs`/`lib.rs` declare `mod snapshot;`); elsewhere a
//!     same-named child module (per_tick), which audits benignly.
//!
//! Every such file must appear in [`AUDITED_FILES`] with a role.
//!
//! ## Completeness argument (name-resolution-free)
//!
//! To read the projection a file must, in ITS OWN text, name the module — and
//! the module's name is `snapshot`, reachable only via one of the four roots
//! above (Rust 2018+ forbids a bare non-rooted `use` of a local module, and
//! `::snapshot` would be a nonexistent extern crate). All four are matched, so
//! the reference is always caught LOCALLY; no cross-file resolution is needed.
//! The only token-free escapes are closed:
//!   - a `pub`/`pub(crate)` RE-EXPORT of `crate::snapshot` (precise-crate form)
//!     is auto-banned; the re-exporting file is itself caught, so the ban is
//!     enforceable there.
//!   - aliasing the crate root, `use crate as c; c::snapshot::…`, is banned.
//!
//! Residual limits (accepted, mirroring `enqueue_drop_invariant`'s honest
//! limits): a `pub` re-export via a `super`/`self`/bare root is flagged as an
//! audited file (so it is REVIEWED) but not auto-banned — distinguishing it
//! from the benign per_tick re-export needs module resolution; a reviewer
//! catches a leaky re-export at audit time. A read manufactured entirely inside
//! a macro body defined in another file, or via `build.rs`/`include!`, is out
//! of scope for an AST-of-source scan. None of these exist today.
//!
//! `scanner_catches_all_bypass_forms` below is the fence's fence: it proves
//! every reviewer form across all four roots is caught and the bans fire.
//!
//! ## The allowlist
//!
//! Adding a file to `AUDITED_FILES` REQUIRES review of the new user — for a
//! `reader`, a fail-open review; the list records that review, it does not
//! waive it. The behavioral companion
//! `snapshot_missing_fails_open_for_dispatch_deciders` (unit test in
//! `src/snapshot.rs`) proves the primitives return the conservative sentinel on
//! a missing/corrupt/old-format snapshot; it lives there because the `snapshot`
//! module is not on the curated `lib.rs` surface.

use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};

/// Every production file that references the `crate::snapshot` projection, with
/// its role. `reader` entries carry the reviewed reason a stale/missing
/// snapshot drives only a REVERSIBLE action; `writer`/`type-user`/`per-tick`
/// produce, type, or merely name-clash with the projection and do not read it
/// for a decision. Suffix-matched against the `src/`-relative path.
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
        "src/inbox/storage.rs",
        "reader",
        "#2622 reclaim_stale_delivering busy gate; missing -> not-busy -> reclaim proceeds (today's behavior, unchanged). Reversible: only shifts REDELIVERY TIMING of an already-authoritative inbox row (never invents or drops a message), and is itself hard-capped at RECLAIM_BUSY_HARD_CAP_SECS so a stale/wedged busy signal can't zombie a row in `delivering` forever.",
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
    (
        "src/inbox/tests.rs",
        "writer",
        "#2622 test fixture: writes a synthetic snapshot (crate::snapshot::save/AgentSnapshot) so agent_is_busy reads a deterministic value in reclaim_stale_delivering tests. Not a reader; no production decision reads snapshot here — the whole file is #[cfg(test)]-only via its `mod tests;` declaration in inbox/mod.rs, which the AST scanner (parsing this file standalone) cannot see.",
    ),
    (
        "src/daemon/per_tick/mod.rs",
        "per-tick",
        "benign name-clash: `pub(crate) use snapshot::SnapshotRotationHandler` re-exports the SAME-NAMED per_tick child module (crate::daemon::per_tick::snapshot, the rotation handler), NOT the crate::snapshot projection. The conservative bare-root rule cannot resolve the two apart, so this is audited as benign. Reads no snapshot.",
    ),
    (
        "src/daemon/ci_watch/poller_tests.rs",
        "writer",
        "#2870 test fixture: writes a synthetic snapshot (crate::snapshot::save/AgentSnapshot) so the watchdog re-nudge test has a deterministic idle agent. Not a reader; no production decision reads snapshot here — the file is tests-only (poller_tests.rs).",
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

/// Does a use-tree name a `snapshot` segment anywhere in it?
fn use_tree_names_snapshot(t: &syn::UseTree) -> bool {
    match t {
        syn::UseTree::Path(p) => p.ident == "snapshot" || use_tree_names_snapshot(&p.tree),
        syn::UseTree::Name(n) => n.ident == "snapshot",
        syn::UseTree::Rename(r) => r.ident == "snapshot",
        syn::UseTree::Group(g) => g.items.iter().any(use_tree_names_snapshot),
        syn::UseTree::Glob(_) => false,
    }
}

/// Immediately after `crate`, does the subtree name the `snapshot` projection
/// module directly? (Precise: excludes `crate::…::per_tick::snapshot`.)
fn after_crate_is_snapshot(t: &syn::UseTree) -> bool {
    match t {
        syn::UseTree::Path(p) => p.ident == "snapshot",
        syn::UseTree::Name(n) => n.ident == "snapshot",
        syn::UseTree::Rename(r) => r.ident == "snapshot",
        syn::UseTree::Group(g) => g.items.iter().any(after_crate_is_snapshot),
        // A glob (`use crate::*;`) is NOT matched here: it does not name
        // `snapshot`, and any actual read it enables is a bare `snapshot::X`
        // path caught by `path_hit`. Flagging every glob would be noise.
        syn::UseTree::Glob(_) => false,
    }
}

/// A match against the projection: `any` = referenced at all; `precise_crate` =
/// via the unambiguous `crate::snapshot` form (the only form that is auto-banned
/// when re-exported).
#[derive(Default, Clone, Copy)]
struct Hit {
    any: bool,
    precise_crate: bool,
}

impl Hit {
    fn merge(self, o: Hit) -> Hit {
        Hit {
            any: self.any || o.any,
            precise_crate: self.precise_crate || o.precise_crate,
        }
    }
}

fn use_tree_hit(t: &syn::UseTree) -> Hit {
    match t {
        syn::UseTree::Path(p) if p.ident == "crate" => {
            let precise = after_crate_is_snapshot(&p.tree);
            Hit {
                any: precise,
                precise_crate: precise,
            }
        }
        // super::/self:: rooted — conservative (module depth unresolved): the
        // subtree must NAME `snapshot` (e.g. `use super::snapshot`). A super/self
        // glob is not matched — a real read through it is a bare `snapshot::X`
        // path caught by `path_hit`.
        syn::UseTree::Path(p) if p.ident == "super" || p.ident == "self" => Hit {
            any: use_tree_names_snapshot(&p.tree),
            precise_crate: false,
        },
        // Bare `snapshot` root (crate-root child, or same-named per_tick child).
        syn::UseTree::Path(p) if p.ident == "snapshot" => Hit {
            any: true,
            precise_crate: false,
        },
        syn::UseTree::Name(n) if n.ident == "snapshot" => Hit {
            any: true,
            precise_crate: false,
        },
        syn::UseTree::Rename(r) if r.ident == "snapshot" => Hit {
            any: true,
            precise_crate: false,
        },
        syn::UseTree::Group(g) => g
            .items
            .iter()
            .map(use_tree_hit)
            .fold(Hit::default(), Hit::merge),
        _ => Hit::default(),
    }
}

/// True if this use tree aliases the crate root, e.g. `use crate as c;`.
fn use_tree_aliases_crate_root(t: &syn::UseTree) -> bool {
    match t {
        syn::UseTree::Rename(r) => r.ident == "crate",
        syn::UseTree::Group(g) => g.items.iter().any(use_tree_aliases_crate_root),
        _ => false,
    }
}

fn path_hit(p: &syn::Path) -> Hit {
    let segs: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
    if segs
        .windows(2)
        .any(|w| w[0] == "crate" && w[1] == "snapshot")
    {
        return Hit {
            any: true,
            precise_crate: true,
        };
    }
    // Conservative: `snapshot` used as a MODULE segment — i.e. `snapshot::X`
    // (module access), NOT a terminal `snapshot` (a variable/fn/field named
    // snapshot). Rooted at super/self, or bare `snapshot` as the first segment.
    let n = segs.len();
    if n >= 2 {
        let first = segs[0].as_str();
        let snapshot_as_module = segs[..n - 1].iter().any(|s| s == "snapshot");
        if snapshot_as_module && matches!(first, "super" | "self" | "snapshot") {
            return Hit {
                any: true,
                precise_crate: false,
            };
        }
    }
    Hit::default()
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
        let hit = use_tree_hit(&i.tree);
        if hit.any {
            self.refs_projection = true;
            // Auto-ban a re-export only for the unambiguous crate::snapshot
            // form (a super/self/bare pub re-export cannot be told apart from
            // the benign per_tick one without resolution — it is audited, not
            // auto-banned; see the module doc's residual limits).
            if hit.precise_crate && !matches!(i.vis, syn::Visibility::Inherited) {
                self.reexports_projection = true;
            }
        }
        visit::visit_item_use(self, i);
    }
    fn visit_path(&mut self, p: &'ast syn::Path) {
        if path_hit(p).any {
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
         one):\n{}",
        parse_failures.join("\n")
    );

    assert!(
        banned.is_empty(),
        "Forbidden snapshot reference form(s) in production.\n\n\
         A `pub`/`pub(crate)` re-export of crate::snapshot, or a crate-root \
         alias, lets a downstream file read the projection WITHOUT any local \
         `snapshot` reference. Reference the projection only in fully-qualified, \
         non-re-exported form.\n\n{}",
        banned.join("\n")
    );

    assert!(
        unaudited.is_empty(),
        "Production file(s) reference crate::snapshot but are not in \
         AUDITED_FILES.\n\n\
         Any reader of the snapshot projection must fail-open: a stale/missing \
         snapshot may cause only a REVERSIBLE action (nudge/warn/log/timing), \
         never an irreversible one. Add each file to AUDITED_FILES with a role \
         (reader/writer/type-user) and — for a reader — a one-line reversibility \
         rationale, only after confirming it fails open.\n\n{}",
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

/// The fence's fence: prove the AST walk catches every read form across all
/// four module-path roots (crate / super / self / bare), that the bans fire,
/// and that benign non-projection references are not flagged.
#[test]
fn scanner_catches_all_bypass_forms() {
    let caught = |src: &str| -> bool {
        let s = scan_file(src).expect("parse snippet");
        s.refs_projection || s.reexports_projection || s.aliases_crate_root
    };

    // Every read form must be caught. Forms 1-4: first review. 5-6: crate-level
    // grouped imports (second review). 7-8: glob / multi-line. 9-12: the
    // super/self/bare roots (third review — 9 is reviewer4's exact injection).
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
        ("form9 super import (reviewer4)", "use super::snapshot;\nfn f(h:&std::path::Path){ let _=snapshot::agent_is_busy(h,\"a\"); }"),
        ("form10 self import", "use self::snapshot;\nfn f(h:&std::path::Path){ let _=snapshot::load(h); }"),
        ("form11 super multi-level path", "fn f(h:&std::path::Path){ let _=super::super::snapshot::load(h); }"),
        ("form12 bare path", "fn f(h:&std::path::Path){ let _=snapshot::load(h); }"),
        ("form13 super path call", "fn f(h:&std::path::Path){ let _=super::snapshot::agent_is_busy(h,\"a\"); }"),
        ("form14 super glob", "use super::*;\nfn f(h:&std::path::Path){ let _=snapshot::load(h); }"),
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
    // rotation module addressed via a crate/bare path, a `snapshot`-PREFIXED
    // identifier (not the module), an unrelated identifier, a foreign Snapshot
    // type, and a comment (syn drops comments).
    let benign: &[(&str, &str)] = &[
        (
            "per_tick via crate path",
            "use crate::daemon::per_tick::snapshot::SnapshotRotationHandler;",
        ),
        (
            "per_tick via bare path",
            "fn f(){ let _ = daemon::per_tick::snapshot::timings(); }",
        ),
        (
            "snapshot-prefixed fn ident",
            "fn f(){ let _ = super::snapshot_handler_timings(); }",
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
