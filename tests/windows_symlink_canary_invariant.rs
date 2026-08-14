const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");

fn canary_step() -> String {
    let (_, check_job) = CI_WORKFLOW
        .split_once("\n  check:\n")
        .expect("CI workflow must contain the check job");
    let (check_job, _) = check_job
        .split_once("\n  msrv:\n")
        .expect("CI workflow must contain the msrv job");
    let marker = "      - name: Windows native symlink readiness canary (#3248)\n";
    let (_, step) = check_job
        .split_once(marker)
        .expect("check job must run the Windows symlink canary");
    step.split_once("\n      - name:")
        .map(|(step, _)| step.to_owned())
        .unwrap_or_else(|| step.to_owned())
}

#[test]
fn windows_canary_is_bounded_and_non_blocking() {
    let step = canary_step();
    assert!(
        step.contains("if: runner.os == 'Windows'\n"),
        "canary must be Windows-only: {step}"
    );
    assert!(
        step.contains("continue-on-error: true\n"),
        "canary must report readiness without blocking existing CI: {step}"
    );
    assert!(
        step.contains("timeout-minutes: 5\n"),
        "canary must have a bounded runtime: {step}"
    );
    assert!(
        step.contains("shell: bash\n")
            && step.contains("run: scripts/test_windows_native_symlink_canary.sh\n"),
        "canary must use the checked-in bash contract: {step}"
    );
}

#[test]
fn canary_contract_covers_three_link_shapes_and_preserves_msys_options() {
    let script = std::fs::read_to_string("scripts/test_windows_native_symlink_canary.sh")
        .expect("Windows symlink canary script must exist");
    assert!(
        script.contains("winsymlinks:nativestrict"),
        "canary must request native symlinks"
    );
    assert!(
        script.contains("${MSYS:-}") && script.contains("winsymlinks:nativestrict"),
        "canary must append native mode to existing MSYS options"
    );
    for shape in ["file target", "dangling target", "directory target"] {
        assert!(
            script.contains(shape),
            "canary must report the {shape} shape"
        );
    }
    assert!(
        script.matches("ln -s").count() >= 3,
        "canary must create each link shape independently"
    );
    assert!(
        script.matches("[ -L").count() >= 3,
        "canary must verify native link identity for each shape"
    );
    assert!(
        script.contains("exit \"$failures\""),
        "canary must expose readiness failures to the non-blocking workflow step"
    );
}
