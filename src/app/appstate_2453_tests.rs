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

// ── #2453 Slice 2: method-extraction / thin-loop guards ──
//
// Same scanning discipline as Slice 1: prod-region only, literal needles are
// safe here because this sibling test file is never part of a scanned region.

/// `run_app`'s source region: from its `fn` line to the next top-level item
/// (`fn setup_app_bootstrap`). Matches root's measurement boundary.
fn run_app_region() -> String {
    let source = std::fs::read_to_string("src/app/mod.rs")
        .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
        .expect("source file must be readable from test cwd");
    let start = source
        .find("fn run_app(")
        .expect("fn run_app must exist in src/app/mod.rs");
    let end = source[start..]
        .find("\nfn setup_app_bootstrap(")
        .map(|offset| start + offset)
        .expect("fn setup_app_bootstrap must follow run_app");
    source[start..end].to_string()
}

/// Inline-logic witnesses that currently live inside `run_app`'s loop and
/// must migrate into cohesive `AppState` methods: boot catch-up drain,
/// throttled session persistence, remote-agent discovery, notification/badge
/// sync, and the restart commit poll.
const LOOP_LOGIC_WITNESSES_2453S2: [&str; 5] = [
    "render::drain_all_panes_until",
    "session::save_session_if_changed",
    "pane_factory::create_remote_pane",
    "should_sync_notifications",
    "poll_commit_pending",
];

/// #2453 Slice 2 RED 1/3: run_app must become genuine thin orchestration —
/// construction/dispatch/render/select/teardown around AppState methods.
/// Closure criterion is ~80 lines (root contract tightening); 90 gives
/// honest comment headroom and is anti-gaming-guarded by the witness and
/// impl pins below.
#[test]
fn run_app_is_thin_orchestration_2453s2() {
    let region = run_app_region();
    let lines = region.lines().count();
    assert!(
        lines <= 90,
        "#2453 Slice 2: run_app must be thin orchestration (~80 lines, cap 90); \
         currently {lines} lines"
    );
}

/// #2453 Slice 2 RED 2/3: the extracted loop logic must live in cohesive
/// `impl AppState` methods — not free helper functions (laundering).
#[test]
fn loop_logic_lives_in_app_state_methods_2453s2() {
    let prod = app_prod_region();
    let impl_start = prod.find("impl AppState").unwrap_or_else(|| {
        panic!(
            "#2453 Slice 2: `impl AppState` must exist in the app production \
             region — the loop logic has not been extracted into methods"
        )
    });
    let impl_end = prod[impl_start..]
        .find("\n}")
        .map(|offset| impl_start + offset)
        .expect("unterminated impl AppState body");
    let body = &prod[impl_start..impl_end];
    for witness in LOOP_LOGIC_WITNESSES_2453S2 {
        assert!(
            body.contains(witness),
            "#2453 Slice 2: loop logic `{witness}` must live inside an \
             AppState method, not a free helper (laundering) or inline in run_app"
        );
    }
}

/// #2453 Slice 2 RED 3/3: the witnesses must actually LEAVE run_app — a
/// copy kept inline would satisfy the impl pin while thinning nothing.
#[test]
fn run_app_witnesses_moved_out_2453s2() {
    let region = run_app_region();
    let still_inline: Vec<&str> = LOOP_LOGIC_WITNESSES_2453S2
        .into_iter()
        .filter(|witness| region.contains(witness))
        .collect();
    assert!(
        still_inline.is_empty(),
        "#2453 Slice 2: loop logic must move out of run_app into AppState \
         methods; still inline: {still_inline:?}"
    );
}

/// #2453 Slice 2 control (green today, pins GREEN): the orchestration
/// skeleton STAYS in run_app — AppState construction, the select! loop, and
/// teardown are the orchestrator's job and may not be laundered away.
#[test]
fn run_app_keeps_orchestration_skeleton_2453s2() {
    let region = run_app_region();
    for anchor in [
        "let mut state = AppState",
        "crossbeam_channel::select!",
        "app_teardown(",
    ] {
        assert!(
            region.contains(anchor),
            "#2453 Slice 2: run_app must keep the orchestration anchor `{anchor}`"
        );
    }
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
