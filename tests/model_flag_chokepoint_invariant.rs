//! #2744 r1 source invariant: model-flag argv assembly happens ONLY inside
//! the `Backend::push_model_arg` chokepoint. The exact-head review caught
//! three production spawn paths (app/pane_factory ×2, bootstrap/agent_resolve)
//! hand-rolling `args.push("--model")` with `from_command` inference —
//! bypassing the capability gate (Shell/Raw breakage, duplicate flags, `--`
//! ordering, wrapper misclassification). This test bans the raw `"--model"`
//! string literal from non-test production code so a future spawn entrypoint
//! cannot reintroduce inline assembly: to emit the flag you need the literal,
//! and the only sanctioned literal is the capability table's `long_flag` in
//! `src/backend_model.rs`.
//!
//! `syn` AST walk (mirrors tests/health_blocked_reason_no_self_ipc_invariant_2454.rs):
//! `#[cfg(test)]` modules and `#[test]` functions are skipped — test fixtures
//! legitimately spell the flag out.

use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};

/// The only production file allowed to carry the literal: the declared
/// capability table (`ModelCapability { long_flag: "--model", .. }`).
const ALLOWLIST: &[&str] = &["src/backend_model.rs"];

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg") && {
            let mut has_test = false;
            let _ = a.parse_nested_meta(|meta| {
                if meta.path.is_ident("test") {
                    has_test = true;
                }
                Ok(())
            });
            has_test
        }
    })
}

fn is_test_fn(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| a.path().is_ident("test")) || is_cfg_test(attrs)
}

#[derive(Default)]
struct ModelLitFinder {
    hits: Vec<String>,
}

impl<'ast> Visit<'ast> for ModelLitFinder {
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        if is_cfg_test(&m.attrs) {
            return; // test module — fixtures may spell the flag
        }
        visit::visit_item_mod(self, m);
    }
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if is_test_fn(&f.attrs) {
            return;
        }
        visit::visit_item_fn(self, f);
    }
    fn visit_lit_str(&mut self, l: &'ast syn::LitStr) {
        let v = l.value();
        // Exact flag, `=`-glued, or a flag-leading format string. Prose that
        // merely mentions the flag mid-sentence does not assemble argv.
        if v == "--model" || v.starts_with("--model=") || v.starts_with("--model ") {
            self.hits.push(v);
        }
        visit::visit_lit_str(self, l);
    }
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn model_flag_assembly_confined_to_push_model_arg_chokepoint_2744() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    assert!(
        files.len() > 100,
        "sanity: src walk found {} files",
        files.len()
    );

    let mut violations = Vec::new();
    for path in files {
        let rel = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let rel = rel.trim_start_matches('/').to_string();
        if ALLOWLIST.iter().any(|a| rel == *a) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read src file");
        let ast = match syn::parse_file(&text) {
            Ok(a) => a,
            Err(e) => panic!("parse {rel}: {e}"),
        };
        let mut finder = ModelLitFinder::default();
        finder.visit_file(&ast);
        for hit in finder.hits {
            violations.push(format!("{rel}: literal {hit:?}"));
        }
    }
    assert!(
        violations.is_empty(),
        "model-flag assembly outside the Backend::push_model_arg chokepoint \
         (route the site through push_model_arg with the DECLARED backend — \
         see src/backend_model.rs and #2744 r1):\n{}",
        violations.join("\n")
    );
}
