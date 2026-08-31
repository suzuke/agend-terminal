const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");

fn on_block(workflow: &str) -> Vec<&str> {
    let mut lines = workflow.lines().skip_while(|line| *line != "on:");
    let _ = lines.next();
    lines
        .take_while(|line| line.is_empty() || line.starts_with(' '))
        .collect()
}

fn has_manual_trigger(workflow: &str) -> bool {
    on_block(workflow).contains(&"  workflow_dispatch:")
}

fn push_branches_include_main(workflow: &str) -> bool {
    let block = on_block(workflow);
    let Some(push_index) = block.iter().position(|line| *line == "  push:") else {
        return false;
    };
    block[push_index + 1..]
        .iter()
        .take_while(|line| line.starts_with("    ") || line.is_empty())
        .find_map(|line| line.trim().strip_prefix("branches: ["))
        .and_then(|branches| branches.strip_suffix(']'))
        .is_some_and(|branches| branches.split(',').any(|branch| branch.trim() == "main"))
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
