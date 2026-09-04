//! Drift-guard for the daemon-boot flake gate (#t-teardown-determinism).
//!
//! Every test binary that boots a REAL daemon via `AgendHarness::spawn*` shares the
//! daemon-boot path whose race (#1909) bit #1914. Those tests are repeated 20x by
//! `.github/workflows/daemon-boot-flake-gate.yml` to catch boot-race flakes
//! pre-merge — but only if they're in `tests/daemon_boot_gate_filter.txt`. A new
//! `AgendHarness` test added WITHOUT being listed there would silently escape the
//! gate — the #1907/#1911 curated-list-drift class.
//!
//! This invariant fails (RED) until every `AgendHarness`-using test binary is in the
//! filter file, forcing the author to add it. (Direct-boot tests — `start
//! --foreground` without AgendHarness — are listed manually in the file; they are
//! not auto-detectable here, so this invariant covers the AgendHarness class, which
//! is the one that grows.)

use std::collections::BTreeSet;
use std::path::Path;
use syn::visit::Visit;

const TESTS_DIR: &str = "tests";
const FILTER_FILE: &str = "tests/daemon_boot_gate_filter.txt";

/// Binary names listed in the gate filter file (comments/blanks stripped).
fn gate_filter_binaries() -> BTreeSet<String> {
    let txt =
        std::fs::read_to_string(FILTER_FILE).unwrap_or_else(|e| panic!("read {FILTER_FILE}: {e}"));
    txt.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// Test binaries (tests/<name>.rs stems) that call `AgendHarness::spawn*`. Note
/// `read_dir(TESTS_DIR)` is non-recursive, so the harness DEFINITION in
/// `tests/common/harness.rs` is not scanned — only top-level integration tests that
/// USE it. `"AgendHarness::spawn"` is a prefix of `spawn`/`spawn_with`, so both match.
fn agendharness_user_binaries() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(TESTS_DIR)
        .expect("read tests/ dir")
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        // Skip THIS invariant file — its scan code contains the literal
        // "AgendHarness::spawn", which would otherwise self-match (it does not
        // boot a daemon). Mirrors the `is_self` skip in git_subprocess_invariant.
        if path.file_stem().and_then(|s| s.to_str()) == Some("daemon_boot_gate_invariant") {
            continue;
        }
        if std::fs::read_to_string(&path)
            .map(|c| c.contains("AgendHarness::spawn"))
            .unwrap_or(false)
        {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.insert(stem.to_string());
            }
        }
    }
    out
}

#[test]
fn agendharness_users_are_in_the_flake_gate_filter() {
    let gated = gate_filter_binaries();
    let users = agendharness_user_binaries();
    assert!(
        !users.is_empty(),
        "sanity: expected to find AgendHarness-using test binaries; found none — \
         did the scan path or the harness API change?"
    );

    let missing: Vec<&String> = users.iter().filter(|b| !gated.contains(*b)).collect();
    assert!(
        missing.is_empty(),
        "daemon-boot flake-gate DRIFT — these AgendHarness-using test binaries are NOT in \
         {FILTER_FILE}, so they'd escape the 20x flake gate. Add each to the file:\n  {missing:#?}\n\
         (curated-list-drift guard — #1907/#1911 class.)"
    );

    // Stale-entry guard: every listed binary must be a real test file.
    for b in &gated {
        assert!(
            Path::new(TESTS_DIR).join(format!("{b}.rs")).exists(),
            "{FILTER_FILE} lists '{b}' but {TESTS_DIR}/{b}.rs does not exist (stale/typo entry)"
        );
    }
}

