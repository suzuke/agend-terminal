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

/// CR-2026-06-14 (dual-2 REJECT follow-up): a lock on ONE side of a read-modify-
/// write is no mutual exclusion. The inbox-drain FILTER (handle_inbox) is locked
/// above, but the telegram inbound APPEND (`src/channel/telegram/inbound.rs`)
/// also read-derive-writes `pending_pickup_ids` — and was using an UNLOCKED
/// `read_to_string` + `save_metadata` overwrite, which could write a stale array
/// back over a concurrent filter, resurrecting a just-processed id. BOTH sides
/// must go through the locked `update_metadata` RMW.
///
/// RED if the append site regresses to an unlocked `save_metadata` write of
/// `pending_pickup_ids`; GREEN once it derives the new value inside
/// `update_metadata`'s lock.
#[test]
fn pending_pickup_id_append_uses_locked_update_metadata_mcp_dispatch_comms() {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/channel/telegram/inbound.rs");
    let src = std::fs::read_to_string(&p).expect("read src/channel/telegram/inbound.rs");

    // The append must run as a locked read-modify-write keyed on pending_pickup_ids.
    let appends_via_locked_rmw =
        src.contains("update_metadata") && src.contains("\"pending_pickup_ids\"");
    assert!(
        appends_via_locked_rmw,
        "the telegram inbound pickup-id APPEND must use the locked `update_metadata` RMW \
         (mirroring the inbox-drain FILTER); it was not found in inbound.rs."
    );

    // ...and must NOT write pending_pickup_ids via the key-overwriting
    // `save_metadata` (the unlocked-derive path that loses the concurrent filter).
    // `save_metadata` for OTHER keys (e.g. last_message_id) is fine, so scan a
    // window around each save_metadata call for the pending_pickup_ids key.
    let bad_unlocked_write = src.match_indices("save_metadata").any(|(i, _)| {
        let window_end = (i + 240).min(src.len());
        src[i..window_end].contains("pending_pickup_ids")
    });
    assert!(
        !bad_unlocked_write,
        "telegram inbound writes `pending_pickup_ids` via the key-overwriting `save_metadata` \
         (unlocked read-derive-write) — a stale array written back over a concurrent \
         handle_inbox filter resurrects a processed id. Use `update_metadata` (locked RMW) so \
         both the append and the filter serialize on the same flock."
    );
}
