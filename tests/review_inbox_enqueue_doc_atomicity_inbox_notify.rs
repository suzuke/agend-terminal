//! Verification/reproduction (static invariant) for the `inbox-notify` batch,
//! Finding #5 (LOW): the `enqueue` docstring and the `inbox/mod.rs` module
//! header CLAIM an atomic tmp+fsync+rename append, but `enqueue` actually does
//! an in-place `OpenOptions::append` + `sync_all` (no tmp, no rename — the inline
//! H1 comment even says so). A crash mid-write CAN leave a half-written trailing
//! line (which `recover_half_writes` exists to repair). The contradicting docs
//! mislead a reader into assuming a crash-atomic guarantee the code lacks.
//!
//! Method: the fix is a doc reword, so we assert the FALSE claims are gone from
//! the source. RED now (both false claims present), GREEN once the enqueue/append
//! docs describe the real contract (in-place flock'd append + fsync, torn-tail
//! repair by recover_half_writes; tmp+rename only in the rewriters).

use std::path::PathBuf;

fn read_source_file(path: &PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
#[ignore = "enqueue-doc-contradicts-impl: red until fix; remove #[ignore] after fix to confirm"]
fn enqueue_doc_does_not_claim_tmp_rename_atomicity_inbox_notify() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let storage = read_source_file(&root.join("src/inbox/storage.rs"));
    let mod_rs = read_source_file(&root.join("src/inbox/mod.rs"));

    let mut violations = Vec::new();

    // 1) The enqueue docstring's false claim of a tmp+rename atomic append.
    //    enqueue does NOT do tmp+rename — only the read-modify-write rewriters
    //    (drain/sweep/clear/supersede) do.
    if storage.contains("atomic append via flock + tmp + fsync + rename") {
        violations.push(
            "src/inbox/storage.rs: enqueue docstring still claims \
             `atomic append via flock + tmp + fsync + rename`, but enqueue does an \
             in-place append + sync_all (no tmp/rename)."
                .to_string(),
        );
    }

    // 2) The mod.rs module header's "Atomic append" bullet describing a
    //    temp-file + fsync + rename for EACH enqueue.
    if mod_rs.contains("each enqueue writes to a temp file") || mod_rs.contains("fsyncs, then") {
        violations.push(
            "src/inbox/mod.rs: module header still claims `each enqueue writes to a \
             temp file, fsyncs, then renames` — enqueue is an in-place flock'd \
             append + fsync, with torn-tail repair by recover_half_writes."
                .to_string(),
        );
    }

    assert!(
        violations.is_empty(),
        "enqueue append docs contradict the non-atomic in-place implementation. \
         Reword to describe the real contract (in-place flock'd append + fsync; \
         tmp+rename only in the drain/sweep/clear/supersede rewriters):\n{}",
        violations.join("\n")
    );
}
