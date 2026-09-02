//! #3281 — shrink-only census of tests mutating production-read env vars.
//!
//! Coverage now runs each test in a nextest process, but plain `cargo test`
//! remains supported. Process-global env mutations in the same test binary can
//! race there. This invariant ratchets the literal-key mutation and production
//! reader shapes it recognizes: new rows fail, and removing a row also fails
//! until the baseline is shrunk. Direct env reads plus the repository's
//! `has_env`, `env_parse`, and `env_parse_min` helpers are covered. Files
//! using an explicit shared mutex are separately audited.

use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::visit::Visit;

const CFG_TEST: &str = "#[cfg(test)]";

/// Exact file/key pairs protected by a shared lock or a serial group whose
/// mutation lives in a helper function. Pair-level exemptions prevent one safe
/// key from hiding a newly introduced, unprotected key in the same file.
const SERIALIZED_PAIRS: &[(&str, &str, &str)] = &[
    (
        "src/api/handlers/mcp_proxy_team_authority_tests.rs",
        "AGEND_HOME",
        "fleet_test_guard serializes the team authority fixtures",
    ),
    (
        "src/app/restart_resume_tests.rs",
        "AGEND_HOME",
        "fleet_test_guard serializes the restart ingress fixture",
    ),
    (
        "src/daemon/ci_watch/poller_tests.rs",
        "GITLAB_TOKEN",
        "GitlabTokenGuard holds ENV_MUTEX and restores the token",
    ),
    (
        "src/daemon/per_tick/recovery_dispatcher.rs",
        "AGEND_AUTO_RECOVERY_STAGE1",
        "with_stage1_flag holds the crate-wide daemon test env lock",
    ),
    (
        "src/daemon/per_tick/shadow_observe.rs",
        "AGEND_SHADOW_OBSERVER",
        "all with_flag callers share serial(shadow_observer)",
    ),
    (
        "src/daemon/retention/worktrees.rs",
        "AGEND_WORKTREE_ARCHIVE_FALLBACK",
        "GC_TRASH_ENV_LOCK / gc_trash_env_guard serializes the env tests",
    ),
    (
        "src/daemon/retention/worktrees.rs",
        "AGEND_WORKTREE_GC_TRASH_DAYS",
        "GC_TRASH_ENV_LOCK / gc_trash_env_guard serializes the env tests",
    ),
    (
        "src/daemon/restart.rs",
        "AGEND_SUCCESSOR_HANDOFF",
        "with_env holds the crate-wide daemon test env lock",
    ),
    (
        "src/daemon/watchdog.rs",
        "AGEND_WATCHDOG_DRY_RUN",
        "tests take the module ENV_LOCK around AGEND_WATCHDOG_DRY_RUN",
    ),
    (
        "src/fleet/tests.rs",
        "AGEND_FLEET_NO_AUTO_MIGRATE",
        "env_guard serializes every mutation of the opt-out key",
    ),
    (
        "src/health.rs",
        "AGEND_PRODUCTIVE_GATE",
        "with_f9_gate uses a OnceLock<Mutex> around AGEND_PRODUCTIVE_GATE",
    ),
    (
        "src/identity.rs",
        "AGEND_INSTANCE_NAME",
        "env_lock guards AGEND_INSTANCE_NAME mutation",
    ),
    (
        "src/inbox/tests.rs",
        "AGEND_POINTER_ONLY_INJECT",
        "ENV_LOCK serializes every pointer-only feature mutation",
    ),
    (
        "src/mcp/handlers/ci/tests.rs",
        "AGEND_HOME",
        "ci_env_test_guard serializes the force-chain fixture",
    ),
    (
        "src/mcp/handlers/tests.rs",
        "AGEND_HOME",
        "fleet_test_guard serializes MCP fixtures",
    ),
    (
        "src/mcp/handlers/tests.rs",
        "AGEND_INSTANCE_NAME",
        "fleet_test_guard serializes MCP fixtures",
    ),
    (
        "src/mcp/handlers/usage_limit_takeover_tests.rs",
        "AGEND_HOME",
        "Fixture retains fleet_test_guard for its lifetime",
    ),
    (
        "src/quickstart/tests.rs",
        "AGEND_BOT_TOKEN",
        "token_env_guard plus the quickstart serial group protects the helper",
    ),
    (
        "src/quickstart/tests.rs",
        "AGEND_TELEGRAM_BOT_TOKEN",
        "token_env_guard plus the quickstart serial group protects the helper",
    ),
    (
        "src/worktree/tests.rs",
        "AGEND_HOME",
        "fleet_test_guard serializes the real repo handler fixture",
    ),
    (
        "src/worktree_cleanup/tests.rs",
        "AGEND_WORKTREE_AUTO_CLEANUP",
        "every env-mutating test takes the module ENV_LOCK",
    ),
    (
        "src/worktree_cleanup/tests.rs",
        "AGEND_WORKTREE_PRUNE_LIVE",
        "every env-mutating test takes the module ENV_LOCK",
    ),
    (
        "src/worktree_cleanup/reconcile_ordering_tests.rs",
        "AGEND_WORKTREE_AUTO_CLEANUP",
        "the test holds worktree_cleanup::tests::ENV_LOCK",
    ),
    (
        "src/worktree_cleanup/windows_cleanup_diagnostics_tests.rs",
        "AGEND_WORKTREE_AUTO_CLEANUP",
        "the test holds worktree_cleanup::tests::ENV_LOCK",
    ),
    (
        "src/worktree_cleanup/windows_cleanup_diagnostics_tests.rs",
        "PATH",
        "the test holds worktree_cleanup::tests::ENV_LOCK",
    ),
    (
        "tests/common/env_gate.rs",
        "AGEND_PRODUCTIVE_GATE",
        "with_f9_gate is the shared OnceLock<Mutex> integration-test helper",
    ),
];

