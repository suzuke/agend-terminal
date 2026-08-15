//! agend-terminal library surface.
pub mod capture;
pub mod sync_audit;

/// Re-export for integration tests. Same source file as the binary crate's
/// `invariant_inputs` module (`#[path]`), so the merge-freshness gate and the
/// `file_size_invariant` cross-check read the identical grandfathered list and
/// cannot drift. #2140 follow-up A.
#[path = "invariant_inputs.rs"]
pub mod invariant_inputs;

/// Re-export for integration tests. The actual implementation lives in the
/// binary crate's `daemon::heartbeat_pair` module.
pub mod daemon {
    pub mod heartbeat_pair {
        // Re-export the HeartbeatPair struct for integration test assertions.
        #[derive(Debug, Clone, Default, PartialEq, Eq)]
        pub struct HeartbeatPair {
            pub reply_to_channel: Option<String>,
            pub reply_to_input_id: Option<u64>,
            pub reply_to_set_at_ms: i64,
            pub last_mirror_event_id: Option<u64>,
            pub mirror_dispatched_for_turn: bool,
            pub mirror_skip_until_next_turn: bool,
        }
    }
}

/// Re-export for integration tests. Same source file as the binary crate's
/// `admin::orphan_provenance` module (`#[path]`), so the #3273 contract tests
/// exercise the identical code the daemon and `doctor` run. The module is
/// deliberately self-contained (std + serde) so it compiles in both crates.
#[path = "admin/orphan_provenance.rs"]
pub mod orphan_provenance_impl;

/// #3273 V2: the manual cleanup executor, re-exported on the same terms as V1
/// so the contract tests exercise the identical code the doctor surface runs.
#[path = "admin/orphan_cleanup.rs"]
pub mod orphan_cleanup_impl;

pub mod admin {
    pub use super::orphan_cleanup_impl as orphan_cleanup;
    pub use super::orphan_provenance_impl as orphan_provenance;
}

/// Re-export for integration tests: the agent-facing background-job guidance
/// text, whose content and instruction-path wiring are pinned by #3273.
#[path = "background_guidance.rs"]
pub mod background_guidance_impl;

pub mod instructions {
    pub use super::background_guidance_impl::background_process_guidance;
}
