//! review-repro (scope: agent-binding) — static-invariant guard for the
//! `spawn_instructions_bootstrap` lock-tier-nesting finding.
//!
//! Finding: inside the bootstrap poll loop, the readiness check acquires
//! `registry.lock()` and then, STILL HOLDING it, acquires the per-agent
//! `core.lock()` to read `state.get_state()`. The repo's lock discipline
//! (registry is tier=1; snapshot under the registry lock then release before
//! touching the core lock for anything blocking) is violated here, and the
//! rest of this same function is careful to do exactly that for the inject. The
//! nesting establishes a registry→core acquisition order that core-then-registry
//! paths could deadlock against, and it runs every 200ms during startup.
//!
//! The fix snapshots `Arc::clone(&h.core)` under the registry lock, drops the
//! registry guard, then locks the core (or reads the lock-free on-disk state
//! snapshot). Driving an actual deadlock requires interleaving the unfixed
//! code, so this is a SOURCE-SCANNING invariant (mirrors
//! `tests/core_mutex_invariant.rs`): the `spawn_instructions_bootstrap` body
//! must not acquire the core lock while the registry lock is held.
//!
//! RED now: the readiness block nests `core.lock()` inside `registry.lock()`.
//! GREEN after fix: the nesting is gone. `#[ignore]`d so CI stays green until
//! the fix lands.

use std::path::PathBuf;

#[test]
fn bootstrap_does_not_nest_core_lock_in_registry_lock_agent_binding() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/agent/mod.rs");
    let text = std::fs::read_to_string(&file).expect("read src/agent/mod.rs");
    let lines: Vec<&str> = text.lines().collect();

    // Locate the `spawn_instructions_bootstrap` function body: from its `fn`
    // line to the next top-level `fn ` at column 0.
    let start = lines
        .iter()
        .position(|l| {
            l.trim_start()
                .starts_with("fn spawn_instructions_bootstrap")
        })
        .expect("spawn_instructions_bootstrap fn must exist in src/agent/mod.rs");
    let mut end = lines.len();
    for (idx, l) in lines.iter().enumerate().skip(start + 1) {
        if l.starts_with("fn ") {
            end = idx;
            break;
        }
    }
    let body = &lines[start..end];

    // Detect registry→core nesting: a `registry.lock()` acquisition followed
    // (within a few lines, i.e. inside the same readiness snapshot block) by a
    // `core.lock()` acquisition while the registry guard is still alive.
    let mut nested = Vec::new();
    for (i, line) in body.iter().enumerate() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        if line.contains("registry.lock()") {
            for (j, look) in body.iter().enumerate().skip(i).take(8) {
                let lt = look.trim_start();
                if lt.starts_with("//") || lt.starts_with('*') {
                    continue;
                }
                if look.contains("core.lock()") {
                    nested.push(format!(
                        "registry.lock() @ fn+{} then core.lock() @ fn+{}: {} / {}",
                        i,
                        j,
                        line.trim(),
                        look.trim()
                    ));
                }
            }
        }
    }

    assert!(
        nested.is_empty(),
        "spawn_instructions_bootstrap acquires the per-agent core lock while \
         holding the registry lock (registry→core nesting), violating the \
         tier-1 registry lock discipline. Snapshot `Arc::clone(&h.core)` under \
         the registry lock, drop the registry guard, THEN lock the core (as the \
         inject snapshot a few lines below already does):\n{}",
        nested.join("\n")
    );
}
