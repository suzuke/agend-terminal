//! Sprint 57 Wave 3 PR-3 (#548 Phase 3) — service helper integration tests.
//!
//! Tier-2 dual review surface. Tests cover:
//!   - user-level no-admin guarantee (template content checks)
//!   - asset template forward-compat (placeholder presence pins)
//!   - escaping-wiring source pins (xml_escape / systemd_quote call sites)
//!
//! NOTE: the real per-platform install/uninstall FILE round-trip lives in
//! the bin crate (`src/service/*`) and is not reachable from this
//! integration crate — see the in-file note where the former stub-based
//! `macos_tests`/`linux_tests` were removed.
//!
//! Per-platform tests are gated by `#[cfg(target_os = "...")]` so
//! each CI runner exercises its own platform path. Cross-platform
//! tests run on all three.

#![allow(clippy::unwrap_used, clippy::expect_used)]

// ---------------------------------------------------------------------
// Cross-platform: assets + template presence pins
// ---------------------------------------------------------------------

#[test]
fn assets_service_templates_exist() {
    let launchd = include_str!("../assets/service/launchd.plist.template");
    assert!(launchd.contains("<plist"));
    assert!(launchd.contains("__EXECUTABLE__"));
    assert!(launchd.contains("__HOME__"));
    // #914: `__LOG__` placeholder removed — StandardOutPath /
    // StandardErrorPath now hard-coded to /dev/null since the daemon's
    // tracing-appender owns rotated log output. Pin the new shape so a
    // future refactor that re-adds an OS-stdio capture has to explicitly
    // update this assertion.
    assert!(launchd.contains("/dev/null"));
    assert!(!launchd.contains("__LOG__"));
    assert!(launchd.contains("__LABEL__"));

    let systemd = include_str!("../assets/service/systemd.service.template");
    assert!(systemd.contains("[Service]"));
    assert!(systemd.contains("__EXECUTABLE__"));
    assert!(systemd.contains("__HOME__"));

    let windows = include_str!("../assets/service/scheduler.task.xml.template");
    assert!(windows.contains("<Task"));
    assert!(windows.contains("__EXECUTABLE__"));
    assert!(windows.contains("__HOME__"));
    assert!(windows.contains("__USER__"));
}

#[test]
fn launchd_template_user_level_no_admin_required() {
    // launchd LaunchAgents at `~/Library/LaunchAgents/` are
    // user-level by definition. The plist's lifecycle keys must
    // match user-session semantics.
    let launchd = include_str!("../assets/service/launchd.plist.template");
    // KeepAlive + RunAtLoad = login-triggered + auto-restart on crash.
    assert!(launchd.contains("<key>KeepAlive</key>"));
    assert!(launchd.contains("<key>RunAtLoad</key>"));
    // ProcessType=Background = system can throttle; appropriate for
    // a user-level helper service.
    assert!(launchd.contains("<key>ProcessType</key>"));
    assert!(launchd.contains("<string>Background</string>"));
}

#[test]
fn systemd_template_user_level_no_admin_required() {
    let systemd = include_str!("../assets/service/systemd.service.template");
    // WantedBy=default.target = user session target (NOT
    // multi-user.target which would require root).
    assert!(systemd.contains("WantedBy=default.target"));
    // Type=simple + Restart=on-failure = standard user service shape.
    assert!(systemd.contains("Type=simple"));
    assert!(systemd.contains("Restart=on-failure"));
    // KillSignal=SIGTERM ties into Wave 3 PR-2's staged termination
    // (the daemon's own SHUTDOWN_GRACE handles the 2s wait).
    assert!(systemd.contains("KillSignal=SIGTERM"));
}

#[test]
fn windows_template_user_level_no_admin_required() {
    let windows = include_str!("../assets/service/scheduler.task.xml.template");
    // RunLevel=LeastPrivilege explicitly avoids admin escalation.
    assert!(windows.contains("<RunLevel>LeastPrivilege</RunLevel>"));
    // LogonTrigger = at-user-logon (not at-system-startup which
    // would need admin). UserId in trigger scopes to current user.
    assert!(windows.contains("<LogonTrigger>"));
    // RestartOnFailure with Count=3 = parity with systemd
    // Restart=on-failure semantic.
    assert!(windows.contains("<RestartOnFailure>"));
}

// ---------------------------------------------------------------------
// NOTE on per-platform install round-trip coverage:
//
// The real file-write + service-manager registration lives in the BIN
// crate (`src/service/{macos,linux,windows}.rs`, `pub(super) fn install`)
// and is NOT reachable from this integration-test crate (no lib re-export
// of `service`). The former `macos_tests` / `linux_tests` here only called
// a local `agend_terminal_service_install_for_test` STUB that returned
// `Ok(())` without writing anything, yet asserted `plist.exists()` — a
// side effect the stub could never produce (the macOS test would fail if
// run; the Linux test guarded its asserts behind `if unit.exists()` so it
// passed vacuously). Both gave false confidence and have been removed.
//
// Template rendering / substitution / no-residual-placeholder behavior IS
// covered in-crate (`src/service/mod.rs` unit tests). A genuine install
// round-trip belongs in-crate too, but `install()` also invokes
// `launchctl`/`systemctl` (no sandbox session in CI), so it first needs
// the plist/unit file-write extracted from the service-manager load step.
// Tracked as follow-up; not faked here.
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// Cross-platform: substitution + template-shape pins
// ---------------------------------------------------------------------

