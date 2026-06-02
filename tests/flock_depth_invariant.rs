//! #1629 invariant: `store::acquire_file_lock` (src/store.rs) must be the SOLE
//! flock chokepoint that bumps `FLOCK_DEPTH` — the thread-local the self-IPC
//! deadlock guard (`assert_no_registry_lock_for_self_ipc`) reads to see the
//! fs4-flock tier. Any RAW fs4 flock OUTSIDE the three vetted files bypasses
//! FLOCK_DEPTH and silently reopens the flock-while-blocking deadlock blind spot
//! (#1617/#1342/#1340/#1624) this guard closes. This RED fails CI if a new raw
//! flock site appears.
//!
//! A raw flock has TWO call forms, BOTH caught:
//!  1. fully-qualified UFCS — `fs4::FileExt::try_lock(&file)` (works with no
//!     import), matched by the `FileExt::<method>(` needle; and
//!  2. imported-trait method — `use fs4::FileExt; file.try_lock()` (codex's
//!     #1635 review probe), matched by the `.<method>(` needle BUT only in files
//!     whose `use` statements import the fs4 `FileExt` trait. That gate is the
//!     disambiguation from `Mutex::lock()`/`RwLock::lock()`: `.lock(`/`.try_lock(`
//!     are ambiguous, so they are only flagged where the fs4 trait is in scope —
//!     `Mutex::lock()` in a non-fs4 file never false-FAILs. (`src/inbox/disk.rs`
//!     imports `fs4::available_space` but NOT `FileExt`, so it is NOT gated on.)
//!
//! The FileExt-in-scope detection parses whole `use` STATEMENTS (each may span
//! several lines until its `;`), so rustfmt's normal multi-line import block —
//!   `use fs4::{`/`    available_space,`/`    FileExt,`/`};` — is handled the
//! same as the single-line form.
//!
//! DOCUMENTED LIMIT (by design — this is an ACCIDENTAL-reintroduction guard, not
//! a security boundary; same philosophy as #1631's documented assign-then-drop
//! limit): the gate keys on the literal token `FileExt` appearing in an `fs4`
//! `use` statement. Deliberate-evasion forms that HIDE that token are out of
//! scope and we do NOT chase them: a glob import `use fs4::*;` (no `FileExt`
//! token), or aliasing the trait to a different name and calling it via that
//! alias's UFCS (`use fs4::FileExt as Z; Z::try_lock(`). A dev accidentally
//! re-adding a raw flock writes an explicit `use fs4::…FileExt…` (single- or
//! multi-line), which IS caught.
//!
//! The three allowed files:
//! - `store.rs` — `acquire_file_lock` itself (the chokepoint) + its tests.
//! - `bootstrap/mod.rs` — `acquire_daemon_lock`, the `.daemon.lock` singleton
//!   held for the daemon's whole life (MUST NOT bump the depth, else it would
//!   pin FLOCK_DEPTH > 0 and false-trip every self-IPC).
//! - `daemon/mod.rs` — the escape-hatch `run` `.daemon.lock` singleton.

use std::path::{Path, PathBuf};

const ALLOWED: [&str; 3] = ["store.rs", "bootstrap/mod.rs", "daemon/mod.rs"];

/// fs4 `FileExt` lock/unlock method names. `lock` / `try_lock` are the exclusive
/// aliases the codebase uses; the `_shared` / `_exclusive` variants are included
/// for forward-coverage.
const FLOCK_METHODS: [&str; 6] = [
    "lock",
    "try_lock",
    "lock_shared",
    "try_lock_shared",
    "lock_exclusive",
    "try_lock_exclusive",
];

/// Does this file import the fs4 `FileExt` trait (so `file.try_lock()` method
/// syntax resolves to an fs4 flock)? Parses whole `use` STATEMENTS — each runs
/// from a `use`-prefixed line until the line carrying its `;` — so a multi-line
/// `use fs4::{ … FileExt … };` block is detected as well as the single-line
/// form. Comment lines are skipped so a commented mention does not flip the gate.
fn imports_fs4_fileext(text: &str) -> bool {
    let mut stmt = String::new();
    let mut in_use = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        if !in_use {
            if t.starts_with("use ") {
                in_use = true;
                stmt.clear();
                stmt.push_str(t);
            } else {
                continue;
            }
        } else {
            stmt.push(' ');
            stmt.push_str(t);
        }
        if stmt.contains(';') {
            if stmt.contains("fs4") && stmt.contains("FileExt") {
                return true;
            }
            in_use = false;
            stmt.clear();
        }
    }
    false
}

