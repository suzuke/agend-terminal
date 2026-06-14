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
