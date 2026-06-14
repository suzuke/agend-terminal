//! Verification/reproduction (static invariant) for the `inbox-notify` batch,
//! Finding #2 (MED): `find_message` aborts the WHOLE cross-inbox scan on the
//! first unreadable file because it uses `std::fs::read_to_string(&path).ok()?`
//! inside the per-file loop — the `?` propagates `None` out of the entire
//! function, so a message living in a LATER inbox file is never found (a
//! swallowed-error correctness bug behind reply-routing / attachment lookups).
//!
//! Method: a behavioral repro is order-nondeterministic — `read_dir` ordering is
//! unspecified, so an unreadable/dangling file may sort AFTER the target and the
//! abort would not fire. We therefore assert the BAD pattern is gone from the
//! source (deterministic). RED now (the `.ok()?` per-file read is present),
//! GREEN once it becomes `let Ok(content) = std::fs::read_to_string(&path) else
//! { continue; };` (skip-and-continue, matching get_thread/collect_thread).

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
#[ignore = "find_message-abort-on-unreadable: red until fix; remove #[ignore] after fix to confirm"]
fn find_message_does_not_abort_scan_on_unreadable_file_inbox_notify() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    assert!(!files.is_empty(), "no src/*.rs files found");

    // The forbidden per-file read: `.ok()?` aborts the whole find_message scan on
    // one bad file. The legitimate `read_dir(&inbox_dir).ok()?` (dir-level) does
    // NOT match this needle, so it is not a false positive. The fix replaces it
    // with a `let Ok(content) = ... else { continue; };`.
    let needle = "read_to_string(&path).ok()?";

    let mut violations = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read src file");
        for (i, line) in text.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with('*') {
                continue; // skip comment/doc lines that merely mention the pattern
            }
            if line.contains(needle) {
                violations.push(format!("{}:{}: {}", file.display(), i + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "find_message must not abort the cross-inbox scan on one unreadable file. \
         `read_to_string(&path).ok()?` propagates None out of the WHOLE function, \
         masking a match in a later inbox. Use \
         `let Ok(content) = std::fs::read_to_string(&path) else {{ continue; }};` \
         (mirror get_thread/collect_thread_messages):\n{}",
        violations.join("\n")
    );
}
