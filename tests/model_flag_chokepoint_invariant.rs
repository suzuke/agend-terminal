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

/// SHORT-spelling (`-m`) exemption only: git plumbing passes `-m` as the
/// commit-message flag. The long `--model` ban still applies to these files.
const SHORT_FLAG_ALLOWLIST: &[&str] = &["src/git_helpers.rs"];

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
    /// File-scoped exemption for the SHORT spelling only: git plumbing
    /// legitimately passes `-m` (commit message). The long `--model` ban is
    /// never exempted outside the capability-table allowlist.
    allow_short: bool,
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
        if is_banned_flag_literal(&v, self.allow_short) {
            self.hits.push(v);
        }
        visit::visit_lit_str(self, l);
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        // r2 (root-review Blocker 2): syn does not descend into macro token
        // streams, so `format!("--model {x}")` evaded visit_lit_str. Walk the
        // raw tokens for string literals and apply the same ban.
        scan_tokens(m.tokens.clone(), self.allow_short, &mut self.hits);
        visit::visit_macro(self, m);
    }
}

/// Exact flag, `=`-glued, or flag-leading format string — long and (declared)
/// short spellings. Prose mentioning the flag mid-sentence does not match.
fn is_banned_flag_literal(v: &str, allow_short: bool) -> bool {
    if v == "--model" || v.starts_with("--model=") || v.starts_with("--model ") {
        return true;
    }
    // Short spelling: only the VALUE-GLUED assembly shapes ("-m X" / "-m=X")
    // are banned. The bare "-m" token is deliberately NOT banned: `git commit
    // -m` pervades git plumbing and file-level test modules the item walker
    // cannot classify; a bare-token model `-m` push is separately futile —
    // push_model_arg dedupe + the real-entry behavioral tests pin it.
    !allow_short && (v.starts_with("-m=") || (v.starts_with("-m ") && v.len() > 3))
}

/// Recursively scan a macro token stream for banned string literals.
fn scan_tokens(tokens: proc_macro2::TokenStream, allow_short: bool, hits: &mut Vec<String>) {
    for tt in tokens {
        match tt {
            proc_macro2::TokenTree::Group(g) => scan_tokens(g.stream(), allow_short, hits),
            proc_macro2::TokenTree::Literal(l) => {
                if let Ok(syn::Lit::Str(s)) = syn::parse_str::<syn::Lit>(&l.to_string()) {
                    let v = s.value();
                    if is_banned_flag_literal(&v, allow_short) {
                        hits.push(v);
                    }
                }
            }
            _ => {}
        }
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
        let mut finder = ModelLitFinder {
            allow_short: SHORT_FLAG_ALLOWLIST.iter().any(|a| rel == *a),
            ..Default::default()
        };
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

/// r2 self-tests (root-review Blocker 2): prove the finder FIRES on both
/// production assembly shapes and stays quiet on test-exempt code — a guard
/// with no negative case is an unverified guard.
#[test]
fn finder_catches_plain_and_macro_assembly_shapes_2744() {
    let snippet = r##"
        fn plain(args: &mut Vec<String>) {
            args.push("--model".to_string());
        }
        fn glued(s: &mut String, m: &str) {
            s.push_str(&format!("--model {m}"));
        }
        fn short(s: &mut String, m: &str) {
            s.push_str(&format!("-m {m}"));
        }
    "##;
    let ast = syn::parse_file(snippet).expect("parse snippet");
    let mut finder = ModelLitFinder::default();
    finder.visit_file(&ast);
    assert_eq!(
        finder.hits.len(),
        3,
        "must catch plain literal + format!-glued long + short spellings, got {:?}",
        finder.hits
    );
}

#[test]
fn finder_exempts_cfg_test_code_2744() {
    let snippet = r##"
        #[cfg(test)]
        mod tests {
            fn fixture(args: &mut Vec<String>) {
                args.push("--model".to_string());
            }
        }
        #[test]
        fn t() {
            let _ = format!("--model {}", "x");
        }
    "##;
    let ast = syn::parse_file(snippet).expect("parse snippet");
    let mut finder = ModelLitFinder::default();
    finder.visit_file(&ast);
    assert!(
        finder.hits.is_empty(),
        "test-exempt code must not trip the guard, got {:?}",
        finder.hits
    );
}