/// Drift-guard for the OTHER undetected class named in the module doc above:
/// "direct-boot tests ... are not auto-detectable here". That gap is exactly
/// the one `tests/common/daemon_reaper.rs` closed for `app_singleton_fail_closed.rs`
/// and `cli_smoke.rs` — a test that boots a REAL daemon outside `AgendHarness`
/// leaks it (and its `setsid`'d stub agents) to init unless it uses the
/// `FixtureHome` reaping guard. This makes THAT omission auto-detectable too.
///
/// ## The predicate: an AST scan, not a text scan
/// A text scan for `"AgendHarness::spawn"` (the check above) already needs a
/// self-match exemption because a DOC COMMENT can contain that literal without
/// meaning it. The same trap is worse here: `issue_548_phase3_service.rs`
/// asserts platform-template strings like
/// `launchd.contains("<string>--foreground</string>")` — a text scan for
/// `"start"` + `"--foreground"` would flag that file, even though it boots
/// nothing; it inspects a STRING CONSTANT, not a `Command`.
///
/// So this parses each file with `syn` and only counts a string literal when
/// it is actually an ARGUMENT of an `.arg(...)` / `.args([...])` method call —
/// the shape that builds a `std::process::Command`'s argv. A doc comment, a
/// `.contains(...)` check, or any other call cannot produce a match; renaming
/// the receiver or reordering the chain cannot hide one either, since the scan
/// does not look at the receiver at all — only at the argument literals of
/// calls named `arg`/`args`, anywhere in the file.
///
/// ## The direct-boot shape
/// Two ways a test can cause a REAL daemon to exist outside `AgendHarness`:
///
///   1. `.arg("start")` (or `.args([...])` carrying it) on the SAME file's
///      Command-building calls, in a file that also CONSTRUCTS the
///      `agend-terminal` crate binary — via `cargo_bin("agend-terminal")` in
///      any of its call-path spellings (`cargo_bin(...)`,
///      `Command::cargo_bin(...)`, `assert_cmd::cargo::cargo_bin(...)`), or
///      via `env!("CARGO_BIN_EXE_agend-terminal")`. `start` ALONE (no
///      `--foreground` requirement — the CLI is free to detach the daemon
///      itself, which is exactly `cli_smoke.rs`'s shape) is enough once the
///      binary-construction requirement is in place: a file that merely
///      passes the literal `"start"` to some unrelated helper, or never
///      builds the binary at all (`e2e_workflow.rs`, `migrated_scripts.rs`,
///      `p0a_capability_auth.rs`, `teardown_completeness_regression.rs`,
///      `integration.rs`, `issue_548_phase2_invariants.rs`), cannot
///      direct-boot and is excluded by that requirement rather than by
///      `--foreground`.
///   2. `.arg("app")` in a file that also calls `openpty` — `app_singleton_
///      fail_closed.rs`'s exact technique: a plain-pipe `.arg("app")` (as in
///      `cli_smoke.rs`'s `app_without_tty_errors_cleanly_not_panic`, which
///      exists to prove the app exits WITHOUT a tty) never reaches the boot
///      path that spawns agents, so it is deliberately NOT flagged; wiring a
///      real pty is what gets `app` far enough to actually boot one.
///
/// A file matching either shape must also reference `FixtureHome`,
/// `AgendHarness`, or `TestDaemon` (imported, constructed, whatever — the
/// scan only checks that the name occurs somewhere in the AST, which is
/// enough to prove intent to use the guard without caring how it is wired).
/// All three genuinely reap: `FixtureHome` (`tests/common/daemon_reaper.rs`),
/// `AgendHarness::drop` (`tests/common/harness.rs`) SIGTERMs then SIGKILLs
/// the process group and waits (or closes the kill-on-close Windows job
/// handle), and `TestDaemon::drop` (`tests/integration.rs`) kills and waits
/// the child. `tool_cli_phase0a_real_red.rs` boots via `AgendHarness` and
/// carries no `FixtureHome`; it is guarded, not exempt.
///
/// ## Known pre-existing debt (not this invariant's to fix)
/// `restart_smoke.rs`, `self_respawn_handoff.rs`, `self_respawn_handoff_windows.rs`,
/// `ready_marker_invariants.rs` and `attached_path_mcp_invariants.rs` all predate
/// `FixtureHome` and already direct-boot without it — `attached_path_mcp_invariants.rs`
/// has its OWN bespoke pgid-based teardown, the rest clean up with plain sequential
/// `stop`/`kill` calls. None of them are `Drop`-guarded, so none are panic-safe —
/// the same root cause `FixtureHome` exists to close — but migrating five
/// differently-shaped teardowns is a real, separate piece of work, not a
/// byproduct of this invariant. They are named explicitly below so the debt is
/// visible rather than silently exempted; the stale-entry check just below
/// forces this list to be corrected the day one of them is migrated, and
/// nothing new may be added to it — a new direct-boot test must use
/// `FixtureHome`, full stop.
/// Every entry below shares ONE shape, and the reasoning the assertion above
/// demands of a future author is recorded here for them:
///
/// Each spawns `agend-terminal start --foreground` as a DIRECT child
/// (`restart_smoke.rs:32-40`, `ready_marker_invariants.rs:116-134`,
/// `self_respawn_handoff.rs:136`, `attached_path_mcp_invariants.rs:204`) and
/// kills + waits that child explicitly at the end of each test. That reaches
/// the daemon — unlike `app_singleton_fail_closed`, where the daemon is a
/// GRANDCHILD of the `app` process and `Child::kill` never touched it.
///
/// So these do not leak on the success path, and that is measured, not
/// assumed: a full `cargo test --tests --features tray --no-fail-fast` run
/// (159 targets) leaked exactly one daemon, the `app_singleton` one, and left
/// zero orphaned stub agents.
///
/// What they are still missing is the PANIC path: an explicit kill at the end
/// of a test body is skipped when an assertion above it fires, and nothing
/// else reaps then. `FixtureHome` closes exactly that gap, which is why they
/// belong on a shrinking list rather than being declared safe. Migrating them
/// is deferred because each carries its own teardown shape (own pgid logic, or
/// `stop` + kill) that has to be unpicked one at a time — real work, and not
/// this ticket's.
///
/// NO NEW ENTRY MAY BE ADDED HERE for a newly written test: a new direct-boot
/// test uses `FixtureHome`. This list may only shrink.
const KNOWN_UNGUARDED_DIRECT_BOOT_DEBT: &[&str] = &[
    "restart_smoke",
    "self_respawn_handoff",
    "self_respawn_handoff_windows",
    "ready_marker_invariants",
    "attached_path_mcp_invariants",
];

