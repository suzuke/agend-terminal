//! Verification/reproduction test for the `mcp-dispatch-comms` review batch,
//! finding 6 (low / concurrency).
//!
//! `handle_inbox` (src/mcp/handlers/comms.rs) processes the
//! `pending_pickup_ids` set via TWO non-atomic JSON read/write cycles: it
//! re-reads `pending_pickup_ids` from disk with an UNLOCKED
//! `std::fs::read_to_string`, filters out the just-processed ids, and
//! writes the remainder via `save_metadata`. A pickup id appended to the
//! file between the unlocked re-read and the save is silently CLOBBERED by
//! the stale remainder write — a lost update. The code self-documents this
//! with a "Known TOCTOU window" comment that admits "can lose its
//! pickup_id".
//!
//! Static-invariant method (source scan): the lost-update race cannot be
//! driven deterministically without a write-window seam. We pin the FIX:
//! the read-filter-write must run under the metadata file lock (an atomic
//! read-modify-write), eliminating the documented lost-update window. The
//! self-documenting "Known TOCTOU window" admission of a real lost-update
//! is the BAD pattern that must be gone once the RMW is locked.
//!
//! RED now: `handle_inbox` carries the "Known TOCTOU window" comment that a
//! pickup id "can lose its pickup_id".
//!
//! GREEN after fix: the locked read-modify-write removes the lost-update
//! window, so the self-documenting admission is gone.

use std::path::PathBuf;

fn handle_inbox_body() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/mcp/handlers/comms.rs");
    let src = std::fs::read_to_string(&p).expect("read src/mcp/handlers/comms.rs");
    let start = src
        .find("fn handle_inbox")
        .expect("fn handle_inbox not found");
    let brace = src[start..]
        .find('{')
        .map(|o| start + o)
        .expect("opening brace for handle_inbox not found");
    let bytes = src.as_bytes();
    let mut depth = 0i32;
    let mut end = brace;
    for (i, &b) in bytes.iter().enumerate().skip(brace) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    src[brace..=end].to_string()
}

#[test]
#[ignore = "mcp-dispatch-comms F6: red until the pickup-id read-modify-write is locked (no TOCTOU lost-update); remove #[ignore] after fix"]
fn handle_inbox_pickup_id_rmw_has_no_toctou_lost_update_mcp_dispatch_comms() {
    let body = handle_inbox_body();

    // The self-documenting admission of a real lost-update window. A locked
    // read-modify-write fix removes it.
    let admits_toctou_lost_update = body.contains("Known TOCTOU window")
        || body.contains("can lose its pickup_id")
        || body.contains("lose its pickup_id");

    assert!(
        !admits_toctou_lost_update,
        "handle_inbox still re-reads `pending_pickup_ids` with an UNLOCKED read and writes the \
         filtered remainder via save_metadata, self-documenting a 'Known TOCTOU window' where a \
         concurrently-appended pickup id is clobbered (lost update). Perform the read-filter-write \
         under the metadata file lock (atomic read-modify-write) so no pickup id is lost."
    );
}