/// Existing unprotected debt. This set may only shrink; never add a row merely
/// to make CI pass. Protect the mutation first, then remove its row.
const KNOWN_UNSERIALIZED: &[(&str, &str)] = &[
    ("src/api/handlers/mcp_proxy.rs", "AGEND_HOME"),
    ("src/api/handlers/mcp_proxy.rs", "AGEND_RESTART_HANDOFF"),
    ("src/api/handlers/mcp_proxy.rs", "AGEND_SUPERVISED"),
    ("src/api/handlers/mcp_proxy_2454_tests.rs", "AGEND_HOME"),
    ("src/api/mod.rs", "AGEND_ALLOWED_ROOTS"),
    (
        "tests/agend_git_shim_phase4_stress.rs",
        "AGEND_WORKTREE_ARCHIVE_FALLBACK",
    ),
    (
        "tests/orphan_provenance_failclosed.rs",
        "AGEND_ORPHAN_LEDGER_FAIL_WRITE",
    ),
];

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_test_file(path: &str) -> bool {
    path.starts_with("tests/")
        || path.ends_with("/tests.rs")
        || path.ends_with("_tests.rs")
        || path.contains("/tests/")
}

fn production_text<'a>(path: &str, body: &'a str) -> &'a str {
    if is_test_file(path) {
        ""
    } else {
        body.find(CFG_TEST).map_or(body, |at| &body[..at])
    }
}