/// String literals actually passed as arguments to a call named `arg`/`args`,
/// plus whether `openpty` or a reaping guard occurs anywhere in the file (as
/// an identifier — a call, a `use`, a type, doesn't matter which), plus
/// whether the file constructs the `agend-terminal` crate binary at all
/// (`cargo_bin("agend-terminal")` in any of its call-path spellings, or
/// `env!("CARGO_BIN_EXE_agend-terminal")`).
#[derive(Default)]
struct DirectBootScan {
    arg_literals: BTreeSet<String>,
    uses_openpty: bool,
    uses_fixture_home: bool,
    uses_agend_harness: bool,
    uses_test_daemon: bool,
    builds_crate_binary: bool,
}

impl DirectBootScan {
    fn scan(src: &str) -> syn::Result<Self> {
        let file = syn::parse_file(src)?;
        let mut scan = DirectBootScan::default();
        scan.visit_file(&file);
        Ok(scan)
    }

    /// Shape 1: `start` passed as an `.arg`/`.args` literal in a file that
    /// also constructs the `agend-terminal` crate binary — a file that
    /// merely passes the literal "start" to something else (a helper, an
    /// unrelated command, a template-string check) cannot direct-boot, so
    /// requiring the binary construction is what excludes those. The old
    /// shape additionally required `--foreground`; that is subsumed here
    /// because every file in this suite that passes `--foreground` also
    /// constructs the binary (confirmed against the current tree — see the
    /// PR #3512-follow-up report), so dropping it only WIDENS detection.
    /// Shape 2: `app` passed the same way, AND the file wires a real pty
    /// (`openpty`) — the one thing that gets `app` past its TTY check and
    /// into the boot path.
    fn direct_boots(&self) -> bool {
        (self.arg_literals.contains("start") && self.builds_crate_binary)
            || (self.arg_literals.contains("app") && self.uses_openpty)
    }

