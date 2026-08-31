//! Shared classifier: is a `src/` file a plain `#[cfg(test)] mod <stem>;` sibling
//! test module?
//!
//! Both `spawn_rationale_audit` and `tty_leak_invariant` exempt test code, and both
//! previously recognised only the *conventional* names — a `tests.rs` file, or a file
//! under a `tests/` directory. A sibling test module with any other name (for example
//! `mcp/handlers/instance_964_tests.rs`, declared `#[cfg(test)] mod instance_964_tests;`
//! in the owning `mod.rs`) carries no inline `#[cfg(test)]` of its own, so both scanners
//! read it as production. The only ways to pass were then to write something false into
//! the source — a `fire-and-forget` rationale on an explicitly joined handle, or a
//! `tty-inherit-allowed` marker on a test fixture. The miss is in the detection, not in
//! the code being flagged.
//!
//! Authority is the OWNING module's declaration, parsed as an AST rather than matched as
//! text: the `#[cfg(test)]` and `#[allow(...)]` attributes may appear in either order and
//! on separate lines, which defeats a line-oriented scan.
//!
//! Pulled into each invariant with `#[path]` so neither drags in the rest of
//! `tests/common/` (the daemon harness).

use std::path::Path;

/// True when `path`'s owning module declares it as a body-less `mod <stem>;` carrying a
/// cfg that cannot hold outside a test build.
pub fn is_cfg_test_module_file(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let Some(dir) = path.parent() else {
        return false;
    };
    // A module living in `<dir>/` is declared either by `<dir>/mod.rs` or, in the
    // 2018-style layout, by the sibling `<dir>.rs` one level up.
    let mut owners = vec![dir.join("mod.rs")];
    if let (Some(up), Some(name)) = (dir.parent(), dir.file_name()) {
        owners.push(up.join(format!("{}.rs", name.to_string_lossy())));
    }
    owners
        .iter()
        .any(|owner| owner_declares_test_module(owner, stem))
}

fn owner_declares_test_module(owner: &Path, stem: &str) -> bool {
    let Ok(source) = std::fs::read_to_string(owner) else {
        return false;
    };
    let Ok(file) = syn::parse_file(&source) else {
        return false;
    };
    file.items.iter().any(|item| match item {
        // `content.is_none()` keeps this to DECLARATIONS (`mod foo;`); an inline
        // `mod foo { .. }` owns its own body and is not this file.
        syn::Item::Mod(m) => {
            m.content.is_none() && m.ident == stem && m.attrs.iter().any(is_test_only_cfg_attr)
        }
        _ => false,
    })
}

fn is_test_only_cfg_attr(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("cfg")
        && attr
            .parse_args::<syn::Meta>()
            .is_ok_and(|meta| is_test_only_cfg(&meta))
}

/// True only for a cfg expression that cannot hold outside a test build: bare `test`, or
/// an `all(...)` with a top-level `test` term. `any(unix, test)` compiles in an ordinary
/// unix build, and a feature named `"test-util"` merely contains the substring — neither
/// may exempt a file from the production scan.
fn is_test_only_cfg(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) if list.path.is_ident("all") => {
            let Ok(terms) = list.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            ) else {
                return false;
            };
            terms.iter().any(is_test_only_cfg)
        }
        _ => false,
    }
}

// ── Discriminating tests ────────────────────────────────────────────────────
// These compile into BOTH including crates, so each invariant carries proof that
// its exemption is truthful: the positives are real test modules, and every
// negative is a shape that must still be scanned as production.

#[cfg(test)]
fn probe(owner_src: &str) -> bool {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agend-modcls-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("fixture dir");
    std::fs::write(dir.join("mod.rs"), owner_src).expect("owner");
    let target = dir.join("sibling_probe.rs");
    std::fs::write(&target, "// body\n").expect("target");
    let verdict = is_cfg_test_module_file(&target);
    std::fs::remove_dir_all(&dir).ok();
    verdict
}

#[test]
fn cfg_test_sibling_declaration_is_test_only() {
    assert!(
        probe("#[cfg(test)]\nmod sibling_probe;\n"),
        "a `#[cfg(test)] mod <stem>;` sibling module is entirely test code"
    );
}

#[test]
fn attributes_in_either_order_are_recognised() {
    // The real shape this fix exists for: `#[cfg(test)]` and `#[allow(..)]` on
    // separate lines, in either order — what defeats a line-oriented scan.
    assert!(
        probe("#[cfg(test)]\n#[allow(clippy::unwrap_used)]\nmod sibling_probe;\n"),
        "cfg first, allow second"
    );
    assert!(
        probe("#[allow(clippy::unwrap_used)]\n#[cfg(test)]\nmod sibling_probe;\n"),
        "allow first, cfg second"
    );
}

#[test]
fn all_with_a_test_term_is_test_only() {
    assert!(
        probe("#[cfg(all(unix, test))]\nmod sibling_probe;\n"),
        "a conjunction containing `test` cannot hold outside a test build"
    );
}

#[test]
fn undeclared_or_plain_module_is_not_test_only() {
    assert!(
        !probe("mod sibling_probe;\n"),
        "a plain `mod <stem>;` is production and must stay in scope"
    );
    assert!(
        !probe("// nothing declares it\n"),
        "an undeclared file must stay in scope"
    );
}

#[test]
fn feature_named_test_does_not_exempt() {
    assert!(
        !probe("#[cfg(feature = \"test-util\")]\nmod sibling_probe;\n"),
        "a feature name merely CONTAINING `test` compiles in a production build"
    );
}

#[test]
fn any_disjunction_does_not_exempt() {
    assert!(
        !probe("#[cfg(any(unix, test))]\nmod sibling_probe;\n"),
        "`any(unix, test)` compiles in an ordinary unix build"
    );
}

#[test]
fn inline_module_body_does_not_exempt_a_sibling_file() {
    assert!(
        !probe("#[cfg(test)]\nmod sibling_probe {}\n"),
        "an inline module with a body owns that body and does not declare the sibling file"
    );
}
