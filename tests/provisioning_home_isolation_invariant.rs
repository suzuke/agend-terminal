//! #46776 R3 — every test that can reach provisioning must scope AGEND_HOME.
//!
//! `instructions::generate*` reads `home_dir()` on every call. This census
//! keeps the test-only environment contract load-bearing: direct provisioning
//! calls, bootstrap `prepare` calls, and bootstrap resolver calls must all be
//! inside a `ScopedAgendHome` guard. A new test that omits the guard fails here
//! instead of silently healing or overwriting the operator's home.

use std::path::{Path, PathBuf};
use syn::visit::Visit;

const DIRECT_PROVISIONING_CALLS: &[&str] = &[
    "generate",
    "generate_for_owner",
    "generate_with_context",
    "generate_agent_instructions",
];

fn is_test_source(path: &str) -> bool {
    path.starts_with("tests/")
        || path.ends_with("/tests.rs")
        || path.ends_with("_tests.rs")
        || path.contains("/tests/")
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Ident>()
                .is_ok_and(|ident| ident == "test")
    })
}

struct ProvisioningCallVisitor {
    path: String,
    calls: Vec<String>,
    scoped_home: bool,
}

impl<'ast> Visit<'ast> for ProvisioningCallVisitor {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(function) = call.func.as_ref() {
            if let Some(name) = function.path.segments.last() {
                let name = name.ident.to_string();
                let indirect = (self.path == "src/bootstrap/mod.rs" && name == "prepare")
                    || (self.path == "src/bootstrap/agent_resolve.rs"
                        && matches!(name.as_str(), "resolve" | "resolve_one"));
                if DIRECT_PROVISIONING_CALLS.contains(&name.as_str()) || indirect {
                    self.calls.push(name);
                }
            }
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if path
            .path
            .segments
            .iter()
            .any(|segment| segment.ident == "ScopedAgendHome")
        {
            self.scoped_home = true;
        }
        syn::visit::visit_expr_path(self, path);
    }
}

fn audit_items(items: &[syn::Item], path: &str, test_scope: bool, violations: &mut Vec<String>) {
    for item in items {
        match item {
            syn::Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    audit_items(
                        nested,
                        path,
                        test_scope || is_cfg_test(&module.attrs),
                        violations,
                    );
                }
            }
            syn::Item::Fn(function) if test_scope => {
                let mut visitor = ProvisioningCallVisitor {
                    path: path.to_owned(),
                    calls: Vec::new(),
                    scoped_home: false,
                };
                visitor.visit_block(&function.block);
                if !visitor.calls.is_empty() && !visitor.scoped_home {
                    let name = function.sig.ident.to_string();
                    violations.push(format!(
                        "{path}::{name} reaches {} without ScopedAgendHome",
                        visitor.calls.join(", ")
                    ));
                }
            }
            _ => {}
        }
    }
}

fn audit_source(path: &str, source: &str) -> Vec<String> {
    let syntax = syn::parse_file(source).expect("parse source");
    let mut violations = Vec::new();
    audit_items(&syntax.items, path, is_test_source(path), &mut violations);
    violations
}

#[test]
fn provisioning_reaching_tests_are_home_scoped() {
    let mut paths = Vec::new();
    collect_rs(Path::new("src"), &mut paths);
    collect_rs(Path::new("tests"), &mut paths);
    let mut violations = Vec::new();
    for path in paths {
        let path = path.to_string_lossy().replace('\\', "/");
        let source =
            std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        violations.extend(audit_source(&path, &source));
    }
    assert!(
        violations.is_empty(),
        "provisioning test-home isolation census found violations:\n{}",
        violations.join("\n")
    );
}

#[test]
fn census_rejects_an_unscoped_provisioning_test() {
    let source = r#"
        #[cfg(test)]
        mod tests {
            #[test]
            fn leaked_home() {
                crate::instructions::generate(&dir, "claude").unwrap();
            }
        }
    "#;
    let violations = audit_source("src/instructions.rs", source);
    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("leaked_home"));
}

#[test]
fn census_accepts_the_shared_home_guard() {
    let source = r#"
        #[cfg(test)]
        mod tests {
            #[test]
            fn isolated_home() {
                let _home = crate::review_repro_test_util::ScopedAgendHome::new(&dir);
                crate::instructions::generate(&dir, "claude").unwrap();
            }
        }
    "#;
    assert!(audit_source("src/instructions.rs", source).is_empty());
}
