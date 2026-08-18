//! #46776 R5 — provisioning must receive its home explicitly.
//!
//! The provisioning API is a central boundary. Tests should not need a
//! reachability census to prove that a transitive caller selected the right
//! home: the low-level API must make the home an explicit argument, and its
//! implementation must not consult the ambient environment.

fn read(path: &str) -> String {
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

#[test]
fn provisioning_boundary_requires_explicit_home_and_has_no_ambient_lookup() {
    let instructions = read("src/instructions.rs");
    for function in ["generate", "generate_for_owner", "generate_with_context"] {
        assert!(
            has_explicit_path_parameter(&instructions, function),
            "instructions::{function} must receive `home: &Path`"
        );
    }
    assert!(
        !instructions.contains("crate::home_dir()"),
        "instructions provisioning must not resolve ambient AGEND_HOME"
    );

    let mcp_config = read("src/mcp_config.rs");
    assert!(
        has_explicit_path_parameter(&mcp_config, "configure"),
        "mcp_config::configure must receive `home: &Path`"
    );
    assert!(
        !mcp_config.contains("crate::home_dir()"),
        "MCP provisioning must not resolve ambient AGEND_HOME"
    );
}