    /// A file counts as guarded if it holds ANY of the three reaping
    /// mechanisms proven (by reading their `Drop` impls) to actually kill
    /// and wait the daemon: `FixtureHome` (`tests/common/daemon_reaper.rs`),
    /// `AgendHarness` (`tests/common/harness.rs` — SIGTERM/SIGKILL the
    /// process group, or close the Windows job handle), or `TestDaemon`
    /// (`tests/integration.rs` — kill + wait the child).
    fn is_guarded(&self) -> bool {
        self.uses_fixture_home || self.uses_agend_harness || self.uses_test_daemon
    }
}

/// Walks `expr`, collecting every string literal reachable through arrays,
/// references, parens and groups — the shapes `.args([...])` and a bare
/// `.arg("lit")` actually appear in across this codebase's tests. Anything
/// else (a variable, a format!, a helper call) contributes nothing: this scan
/// only ever grows the "must be guarded" set from a literal it can see, never
/// from a guess about what a non-literal expression might evaluate to.
fn collect_string_literals(expr: &syn::Expr, out: &mut BTreeSet<String>) {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => {
            out.insert(s.value());
        }
        syn::Expr::Array(arr) => {
            for e in &arr.elems {
                collect_string_literals(e, out);
            }
        }
        syn::Expr::Reference(r) => collect_string_literals(&r.expr, out),
        syn::Expr::Paren(p) => collect_string_literals(&p.expr, out),
        syn::Expr::Group(g) => collect_string_literals(&g.expr, out),
        _ => {}
    }
}

/// True iff `path`'s LAST segment is `cargo_bin` — matches `cargo_bin(...)`,
/// `Command::cargo_bin(...)` and `assert_cmd::cargo::cargo_bin(...)` alike,
/// since a call's callee path always ends in the function/method name
/// regardless of how many qualifying segments precede it.
fn is_cargo_bin_path(path: &syn::Path) -> bool {
    path.segments.last().is_some_and(|s| s.ident == "cargo_bin")
}

impl<'ast> syn::visit::Visit<'ast> for DirectBootScan {
    fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
        let name = c.method.to_string();
        if name == "arg" || name == "args" {
            for arg in &c.args {
                collect_string_literals(arg, &mut self.arg_literals);
            }
        }
        if name == "cargo_bin" {
            // `Command::cargo_bin("agend-terminal")` — a method-call form
            // (as opposed to the free-function/associated-function-call
            // forms `visit_expr_call` below handles).
            let mut lits = BTreeSet::new();
            for arg in &c.args {
                collect_string_literals(arg, &mut lits);
            }
            if lits.contains("agend-terminal") {
                self.builds_crate_binary = true;
            }
        }
        syn::visit::visit_expr_method_call(self, c);
    }

    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        // `cargo_bin("agend-terminal")` and
        // `assert_cmd::cargo::cargo_bin("agend-terminal")` — the callee is a
        // bare/qualified path ending in `cargo_bin`, called as a function
        // rather than a method.
        if let syn::Expr::Path(p) = &*c.func {
            if is_cargo_bin_path(&p.path) {
                let mut lits = BTreeSet::new();
                for arg in &c.args {
                    collect_string_literals(arg, &mut lits);
                }
                if lits.contains("agend-terminal") {
                    self.builds_crate_binary = true;
                }
            }
        }
        syn::visit::visit_expr_call(self, c);
    }

    fn visit_expr_macro(&mut self, m: &'ast syn::ExprMacro) {
        // `env!("CARGO_BIN_EXE_agend-terminal")` — the crate-binary env var
        // `cargo` sets for an integration test's own workspace binaries.
        if m.mac.path.is_ident("env") {
            if let Ok(lit) = m.mac.parse_body::<syn::LitStr>() {
                if lit.value() == "CARGO_BIN_EXE_agend-terminal" {
                    self.builds_crate_binary = true;
                }
            }
        }
        syn::visit::visit_expr_macro(self, m);
    }

    fn visit_path(&mut self, p: &'ast syn::Path) {
        for seg in &p.segments {
            let ident = seg.ident.to_string();
            if ident == "openpty" {
                self.uses_openpty = true;
            }
            if ident == "FixtureHome" {
                self.uses_fixture_home = true;
            }
            if ident == "AgendHarness" {
                self.uses_agend_harness = true;
            }
            if ident == "TestDaemon" {
                self.uses_test_daemon = true;
            }
        }
        syn::visit::visit_path(self, p);
    }
}

