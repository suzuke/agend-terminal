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

/// #2764 R7/R8: the per-name lifecycle hold. `Creating` and `Deleting` are
/// MUTUALLY EXCLUSIVE per `(home, name)` — a create admitted mid-delete would
/// have its fresh generation erased by the delete's tail cleanup, and a delete
/// started mid-create would destroy a half-created generation.
///
/// R8: `Creating` is TOKEN-bound. Only the flow that minted the token may
/// re-enter (nested spawn stage of the SAME create — e.g. MCP create → SPAWN
/// RPC → spawn_one); an INDEPENDENT concurrent same-name create is refused,
/// never refcounted in. Deleting holds refcount for re-entrant deletes.
enum Hold {
    Creating { count: u32, token: u128 },
    Deleting(u32),
}

fn registry() -> &'static Mutex<HashMap<(PathBuf, String), Hold>> {
    static HOLDS: OnceLock<Mutex<HashMap<(PathBuf, String), Hold>>> = OnceLock::new();
    HOLDS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(home: &Path, name: &str) -> (PathBuf, String) {
    (home.to_path_buf(), name.to_string())
}

/// True iff `name` under `home` is currently mid-delete and must not be spawned.
pub fn is_deleting(home: &Path, name: &str) -> bool {
    matches!(
        registry().lock().get(&key(home, name)),
        Some(Hold::Deleting(_))
    )
}

/// #2764 R9: tokens are RANDOM 128-bit values (uuid v4), not sequential — a
/// re-entry token crosses the SPAWN RPC boundary, so a guessable counter would
/// let an unrelated caller forge its way into another create transaction.
fn mint_token() -> u128 {
    uuid::Uuid::new_v4().as_u128()
}

/// Mark `name` under `home` as deleting for the lifetime of the returned guard.
/// Hold the guard across the WHOLE outermost-delete teardown; its `Drop` un-marks
/// on every path (incl. panic) so the name can be re-created afterwards.
/// #2764 R7: refuses (Err) while a same-name CREATE admission is in flight —
/// deleting a half-created generation is neither "zero-side-effect refused"
/// nor "survives completely".
pub fn mark_deleting(home: &Path, name: &str) -> Result<DeletingGuard, String> {
    let k = key(home, name);
    let mut reg = registry().lock();
    match reg.entry(k.clone()).or_insert(Hold::Deleting(0)) {
        Hold::Creating { .. } => {
            // The entry existed as Creating — or_insert did not overwrite it.
            Err(format!(
                "'{name}' has a create in flight — delete refused (retry after it settles)"
            ))
        }
        Hold::Deleting(n) => {
            *n += 1;
            Ok(DeletingGuard { key: k })
        }
    }
}

/// #2764 R7/R8 (codex P0-1): TOP-LEVEL create-side ADMISSION — every
/// create/deploy/TUI/boot instance-creation transaction must acquire this
/// BEFORE its first mutation (worktree/workdir creation, fleet.yaml add,
/// topic create, skills install) and hold it through the WHOLE transaction
/// (success or rollback). Refuses (Err) while the name is mid-delete OR while
/// an INDEPENDENT same-name create is already in flight. While it lives, a
/// same-name delete refuses to START (see [`mark_deleting`]). Nested spawn
/// stages of the SAME create re-enter with [`CreateAdmission::token`] via
/// [`admit_or_reenter_create`].
pub fn admit_create(home: &Path, name: &str) -> Result<CreateAdmission, String> {
    admit_or_reenter_create(home, name, None)
}

/// #2764 R8: admission with optional re-entry. `token` = the top-level
/// admission's token forwarded through the spawn chain (SPAWN RPC params) —
/// matching it re-enters the SAME create transaction; a mismatched/missing
/// token against a live independent hold is refused (same-kind refcounting
/// must never admit a concurrent INDEPENDENT create). No live hold → this
/// call becomes the top-level admission.
pub fn admit_or_reenter_create(
    home: &Path,
    name: &str,
    token: Option<u128>,
) -> Result<CreateAdmission, String> {
    let k = key(home, name);
    let mut reg = registry().lock();
    match reg.get_mut(&k) {
        Some(Hold::Deleting(_)) => Err(format!(
            "'{name}' is mid-delete — create refused before any side effect"
        )),
        Some(Hold::Creating { count, token: t }) => {
            if token == Some(*t) {
                *count += 1;
                Ok(CreateAdmission { key: k, token: *t })
            } else {
                Err(format!(
                    "'{name}' already has an independent create in flight — refused"
                ))
            }
        }
        None => {
            let t = mint_token();
            reg.insert(k.clone(), Hold::Creating { count: 1, token: t });
            Ok(CreateAdmission { key: k, token: t })
        }
    }
}

