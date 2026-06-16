//! #2140 follow-up A: the single source of truth for the grandfathered
//! oversized handler files.
//!
//! These two paths are referenced in two places that must never drift:
//! - `tests/file_size_invariant.rs::KNOWN_OVERSIZED` — the per-file LOC ceilings
//!   that grandfather them past `MAX_LOC`.
//! - `mcp::handlers::ci::merge_freshness::is_invariant_input` — the
//!   merge-freshness gate that refuses a stale-base merge touching them.
//!
//! If a third oversized handler is grandfathered into `KNOWN_OVERSIZED` but not
//! into the freshness gate, that file is *silently un-gated* from the #2140
//! protection (the exact silent-gate-miss class #2140 exists to close). This
//! file is compiled into BOTH the binary crate (`mod invariant_inputs` in
//! `main.rs`) and the lib crate (`#[path]` re-home in `lib.rs`), so the gate and
//! the file-size test read the *same* literal list — they cannot diverge. The
//! `known_oversized_paths_match_merge_freshness_inputs` test in
//! `tests/file_size_invariant.rs` pins `KNOWN_OVERSIZED`'s paths to this list so
//! any future addition to one without the other fails CI loud.

/// Pre-existing oversized handler files, grandfathered past the file-size
/// invariant AND gated as merge-freshness invariant inputs. Add a new path here
/// (and its ceiling in `KNOWN_OVERSIZED`) when grandfathering another file.
pub const GRANDFATHERED_OVERSIZED_HANDLERS: &[&str] = &[
    // #t-61: src/mcp/handlers/ci/mod.rs was split into per-action submodules and is
    // no longer oversized — removed from here AND from KNOWN_OVERSIZED (kept in sync).
    "src/mcp/handlers/dispatch_hook/mod.rs",
];