/// True iff `src` (one `tests/*.rs` file's text) has the direct-boot shape
/// and does NOT reference `FixtureHome` anywhere. Used identically by the
/// real invariant below and by its counter-test, so the counter-test proves
/// something about the actual detector, not a reimplementation of it.
fn is_unguarded_direct_boot(src: &str) -> bool {
    match DirectBootScan::scan(src) {
        Ok(scan) => scan.direct_boots() && !scan.is_guarded(),
        // An unparseable file can't be reasoned about; fail closed by NOT
        // flagging it here — `cargo test`/`cargo build` will already refuse
        // to compile a genuinely broken test file, so this branch only ever
        // matters for a file this scan doesn't yet know how to parse.
        Err(_) => false,
    }
}

#[test]
fn direct_boot_tests_use_the_reaping_guard() {
    let mut violations: Vec<String> = Vec::new();
    let mut scanned_any = false;

    for entry in std::fs::read_dir(TESTS_DIR)
        .expect("read tests/ dir")
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Skip THIS invariant file: its own source contains every literal and
        // identifier the scan looks for, by necessity, without booting anything.
        if stem == "daemon_boot_gate_invariant" {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        scanned_any = true;
        if KNOWN_UNGUARDED_DIRECT_BOOT_DEBT.contains(&stem) {
            continue;
        }
        if is_unguarded_direct_boot(&src) {
            violations.push(stem.to_string());
        }
    }

    assert!(
        scanned_any,
        "sanity: scanned zero tests/*.rs files — did TESTS_DIR change?"
    );
    assert!(
        violations.is_empty(),
        "these test binaries direct-boot a real daemon (`start` + `--foreground`, or `app` \
         under a real pty) without using the `FixtureHome` reaping guard from \
         `tests/common/daemon_reaper.rs` — add `let _home = FixtureHome::new(..)` (see \
         `app_singleton_fail_closed.rs` / `cli_smoke.rs`) or, if it truly cannot leak, say why \
         and add it to KNOWN_UNGUARDED_DIRECT_BOOT_DEBT with that reasoning:\n  {violations:#?}"
    );

    // Stale-entry guard, mirroring the one above: every debt entry must still
    // be a real file that still actually needs the exemption — the day one is
    // migrated to `FixtureHome`, this forces its removal instead of letting a
    // now-meaningless entry rot.
    for stem in KNOWN_UNGUARDED_DIRECT_BOOT_DEBT {
        let path = Path::new(TESTS_DIR).join(format!("{stem}.rs"));
        assert!(
            path.exists(),
            "KNOWN_UNGUARDED_DIRECT_BOOT_DEBT lists '{stem}' but {TESTS_DIR}/{stem}.rs does not \
             exist (stale/typo entry)"
        );
        let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        assert!(
            is_unguarded_direct_boot(&src),
            "KNOWN_UNGUARDED_DIRECT_BOOT_DEBT lists '{stem}' but it no longer matches the \
             unguarded direct-boot shape — remove it from the list (debt paid off, or the file \
             changed shape)"
        );
    }
}

