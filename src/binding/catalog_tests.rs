#[cfg(target_os = "linux")]
#[test]
fn unreadable_catalog_conservatively_preserves_branch_binding() {
    use std::os::unix::ffi::OsStringExt;

    let home = std::env::temp_dir().join(format!(
        "agend-binding-catalog-test-{}",
        uuid::Uuid::new_v4()
    ));
    let invalid = std::ffi::OsString::from_vec(vec![b'b', 0xff]);
    std::fs::create_dir_all(home.join("boards").join(invalid)).unwrap();

    assert!(super::branch_has_active_task(&home, "feat/keep"));
    std::fs::remove_dir_all(home).ok();
}
