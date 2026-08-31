const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");

#[test]
fn ci_keeps_main_push_and_manual_recovery_triggers() {
    let workflow = CI_WORKFLOW.replace("\r\n", "\n");

    assert!(
        workflow.contains("\n  push:\n    branches: [main,"),
        "CI must continue to run automatically for pushes to main"
    );
    assert!(
        workflow.contains("\n  workflow_dispatch:\n"),
        "CI must retain a manual recovery trigger when GitHub omits a push event"
    );
}