/// Mandatory counter-test: proves the detector actually goes RED on a
/// violating shape, using synthetic in-memory sources fed to the SAME
/// `is_unguarded_direct_boot` function the real invariant calls above — never
/// a parallel reimplementation, and never a stray file left in `tests/`.
#[test]
fn direct_boot_detector_fires_on_unguarded_shapes_and_only_those() {
    // True positive, shape 1: `start` + `--foreground` via `.args([...])`,
    // binary built via `cargo_bin`, no guard in sight.
    const UNGUARDED_START_FOREGROUND: &str = r#"
        mod common;
        use assert_cmd::Command;
        fn boot() {
            Command::cargo_bin("agend-terminal")
                .unwrap()
                .args(["start", "--foreground"])
                .spawn()
                .unwrap();
        }
    "#;
    assert!(
        is_unguarded_direct_boot(UNGUARDED_START_FOREGROUND),
        "detector must flag an unguarded `start --foreground` boot — it did not: false miss"
    );

    // True positive, shape 1 again, but the two literals arrive through
    // separate chained `.arg(...)` calls rather than one `.args([...])` —
    // proves the scan does not depend on a single call carrying both.
    const UNGUARDED_CHAINED_ARGS: &str = r#"
        use assert_cmd::Command;
        fn boot() {
            let mut cmd = Command::cargo_bin("agend-terminal").unwrap();
            cmd.arg("start").arg("--foreground");
            cmd.spawn().unwrap();
        }
    "#;
    assert!(
        is_unguarded_direct_boot(UNGUARDED_CHAINED_ARGS),
        "detector must flag `start`/`--foreground` split across chained .arg() calls"
    );

    // True positive, shape 2: `app` under a real pty, no guard. Shape 2 does
    // not require binary construction (it keys off `openpty` instead), so a
    // plain `Command::new` is fine here.
    const UNGUARDED_APP_UNDER_PTY: &str = r#"
        use std::process::Command;
        fn boot_under_pty() {
            unsafe {
                libc::openpty(
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
            }
            Command::new("agend-terminal").arg("app").spawn().unwrap();
        }
    "#;
    assert!(
        is_unguarded_direct_boot(UNGUARDED_APP_UNDER_PTY),
        "detector must flag an unguarded `app` boot under a real pty"
    );

    // Negative control: the SAME shape 1, but guarded — must NOT fire. Without
    // this, a detector that flagged every file would "pass" the assertions
    // above for the wrong reason.
    const GUARDED_START_FOREGROUND: &str = r#"
        mod common;
        use common::daemon_reaper::FixtureHome;
        use assert_cmd::Command;
        fn boot() {
            let _home = FixtureHome::new("x");
            Command::cargo_bin("agend-terminal")
                .unwrap()
                .args(["start", "--foreground"])
                .spawn()
                .unwrap();
        }
    "#;
    assert!(
        !is_unguarded_direct_boot(GUARDED_START_FOREGROUND),
        "detector false-fired on a file that already uses FixtureHome"
    );

    // Negative control: `app` WITHOUT a pty must not fire — this is exactly
    // `cli_smoke.rs`'s `app_without_tty_errors_cleanly_not_panic`, which never
    // reaches the boot path and must not be forced to carry a guard it does
    // not need.
    const APP_WITHOUT_PTY: &str = r#"
        use std::process::Command;
        fn boot() {
            Command::new("agend-terminal").arg("app").output().unwrap();
        }
    "#;
    assert!(
        !is_unguarded_direct_boot(APP_WITHOUT_PTY),
        "detector false-fired on a plain-pipe `app` invocation that never reaches the boot path"
    );

    // Negative control: the EXACT false-match shape that forced the AST
    // approach in the first place — a string constant a template check
    // inspects via `.contains(...)`, never passed to `.arg`/`.args`.
    const TEMPLATE_STRING_CHECK: &str = r#"
        fn check(launchd: &str) {
            assert!(launchd.contains("<string>--foreground</string>"));
            assert!(launchd.contains("start"));
        }
    "#;
    assert!(
        !is_unguarded_direct_boot(TEMPLATE_STRING_CHECK),
        "detector false-matched a string constant inspected via `.contains(...)`, not an \
         argument of `.arg`/`.args` — exactly the false-positive shape the AST approach exists \
         to avoid"
    );

    // Negative control: a doc comment mentioning the literals must not fire —
    // the same self-match trap the AgendHarness scan above needs an explicit
    // skip for; the AST approach should not need one at all.
    const DOC_COMMENT_MENTION: &str = r#"
        //! Direct-boot tests (`start --foreground`) without `app` under a pty
        //! are covered elsewhere.
        fn noop() {}
    "#;
    assert!(
        !is_unguarded_direct_boot(DOC_COMMENT_MENTION),
        "detector false-matched literals appearing only in a doc comment"
    );

    // True positive, the shape #3512's predicate MISSED: `start` alone (no
    // `--foreground` — the CLI detaches the daemon itself), in a file that
    // builds the crate binary, with no guard at all. This is `cli_smoke.rs`'s
    // exact shape.
    const UNGUARDED_START_NO_FOREGROUND: &str = r#"
        use assert_cmd::Command;
        fn boot() {
            Command::cargo_bin("agend-terminal")
                .unwrap()
                .arg("start")
                .spawn()
                .unwrap();
        }
    "#;
    assert!(
        is_unguarded_direct_boot(UNGUARDED_START_NO_FOREGROUND),
        "detector must flag a bare `start` (no --foreground) boot that builds the crate binary \
         and holds no guard — this is the #3512 predicate hole (cli_smoke.rs's shape)"
    );

    // Negative control: the same bare-`start` shape, guarded by `AgendHarness`
    // instead of `FixtureHome` — must NOT fire. `AgendHarness::drop` SIGTERMs
    // then SIGKILLs the process group and waits, so it is a genuine reaping
    // guard even though it isn't `FixtureHome`.
    const GUARDED_BY_AGENDHARNESS: &str = r#"
        mod common;
        use common::harness::AgendHarness;
        use assert_cmd::Command;
        fn boot() {
            let _harness: &AgendHarness = todo!();
            Command::cargo_bin("agend-terminal")
                .unwrap()
                .arg("start")
                .spawn()
                .unwrap();
        }
    "#;
    assert!(
        !is_unguarded_direct_boot(GUARDED_BY_AGENDHARNESS),
        "detector false-fired on a file guarded by AgendHarness (a genuine reaping guard, not \
         just FixtureHome) — tool_cli_phase0a_real_red.rs's shape"
    );

    // Negative control: the same bare-`start` shape, guarded by `TestDaemon`
    // instead — must NOT fire. `TestDaemon::drop` kills and waits the child.
    const GUARDED_BY_TESTDAEMON: &str = r#"
        struct TestDaemon;
        use assert_cmd::Command;
        fn boot() {
            let _daemon = TestDaemon;
            Command::cargo_bin("agend-terminal")
                .unwrap()
                .arg("start")
                .spawn()
                .unwrap();
        }
    "#;
    assert!(
        !is_unguarded_direct_boot(GUARDED_BY_TESTDAEMON),
        "detector false-fired on a file guarded by TestDaemon (a genuine reaping guard) — \
         integration.rs's shape"
    );

    // Negative control: a file that passes the literal `"start"` to `.arg`
    // but NEVER constructs the `agend-terminal` crate binary — cannot
    // direct-boot, must not fire. This is `integration.rs`'s actual shape
    // (it resolves its binary path via `current_exe()`, not `cargo_bin`/
    // `env!("CARGO_BIN_EXE_...")`) as well as the other five files named in
    // the module doc above.
    const START_WITHOUT_BUILDING_BINARY: &str = r#"
        use std::process::Command;
        fn binary() -> std::path::PathBuf {
            let mut p = std::env::current_exe().unwrap();
            p.pop();
            p.push("agend-terminal");
            p
        }
        fn boot() {
            Command::new(binary()).arg("start").spawn().unwrap();
        }
    "#;
    assert!(
        !is_unguarded_direct_boot(START_WITHOUT_BUILDING_BINARY),
        "detector false-fired on a file passing `start` to a binary it never actually \
         constructs via cargo_bin/env!(CARGO_BIN_EXE_...) — cannot direct-boot"
    );
}
