//! #46776 R5 — provisioning must receive its home explicitly.
//!
//! The provisioning API is a central boundary. Tests should not need a
//! reachability census to prove that a transitive caller selected the right
//! home: the low-level API must make the home an explicit argument, and its
//! implementation must not consult the ambient environment.

use std::collections::HashSet;

use syn::parse::Parser;
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

#[derive(Default)]
struct AmbientAliases {
    home_dir: HashSet<String>,
    env_modules: HashSet<String>,
    env_functions: HashSet<String>,
    function_pointers: HashSet<String>,
}

fn is_env_lookup_name(name: &str) -> bool {
    matches!(name, "var" | "var_os" | "vars" | "vars_os")
}

fn collect_ambient_aliases(
    tree: &syn::UseTree,
    prefix: &mut Vec<String>,
    aliases: &mut AmbientAliases,
) {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_ambient_aliases(&path.tree, prefix, aliases);
            prefix.pop();
        }
        syn::UseTree::Name(name) => {
            if prefix.len() == 1 && prefix[0] == "crate" && name.ident == "home_dir" {
                aliases.home_dir.insert(name.ident.to_string());
            }
            if prefix.len() == 1 && prefix[0] == "std" && name.ident == "env" {
                aliases.env_modules.insert(name.ident.to_string());
            }
            if prefix.len() == 2
                && prefix[0] == "std"
                && prefix[1] == "env"
                && is_env_lookup_name(&name.ident.to_string())
            {
                aliases.env_functions.insert(name.ident.to_string());
            }
        }
        syn::UseTree::Rename(rename) => {
            if prefix.len() == 1 && prefix[0] == "crate" && rename.ident == "home_dir" {
                aliases.home_dir.insert(rename.rename.to_string());
            }
            if prefix.len() == 1 && prefix[0] == "std" && rename.ident == "env" {
                aliases.env_modules.insert(rename.rename.to_string());
            }
            if prefix.len() == 2
                && prefix[0] == "std"
                && prefix[1] == "env"
                && is_env_lookup_name(&rename.ident.to_string())
            {
                aliases.env_functions.insert(rename.rename.to_string());
            }
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_ambient_aliases(item, prefix, aliases);
            }
        }
        syn::UseTree::Glob(_) => {
            if prefix.len() == 1 && prefix[0] == "crate" {
                aliases.home_dir.insert("*".to_string());
            }
            if prefix.len() == 2 && prefix[0] == "std" && prefix[1] == "env" {
                aliases.env_functions.insert("*".to_string());
            }
        }
    }
}

struct HomeDirAliasCollector<'a> {
    aliases: &'a mut AmbientAliases,
}

impl<'ast> Visit<'ast> for HomeDirAliasCollector<'_> {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        collect_ambient_aliases(&item.tree, &mut Vec::new(), self.aliases);
        visit::visit_item_use(self, item);
    }
}

fn path_segments(path: &syn::Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect()
}

fn is_home_dir_path(path: &syn::Path, aliases: &AmbientAliases) -> bool {
    let segments = path_segments(path);
    segments == ["crate", "home_dir"]
        || (segments.len() == 1
            && (aliases.home_dir.contains(&segments[0]) || aliases.home_dir.contains("*")))
}

fn is_env_lookup_path(path: &syn::Path, aliases: &AmbientAliases) -> bool {
    let segments = path_segments(path);
    let direct = segments.len() == 3
        && segments[0] == "std"
        && segments[1] == "env"
        && is_env_lookup_name(&segments[2]);
    let module_alias = segments.len() == 2
        && aliases.env_modules.contains(&segments[0])
        && is_env_lookup_name(&segments[1]);
    let imported = segments.len() == 1
        && (aliases.env_functions.contains(&segments[0]) || aliases.env_functions.contains("*"));
    let pointer = segments.len() == 1 && aliases.function_pointers.contains(&segments[0]);
    direct || module_alias || imported || pointer
}

