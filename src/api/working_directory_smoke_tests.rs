#![allow(clippy::unwrap_used)]

use super::{tests::tmp_home, validate_working_directory};

#[test]
fn validate_work_dir_rejects_parent_dir() {
    let home = tmp_home("validate_parent");
    let bad = home.join("..").join("escape");
    let err = validate_working_directory(&bad, &home).unwrap_err();
    assert!(
        format!("{err}").contains(".."),
        "expected parent-dir rejection, got: {err}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn validate_work_dir_allows_normal_path() {
    let home = tmp_home("validate_normal");
    let ok = crate::paths::workspace_dir(&home).join("agent");
    std::fs::create_dir_all(&ok).expect("create dir");
    let resolved = validate_working_directory(&ok, &home).expect("normal path must validate");
    assert!(resolved.ends_with("agent"));
    std::fs::remove_dir_all(&home).ok();
}
