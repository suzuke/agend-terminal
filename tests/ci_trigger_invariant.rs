const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");

fn has_manual_trigger(workflow: &str) -> bool {
    workflow.contains("\n  workflow_dispatch:\n")
}

fn push_branches_include_main(workflow: &str) -> bool {
    workflow.contains("\n  push:\n    branches: [main,")
}

#[test]
fn ci_keeps_main_push_and_manual_recovery_triggers() {
    let workflow = CI_WORKFLOW.replace("\r\n", "\n");

    assert!(
        push_branches_include_main(&workflow),
        "CI must continue to run automatically for pushes to main"
    );
    assert!(
        has_manual_trigger(&workflow),
        "CI must retain a manual recovery trigger when GitHub omits a push event"
    );
}

#[test]
fn ci_trigger_check_is_order_independent() {
    let valid = "on:\n  push:\n    branches: [release, main]\n  workflow_dispatch:\njobs:\n";
    assert!(push_branches_include_main(valid));
}

#[test]
fn ci_trigger_check_is_top_level_aware() {
    let jobs_only = "on:\n  push:\n    branches: [main]\njobs:\n  workflow_dispatch:\n";
    assert!(!has_manual_trigger(jobs_only));
}
