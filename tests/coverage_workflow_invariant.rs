const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");

fn coverage_job(workflow: &str) -> &str {
    let (_, coverage) = workflow
        .split_once("\n  coverage:\n")
        .expect("CI workflow must contain the coverage job");
    coverage
}

#[test]
fn coverage_job_installs_nextest_before_running_wrapper() {
    let job = coverage_job(CI_WORKFLOW);
    let install = job
        .find("      - name: Install nextest for coverage isolation (#3281)\n")
        .expect("coverage job must install nextest");
    let tool = job[install..]
        .find("          tool: nextest\n")
        .map(|offset| install + offset)
        .expect("coverage job must ask install-action for nextest");
    let run = job
        .find("        run: scripts/coverage-run.sh\n")
        .expect("coverage job must run the coverage wrapper");

    assert!(
        install < tool && tool < run,
        "nextest must be installed before coverage runs"
    );
}
