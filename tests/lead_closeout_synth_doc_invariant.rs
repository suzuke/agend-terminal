//! Sprint 59 Wave 1 PR-3 (#11 lead closeout synth claim-state
//! discipline) — source-text invariant pins for the process doc.
//!
//! The doc captures the Sprint 58 Wave 3 PR-1 incident + cross-
//! check procedure that prevents recurrence. Its value depends on
//! continued presence + accurate cross-references. These tests
//! pin the structural contract so a future edit can't silently
//! drop a section.

use std::path::PathBuf;

fn doc_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("PROCESS-LEAD-CLOSEOUT-CLAIM-STATE-DISCIPLINE.md")
}

fn doc_content() -> String {
    std::fs::read_to_string(doc_path()).expect("doc file must be readable")
}

// ── Lead-spec named tests (per dispatch m-20260509092805759101-133) ──

#[test]
fn lead_closeout_synth_doc_exists() {
    assert!(
        doc_path().exists(),
        "docs/PROCESS-LEAD-CLOSEOUT-CLAIM-STATE-DISCIPLINE.md must exist"
    );
}

#[test]
fn lead_closeout_synth_doc_references_sprint_58_wave_3_pr_1_incident() {
    let content = doc_content();
    assert!(
        content.contains("Sprint 58 Wave 3 PR-1"),
        "doc must reference the Sprint 58 Wave 3 PR-1 incident as the \
         motivating example — without it, the cross-check checklist \
         is unanchored"
    );
    // The doc should also cite the specific failure mode (not just
    // the incident name).
    assert!(
        content.contains("100") || content.contains("idle-wait") || content.contains("idle-poll"),
        "doc must cite the specific failure mode (idle-wait duration / pattern)"
    );
}

#[test]
fn lead_closeout_synth_doc_includes_explicit_cross_check_checklist() {
    let content = doc_content();
    // The cross-check checklist (§3) is the load-bearing surface —
    // pin its presence + the specific verification commands.
    assert!(
        content.contains("Cross-check"),
        "doc must include a cross-check section heading"
    );
    // The checklist should reference the concrete commands.
    let required_checks = [
        "task action=list", // task board state
        "gh pr list",       // git/PR state
        "broadcast",        // dev's broadcast trail
    ];
    for check in &required_checks {
        assert!(
            content.contains(check),
            "cross-check checklist must reference `{check}` verification \
             surface (without it, the checklist is incomplete)"
        );
    }
}

#[test]
fn lead_closeout_synth_doc_includes_anti_pattern_examples() {
    let content = doc_content();
    // Anti-pattern examples (§4) — pin their presence so future
    // edits don't soften the doc into pure principle without
    // actionable bad-example surface.
    assert!(
        content.contains("Don't write")
            || content.contains("don't write")
            || content.contains("DON'T"),
        "doc must include explicit `Don't write` anti-pattern examples"
    );
    assert!(
        content.contains("Do write") || content.contains("DO"),
        "doc must include `Do write` correct-pattern examples paired with \
         the anti-patterns"
    );
}

// ── Defensive bonuses ─────────────────────────────────────────────

#[test]
fn doc_integrates_with_wave_4_pr_1_task_id_protocol() {
    let content = doc_content();
    // The Wave 4 PR-1 #566 schema gate is the structural counterpart
    // to this doc's narrative discipline. Pin the integration
    // section so the two layers stay coherently described.
    assert!(
        content.contains("Wave 4 PR-1") && content.contains("task_id"),
        "doc must explain integration with Wave 4 PR-1 #566 task_id \
         schema gate"
    );
    // The doc must explicitly state task_id ALONE is not sufficient
    // — that's the key narrative-vs-structural distinction.
    assert!(
        content.contains("not sufficient")
            || content.contains("NOT sufficient")
            || content.contains("alone"),
        "doc must state that task_id alone is insufficient signal of \
         claim — the cross-check procedure is the gap-closing surface"
    );
}

#[test]
fn doc_distinguishes_dispatched_from_claimed_explicitly() {
    let content = doc_content();
    // Pin the core principle: the doc must explicitly distinguish
    // "dispatched" from "claimed" as not-the-same. Without this
    // pin, a future rewrite could drift into language that
    // re-conflates the two states.
    assert!(
        content.contains("dispatched")
            && (content.contains("not") || content.contains("NOT") || content.contains("≠")),
        "doc must explicitly state dispatched ≠ claimed"
    );
}

#[test]
fn doc_includes_history_section_with_companion_prs() {
    let content = doc_content();
    // The history section ties the structural + watchdog +
    // narrative layers together. Pin that all three companion PRs
    // are referenced so future readers see the full picture.
    let companions = [
        ("Wave 4 PR-1", "structural gate"),
        ("Wave 1 PR-1", "task stall watchdog"),
        ("Wave 1 PR-2", "idle watchdog"),
    ];
    for (pr, _purpose) in &companions {
        assert!(
            content.contains(pr),
            "history section must reference companion PR `{pr}` so the \
             narrative doc is anchored within the broader anti-stall arc"
        );
    }
}

#[test]
fn doc_size_is_meaningful() {
    let content = doc_content();
    // Lower bound: a process doc that's < 500 chars is functionally
    // empty (won't carry the principle + incident + checklist + 6
    // anti-pattern examples + integration + history sections). Pin
    // a sane floor so an accidental empty-write would surface.
    assert!(
        content.len() > 2000,
        "doc must contain substantive content (> 2000 chars; got {})",
        content.len()
    );
}
