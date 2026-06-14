//! state-capture finding #3 (design / misleading comment): the rationale for
//! omitting a dedup latch on `capture_unclassified_throttle` is the in-code
//! comment
//!
//!   "The feed-level screen hash-dedup bounds this to once per screen."
//!
//! That invariant is FALSE: (1) `apply_hash_dedup_gate` deliberately does NOT
//! skip identical frames when a throttle hint is present and the tracker is not
//! throttle-latched (the exact scenario the instrument fires in), and (2) any
//! spinner/clock-tick redraw produces a different screen hash, so the same
//! logical "unclassified throttle" screen is logged repeatedly even on the
//! normal path. The comment gives a false sense the growth is bounded and is the
//! root cause of the resource-leak finding (#1) — no latch was added because the
//! comment claimed one was unnecessary.
//!
//! This is a SOURCE-SCANNING invariant (mirrors tests/core_mutex_invariant.rs):
//! RED now because the misleading claim is present; GREEN once the comment is
//! corrected (and the per-signature latch the comment wrongly deemed
//! unnecessary is added). It deliberately matches the load-bearing FALSE clause
//! "bounds this to once per screen" rather than the whole sentence, so a
//! corrected comment that still mentions the hash-dedup (e.g. "the hash-dedup is
//! BYPASSED for throttle-hint frames, so this is NOT bounded to once per
//! screen") only passes if it drops the false claim.

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
#[ignore = "state-capture #3: red until fix; remove #[ignore] after fix to confirm"]
fn unclassified_throttle_dedup_comment_is_not_misleading() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    assert!(!files.is_empty(), "no src/*.rs files found");

    // The load-bearing FALSE clause. Present on src/state/mod.rs's
    // `capture_unclassified_throttle` rationale today.
    const FALSE_CLAIM: &str = "bounds this to once per screen";

    let mut violations = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read src file");
        for (i, line) in text.lines().enumerate() {
            if line.contains(FALSE_CLAIM) {
                violations.push(format!("{}:{}: {}", file.display(), i + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "#3 design: the comment claiming the feed-level hash-dedup \"{FALSE_CLAIM}\" is \
         FALSE — the throttle-hint bypass in apply_hash_dedup_gate AND spinner-driven \
         hash churn both defeat it, so the unclassified-throttle sidelog is NOT bounded \
         to once per screen (see resource-leak finding #1). Correct the comment and add \
         the per-signature fire-once latch it wrongly deemed unnecessary:\n{}",
        violations.join("\n")
    );
}
