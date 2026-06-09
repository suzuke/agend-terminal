//! #1915: the delete-vs-spawn chokepoint — a process-global set of the
//! `(home, name)` instances currently undergoing teardown.
//!
//! A spawn racing a delete RESURRECTS the instance: it re-creates
//! `workspace/<name>` + a registry handle AFTER teardown cleanup already ran (the
//! intermittent residual root of the #1902–#1909 teardown class — boot-stagger
//! spawn, crash-respawn worker, stage2-restart). The per-path `deleted`-flag
//! checks (#1918 A1) only cover the window while the registry handle still
//! exists; this set survives handle removal and covers the whole delete.
//!
//! `spawn_agent` and `spawn_and_register_agent` consult [`is_deleting`] at their
//! TOP — before ANY side effect (skills-install, `build_command`, child spawn) —
//! and refuse to spawn a name that is mid-delete. The outermost delete entries
//! (`full_delete_instance`, app-mode `kill_agent`, the daemon replace paths)
//! call [`mark_deleting`] at their top and hold the returned [`DeletingGuard`]
//! until ALL teardown side-effects (incl. workspace cleanup) complete.
//!
//! **Lock discipline (#1492 class):** the set is a LEAF lock — acquired
//! standalone, NEVER nested inside the registry lock. The two spawn checks run at
//! function top, before any `registry.lock()`; the delete marks run before the
//! delete touches the registry. So the deleting-set lock and the registry lock
//! are never held simultaneously → no lock-order deadlock.
//!
//! **No-leak guarantee:** [`DeletingGuard`]'s `Drop` decrements a refcount and
//! removes the entry at zero on EVERY path — normal return, early `Err`, or
//! panic-unwind. A leaked entry would make that `(home, name)` un-spawnable for
//! the daemon's lifetime (a name that can never be re-created), so the guard is
//! the load-bearing invariant. The refcount (vs a bare set) keeps re-entrant or
//! concurrent same-name marks correct: the entry clears only when the LAST guard
//! drops.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

fn registry() -> &'static Mutex<HashMap<(PathBuf, String), u32>> {
    static DELETING: OnceLock<Mutex<HashMap<(PathBuf, String), u32>>> = OnceLock::new();
    DELETING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(home: &Path, name: &str) -> (PathBuf, String) {
    (home.to_path_buf(), name.to_string())
}

/// True iff `name` under `home` is currently mid-delete and must not be spawned.
pub fn is_deleting(home: &Path, name: &str) -> bool {
    registry().lock().contains_key(&key(home, name))
}

/// Mark `name` under `home` as deleting for the lifetime of the returned guard.
/// Hold the guard across the WHOLE outermost-delete teardown; its `Drop` un-marks
/// on every path (incl. panic) so the name can be re-created afterwards.
#[must_use = "hold the DeletingGuard for the delete's duration; dropping it immediately un-marks the instance"]
pub fn mark_deleting(home: &Path, name: &str) -> DeletingGuard {
    let k = key(home, name);
    *registry().lock().entry(k.clone()).or_insert(0) += 1;
    DeletingGuard { key: k }
}

/// RAII marker: the `(home, name)` is "deleting" while this guard is alive.
pub struct DeletingGuard {
    key: (PathBuf, String),
}

impl Drop for DeletingGuard {
    fn drop(&mut self) {
        let mut reg = registry().lock();
        if let Some(count) = reg.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                reg.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        // Unique per call so concurrent tests (thread-mode `cargo test`) never
        // share a key — the set is a process-global.
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "agend-deleting-{}-{}-{}",
            std::process::id(),
            tag,
            C.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn mark_sets_then_drop_clears() {
        let home = tmp("basic");
        assert!(!is_deleting(&home, "victim"));
        {
            let _g = mark_deleting(&home, "victim");
            assert!(is_deleting(&home, "victim"), "marked → deleting");
        }
        assert!(
            !is_deleting(&home, "victim"),
            "guard drop → un-marked (name re-creatable)"
        );
    }

    #[test]
    fn home_scoped_keys_do_not_collide() {
        let a = tmp("home-a");
        let b = tmp("home-b");
        let _g = mark_deleting(&a, "victim");
        assert!(is_deleting(&a, "victim"));
        assert!(
            !is_deleting(&b, "victim"),
            "same name under a different home is independent"
        );
    }

    #[test]
    fn refcount_clears_only_on_last_guard() {
        let home = tmp("refcount");
        let g1 = mark_deleting(&home, "victim");
        let g2 = mark_deleting(&home, "victim");
        drop(g1);
        assert!(
            is_deleting(&home, "victim"),
            "still deleting while a second guard is held"
        );
        drop(g2);
        assert!(!is_deleting(&home, "victim"), "cleared when the last drops");
    }

    #[test]
    fn panic_in_delete_still_unmarks() {
        // The load-bearing guarantee: a panic mid-delete must not leave the name
        // permanently stuck (un-re-creatable). Drop runs on unwind.
        let home = tmp("panic");
        let home2 = home.clone();
        let r = std::panic::catch_unwind(move || {
            let _g = mark_deleting(&home2, "victim");
            assert!(is_deleting(&home2, "victim"));
            panic!("delete blew up mid-teardown");
        });
        assert!(r.is_err(), "the closure panicked");
        assert!(
            !is_deleting(&home, "victim"),
            "guard Drop ran on unwind → name un-marked even after a panic"
        );
    }
}
