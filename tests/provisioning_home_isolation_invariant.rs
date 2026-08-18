//! #46776 R5 — provisioning must receive its home explicitly.
//!
//! The provisioning API is a central boundary. Tests should not need a
//! reachability census to prove that a transitive caller selected the right
//! home: the low-level API must make the home an explicit argument, and its
//! implementation must not consult the ambient environment.

use std::collections::HashSet;

use syn::visit::{self, Visit};

fn read_source(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn has_explicit_path_parameter(source: &str, function: &str) -> bool {
    let file = syn::parse_file(source).expect("parse source");
    file.items.iter().any(|item| {
        let syn::Item::Fn(function_item) = item else {
            return false;
        };
        if function_item.sig.ident != function {
            return false;
        }
        matches!(
            function_item.sig.inputs.first(),
            Some(syn::FnArg::Typed(argument))
                if matches!(argument.pat.as_ref(), syn::Pat::Ident(pattern) if pattern.ident == "home")
                    && matches!(argument.ty.as_ref(), syn::Type::Reference(reference)
                        if matches!(reference.elem.as_ref(), syn::Type::Path(path)
                            if path.path.is_ident("Path")))
        )
    })
}

fn collect_home_dir_aliases(
    tree: &syn::UseTree,
    prefix: &mut Vec<String>,
    aliases: &mut HashSet<String>,
) {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_home_dir_aliases(&path.tree, prefix, aliases);
            prefix.pop();
        }
        syn::UseTree::Name(name) => {
            if prefix.len() == 1 && prefix[0] == "crate" && name.ident == "home_dir" {
                aliases.insert(name.ident.to_string());
            }
        }
        syn::UseTree::Rename(rename) => {
            if prefix.len() == 1 && prefix[0] == "crate" && rename.ident == "home_dir" {
                aliases.insert(rename.rename.to_string());
            }
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_home_dir_aliases(item, prefix, aliases);
            }
        }
        syn::UseTree::Glob(_) => {
            if prefix.len() == 1 && prefix[0] == "crate" {
                aliases.insert("*".to_string());
            }
        }
    }
}

struct HomeDirAliasCollector<'a> {
    aliases: &'a mut HashSet<String>,
}

impl<'ast> Visit<'ast> for HomeDirAliasCollector<'_> {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        collect_home_dir_aliases(&item.tree, &mut Vec::new(), self.aliases);
        visit::visit_item_use(self, item);
    }
}

struct AmbientHomeCallFinder<'a> {
    aliases: &'a HashSet<String>,
    found: bool,
}

impl<'ast> Visit<'ast> for AmbientHomeCallFinder<'_> {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = call.func.as_ref() {
            let segments: Vec<_> = path
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();
            let direct = segments == ["crate", "home_dir"];
            let imported = segments.len() == 1
                && (self.aliases.contains(&segments[0]) || self.aliases.contains("*"));
            self.found |= direct || imported;
        }
        visit::visit_expr_call(self, call);
    }
}

fn has_ambient_home_lookup(source: &str) -> bool {
    let file = syn::parse_file(source).expect("parse source");
    let mut aliases = HashSet::new();
    HomeDirAliasCollector {
        aliases: &mut aliases,
    }
    .visit_file(&file);

    let mut finder = AmbientHomeCallFinder {
        aliases: &aliases,
        found: false,
    };
    finder.visit_file(&file);
    finder.found
}

#[test]
fn aliased_ambient_home_lookup_is_rejected() {
    let source = r#"
        use crate::home_dir as ambient;

        fn generate(home: &Path) {
            let _ = home;
            let _ = ambient();
        }
    "#;
    assert!(
        has_ambient_home_lookup(source),
        "aliased crate::home_dir calls must be rejected"
    );
}

#[test]
fn direct_ambient_env_lookup_is_rejected() {
    for source in [
        r#"
            fn generate(home: &Path) {
                let _ = home;
                let _ = std::env::var("AGEND_HOME");
            }
        "#,
        r#"
            fn generate(home: &Path) {
                let _ = home;
                let _ = std::env::var_os("AGEND_HOME");
            }
        "#,
    ] {
        assert!(
            has_ambient_home_lookup(source),
            "direct std::env AGEND_HOME lookups must be rejected"
        );
    }
}

#[test]
fn aliased_ambient_env_lookup_is_rejected() {
    let source = r#"
        use std::env as ambient_env;
        use std::env::{var as read_home, var_os as read_home_os};

        fn generate(home: &Path) {
            let _ = home;
            let _ = ambient_env::var("AGEND_HOME");
            let _ = read_home("AGEND_HOME");
            let _ = read_home_os("AGEND_HOME");
        }
    "#;
    assert!(
        has_ambient_home_lookup(source),
        "aliased std::env AGEND_HOME lookups must be rejected"
    );
}

#[test]
fn function_pointer_ambient_env_lookup_is_rejected() {
    let source = r#"
        fn generate(home: &Path) {
            let _ = home;
            let lookup = std::env::var;
            let _ = lookup("AGEND_HOME");
        }
    "#;
    assert!(
        has_ambient_home_lookup(source),
        "function-pointer std::env AGEND_HOME lookups must be rejected"
    );
}

#[test]
fn provisioning_boundary_requires_explicit_home_and_has_no_ambient_lookup() {
    let instructions = read_source("src/instructions.rs");
    for function in ["generate", "generate_for_owner", "generate_with_context"] {
        assert!(
            has_explicit_path_parameter(&instructions, function),
            "instructions::{function} must receive `home: &Path`"
        );
    }
    assert!(
        !has_ambient_home_lookup(&instructions),
        "instructions provisioning must not resolve ambient AGEND_HOME"
    );

    let mcp_config = read_source("src/mcp_config.rs");
    assert!(
        has_explicit_path_parameter(&mcp_config, "configure"),
        "mcp_config::configure must receive `home: &Path`"
    );
    assert!(
        !has_ambient_home_lookup(&mcp_config),
        "MCP provisioning must not resolve ambient AGEND_HOME"
    );
}