fn release(kind_is_delete: bool, k: &(PathBuf, String)) {
    let mut reg = registry().lock();
    let remove = match (kind_is_delete, reg.get_mut(k)) {
        (true, Some(Hold::Deleting(n))) => {
            *n -= 1;
            *n == 0
        }
        (false, Some(Hold::Creating { count, .. })) => {
            *count -= 1;
            *count == 0
        }
        // Mismatched/absent entry: unreachable by construction (each guard is
        // minted with its own variant present); tolerate silently.
        _ => false,
    };
    if remove {
        reg.remove(k);
    }
}

/// RAII marker: the `(home, name)` is "deleting" while this guard is alive.
pub struct DeletingGuard {
    key: (PathBuf, String),
}

impl Drop for DeletingGuard {
    fn drop(&mut self) {
        release(true, &self.key);
    }
}

/// RAII marker: a create for `(home, name)` is in flight while this guard is
/// alive; a same-name delete refuses to start.
pub struct CreateAdmission {
    key: (PathBuf, String),
    token: u128,
}

impl CreateAdmission {
    /// #2764 R8: the transaction token — forward it through the spawn chain
    /// (SPAWN RPC params) so nested stages re-enter THIS create instead of
    /// being refused as an independent concurrent one.
    pub fn token(&self) -> u128 {
        self.token
    }
}

impl Drop for CreateAdmission {
    fn drop(&mut self) {
        release(false, &self.key);
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
            let _g = mark_deleting(&home, "victim").expect("no create in flight");
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
        let _g = mark_deleting(&a, "victim").expect("mark");
        assert!(is_deleting(&a, "victim"));
        assert!(
            !is_deleting(&b, "victim"),
            "same name under a different home is independent"
        );
    }

    #[test]
    fn refcount_clears_only_on_last_guard() {
        let home = tmp("refcount");
        let g1 = mark_deleting(&home, "victim").expect("first mark");
        let g2 = mark_deleting(&home, "victim").expect("re-entrant mark");
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
            let _g = mark_deleting(&home2, "victim").expect("mark");
            assert!(is_deleting(&home2, "victim"));
            panic!("delete blew up mid-teardown");
        });
        assert!(r.is_err(), "the closure panicked");
        assert!(
            !is_deleting(&home, "victim"),
            "guard Drop ran on unwind → name un-marked even after a panic"
        );
    }

    /// #2764 R7: a create admission mid-delete is REFUSED (zero side effect),
    /// and admission works again once the delete guard drops.
    #[test]
    fn admit_create_refused_while_deleting_then_allowed() {
        let home = tmp("adm-vs-del");
        let g = mark_deleting(&home, "victim").expect("mark");
        assert!(
            admit_create(&home, "victim").is_err(),
            "create must be refused while mid-delete"
        );
        drop(g);
        let _a = admit_create(&home, "victim").expect("admitted after delete ends");
    }

    /// #2764 R7: a delete cannot START while a create admission is in flight;
    /// it works again once the admission drops.
    #[test]
    fn mark_deleting_refused_while_creating_then_allowed() {
        let home = tmp("del-vs-adm");
        let a = admit_create(&home, "victim").expect("admit");
        assert!(
            !is_deleting(&home, "victim"),
            "a create admission is NOT a deleting hold"
        );
        assert!(
            mark_deleting(&home, "victim").is_err(),
            "delete must refuse while a create is in flight"
        );
        drop(a);
        let _g = mark_deleting(&home, "victim").expect("mark after create settles");
    }

    /// #2764 R8: an INDEPENDENT concurrent same-name create is refused — the
    /// same-kind hold must never refcount a second top-level create in.
    #[test]
    fn independent_concurrent_create_is_refused() {
        let home = tmp("indep-create");
        let a = admit_create(&home, "victim").expect("first top-level admit");
        assert!(
            admit_create(&home, "victim").is_err(),
            "second independent create must be refused"
        );
        drop(a);
        let _b = admit_create(&home, "victim").expect("admitted after the first settles");
    }

    /// #2764 R8: the SAME create transaction re-enters with its token; a
    /// mismatched token is refused; the hold clears only when all guards drop.
    #[test]
    fn token_reenter_same_transaction_only() {
        let home = tmp("token-reenter");
        let top = admit_create(&home, "victim").expect("top-level admit");
        let tok = top.token();
        let nested =
            admit_or_reenter_create(&home, "victim", Some(tok)).expect("matching token re-enters");
        assert!(
            admit_or_reenter_create(&home, "victim", Some(tok ^ 1)).is_err(),
            "mismatched token must be refused"
        );
        drop(top);
        assert!(
            mark_deleting(&home, "victim").is_err(),
            "nested guard still holds the create — delete must refuse"
        );
        drop(nested);
        let _g = mark_deleting(&home, "victim").expect("all guards dropped → delete admitted");
    }
}
