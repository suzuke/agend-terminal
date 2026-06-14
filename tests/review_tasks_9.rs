//! Review-repro static-invariant (SCOPEKEY: tasks) — FINDING #9.
//!
//! `auto_close_on_report` emits as InstanceName "system:auto-close" (HYPHEN),
//! but `acl::SYSTEM_IDENTITIES` lists "system:auto_close" (UNDERSCORE). The two
//! names for the same logical subsystem are an event-log audit inconsistency,
//! and if this emitter is ever routed through `can_mutate_record` it is denied
//! (`is_system_identity` returns false for the hyphen form). `status_summary`
//! already routes the underscore form.
//!
//! Guard: the production code in `auto_close.rs` must NOT emit the hyphen form
//! `system:auto-close`; it must standardize on the underscore `system:auto_close`
//! that the ACL allow-list and status_summary use. RED now (hyphen present in
//! the production region); GREEN once standardized.

use std::path::PathBuf;

fn read_auto_close() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/tasks/auto_close.rs");
    std::fs::read_to_string(&p).expect("read src/tasks/auto_close.rs")
}

/// Production region only (before the `#[cfg(test)]` module) so test fixtures
/// don't influence the guard.
fn production_region(text: &str) -> &str {
    match text.find("#[cfg(test)]") {
        Some(i) => &text[..i],
        None => text,
    }
}

#[test]
#[ignore = "tasks-auto-close-emitter-hyphen: red until fix; remove #[ignore] after fix to confirm"]
fn auto_close_emitter_matches_acl_allow_list_tasks() {
    let text = read_auto_close();
    let prod = production_region(&text);

    let uses_hyphen_form = prod.contains("system:auto-close");
    assert!(
        !uses_hyphen_form,
        "FINDING #9: auto_close emits as \"system:auto-close\" (hyphen), which is NOT in \
         acl::SYSTEM_IDENTITIES (\"system:auto_close\", underscore). is_system_identity \
         returns false for the hyphen form, so any future ACL routing would deny it, and \
         the audit trail carries two names for one subsystem. Standardize on the \
         underscore form."
    );
}
