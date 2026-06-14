//! Repro guard for: "macOS service plist written non-atomically before
//! launchctl load" (src/service/macos.rs `install`, and the parallel
//! linux.rs / windows.rs paths).
//!
//! `install()` writes the rendered plist with `std::fs::write` (truncate-then-
//! write, non-atomic) and immediately `launchctl load -w`s it. The linux and
//! windows paths use the same non-atomic `std::fs::write`. If the process is
//! interrupted mid-write (crash, signal, disk-full) the on-disk plist / unit /
//! task XML is left truncated/partial; a later `status` (which keys off file
//! presence) reports installed, and a subsequent launchctl/systemctl/schtasks
//! load reads a malformed file. The repo already provides
//! `crate::store::atomic_write` (temp + fsync + rename) for exactly this
//! "readers expect a complete document on disk at all times" contract.
//!
//! METHOD: static_invariant (source-scan). The service modules live in the bin
//! crate and have no `lib` re-export reachable from an integration test (see the
//! note in tests/issue_548_phase3_service.rs), so behavior is pinned at the
//! source level — mirroring the existing `macos_install_path_invokes_xml_escape`
//! style pins in that file.
//!
//! RED now: each service module writes its lifecycle file with `std::fs::write`
//! and never calls `atomic_write`. GREEN after fix: the service-install lifecycle
//! files are written via `crate::store::atomic_write` so an interrupted install
//! never leaves a half-written file the service manager (or `status`) then trusts.

fn assert_atomic_install(module: &str, src: &str) {
    // The fixed module must route its lifecycle-file write through atomic_write.
    assert!(
        src.contains("atomic_write"),
        "src/service/{module}.rs must write its service lifecycle file via \
         crate::store::atomic_write (temp + fsync + rename) so an interrupted \
         install never leaves a truncated/partial plist|unit|task XML that \
         `status` reports as installed and the service manager then reads."
    );

    // And it must NOT still write that file with the non-atomic `std::fs::write`
    // (ignoring comment/doc lines that merely mention the old pattern).
    let mut offending = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        if line.contains("std::fs::write(") {
            offending.push(format!("{module}.rs:{}: {}", i + 1, line.trim()));
        }
    }
    assert!(
        offending.is_empty(),
        "src/service/{module}.rs still writes a service file with non-atomic \
         std::fs::write — replace with crate::store::atomic_write:\n{}",
        offending.join("\n")
    );
}

#[test]
#[ignore = "service-plist-nonatomic: red until fix; remove #[ignore] after fix to confirm"]
fn macos_service_install_writes_plist_atomically_bootstrap_config_cli() {
    assert_atomic_install("macos", include_str!("../src/service/macos.rs"));
}

#[test]
#[ignore = "service-unit-nonatomic: red until fix; remove #[ignore] after fix to confirm"]
fn linux_service_install_writes_unit_atomically_bootstrap_config_cli() {
    assert_atomic_install("linux", include_str!("../src/service/linux.rs"));
}

#[test]
#[ignore = "service-taskxml-nonatomic: red until fix; remove #[ignore] after fix to confirm"]
fn windows_service_install_writes_taskxml_atomically_bootstrap_config_cli() {
    assert_atomic_install("windows", include_str!("../src/service/windows.rs"));
}
