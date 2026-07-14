//! Typed per-agent lifecycle authority shared by competing outer handlers.
//!
//! A permit is acquired before a lifecycle entry point performs any preflight
//! read and is carried through the complete mutation/rollback transaction.
//! Nested helpers validate the caller-owned permit instead of peeking at a
//! process-global "in flight" bit or accepting an untyped bypass flag.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LifecycleOperation {
    Bind,
    Rebase,
    Release,
    Delete,
}

fn active_permits() -> &'static Mutex<HashMap<(String, String), u64>> {
    static ACTIVE: OnceLock<Mutex<HashMap<(String, String), u64>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Exclusive lifecycle authority for one `(home, agent)` pair.
///
/// Non-Clone RAII guard: `Drop` removes the active-map entry exactly once.
/// Nested helpers take `&LifecyclePermit` — no cloning needed.
pub(crate) struct LifecyclePermit {
    key: (String, String),
    token: u64,
    pub(crate) operation: LifecycleOperation,
}

impl std::fmt::Debug for LifecyclePermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LifecyclePermit")
            .field("operation", &self.operation)
            .field("key", &self.key)
            .finish()
    }
}

impl Drop for LifecyclePermit {
    fn drop(&mut self) {
        let mut active = active_permits().lock();
        if active.get(&self.key) == Some(&self.token) {
            active.remove(&self.key);
        }
    }
}

impl LifecyclePermit {
    pub(crate) fn acquire(
        home: &Path,
        agent: &str,
        operation: LifecycleOperation,
    ) -> Result<Self, String> {
        let key = (home.display().to_string(), agent.to_string());
        let token = next_token();
        let mut active = active_permits().lock();
        if active.contains_key(&key) {
            return Err(format!(
                "lifecycle transaction already in flight for agent '{agent}'"
            ));
        }
        active.insert(key.clone(), token);
        Ok(Self {
            key,
            token,
            operation,
        })
    }

    pub(crate) fn authorizes(&self, home: &Path, agent: &str) -> bool {
        let key = (home.display().to_string(), agent.to_string());
        active_permits().lock().get(&key) == Some(&self.token)
    }
}

/// Compatibility wrapper retained for dispatch's pre-held bind path. The
/// underlying authority is the same typed permit used by release/delete.
pub(crate) struct BindGuard {
    permit: LifecyclePermit,
}

impl std::fmt::Debug for BindGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.permit.fmt(f)
    }
}

impl BindGuard {
    pub(crate) fn try_acquire(home: &Path, target: &str) -> Result<Self, String> {
        LifecyclePermit::acquire(home, target, LifecycleOperation::Bind)
            .map(|permit| Self { permit })
    }

    pub(crate) fn acquire_rebase(home: &Path, target: &str) -> Result<Self, String> {
        LifecyclePermit::acquire(home, target, LifecycleOperation::Rebase)
            .map(|permit| Self { permit })
    }

    pub(crate) fn permit(&self) -> &LifecyclePermit {
        &self.permit
    }
}

pub(crate) fn is_active(home: &Path, agent: &str) -> bool {
    let key = (home.display().to_string(), agent.to_string());
    active_permits().lock().contains_key(&key)
}
