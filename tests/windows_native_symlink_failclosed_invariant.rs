const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");
const COVERAGE_CONTRACT: &str = include_str!("../scripts/test_coverage_run.sh");

fn normalized_workflow() -> String {
    CI_WORKFLOW.replace("\r\n", "\n")
}

fn coverage_contract_step() -> String {
    let workflow = normalized_workflow();
    let (_, check_job) = workflow
        .split_once("\n  check:\n")
        .expect("CI workflow must contain the check job");
    let (check_job, _) = check_job
        .split_once("\n  msrv:\n")
        .expect("CI workflow must contain the msrv job");
    let marker = "      - name: Coverage wrapper contract test\n";
    let (_, step) = check_job
        .split_once(marker)
        .expect("check job must run the coverage wrapper contract");
    step.split_once("\n      - name:")
        .map(|(step, _)| step.to_owned())
        .unwrap_or_else(|| step.to_owned())
}

#[test]
fn exactly_fourteen_symlink_premise_skips_are_fail_closed() {
    let skip_count = COVERAGE_CONTRACT
        .lines()
        .filter(|line| line.trim_start().starts_with("report_symlink_skip \""))
        .count();
    assert_eq!(
        skip_count, 14,
        "only the fourteen symlink-premise skips may be gated; found {skip_count}"
    );
    let wrapper_count = COVERAGE_CONTRACT
        .lines()
        .filter(|line| line.trim_start().starts_with("run_native_symlink_test test_"))
        .count();
    assert_eq!(
        wrapper_count, 14,
        "exactly the fourteen symlink-premise functions must use the scoped wrapper; found {wrapper_count}"
    );
}

#[test]
fn native_msys_normalization_is_deterministic_and_bounded() {
    for marker in [
        "normalize_native_msys()",
        "winsymlinks:nativestrict",
        "awk",
        "native-symlink MSYS before=",
        "native_msys_scope_preserves_unrelated_probe",
        "run_native_symlink_test",
        "COVERAGE_REQUIRE_NATIVE_SYMLINKS",
    ] {
        assert!(
            COVERAGE_CONTRACT.contains(marker),
            "coverage contract must contain {marker:?}"
        );
    }
}

#[test]
fn native_symlink_requirement_is_only_on_windows_coverage_contract_step() {
    let step = coverage_contract_step();
    assert!(
        step.contains(
            "COVERAGE_REQUIRE_NATIVE_SYMLINKS: ${{ runner.os == 'Windows' && '1' || '0' }}"
        ),
        "coverage contract step must enable the native premise only on Windows: {step}"
    );
    assert_eq!(
        normalized_workflow()
            .matches("COVERAGE_REQUIRE_NATIVE_SYMLINKS")
            .count(),
        1,
        "native premise must not leak into another workflow step"
    );
}