#[test]
fn template_substitution_pin_each_placeholder() {
    // Walk the three templates and assert every documented
    // placeholder appears at least once. A future template edit
    // that drops a placeholder without updating the per-platform
    // `apply_substitutions` call site would silently leave the
    // literal `__FOO__` in the rendered file — operator-visible
    // breakage. This pin catches it.
    let launchd = include_str!("../assets/service/launchd.plist.template");
    // #914: `__LOG__` removed; StandardOutPath/StandardErrorPath hard-coded
    // to /dev/null because the daemon's tracing-appender owns rotated
    // log output. The remaining 3 placeholders still flow through
    // `apply_substitutions` at install time.
    for placeholder in &["__LABEL__", "__EXECUTABLE__", "__HOME__"] {
        assert!(
            launchd.contains(placeholder),
            "launchd template missing placeholder: {placeholder}"
        );
    }
    assert!(
        !launchd.contains("__LOG__"),
        "launchd template MUST NOT carry __LOG__ post-#914; \
         StandardOutPath/StandardErrorPath route to /dev/null."
    );
    let systemd = include_str!("../assets/service/systemd.service.template");
    for placeholder in &["__EXECUTABLE__", "__HOME__"] {
        assert!(
            systemd.contains(placeholder),
            "systemd template missing placeholder: {placeholder}"
        );
    }
    let windows = include_str!("../assets/service/scheduler.task.xml.template");
    for placeholder in &["__EXECUTABLE__", "__HOME__", "__USER__"] {
        assert!(
            windows.contains(placeholder),
            "windows template missing placeholder: {placeholder}"
        );
    }
}

#[test]
fn templates_invoke_start_foreground_so_service_owns_lifecycle() {
    // Pin: every platform template invokes `start --foreground` so
    // the service manager (launchd / systemd / Task Scheduler) owns
    // the daemon process directly rather than the daemon detaching
    // from its supervisor (which would defeat auto-restart).
    let launchd = include_str!("../assets/service/launchd.plist.template");
    assert!(
        launchd.contains("<string>start</string>")
            && launchd.contains("<string>--foreground</string>"),
        "launchd template must invoke `start --foreground`"
    );
    let systemd = include_str!("../assets/service/systemd.service.template");
    assert!(
        systemd.contains("ExecStart=") && systemd.contains("start --foreground"),
        "systemd template must invoke `start --foreground`"
    );
    let windows = include_str!("../assets/service/scheduler.task.xml.template");
    assert!(
        windows.contains("<Arguments>start --foreground</Arguments>"),
        "windows template must invoke `start --foreground`"
    );
}

// ---------------------------------------------------------------------
// Sprint 57 Wave 3 PR-3 r2 (#548 Phase 3, Tier-2 Pass 2 fixup) —
// format-aware escaping integration pins. Reach into the bin's
// service module via the test crate's `agend_terminal::service`
// re-export... wait, the bin doesn't re-export. Pin via source-text
// invariants on the production-side escaping wiring instead.
// ---------------------------------------------------------------------

#[test]
fn macos_install_path_invokes_xml_escape() {
    // Source-text pin: the macOS install function MUST call
    // xml_escape on each substituted value. Pre-r2 it fed raw paths
    // straight into apply_substitutions, producing malformed plist
    // for paths with `&`/`<`/`>`/`"`/`'`.
    let macos_rs = include_str!("../src/service/macos.rs");
    assert!(
        macos_rs.contains("xml_escape("),
        "src/service/macos.rs MUST call xml_escape on substituted values \
         (Tier-2 Pass 2 fixup — Class-A cross-platform escaping bug)"
    );
}

#[test]
fn linux_install_path_invokes_systemd_quote() {
    // Source-text pin: the Linux install function MUST call
    // systemd_quote on the executable + home paths. Pre-r2 it fed
    // raw paths into ExecStart= which would mis-tokenize at any
    // whitespace.
    let linux_rs = include_str!("../src/service/linux.rs");
    assert!(
        linux_rs.contains("systemd_quote("),
        "src/service/linux.rs MUST call systemd_quote on substituted values \
         (Tier-2 Pass 2 fixup — ExecStart= tokenization bug)"
    );
}

#[test]
fn windows_install_path_invokes_xml_escape() {
    let windows_rs = include_str!("../src/service/windows.rs");
    assert!(
        windows_rs.contains("xml_escape("),
        "src/service/windows.rs MUST call xml_escape on substituted values \
         (Tier-2 Pass 2 fixup — Task Scheduler XML escaping bug)"
    );
}

#[test]
fn service_mod_exports_both_escape_helpers() {
    // Cross-cutting: both helpers must be in service::mod.rs
    // as the single source of truth. Per-platform modules import
    // from there.
    let mod_rs = include_str!("../src/service/mod.rs");
    assert!(
        mod_rs.contains("pub fn xml_escape("),
        "service::mod.rs MUST define `xml_escape` helper"
    );
    assert!(
        mod_rs.contains("pub fn systemd_quote("),
        "service::mod.rs MUST define `systemd_quote` helper"
    );
}

#[test]
fn cargo_toml_includes_service_assets() {
    // Pin: the published crate ships the templates so `cargo publish`
    // verify builds + downstream consumers get them. Mirrors the
    // Sprint 53 assets/hooks lesson.
    let cargo_toml = include_str!("../Cargo.toml");
    assert!(
        cargo_toml.contains("assets/service/*"),
        "Cargo.toml include list must contain `assets/service/*` so the \
         templates ship in the published crate (mirrors assets/hooks/* lesson)"
    );
}