fn cfg_is_test_only(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) => {
            let Ok(children) =
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
                    .parse2(list.tokens.clone())
            else {
                return false;
            };
            if list.path.is_ident("all") {
                !children.is_empty() && children.iter().any(cfg_is_test_only)
            } else if list.path.is_ident("any") {
                !children.is_empty() && children.iter().all(cfg_is_test_only)
            } else {
                false
            }
        }
        syn::Meta::NameValue(_) => false,
    }
}

fn has_test_cfg(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attribute| {
        if !attribute.path().is_ident("cfg") {
            return false;
        }
        let syn::Meta::List(list) = &attribute.meta else {
            return false;
        };
        syn::parse2::<syn::Meta>(list.tokens.clone())
            .map(|meta| cfg_is_test_only(&meta))
            .unwrap_or(false)
    })
}

struct AmbientHomeCallFinder<'a> {
    aliases: &'a mut AmbientAliases,
    found: bool,
}

impl<'ast> Visit<'ast> for AmbientHomeCallFinder<'_> {
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if has_test_cfg(&module.attrs) {
            return;
        }
        visit::visit_item_mod(self, module);
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let Some(init) = &local.init {
            if let syn::Expr::Path(path) = init.expr.as_ref() {
                if is_env_lookup_path(&path.path, self.aliases) {
                    if let syn::Pat::Ident(pattern) = &local.pat {
                        self.aliases
                            .function_pointers
                            .insert(pattern.ident.to_string());
                    }
                }
            }
        }
        visit::visit_local(self, local);
    }

    fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
        self.found |= is_home_dir_path(&expression.path, self.aliases)
            || is_env_lookup_path(&expression.path, self.aliases);
        visit::visit_expr_path(self, expression);
    }
}

fn has_ambient_home_lookup(source: &str) -> bool {
    let file = syn::parse_file(source).expect("parse source");
    let mut aliases = AmbientAliases::default();
    HomeDirAliasCollector {
        aliases: &mut aliases,
    }
    .visit_file(&file);

    let mut finder = AmbientHomeCallFinder {
        aliases: &mut aliases,
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
fn cfg_filter_only_skips_positive_test_modules() {
    for (label, source) in [
        (
            "cfg(not(test))",
            r#"
                #[cfg(not(test))]
                mod production {
                    fn generate(home: &Path) {
                        let _ = home;
                        let _ = std::env::var("AGEND_HOME");
                    }
                }
            "#,
        ),
        (
            "cfg(feature = \"test-mode\")",
            r#"
                #[cfg(feature = "test-mode")]
                mod production {
                    fn generate(home: &Path) {
                        let _ = home;
                        let _ = std::env::var("AGEND_HOME");
                    }
                }
            "#,
        ),
    ] {
        assert!(
            has_ambient_home_lookup(source),
            "{label} must not hide an ambient-home lookup from the production invariant"
        );
    }
}

#[test]
fn structural_ambient_env_reference_matrix_is_rejected() {
    for source in [
        r#"
            fn generate(home: &Path) {
                let _ = home;
                const KEY: &str = "AGEND_HOME";
                let _ = std::env::var(KEY);
            }
        "#,
        r#"
            fn generate(home: &Path) {
                let _ = home;
                let key = "AGEND_HOME";
                let _ = std::env::var(key);
            }
        "#,
        r#"
            fn generate(home: &Path) {
                let _ = home;
                let _ = std::env::var(&"AGEND_HOME");
                let _ = std::env::var(concat!("AGEND_", "HOME"));
            }
        "#,
        r#"
            fn generate(home: &Path) {
                let _ = home;
                let _ = std::env::vars();
            }
        "#,
        r#"
            static LOOKUP: fn(&str) -> Result<String, std::env::VarError> = std::env::var;

            fn generate(home: &Path) {
                let _ = home;
                let _ = LOOKUP("AGEND_HOME");
            }
        "#,
    ] {
        assert!(
            has_ambient_home_lookup(source),
            "every std::env lookup reference must be rejected regardless of key syntax"
        );
    }
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
