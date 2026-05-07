//! Cargo.toml `[package].include` invariant — prevents publish-blocker
//! regression for the silent-drop class 5th instance pattern.
//!
//! ## Why this exists
//! The `cargo publish` verify step builds from the packaged tarball, NOT
//! the source tree. Any production `include_str!` / `include_bytes!` macro
//! pointing at a path NOT in `[package].include` fails verbatim with
//! `couldn't read src/../assets/hooks/...` during publish. Caught the
//! hard way at:
//! - **v0.4.0** — `docs/FLEET-DEV-PROTOCOL-v1.md` missing from include
//! - **v0.6.0** — `assets/hooks/*` missing (publish-blocker hotfix #505)
//!
//! Per #505 commit `2c124c0`: "Phase 2 invariant test (grep `include_str!`
//! against include list) deferred to Sprint 55 / v0.6.1." This test
//! discharges that deferral. Documented in
//! `d-20260507062618667443-1` (silent-drop systematic ledger entry #5).
//!
//! ## What it checks
//! For every production `include_str!` / `include_bytes!` macro literal
//! in `src/**/*.rs`, the resolved absolute-from-repo-root path must be
//! covered by at least one glob pattern in `[package].include`.
//!
//! Test-only macros (inside `#[cfg(test)]` modules, in test-only files
//! declared via `#[cfg(test)] mod NAME;` from a parent, or in test
//! fixture-loading sites) are ignored — they never see a `cargo publish`
//! tarball and are free to point anywhere in the workspace.
//!
//! ## How the regression-proof works
//! `mock_invariant_fires_when_assets_hooks_removed_from_include` calls
//! the SAME `scan_production_violations` entry point as the prod test
//! but feeds it a hand-mutated include list with `assets/hooks/*`
//! removed — expects the resulting violation list to surface the
//! `prepare-commit-msg` paths verbatim. Same code path, no
//! mock-only branches.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use syn::{visit::Visit, Macro};

const SRC_DIR: &str = "src";

/// Walk every `.rs` file under `src/` (recursive). Mirrors the helper
/// used by `tests/anti_pattern_invariant.rs`.
fn rs_files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
    walk(root, &mut out);
    out
}

/// True iff any `cfg(test)` reference appears in `attrs`. Covers
/// `#[cfg(test)]`, `#[cfg(any(test, …))]`, `#[cfg(all(test, …))]`,
/// `#[cfg(not(not(test)))]`, etc. — we string-match the attribute's
/// token stream rather than chase syn's Meta tree, since any mention
/// of `test` inside a `cfg(...)` predicate is a strong test-only
/// signal in practice.
fn attrs_contain_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        if !a.path().is_ident("cfg") {
            return false;
        }
        let syn::Meta::List(list) = &a.meta else {
            return false;
        };
        list.tokens
            .to_string()
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .any(|tok| tok == "test")
    })
}

/// Collects every `include_str!("…")` / `include_bytes!("…")` literal
/// reachable from production code in a single source file. Items
/// inside `#[cfg(test)]` modules are pruned at the visitor level
/// (the visitor never recurses into them).
struct ProductionMacroCollector {
    /// (literal_path_as_written, source_file) pairs. Source file is
    /// captured by the caller before invoking the visitor.
    literals: Vec<String>,
}

impl<'ast> Visit<'ast> for ProductionMacroCollector {
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        if attrs_contain_cfg_test(&m.attrs) {
            return;
        }
        syn::visit::visit_item_mod(self, m);
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if attrs_contain_cfg_test(&f.attrs) {
            return;
        }
        syn::visit::visit_item_fn(self, f);
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if attrs_contain_cfg_test(&i.attrs) {
            return;
        }
        syn::visit::visit_item_impl(self, i);
    }

    fn visit_macro(&mut self, m: &'ast Macro) {
        let Some(seg) = m.path.segments.last() else {
            return;
        };
        let name = seg.ident.to_string();
        if name != "include_str" && name != "include_bytes" {
            return;
        }
        if let Ok(lit) = syn::parse2::<syn::LitStr>(m.tokens.clone()) {
            self.literals.push(lit.value());
        }
    }
}

/// Lightweight test-only-file detector. A `.rs` file under `src/` is
/// treated as test-only when its parent module declares it via
/// `#[cfg(test)] mod {stem};` — without this hop, files like
/// `src/mcp/handlers/tests.rs` (which has no top-level `#[cfg(test)]`
/// of its own) would leak production-shaped macros through the gate.
fn is_test_only_file(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    if stem == "lib" || stem == "main" || stem == "mod" {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let candidates = [
        parent.join("mod.rs"),
        parent
            .parent()
            .map(|gp| {
                let pname = parent.file_name().and_then(|n| n.to_str()).unwrap_or("");
                gp.join(format!("{pname}.rs"))
            })
            .unwrap_or_default(),
    ];
    let needle = format!("mod {stem}");
    for cand in candidates.iter() {
        if !cand.exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(cand) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            if !line.contains(&needle) {
                continue;
            }
            // Look upward at most 3 lines for an attribute group; any
            // `#[cfg(...test...)]` line counts.
            let mut cursor = idx;
            for _ in 0..3 {
                let Some(prev) = cursor.checked_sub(1).and_then(|i| lines.get(i)) else {
                    break;
                };
                let trimmed = prev.trim();
                if trimmed.is_empty() || trimmed.starts_with("//") {
                    cursor -= 1;
                    continue;
                }
                if trimmed.starts_with("#[") && trimmed.contains("cfg") && trimmed.contains("test")
                {
                    return true;
                }
                if !trimmed.starts_with("#[") {
                    break;
                }
                cursor -= 1;
            }
        }
    }
    false
}

