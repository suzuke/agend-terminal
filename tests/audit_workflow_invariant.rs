const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");
const AUDIT_CONFIG: &str = include_str!("../.cargo/audit.toml");

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn audit_job_from(workflow: &str) -> String {
    let normalized = normalize_line_endings(workflow);
    normalized
        .split_once("\n  audit:\n")
        .and_then(|(_, rest)| rest.split_once("\n  coverage:\n"))
        .map(|(job, _)| job.to_owned())
        .expect("ci workflow must contain the audit and coverage jobs")
}

fn audit_job() -> String {
    audit_job_from(CI_WORKFLOW)
}

#[test]
fn workflow_matching_normalizes_crlf() {
    let crlf_workflow = CI_WORKFLOW.replace("\r\n", "\n").replace('\n', "\r\n");
    let job = audit_job_from(&crlf_workflow);
    assert!(
        job.contains("run: cargo audit\n"),
        "CRLF workflow input must be normalized before matching"
    );
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
        gate.contains("id: cargo_audit_gate\n"),
        "the authoritative gate must expose a stable step id"
    );
    assert!(
        !gate.contains("if:") && !gate.contains("continue-on-error:"),
        "the authoritative gate must not be conditional or non-fatal"
    );
    assert!(
        !gate.contains("rustsec/audit-check"),
        "the authoritative gate must not be coupled to the reporting action"
    );

    let reporter = &job[report_start..];
    assert!(
        reporter.contains(
            "if: >-\n          !cancelled() &&\n          (steps.cargo_audit_gate.outcome == 'success' ||\n          steps.cargo_audit_gate.outcome == 'failure')\n"
        ),
        "reporting must run only after an actual gate success/failure and not cancellation"
    );
    assert!(
        reporter.contains("continue-on-error: true\n"),
        "reporting failure must not change the authoritative audit result"
    );
    assert!(
        reporter.contains("uses: rustsec/audit-check@69366f33c96575abad1ee0dba8212993eecbe998\n"),
        "the RustSec UI reporter must remain present at its exact reviewed SHA"
    );
    assert!(
        reporter.contains("The preceding plain `cargo audit` step is authoritative for the\n"),
        "reporter comments must identify plain cargo audit as authoritative"
    );
    assert!(
        !reporter.contains("The action gate still BLOCKS"),
        "reporter comments must not claim the non-fatal reporter is the gate"
    );
    assert!(
        job.contains("uses: taiki-e/install-action@7f4eb899022d8fe70b20c4f3de697aa85c309026\n"),
        "cargo-audit installer must be pinned to an exact reviewed commit"
    );
}

#[test]
fn audit_config_documents_local_and_ci_authority() {
    let config = normalize_line_endings(AUDIT_CONFIG).replace("\n# ", " ");
    assert!(
        config.contains("local and authoritative CI `cargo audit` runs"),
        "audit policy must document both local and authoritative CI cargo audit"
    );
    assert!(
        config.contains("CI audit gate invokes `cargo audit` directly and reads this file"),
        "audit policy must document that the CI gate reads this config"
    );
    assert!(
        !config.contains("LOCAL `cargo audit` runs"),
        "audit policy must not claim it is local-only"
    );
}

#[test]
fn audit_config_matching_normalizes_crlf() {
    let crlf_config = normalize_line_endings(AUDIT_CONFIG).replace('\n', "\r\n");
    let config = normalize_line_endings(&crlf_config).replace("\n# ", " ");
    assert!(
        config.contains("local and authoritative CI `cargo audit` runs"),
        "CRLF audit config input must be normalized before matching"
    );
}