fn constants(body: &str) -> BTreeMap<String, String> {
    let re = Regex::new(r#"(?m)\bconst\s+([A-Z][A-Z0-9_]*)\s*:\s*&str\s*=\s*"([A-Za-z0-9_]+)""#)
        .expect("constant regex");
    re.captures_iter(body)
        .map(|cap| (cap[1].to_string(), cap[2].to_string()))
        .collect()
}

fn env_args(body: &str, operation: &str, constants: &BTreeMap<String, String>) -> BTreeSet<String> {
    let pattern = format!(
        r#"(?:std::)?env::{}(?:_os)?\(\s*(?:"([A-Za-z0-9_]+)"|([A-Z][A-Z0-9_]*))"#,
        operation
    );
    let re = Regex::new(&pattern).expect("env operation regex");
    re.captures_iter(body)
        .filter_map(|cap| {
            cap.get(1)
                .map(|value| value.as_str().to_string())
                .or_else(|| {
                    cap.get(2)
                        .and_then(|name| constants.get(name.as_str()).cloned())
                })
        })
        .collect()
}

fn named_call_args(
    body: &str,
    function: &str,
    constants: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    let pattern = format!(
        r#"\b{}\(\s*(?:\"([A-Za-z0-9_]+)\"|([A-Z][A-Z0-9_]*))"#,
        function
    );
    let re = Regex::new(&pattern).expect("named env-reader regex");
    re.captures_iter(body)
        .filter_map(|cap| {
            cap.get(1)
                .map(|value| value.as_str().to_string())
                .or_else(|| {
                    cap.get(2)
                        .and_then(|name| constants.get(name.as_str()).cloned())
                })
        })
        .collect()
}

fn is_serialized(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let path = attr.path();
        path.is_ident("serial")
            || path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .eq(["serial_test", "serial"].into_iter().map(str::to_string))
    })
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Ident>()
                .is_ok_and(|ident| ident == "test")
    })
}

struct MutationVisitor<'a> {
    constants: &'a BTreeMap<String, String>,
    keys: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for MutationVisitor<'_> {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let syn::Expr::Path(function) = call.func.as_ref() else {
            syn::visit::visit_expr_call(self, call);
            return;
        };
        let segments: Vec<_> = function.path.segments.iter().collect();
        let is_mutation = segments.len() >= 2
            && segments[segments.len() - 2].ident == "env"
            && matches!(
                segments
                    .last()
                    .map(|segment| segment.ident.to_string())
                    .as_deref(),
                Some("set_var" | "remove_var")
            );
        if is_mutation {
            if let Some(argument) = call.args.first() {
                let key = match argument {
                    syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(value),
                        ..
                    }) => Some(value.value()),
                    syn::Expr::Path(path) => path.path.segments.last().and_then(|segment| {
                        self.constants.get(&segment.ident.to_string()).cloned()
                    }),
                    _ => None,
                };
                if let Some(key) = key {
                    self.keys.insert(key);
                }
            }
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_item_fn(&mut self, _function: &'ast syn::ItemFn) {}
}

fn collect_test_mutations(
    items: &[syn::Item],
    in_test_scope: bool,
    constants: &BTreeMap<String, String>,
    keys: &mut BTreeSet<String>,
) {
    for item in items {
        match item {
            syn::Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_test_mutations(
                        nested,
                        in_test_scope || is_cfg_test(&module.attrs),
                        constants,
                        keys,
                    );
                }
            }
            syn::Item::Fn(function)
                if (in_test_scope || is_cfg_test(&function.attrs))
                    && !is_serialized(&function.attrs) =>
            {
                let mut mutations = MutationVisitor {
                    constants,
                    keys: BTreeSet::new(),
                };
                mutations.visit_block(&function.block);
                keys.extend(mutations.keys);
            }
            _ => {}
        }
    }
}

fn unserialized_env_mutations(
    body: &str,
    root_is_test: bool,
    constants: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    let syntax = syn::parse_file(body).expect("parse Rust source");
    let mut keys = BTreeSet::new();
    collect_test_mutations(&syntax.items, root_is_test, constants, &mut keys);
    keys
}

