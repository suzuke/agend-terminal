//! The DISCONNECTED recovery path is a bounded bridge-only call chain. Parse
//! every named body in that chain so function reordering and imported aliases
//! cannot hide destructive instance-lifecycle references from this guard.

use std::collections::{HashMap, HashSet};

use syn::visit::Visit;

const FORBIDDEN: &[&str] = &[
    "restart_instance",
    "crash_respawn",
    "respawn_watchdog",
    "spawn_agent",
    "delete_instance",
];

struct Target<'a> {
    source: &'a str,
    owner: Option<&'a str>,
    function: &'a str,
    must_name: &'a [&'a str],
}

fn find_body<'a>(file: &'a syn::File, target: &Target<'_>) -> &'a syn::Block {
    if let Some(owner) = target.owner {
        return file
            .items
            .iter()
            .filter_map(|item| match item {
                syn::Item::Impl(item_impl) => Some(item_impl),
                _ => None,
            })
            .filter(|item_impl| match item_impl.self_ty.as_ref() {
                syn::Type::Path(path) => path
                    .path
                    .segments
                    .last()
                    .is_some_and(|segment| segment.ident == owner),
                _ => false,
            })
            .flat_map(|item_impl| &item_impl.items)
            .find_map(|item| match item {
                syn::ImplItem::Fn(method) if method.sig.ident == target.function => {
                    Some(&method.block)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("method {owner}::{} must exist", target.function));
    }
    file.items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(item_fn) if item_fn.sig.ident == target.function => Some(&item_fn.block),
            _ => None,
        })
        .unwrap_or_else(|| panic!("free function {} must exist", target.function))
}

struct AliasCollector(HashMap<String, String>);

impl<'ast> Visit<'ast> for AliasCollector {
    fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
        self.0
            .insert(rename.rename.to_string(), rename.ident.to_string());
    }
}

fn resolve_alias(mut ident: String, aliases: &HashMap<String, String>) -> String {
    for _ in 0..aliases.len() {
        let Some(next) = aliases.get(&ident) else {
            break;
        };
        ident.clone_from(next);
    }
    ident
}

fn visit_token_idents(
    tokens: proc_macro2::TokenStream,
    aliases: &HashMap<String, String>,
    found: &mut HashSet<String>,
) {
    for token in tokens {
        match token {
            proc_macro2::TokenTree::Group(group) => {
                visit_token_idents(group.stream(), aliases, found);
            }
            proc_macro2::TokenTree::Ident(ident) => {
                found.insert(resolve_alias(ident.to_string(), aliases));
            }
            _ => {}
        }
    }
}

fn body_identifiers(file: &syn::File, body: &syn::Block) -> HashSet<String> {
    let mut aliases = AliasCollector(HashMap::new());
    for item in &file.items {
        if let syn::Item::Use(item_use) = item {
            aliases.visit_item_use(item_use);
        }
    }
    aliases.visit_block(body);

    struct IdentifierCollector<'a> {
        aliases: &'a HashMap<String, String>,
        found: HashSet<String>,
    }
    impl<'ast> Visit<'ast> for IdentifierCollector<'_> {
        fn visit_ident(&mut self, ident: &'ast syn::Ident) {
            self.found
                .insert(resolve_alias(ident.to_string(), self.aliases));
        }

        fn visit_macro(&mut self, item_macro: &'ast syn::Macro) {
            visit_token_idents(item_macro.tokens.clone(), self.aliases, &mut self.found);
        }
    }
    let mut identifiers = IdentifierCollector {
        aliases: &aliases.0,
        found: HashSet::new(),
    };
    identifiers.visit_block(body);
    identifiers.found
}

fn scan_target_identifiers(target: &Target<'_>) -> HashSet<String> {
    let file = syn::parse_file(target.source).expect("production source must parse");
    body_identifiers(&file, find_body(&file, target))
}

#[test]
fn focused_pane_reconnect_call_chain_has_no_agent_lifecycle_path() {
    let targets = [
        Target {
            source: include_str!("../src/app/dispatch.rs"),
            owner: None,
            function: "reconnect_focused_pane_with",
            must_name: &["create_remote_pane", "reconnect_or_append_agent_pane"],
        },
        Target {
            source: include_str!("../src/app/pane_factory.rs"),
            owner: None,
            function: "create_remote_pane",
            must_name: &["BridgeClient", "connect"],
        },
        Target {
            source: include_str!("../src/bridge_client.rs"),
            owner: Some("BridgeClient"),
            function: "connect",
            must_name: &["connect_agent_timeout"],
        },
        Target {
            source: include_str!("../src/ipc.rs"),
            owner: None,
            function: "connect_agent_timeout",
            must_name: &[],
        },
        Target {
            source: include_str!("../src/layout/mod.rs"),
            owner: Some("Layout"),
            function: "reconnect_or_append_agent_pane",
            must_name: &[],
        },
    ];

    for target in targets {
        let identifiers = scan_target_identifiers(&target);
        for required in target.must_name {
            assert!(
                identifiers.contains(*required),
                "{} must keep the bounded call-chain edge to {required}",
                target.function
            );
        }
        for forbidden in FORBIDDEN {
            assert!(
                !identifiers.contains(*forbidden),
                "{} must not reference destructive lifecycle path {forbidden}",
                target.function
            );
        }
    }
}

#[test]
fn ast_guard_resolves_aliases_and_ignores_function_order() {
    let source = r#"
        fn unrelated() {}
        fn create_remote_pane() {
            use crate::agent_ops::restart_instance as ri;
            ri();
        }
    "#;
    let target = Target {
        source,
        owner: None,
        function: "create_remote_pane",
        must_name: &[],
    };

    assert!(scan_target_identifiers(&target).contains("restart_instance"));
}
