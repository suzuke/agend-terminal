//! #3631: Team Organizer overlay glue — palette entry, preview construction,
//! transactional commit, and post-commit session save. Extracted from
//! `overlay.rs` so that file stays under the anti-monolith LOC ceiling.
//!
//! The Organizer never mutates the layout on preview or scope change; only
//! Enter commits (via `layout::organizer::apply`, a single transaction) and
//! only after that succeeds is the session persisted.

use super::{Overlay, OverlayCtx, OverlayOutcome};
use crate::layout::organizer::{self, OrganizerScope};
use crate::layout::Layout;
use crossterm::event::{KeyCode, KeyEvent};
use std::path::Path;

/// Load the fleet authority the Organizer orders by. A missing or malformed
/// file degrades to an empty config → an empty plan, never a panic.
fn load_fleet_config(home: &Path) -> crate::fleet::FleetConfig {
    crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).unwrap_or_default()
}

/// Persist the layout ONLY after a successful transaction. Returns false on a
/// write failure so the caller can surface it and offer a retry.
fn persist_session(home: &Path, layout: &Layout) -> bool {
    super::super::session::save_session(home, layout)
}

/// Build the Organizer preview overlay for `scope`.
fn organizer_overlay(layout: &Layout, home: &Path, scope: OrganizerScope) -> Overlay {
    let fleet = load_fleet_config(home);
    let scopes = organizer::available_scopes(layout, &fleet);
    let selected = scopes.iter().position(|s| *s == scope).unwrap_or(0);
    let active = scopes.get(selected).cloned().unwrap_or(scope);
    let plan = organizer::plan(layout, &fleet, &active);
    let notice = plan.groups.is_empty().then(|| {
        format!(
            "nothing to arrange for {} (scope: {})",
            active.label(),
            layout
                .active_tab()
                .map(|t| t.name.as_str())
                .unwrap_or("no tab")
        )
    });
    Overlay::Organizer {
        scope: active,
        scopes,
        selected,
        plan,
        applied: false,
        notice,
    }
}

/// Intercept `:arrange …` from the command palette. Returns the overlay to
/// install (or a notice overlay), or None when `cmd` is not an arrange line.
pub(super) fn open_arrange_overlay(
    cmd: &str,
    ctx: &mut OverlayCtx<'_>,
    outcome: &mut OverlayOutcome,
) -> Option<Overlay> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.first() != Some(&"arrange") {
        return None;
    }
    match parts.as_slice() {
        ["arrange", "undo"] => {
            let message = match organizer::undo(ctx.layout) {
                Ok(()) => {
                    outcome.needs_resize = true;
                    if persist_session(ctx.home, ctx.layout) {
                        "arrange undone".to_string()
                    } else {
                        "undone, but session save FAILED".to_string()
                    }
                }
                Err(error) => error.message(),
            };
            Some(Overlay::ReconnectNotice { message })
        }
        ["arrange"] | ["arrange", "current"] => Some(organizer_overlay(
            ctx.layout,
            ctx.home,
            OrganizerScope::CurrentTeam,
        )),
        ["arrange", "all"] => Some(organizer_overlay(
            ctx.layout,
            ctx.home,
            OrganizerScope::AllTeams,
        )),
        ["arrange", name] => Some(organizer_overlay(
            ctx.layout,
            ctx.home,
            OrganizerScope::Team((*name).into()),
        )),
        _ => Some(Overlay::ReconnectNotice {
            message: "usage: arrange [current|<team>|all|undo]".to_string(),
        }),
    }
}

/// Handle one key while the Organizer is the active overlay. Pure preview on
/// scope cycling; a single transactional apply on Enter; snapshot undo on `u`.
pub(super) fn handle_organizer_key(
    overlay: &mut Overlay,
    key: KeyEvent,
    ctx: &mut OverlayCtx<'_>,
) -> OverlayOutcome {
    let mut outcome = OverlayOutcome::default();
    let Overlay::Organizer {
        scope,
        scopes,
        selected,
        plan,
        applied,
        notice,
    } = overlay
    else {
        return outcome;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => *overlay = Overlay::None,
        // Scope cycling is a PURE preview change — it never touches layout.
        KeyCode::Tab | KeyCode::Char(']') if !*applied && !scopes.is_empty() => {
            *selected = (*selected + 1) % scopes.len();
            *scope = scopes[*selected].clone();
            *plan = organizer::plan(ctx.layout, &load_fleet_config(ctx.home), scope);
            *notice = plan
                .groups
                .is_empty()
                .then(|| "nothing to arrange".to_string());
        }
        KeyCode::BackTab | KeyCode::Char('[') if !*applied && !scopes.is_empty() => {
            *selected = (*selected + scopes.len() - 1) % scopes.len();
            *scope = scopes[*selected].clone();
            *plan = organizer::plan(ctx.layout, &load_fleet_config(ctx.home), scope);
            *notice = plan
                .groups
                .is_empty()
                .then(|| "nothing to arrange".to_string());
        }
        KeyCode::Enter => {
            if *applied {
                if notice.is_some() && persist_session(ctx.home, ctx.layout) {
                    *notice = None;
                }
            } else {
                match organizer::apply(ctx.layout, plan) {
                    Ok(()) => {
                        *applied = true;
                        outcome.needs_resize = true;
                        if persist_session(ctx.home, ctx.layout) {
                            *notice = None;
                        } else {
                            *notice =
                                Some("applied; session save FAILED — Enter retries".to_string());
                        }
                    }
                    Err(error) => *notice = Some(error.message()),
                }
            }
        }
        KeyCode::Char('u') => match organizer::undo(ctx.layout) {
            Ok(()) => {
                outcome.needs_resize = true;
                if persist_session(ctx.home, ctx.layout) {
                    *overlay = Overlay::None;
                } else {
                    *notice = Some("undone; session save FAILED".to_string());
                }
            }
            Err(error) => *notice = Some(error.message()),
        },
        _ => {}
    }
    outcome
}
