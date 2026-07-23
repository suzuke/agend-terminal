use std::path::Path;
use std::process::Command;

#[test]
fn agentic_git_is_tracked_in_tree_not_as_a_submodule() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let gitmodules = std::fs::read_to_string(root.join(".gitmodules")).unwrap_or_default();
    assert!(
        !gitmodules.contains("vendor/agentic-git"),
        "vendor/agentic-git must be an in-tree crate, not a configured submodule"
    );

    let output = Command::new("git")
        .args(["ls-files", "--stage", "--", "vendor/agentic-git"])
        .current_dir(root)
        .output()
        .expect("run git ls-files");
    assert!(output.status.success(), "git ls-files failed");

    let entries = String::from_utf8(output.stdout).expect("git ls-files output is UTF-8");
    assert!(
        !entries.lines().any(|line| line.starts_with("160000 ")),
        "vendor/agentic-git is still a gitlink:\n{entries}"
    );
    assert!(
        entries
            .lines()
            .any(|line| line.ends_with("vendor/agentic-git/crates/agentic-git/src/lib.rs")),
        "agentic-git source is not tracked by the parent repository:\n{entries}"
    );
}
