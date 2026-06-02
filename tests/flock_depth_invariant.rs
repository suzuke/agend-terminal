//! #1629 invariant: `store::acquire_file_lock` (src/store.rs) must be the SOLE
//! flock chokepoint that bumps `FLOCK_DEPTH` — the thread-local the self-IPC
//! deadlock guard (`assert_no_registry_lock_for_self_ipc`) reads to see the
//! fs4-flock tier. Any RAW `fs4::FileExt::lock` / `fs4::FileExt::try_lock`
//! OUTSIDE the three vetted files bypasses FLOCK_DEPTH and silently reopens the
//! flock-while-blocking deadlock blind spot (#1617/#1342/#1340/#1624) this guard
//! closes. This RED fails CI if a new raw flock site appears.
//!
//! The three allowed files:
//! - `store.rs` — `acquire_file_lock` itself (the chokepoint) + its tests.
//! - `bootstrap/mod.rs` — `acquire_daemon_lock`, the `.daemon.lock` singleton
//!   held for the daemon's whole life (MUST NOT bump the depth, else it would
//!   pin FLOCK_DEPTH > 0 and false-trip every self-IPC).
//! - `daemon/mod.rs` — the escape-hatch `run` `.daemon.lock` singleton.

use std::path::{Path, PathBuf};

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

    // The 3 vetted files allowed to contain a raw fs4 flock call. store.rs is the
    // chokepoint; the other two are the daemon-singleton `.daemon.lock` sites
    // that deliberately bypass FLOCK_DEPTH (held for the daemon's whole life).
    let allowed = ["store.rs", "bootstrap/mod.rs", "daemon/mod.rs"];

    let needles = ["fs4::FileExt::lock", "fs4::FileExt::try_lock"];
    let mut violations = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&src)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        if allowed.contains(&rel.as_str()) {
            continue;
        }
        let text = std::fs::read_to_string(file).expect("read src file");
        for (i, line) in text.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with('*') {
                continue; // skip comment/doc lines that merely mention the pattern
            }
            for needle in &needles {
                if line.contains(needle) {
                    violations.push(format!("{}:{}: {}", rel, i + 1, line.trim()));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "#1629: raw `fs4::FileExt::{{lock,try_lock}}` outside the 3 vetted files bypasses \
         store::acquire_file_lock's FLOCK_DEPTH tracking → reopens the flock-while-blocking \
         self-IPC deadlock blind spot (#1617 class). Route the lock through \
         `crate::store::acquire_file_lock` instead; or — if it is a deliberate lifetime-held \
         singleton like `.daemon.lock` — add its file to the allowlist with rationale:\n{}",
        violations.join("\n")
    );
}