fn census(root: &Path, serialized: &BTreeSet<(String, String)>) -> BTreeSet<(String, String)> {
    let mut files = Vec::new();
    collect_rs(&root.join("src"), &mut files);
    collect_rs(&root.join("tests"), &mut files);

    let mut production_keys = BTreeSet::new();
    let mut bodies = Vec::new();
    for path in files {
        let path_rel = rel(&path, root);
        let body = std::fs::read_to_string(&path).expect("read Rust source");
        let consts = constants(&body);
        let production = production_text(&path_rel, &body);
        production_keys.extend(env_args(production, "var", &consts));
        production_keys.extend(named_call_args(production, "has_env", &consts));
        production_keys.extend(named_call_args(production, "env_parse", &consts));
        production_keys.extend(named_call_args(production, "env_parse_min", &consts));
        bodies.push((path_rel, body, consts));
    }

    let mut unprotected = BTreeSet::new();
    for (path, body, consts) in bodies {
        let mutated = unserialized_env_mutations(&body, is_test_file(&path), &consts);
        for key in mutated.intersection(&production_keys) {
            let pair = (path.clone(), key.clone());
            if !serialized.contains(&pair) {
                unprotected.insert(pair);
            }
        }
    }
    unprotected
}

#[test]
fn production_env_mutation_debt_can_only_shrink() {
    let serialized: BTreeSet<(String, String)> = SERIALIZED_PAIRS
        .iter()
        .map(|(path, key, _)| (path.to_string(), key.to_string()))
        .collect();
    let actual = census(Path::new("."), &serialized);
    let expected: BTreeSet<(String, String)> = KNOWN_UNSERIALIZED
        .iter()
        .map(|(path, key)| (path.to_string(), key.to_string()))
        .collect();
    assert_eq!(
        actual, expected,
        "production-read env mutation census changed. New rows must be protected by a shared \
         serial group; removed rows must also be removed from KNOWN_UNSERIALIZED (never grow it)"
    );
}

#[test]
fn census_detects_a_new_unserialized_mutation() {
    let root = std::env::temp_dir().join(format!("agend-env-census-{}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).expect("create synthetic src");
    std::fs::create_dir_all(root.join("tests")).expect("create synthetic tests");
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn enabled() -> bool { std::env::var(\"AGEND_DEMO\").is_ok() }\n",
    )
    .expect("write synthetic production source");
    std::fs::write(
        root.join("tests/racy.rs"),
        "#[test]\nfn racy() { std::env::set_var(\"AGEND_DEMO\", \"1\"); }\n",
    )
    .expect("write synthetic racy test");
    let rows = census(&root, &BTreeSet::new());
    assert!(rows.contains(&("tests/racy.rs".into(), "AGEND_DEMO".into())));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn census_detects_a_key_read_through_env_parse() {
    let root = std::env::temp_dir().join(format!("agend-env-helper-census-{}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).expect("create synthetic src");
    std::fs::create_dir_all(root.join("tests")).expect("create synthetic tests");
    std::fs::write(
        root.join("src/lib.rs"),
        "fn env_parse<T>(_: &str, default: T) -> T { default }\n\
         pub fn enabled() -> bool { env_parse(\"AGEND_DEMO\", false) }\n",
    )
    .expect("write synthetic production source");
    std::fs::write(
        root.join("tests/racy.rs"),
        "#[test]\nfn racy() { std::env::set_var(\"AGEND_DEMO\", \"1\"); }\n",
    )
    .expect("write synthetic racy test");
    let rows = census(&root, &BTreeSet::new());
    assert!(rows.contains(&("tests/racy.rs".into(), "AGEND_DEMO".into())));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn census_accepts_a_standard_serial_group() {
    let root = std::env::temp_dir().join(format!(
        "agend-env-serialized-census-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(root.join("src")).expect("create synthetic src");
    std::fs::create_dir_all(root.join("tests")).expect("create synthetic tests");
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn enabled() -> bool { std::env::var(\"AGEND_DEMO\").is_ok() }\n",
    )
    .expect("write synthetic production source");
    std::fs::write(
        root.join("tests/serialized.rs"),
        "#[test]\n#[serial_test::serial(env)]\nfn safe() { std::env::set_var(\"AGEND_DEMO\", \"1\"); }\n",
    )
    .expect("write synthetic serialized test");
    assert!(census(&root, &BTreeSet::new()).is_empty());
    std::fs::remove_dir_all(root).ok();
}
