const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");

fn coverage_job(workflow: &str) -> &str {
    let (job_marker, job_prefix) = if workflow.contains("\r\n") {
        ("\r\n  coverage:\r\n", "\r\n  ")
    } else {
        ("\n  coverage:\n", "\n  ")
    };
    let (_, coverage) = workflow
        .split_once(job_marker)
        .expect("CI workflow must contain the coverage job");
    let end = coverage
        .match_indices(job_prefix)
        .find_map(|(at, _)| {
            let line = coverage[at + job_prefix.len()..]
                .lines()
                .next()?
                .trim_end_matches('\r');
            (!line.starts_with(' ') && !line.starts_with('#') && line.ends_with(':')).then_some(at)
        })
        .unwrap_or(coverage.len());
    &coverage[..end]
}

#[test]
fn coverage_job_is_bounded_before_the_next_job() {
    let workflow = "jobs:\n  coverage:\n    steps: []\n  later-job:\n    marker: later\n";
    assert_eq!(coverage_job(workflow), "    steps: []");
}

#[test]
fn coverage_job_accepts_windows_checkout_line_endings() {
    let workflow = "jobs:\r\n  coverage:\r\n    steps: []\r\n  later-job:\r\n    marker: later\r\n";
    assert_eq!(coverage_job(workflow), "    steps: []");
}

#[test]
fn coverage_job_installs_nextest_before_running_wrapper() {
    let raw_job = coverage_job(CI_WORKFLOW);
    let job = raw_job.replace("\r\n", "\n");
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
