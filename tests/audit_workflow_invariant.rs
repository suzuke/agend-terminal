const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");

fn audit_job() -> &'static str {
    CI_WORKFLOW
        .split_once("\n  audit:\n")
        .and_then(|(_, rest)| rest.split_once("\n  coverage:\n"))
        .map(|(job, _)| job)
        .expect("ci workflow must contain the audit and coverage jobs")
}

#[test]
fn audit_gate_is_separate_from_best_effort_reporting() {
    let job = audit_job();
    let gate_marker = "      - name: Cargo audit gate\n";
    let report_marker = "      - name: RustSec advisory report\n";
    let gate_start = job
        .find(gate_marker)
        .expect("audit job must have a standalone cargo audit gate");
    let report_start = job
        .find(report_marker)
        .expect("audit job must have a named RustSec reporting step");
    assert!(
        gate_start < report_start,
        "the authoritative audit gate must run before reporting"
    );

    let gate = &job[gate_start..report_start];
    assert!(
        gate.contains("run: cargo audit\n"),
        "the standalone gate must use plain cargo audit"
    );
    assert!(
        !gate.contains("rustsec/audit-check"),
        "the authoritative gate must not be coupled to the reporting action"
    );

    let reporter = &job[report_start..];
    assert!(
        reporter.contains("if: always()\n"),
        "reporting must still be attempted after an audit result"
    );
    assert!(
        reporter.contains("continue-on-error: true\n"),
        "reporting failure must not change the authoritative audit result"
    );
    assert!(
        reporter.contains("uses: rustsec/audit-check@"),
        "the RustSec UI reporter must remain present"
    );
}
