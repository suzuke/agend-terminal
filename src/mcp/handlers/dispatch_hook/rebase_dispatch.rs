use super::{BindGuard, DispatchError, DispatchOutcome};
use std::cell::RefCell;
use std::path::Path;

thread_local! {
    static PREHELD_DISPATCH: RefCell<Option<(BindGuard, bool)>> =
        const { RefCell::new(None) };
}

pub(super) fn take_preheld_dispatch() -> (Option<BindGuard>, bool) {
    PREHELD_DISPATCH
        .with(|slot| slot.borrow_mut().take())
        .map_or((None, false), |(guard, rebase)| (Some(guard), rebase))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_auto_bind_lease_with_source_and_chain_preheld(
    home: &Path,
    target: &str,
    task_id: &str,
    branch: &str,
    repo: Option<&str>,
    source_repo_override: Option<&Path>,
    guard: BindGuard,
) -> Result<DispatchOutcome, DispatchError> {
    PREHELD_DISPATCH.with(|slot| *slot.borrow_mut() = Some((guard, true)));
    super::dispatch_auto_bind_lease_with_source_and_chain(
        home,
        target,
        task_id,
        branch,
        repo,
        source_repo_override,
        &[],
        None,
        false,
    )
}