/// Resolve a literal as written inside `include_str!` against the
/// directory of the source file that contains the macro, then strip
/// the leading components so the result is relative to the repo root
/// (same shape as patterns in `[package].include`).
fn resolve_literal(src_file: &Path, literal: &str) -> PathBuf {
    let base = src_file.parent().unwrap_or_else(|| Path::new("."));
    let joined = base.join(literal);
    // Normalize . / ..
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Parse `[package].include` from `Cargo.toml` into raw glob strings.
fn read_cargo_include() -> Vec<String> {
    let content = std::fs::read_to_string("Cargo.toml").expect("read Cargo.toml");
    let parsed: toml::Value = toml::from_str(&content).expect("parse Cargo.toml");
    parsed
        .get("package")
        .and_then(|p| p.get("include"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .expect("[package].include must be a string array")
}

/// Compile glob patterns. Panics on malformed patterns so a
/// fat-finger in `Cargo.toml` shows as a loud test failure.
fn compile_globs(patterns: &[String]) -> Vec<glob::Pattern> {
    patterns
        .iter()
        .map(|p| glob::Pattern::new(p).unwrap_or_else(|e| panic!("bad include glob {p}: {e}")))
        .collect()
}

/// Single entry point shared by the prod invariant test and the
/// regression-proof mock test. Returns one human-readable violation
/// line per uncovered macro literal.
fn scan_production_violations(globs: &[glob::Pattern]) -> Vec<String> {
    let src_root = Path::new(SRC_DIR);
    let mut violations = Vec::new();
    let mut seen: HashSet<(PathBuf, String)> = HashSet::new();
    for src in rs_files_under(src_root) {
        if is_test_only_file(&src) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&src) else {
            continue;
        };
        let Ok(file) = syn::parse_file(&content) else {
            continue;
        };
        let mut visitor = ProductionMacroCollector { literals: vec![] };
        visitor.visit_file(&file);
        for literal in visitor.literals {
            let resolved = resolve_literal(&src, &literal);
            let resolved_str = resolved.to_string_lossy().replace('\\', "/");
            if seen.contains(&(src.clone(), literal.clone())) {
                continue;
            }
            seen.insert((src.clone(), literal.clone()));
            let covered = globs.iter().any(|g| g.matches(&resolved_str));
            if !covered {
                violations.push(format!(
                    "{}: include_(str|bytes)!({:?}) -> resolved {:?} not in [package].include",
                    src.display(),
                    literal,
                    resolved_str
                ));
            }
        }
    }
    violations.sort();
    violations
}

#[test]
fn production_include_macros_are_in_cargo_include_list() {
    let patterns = read_cargo_include();
    let globs = compile_globs(&patterns);
    let violations = scan_production_violations(&globs);
    assert!(
        violations.is_empty(),
        "production include_(str|bytes)! macros reference paths missing from \
         Cargo.toml [package].include — `cargo publish` will fail with \"couldn't \
         read ...\". Add the path (or a covering glob) to the include whitelist:\n\
         {}",
        violations.join("\n")
    );
}

#[test]
fn mock_invariant_fires_when_assets_hooks_removed_from_include() {
    // Empirical regression-proof: same scan_production_violations entry
    // point as the prod test, but with `assets/hooks/*` deleted from
    // the include list. Expected to surface the binding.rs hook paths
    // by their resolved form.
    let mock_patterns: Vec<String> = read_cargo_include()
        .into_iter()
        .filter(|p| p != "assets/hooks/*")
        .collect();
    let globs = compile_globs(&mock_patterns);
    let violations = scan_production_violations(&globs);
    let bash_present = violations
        .iter()
        .any(|v| v.contains("assets/hooks/prepare-commit-msg") && !v.contains(".ps1"));
    let ps_present = violations
        .iter()
        .any(|v| v.contains("assets/hooks/prepare-commit-msg.ps1"));
    assert!(
        bash_present && ps_present,
        "mock-mutate dropping `assets/hooks/*` must surface BOTH hook paths \
         (bash + ps1) — empirical regression-proof for #505 publish-blocker. \
         Got violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn mock_invariant_fires_when_protocol_doc_removed_from_include() {
    // v0.4.0 publish-blocker repro: drop the protocol doc include.
    let mock_patterns: Vec<String> = read_cargo_include()
        .into_iter()
        .filter(|p| p != "docs/FLEET-DEV-PROTOCOL-v1.md")
        .collect();
    let globs = compile_globs(&mock_patterns);
    let violations = scan_production_violations(&globs);
    assert!(
        violations
            .iter()
            .any(|v| v.contains("docs/FLEET-DEV-PROTOCOL-v1.md")),
        "mock-mutate dropping `docs/FLEET-DEV-PROTOCOL-v1.md` must surface \
         the protocol.rs include — v0.4.0 publish-blocker repro. \
         Got violations:\n{}",
        violations.join("\n")
    );
}
