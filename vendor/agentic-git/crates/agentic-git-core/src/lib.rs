//! agentic-git-core — the contract surface shared between the `agentic-git`
//! shim binary and any embedding system (a fleet daemon, an orchestrator,
//! tests). Everything here is deliberately dependency-light so a consumer
//! links the EXACT same verifier/predicate source as the shim and no
//! algorithm or ref-set drift is possible.

pub mod binding;
pub mod integrity_core;
pub mod protected_refs;
