//! #2453 AppState ownership slice — structural RED guards.
//!
//! All scans read ONLY production regions (before any `#[cfg(test)]` cutoff),
//! mirroring `run_app_wires_shadow_socket_server_2413` in the parent module,
//! so needles in this test module can never self-match (#2433 vacuous-pin
//! class). Owner needles are built with `format!` at runtime for the same
//! reason.

/// The 16 durable loop owners that root `AppState` must own after GREEN.
/// `attach_jobs`/`attach_workers` are the ONLY permitted mutable lifecycle
/// exceptions outside `AppState` (frozen contract) and are deliberately
/// absent from this list.
const DURABLE_LOOP_OWNERS_2453: [&str; 16] = [
    "ui",
    "known_remote_agents",
    "pending_fwd",
    "needs_resize",
    "last_remote_sync",
    "last_session_save",
    "last_session_json",
    "last_draw",
    "dirty",
    "last_notif_sync",
    "last_decision_sync",
    "pending_decisions_total",
    "booting",
    "restart_outcome",
    "restart_probe",
    "restart_commit_pending",
];

/// Combined production region: src/app/mod.rs (before its `#[cfg(test)]`
/// cutoff) plus, when present, the sibling `src/app/app_state.rs` that owns
/// the AppState/RestartState definitions after GREEN. The sibling is OPTIONAL:
/// on the exact RED base it does not exist yet, and a missing file must
/// contribute nothing (never an I/O panic) so the intended RED failures fire
/// for the right structural reason.
fn app_prod_region() -> String {
    let source = std::fs::read_to_string("src/app/mod.rs")
        .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
        .expect("source file must be readable from test cwd");
    let cutoff = source.find("#[cfg(test)]").unwrap_or(source.len());
    let mut prod = source[..cutoff].to_string();
    for candidate in [
        "src/app/app_state.rs",
        "agend-terminal/src/app/app_state.rs",
    ] {
        if let Ok(extra) = std::fs::read_to_string(candidate) {
            let extra_cutoff = extra.find("#[cfg(test)]").unwrap_or(extra.len());
            prod.push_str(&extra[..extra_cutoff]);
            break;
        }
    }
    prod
}

/// Module-level struct body: from the declaration to the first line-start
/// closing brace. Struct field lists contain no nested line-start braces,
/// so this is deterministic without a full lexer.
fn struct_body<'a>(prod: &'a str, decl: &str) -> &'a str {
    let start = prod
        .find(decl)
        .unwrap_or_else(|| panic!("missing `{decl}` in the app production region"));
    let end = prod[start..]
        .find("\n}")
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("unterminated `{decl}` body"));
    &prod[start..end]
}

/// #2453 RED 1/3: the root `AppState` struct must exist in the production
/// region and own ALL 16 durable loop owners as fields.
#[test]
fn app_state_owns_all_durable_loop_owners_2453() {
    let prod = app_prod_region();
    assert!(
        prod.contains("struct AppState"),
        "#2453: root `struct AppState` must exist in the app production \
         region — the durable loop owners are still loose run_app locals"
    );
    let body = struct_body(&prod, "struct AppState");
    for owner in DURABLE_LOOP_OWNERS_2453 {
        let direct = format!("{owner}:");
        // restart_outcome/restart_probe/restart_commit_pending live inside
        // the bounded typed RestartState sub-owner (RED 2/3); AppState owns
        // them transitively through its RestartState field.
        let via_restart = matches!(
            owner,
            "restart_outcome" | "restart_probe" | "restart_commit_pending"
        ) && body.contains("RestartState");
        assert!(
            body.contains(&direct) || via_restart,
            "#2453: AppState must own durable loop owner `{owner}` \
             (directly or via the typed RestartState field)"
        );
    }
}

/// #2453 RED 2/3: a bounded typed `RestartState` must own exactly the
/// three restart owners — and none of the other 13 durable owners.
#[test]
fn restart_state_typed_owner_bounded_2453() {
    let prod = app_prod_region();
    assert!(
        prod.contains("struct RestartState"),
        "#2453: bounded typed `struct RestartState` must exist in the app \
         production region and own restart_outcome/restart_probe/restart_commit_pending"
    );
    let body = struct_body(&prod, "struct RestartState");
    for field in ["restart_outcome", "restart_probe", "restart_commit_pending"] {
        assert!(
            body.contains(&format!("{field}:")),
            "#2453: RestartState must own `{field}`"
        );
    }
    for other in DURABLE_LOOP_OWNERS_2453 {
        if matches!(
            other,
            "restart_outcome" | "restart_probe" | "restart_commit_pending"
        ) {
            continue;
        }
        assert!(
            !body.contains(&format!("{other}:")),
            "#2453: RestartState is BOUNDED to the three restart owners — \
             `{other}` must not migrate into it"
        );
    }
    let app_state = struct_body(&prod, "struct AppState");
    assert!(
        app_state.contains("RestartState"),
        "#2453: AppState must hold the typed RestartState sub-owner"
    );
}

/// #2453 RED 3/3: no loose durable-owner `let` locals may remain in the
/// production region — after GREEN the owners live in AppState. Only
/// `attach_jobs`/`attach_workers` stay as permitted lifecycle locals.
#[test]
fn run_app_no_loose_durable_owner_locals_2453() {
    let prod = app_prod_region();
    let mut loose: Vec<String> = Vec::new();
    for owner in DURABLE_LOOP_OWNERS_2453 {
        for needle in [
            format!("let mut {owner} ="),
            format!("let mut {owner}:"),
            format!("let {owner} ="),
            format!("let {owner}:"),
        ] {
            if prod.contains(&needle) {
                loose.push(needle);
            }
        }
    }
    assert!(
        loose.is_empty(),
        "#2453: durable loop owners must live in AppState, not as loose \
         production locals; found: {loose:?}"
    );
}

/// #2453 control (green today, pins GREEN): run_app and the daemon's
/// run_core remain SEPARATE entry points — this slice must not unify them.
#[test]
fn run_app_and_run_core_remain_separate_2453() {
    let prod = app_prod_region();
    assert!(
        prod.contains("fn run_app("),
        "#2453: fn run_app must remain in src/app/mod.rs"
    );
    let daemon = std::fs::read_to_string("src/daemon/mod.rs")
        .or_else(|_| std::fs::read_to_string("agend-terminal/src/daemon/mod.rs"))
        .expect("daemon source must be readable from test cwd");
    assert!(
        daemon.contains("fn run_core("),
        "#2453: fn run_core must remain in src/daemon/mod.rs — no \
         run_app/run_core unification in this slice"
    );
}

/// #2453 control (green today, pins GREEN): ownership must move by real
/// restructuring — RefCell/mem::take evasion may not enter the production
/// region (both are absent today).
#[test]
fn app_prod_region_bans_ownership_evasion_2453() {
    let prod = app_prod_region();
    for evasion in ["RefCell", "mem::take"] {
        assert!(
            !prod.contains(evasion),
            "#2453: `{evasion}` must not appear in the app production \
             region — ownership moves by restructuring, not interior \
             mutability or take-and-swap evasion"
        );
    }
}
