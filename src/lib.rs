//! agend-terminal library surface.
//!
//! Most of the daemon lives in `src/main.rs` (binary-local modules). This lib
//! exists to share a small, stable slice — the self-healing supervisor
//! protocol and its client/server halves — across two binaries:
//!
//! - `agend-terminal` (the daemon + CLI, `src/main.rs`)
//! - `agend-supervisor` (the frozen supervisor, `src/bin/agend-supervisor.rs`)
//!
//! Everything exposed here must stay minimal and free of heavy deps (no
//! ratatui, teloxide, tokio) so the supervisor binary remains small and its
//! semantic surface rarely changes.

pub mod supervisor;