/// Return the raw-flock violations in `text` (empty for allowlisted files or
/// clean files). Pure + path-string-driven so the meta-test can probe both
/// directions without touching the filesystem.
fn scan_file(rel: &str, text: &str) -> Vec<String> {
    if ALLOWED.contains(&rel) {
        return Vec::new();
    }
    let gated = imports_fs4_fileext(text);
    let mut violations = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            continue; // skip comment/doc lines that merely mention the pattern
        }
        for m in &FLOCK_METHODS {
            // Form 1: fully-qualified UFCS, e.g. `fs4::FileExt::try_lock(` or
            // `fs4::fs_std::FileExt::try_lock(`. `FileExt::` is fs4-specific
            // (std's unix FileExt has no lock methods), so no import is needed.
            if line.contains(&format!("FileExt::{m}(")) {
                violations.push(format!("{}:{}: {}", rel, i + 1, line.trim()));
            }
            // Form 2: imported-trait method call, e.g. `file.try_lock(` — only
            // an fs4 flock when the file imports the FileExt trait (else it is
            // Mutex/RwLock and must NOT false-FAIL).
            if gated && line.contains(&format!(".{m}(")) {
                violations.push(format!("{}:{}: {}", rel, i + 1, line.trim()));
            }
        }
    }
    violations
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir src") {
        let p = entry.expect("dir entry").path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

#[test]
fn acquire_file_lock_is_sole_flock_depth_bumper_1629() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    assert!(!files.is_empty(), "no src/*.rs files found");

    let mut violations = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&src)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(file).expect("read src file");
        violations.extend(scan_file(&rel, &text));
    }

    assert!(
        violations.is_empty(),
        "#1629: a raw fs4 flock outside the 3 vetted files bypasses \
         store::acquire_file_lock's FLOCK_DEPTH tracking → reopens the flock-while-blocking \
         self-IPC deadlock blind spot (#1617 class). Route the lock through \
         `crate::store::acquire_file_lock` instead; or — if it is a deliberate lifetime-held \
         singleton like `.daemon.lock` — add its file to the allowlist with rationale:\n{}",
        violations.join("\n")
    );
}

/// Meta-test: prove the scanner catches all the realistic raw-flock forms and
/// does NOT false-FAIL on `Mutex::lock()`. Guards against the #1635
/// too-narrow-detection regressions (rounds 1–3): the original matched only the
/// fully-qualified string; round 2 added the imported-trait method; round 3
/// (this) handles the multi-line import block.
#[test]
fn scanner_catches_all_realistic_forms_and_spares_mutex() {
    // codex r2 probe: single-line imported-trait method call.
    let single = "use fs4::FileExt;\nfn f(file: std::fs::File) { let _ = file.try_lock(); }";
    assert!(
        !scan_file("some/new_probe.rs", single).is_empty(),
        "must catch single-line `use fs4::FileExt; file.try_lock()` (#1635 r2)"
    );

    // codex r3 probe: rustfmt's multi-line import block + method call.
    let multiline = "use fs4::{\n    available_space,\n    FileExt,\n};\nfn f(file: std::fs::File) { let _ = file.try_lock(); }";
    assert!(
        !scan_file("some/new_probe.rs", multiline).is_empty(),
        "must catch the MULTI-LINE `use fs4::{{ …FileExt… }}; file.try_lock()` (#1635 r3)"
    );

    // Fully-qualified UFCS form (no import needed).
    let ufcs = "fn f(file: std::fs::File) { let _ = fs4::FileExt::try_lock(&file); }";
    assert!(
        !scan_file("some/new_probe.rs", ufcs).is_empty(),
        "must catch the fully-qualified `fs4::FileExt::try_lock(` form"
    );

    // Legit Mutex::lock() in a non-fs4 file → must NOT false-FAIL.
    let mutex = "use parking_lot::Mutex;\nfn f(m: &Mutex<u8>) { let _g = m.lock(); }";
    assert!(
        scan_file("some/other.rs", mutex).is_empty(),
        "must NOT flag Mutex::lock() in a non-fs4 file"
    );

    // fs4 disk-space import WITHOUT FileExt → not gated; a `.lock()` here is
    // necessarily non-fs4 and must NOT false-FAIL (mirrors src/inbox/disk.rs),
    // including the multi-line grouped form that omits FileExt.
    let disk = "use fs4::{\n    available_space,\n    total_space,\n};\nfn f(m: &std::sync::Mutex<u8>) { let _g = m.lock(); }";
    assert!(
        scan_file("inbox/disk.rs", disk).is_empty(),
        "fs4 import without FileExt must not gate the method check on"
    );

    // The 3 vetted files are exempt even with raw fs4 flock usage.
    assert!(
        scan_file("store.rs", single).is_empty(),
        "allowlisted files are exempt"
    );
}
