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
//!
//! One further shape is not assembly at all: a table that TRANSCRIBES another
//! CLI's grammar, so the session sanitizer knows `--model` owns the token after
//! it. There is no argv being built there and nothing for the chokepoint to
//! route, so `DECLARATION_SITES` exempts a literal sitting directly in such a
//! table's constructor — and nothing else in the same file. A push, a format!,
//! or a computed argument inside that very constructor still fails.

use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};

/// The only production file allowed to carry the literal: the declared
/// capability table (`ModelCapability { long_flag: "--model", .. }`).
const ALLOWLIST: &[&str] = &["src/backend_model.rs"];

/// SHORT-spelling (`-m`) exemption only: git plumbing passes `-m` as the
/// commit-message flag. The long `--model` ban still applies to these files.
const SHORT_FLAG_ALLOWLIST: &[&str] = &["src/git_helpers.rs"];

/// Files that transcribe a foreign CLI's grammar, with the constructors whose
/// DIRECT string arguments are declarations rather than assembly.
///
/// `src/backend_session.rs` reads off `codex --help` into `CODEX_GLOBALS` so the
/// session sanitizer knows the arity of each global — `g("--model", Some("-m"),
/// GlobalArity::One)` says Codex's `--model` owns the next token. That is a fact
/// about someone else's CLI, not this daemon emitting a flag, and routing it
/// through `push_model_arg` would be meaningless. The exemption is deliberately
/// narrow: only a literal passed straight to one of these constructors, in one
/// of these files.
const DECLARATION_SITES: &[(&str, &str)] = &[("src/backend_session.rs", "CODEX_GLOBALS")];

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
    /// The `const` item in THIS file whose initializer transcribes a foreign
    /// CLI's grammar. `None` for every other file.
    ///
    /// r3: this was keyed on the CONSTRUCTOR'S NAME, which exempted any call to
    /// anything named `g` anywhere in the file — a shadow helper, or
    /// `some_module::g(..)`, because the match was on the path's LAST segment.
    /// The exemption is a property of WHERE a literal sits, not of what the
    /// surrounding function happens to be called, so it is now a scope.
    declaration_const: Option<&'static str>,
    /// True while descending the initializer of that const.
    in_declaration_const: bool,
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
    fn visit_item_const(&mut self, c: &'ast syn::ItemConst) {
        let is_declaration = self.declaration_const == Some(c.ident.to_string().as_str());
        let outer = self.in_declaration_const;
        self.in_declaration_const = outer || is_declaration;
        visit::visit_item_const(self, c);
        self.in_declaration_const = outer;
    }
    fn visit_lit_str(&mut self, l: &'ast syn::LitStr) {
        let v = l.value();
        // A literal INSIDE the grammar table is the transcription itself. A
        // computed argument is not exempt even there: `format!` is caught by
        // `visit_macro`, which records independently of this scope.
        if !self.in_declaration_const && is_banned_flag_literal(&v, self.allow_short) {
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
            declaration_const: DECLARATION_SITES
                .iter()
                .find(|(file, _)| rel == *file)
                .map(|(_, item)| *item),
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

/// The declaration exemption, and the proof it is load-bearing rather than
/// decorative: the same snippet trips the guard the moment the constructor is
/// not recognised.
#[test]
fn grammar_declaration_is_not_assembly_2744() {
    let snippet = r##"
        const CODEX_GLOBALS: &[CodexGlobal] = &{
            const fn g(long: &'static str, short: Option<&'static str>, arity: GlobalArity) -> CodexGlobal {
                CodexGlobal { long, short, arity }
            }
            [
                g("--image", Some("-i"), GlobalArity::Variadic),
                g("--model", Some("-m"), GlobalArity::One),
            ]
        };
    "##;
    let ast = syn::parse_file(snippet).expect("parse snippet");

    let mut declared = ModelLitFinder {
        declaration_const: Some("CODEX_GLOBALS"),
        ..Default::default()
    };
    declared.visit_file(&ast);
    assert!(
        declared.hits.is_empty(),
        "transcribing another CLI's grammar is a declaration, not assembly, got {:?}",
        declared.hits
    );

    let mut unrecognised = ModelLitFinder::default();
    unrecognised.visit_file(&ast);
    assert_eq!(
        unrecognised.hits.len(),
        1,
        "control: without the constructor the same literal must still be a hit, got {:?}",
        unrecognised.hits
    );
}

/// The exemption covers the declaration and nothing else. Assembly elsewhere in
/// the file still fails, and so does assembly smuggled INTO the constructor as
/// a computed argument — the shape a mutation would reach for once it knows the
/// constructor is exempt.
#[test]
fn declaration_exemption_does_not_cover_assembly_in_the_same_file_2744() {
    let snippet = r##"
        const CODEX_GLOBALS: &[CodexGlobal] = &{
            [g("--model", Some("-m"), GlobalArity::One)]
        };
        fn assemble(args: &mut Vec<String>, m: &str) {
            args.push("--model".to_string());
            args.push(format!("--model {m}"));
        }
        fn smuggled(m: &str) -> CodexGlobal {
            g(&format!("--model {m}"), None, GlobalArity::One)
        }
    "##;
    let ast = syn::parse_file(snippet).expect("parse snippet");
    let mut finder = ModelLitFinder {
        declaration_const: Some("CODEX_GLOBALS"),
        ..Default::default()
    };
    finder.visit_file(&ast);
    assert_eq!(
        finder.hits.len(),
        3,
        "the two assembly sites and the one smuggled into the constructor must all \
         still be hits, got {:?}",
        finder.hits
    );
}

/// r3 killer: the exemption used to be keyed on the CONSTRUCTOR'S NAME, matched
/// on the path's last segment. Two shapes walked straight through it, and both
/// are real assembly emitting this daemon's own `--model`.
///
/// A SHADOW `g` defined outside the grammar table is one. `reviewer_h::g(..)` is
/// the other: a different module entirely, exempted because its last path
/// segment happened to be `g`. Neither sits in the transcribed table, so under a
/// scope-based exemption neither is a declaration.
///
/// The real table in the same snippet must still pass, or this test would prove
/// only that the guard can refuse everything.
#[test]
fn shadow_and_path_constructors_are_not_declarations_2744() {
    let snippet = r##"
        const CODEX_GLOBALS: &[CodexGlobal] = &{
            [g("--model", Some("-m"), GlobalArity::One)]
        };
        fn g(flag: &str) -> String {
            flag.to_string()
        }
        fn shadow_assembly() -> String {
            g("--model")
        }
        fn path_assembly() -> String {
            reviewer_h::g("--model")
        }
    "##;
    let ast = syn::parse_file(snippet).expect("parse snippet");
    let mut finder = ModelLitFinder {
        declaration_const: Some("CODEX_GLOBALS"),
        ..Default::default()
    };
    finder.visit_file(&ast);
    assert_eq!(
        finder.hits.len(),
        2,
        "a shadow `g` and a `reviewer_h::g` outside the table are assembly, not \
         declarations — the transcribed table is the only exempt scope, got {:?}",
        finder.hits
    );
}

/// The scope is the const, not the file: a literal in the table is exempt and
/// the identical literal one item later is not. Pins that replacing the name key
/// with a scope did not simply exempt the whole declaring file.
#[test]
fn declaration_scope_ends_with_the_const_2744() {
    let exempt = r##"
        const CODEX_GLOBALS: &[CodexGlobal] = &{
            [g("--model", Some("-m"), GlobalArity::One)]
        };
    "##;
    let leaked = r##"
        const CODEX_GLOBALS: &[CodexGlobal] = &{
            [g("--model", Some("-m"), GlobalArity::One)]
        };
        const NOT_THE_TABLE: &str = "--model";
    "##;
    let scan = |src: &str| {
        let ast = syn::parse_file(src).expect("parse snippet");
        let mut finder = ModelLitFinder {
            declaration_const: Some("CODEX_GLOBALS"),
            ..Default::default()
        };
        finder.visit_file(&ast);
        finder.hits.len()
    };
    assert_eq!(scan(exempt), 0, "the table itself stays exempt");
    assert_eq!(
        scan(leaked),
        1,
        "a sibling const in the same file is outside the scope and must be caught"
    );
}
