//! #1527 invariant: `StateTracker::current` must be ASSIGNED through exactly
//! one funnel (`record_set`), so every state change is recorded for
//! `state-transitions.jsonl`. The root cause #1527 fixes was transitions
//! mutating `current` on paths the supervisor's prev/new-at-tick comparison
//! couldn't see (async feed thread + the `set_restarting`/`set_awaiting_operator`
//! direct setters). Routing all mutation through `record_set` is what makes the
//! log complete — so a future direct `self.current = …` would silently
//! reintroduce the missing-transition bug. This RED fails CI if that happens.
//!
//! (Comparisons `== self.current` / `self.current ==` are NOT assignments and
//! are excluded; only `self.current = <rhs>` assignment is counted.)

#[test]
fn state_current_assigned_only_via_record_set_1527() {
    let src = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/state/mod.rs"),
    )
    .expect("read src/state/mod.rs");

    let mut assignments = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            continue; // skip comments/docs that mention the pattern
        }
        // Match `self.current =` assignment, but NOT `self.current ==`
        // (comparison) and NOT `== self.current`.
        if let Some(rest) = line.split("self.current").nth(1) {
            let rest = rest.trim_start();
            if let Some(after_eq) = rest.strip_prefix('=') {
                if !after_eq.starts_with('=') {
                    assignments.push(format!("{}: {}", i + 1, line.trim()));
                }
            }
        }
    }

    assert_eq!(
        assignments.len(),
        1,
        "#1527: `self.current` must be assigned in exactly ONE place (record_set) so every \
         transition is recorded; a new direct assignment reintroduces the missing-transition \
         bug. Found {} assignment site(s):\n{}",
        assignments.len(),
        assignments.join("\n")
    );
}
