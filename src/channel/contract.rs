//! Channel registry + caps + binding contract harness.
//!
//! Call [`run_registry_contract`] against any `impl Channel` to verify it
//! satisfies the subset of the trait's behavioural invariants that are
//! observable **without** hitting a real backend: the in-memory instance →
//! binding registry, `ChannelCapabilities::default()` conservatism, and
//! `BindingRef` / `kind()` stability. Telegram is the only adapter today
//! (see `#[cfg(test)] mod tests` below); future Discord / Slack adapters
//! add their own call site without duplicating invariant logic.
//!
//! ## Scope (what this harness does NOT cover)
//!
//! The transport methods on `Channel` — `send`, `edit`, `delete`,
//! `poll_event`, `create_binding`, `remove_binding`, and any non-default
//! `caps()` shape — talk to real servers and are out of scope here. For
//! full adapter verification, those must be exercised via integration
//! tests against the actual backend; this harness only covers the
//! registry-side trait surface plus default capabilities.
//!
//! Hosted in-crate rather than `tests/` because `src/lib.rs` is
//! intentionally minimal (the supervisor binary must compile without
//! teloxide/ratatui/tokio) — see the PR body for full rationale.
//!
//! ## Invariants checked (one `assert_*` helper each below)
//!
//! - `record → has → take` round-trip, preserving `kind()`
//! - `has_binding` / `take_binding` on an unknown instance are total
//!   (false / None, never panic)
//! - Duplicate `record_binding`: `has_binding` stays true, and — when the
//!   adapter produces distinguishable `display_tag`s — the later write
//!   is the one returned by `take_binding` (last write wins)
//! - Double `take_binding`: second call returns `None`
//! - `ChannelCapabilities::default()` is fully conservative
//! - `BindingRef::display_tag()` is stable and side-effect-free
//! - `attach_registry`: repeat calls do not panic. Write order is NOT
//!   observable through the `Channel` trait (no registry getter), so
//!   last-write-wins cannot be asserted at the contract level — only
//!   tested where the adapter exposes a concrete accessor
//! - `kind()` is stable and non-empty across calls

use crate::channel::{BindingRef, Channel, ChannelCapabilities, MarkdownDialect, MentionStyle};

/// Run the full contract suite against a channel instance.
///
/// `make_binding` is adapter-specific: it must construct a valid
/// `BindingRef` of the shape the adapter expects for `record_binding`
/// without invoking platform side effects (no network / API calls).
/// Each call site in the test module below wires this closure to its
/// adapter's internal payload type.
pub fn run_registry_contract<C: Channel>(ch: C, make_binding: impl Fn(&str) -> BindingRef) {
    assert_default_capabilities_are_conservative();
    assert_kind_is_stable_and_non_empty(&ch);
    assert_has_is_false_for_unknown(&ch);
    assert_take_unknown_returns_none(&ch);
    assert_record_has_take_round_trip(&ch, &make_binding);
    assert_double_take_returns_none(&ch, &make_binding);
    assert_duplicate_record_keeps_bound(&ch, &make_binding);
    assert_display_tag_is_stable(&make_binding);
    assert_attach_registry_is_repeatable(&ch);
}

fn assert_default_capabilities_are_conservative() {
    let c = ChannelCapabilities::default();
    // Transport region.
    assert!(!c.emits_deletion_events, "default emits_deletion_events");
    assert!(!c.threads, "default threads");
    assert!(!c.buttons, "default buttons");
    assert!(!c.attachments, "default attachments");
    assert_eq!(c.markdown, MarkdownDialect::None);
    assert!(c.max_msg_bytes > 0, "default max_msg_bytes must be > 0");
    assert_eq!(c.rate_budget.per_second, 1);
    assert_eq!(c.rate_budget.per_minute, 20);
    // UX region.
    assert!(!c.react, "default react");
    assert!(!c.edit, "default edit");
    assert!(!c.typing_indicator, "default typing_indicator");
    assert!(!c.receives_edit_events, "default receives_edit_events");
    assert_eq!(c.mention_parsing_hint, MentionStyle::None);
    assert!(!c.bot_sees_read_receipts, "default bot_sees_read_receipts");
    assert!(c.has_native_multi_thread_view.is_none());
    assert!(!c.ephemeral, "default ephemeral");
}

fn assert_kind_is_stable_and_non_empty<C: Channel>(ch: &C) {
    let first = ch.kind();
    let second = ch.kind();
    assert_eq!(first, second, "kind() must be stable across calls");
    assert!(!first.is_empty(), "kind() must be non-empty");
}

fn assert_has_is_false_for_unknown<C: Channel>(ch: &C) {
    assert!(
        !ch.has_binding("__contract_unknown_instance_never_recorded__"),
        "has_binding on unknown instance must be false"
    );
}

fn assert_take_unknown_returns_none<C: Channel>(ch: &C) {
    assert!(
        ch.take_binding("__contract_unknown_instance_never_recorded__")
            .is_none(),
        "take_binding on unknown instance must return None (no panic)"
    );
}

fn assert_record_has_take_round_trip<C: Channel>(
    ch: &C,
    make_binding: &impl Fn(&str) -> BindingRef,
) {
    let name = "__contract_round_trip__";
    assert!(!ch.has_binding(name));
    let binding = make_binding(name);
    let kind = binding.kind();
    ch.record_binding(name, binding, "\r".to_string());
    assert!(ch.has_binding(name), "has_binding true after record");
    let taken = ch
        .take_binding(name)
        .expect("take_binding must return Some after record");
    assert_eq!(taken.kind(), kind, "taken binding kind matches recorded");
    assert!(!ch.has_binding(name), "has_binding false after take");
}

fn assert_double_take_returns_none<C: Channel>(ch: &C, make_binding: &impl Fn(&str) -> BindingRef) {
    let name = "__contract_double_take__";
    ch.record_binding(name, make_binding(name), "\r".to_string());
    let _first = ch.take_binding(name).expect("first take");
    assert!(
        ch.take_binding(name).is_none(),
        "second take after successful first take must be None"
    );
}

fn assert_duplicate_record_keeps_bound<C: Channel>(
    ch: &C,
    make_binding: &impl Fn(&str) -> BindingRef,
) {
    let instance = "__contract_duplicate_record__";
    // Build two *distinguishable* bindings by seeding `make_binding` with
    // two different strings. Adapters whose `display_tag` is derived from
    // the seed (Telegram: "TG#<topic_id>") will return different tags;
    // adapters that return `None` degrade gracefully below.
    let first = make_binding("__contract_dup_first__");
    let second = make_binding("__contract_dup_second__");
    let first_tag = first.display_tag().map(str::to_string);
    let second_tag = second.display_tag().map(str::to_string);

    ch.record_binding(instance, first, "\r".to_string());
    assert!(ch.has_binding(instance));
    // Second record for the same instance: contract is "last write wins".
    ch.record_binding(instance, second, "\x03".to_string());
    assert!(
        ch.has_binding(instance),
        "has_binding still true after duplicate record"
    );

    // If the adapter's `display_tag`s distinguish the two bindings, the
    // last-write-wins claim is observable: the `take_binding` return
    // must match the *second* recorded binding, not the first.
    let taken = ch
        .take_binding(instance)
        .expect("take after duplicate record");
    if let (Some(a), Some(b)) = (&first_tag, &second_tag) {
        if a != b {
            assert_eq!(
                taken.display_tag().map(str::to_string),
                second_tag,
                "duplicate record: last write must win (taken tag != second)"
            );
        }
    }
    // Else: tags are None or identical → adapter gives us no signal to
    // distinguish writes at the contract level. has_binding / take round
    // trip is all we can check; leave the stronger assertion to adapter-
    // specific tests that can see the underlying payload.
}

fn assert_display_tag_is_stable(make_binding: &impl Fn(&str) -> BindingRef) {
    let b = make_binding("__contract_display_tag__");
    let first = b.display_tag().map(str::to_string);
    let second = b.display_tag().map(str::to_string);
    assert_eq!(
        first, second,
        "display_tag must be side-effect-free and stable"
    );
}

fn assert_attach_registry_is_repeatable<C: Channel>(ch: &C) {
    use crate::agent::AgentRegistry;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    let r1: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let r2: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    ch.attach_registry(r1);
    // Second attach: the `Channel` trait exposes no registry getter, so
    // write order is NOT observable at the contract level. All we can
    // enforce here is that repeated calls do not panic; the actual
    // "last write wins" guarantee is checked in adapter-specific tests
    // that can read the private registry field directly.
    ch.attach_registry(r2);
}

// ---------------------------------------------------------------------------
// Call sites — one #[test] per adapter implementation. Future adapters
// add their own alongside this one.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::telegram::{TelegramBindingPayload, TelegramChannel, TelegramState};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// Construct a `BindingRef` shaped for the Telegram adapter without
    /// hitting the teloxide runtime — mirrors what
    /// `TelegramChannel::create_binding` would produce on a successful
    /// API call. `TelegramBindingPayload` is `pub(crate)`, so this stays
    /// inside the adapter boundary.
    fn telegram_make_binding(name: &str) -> BindingRef {
        // Deterministic topic_id per instance name so repeat runs
        // and duplicate-record checks stay reproducible.
        let topic_id = 1_000 + name.bytes().map(|b| b as i32).sum::<i32>();
        let payload = TelegramBindingPayload { topic_id };
        BindingRef::new("telegram", Some(format!("TG#{topic_id}")), payload)
    }

    #[test]
    fn telegram_channel_satisfies_contract() {
        // Bot-free constructor: `TelegramState::new` eagerly runs
        // `Bot::new`, which initializes reqwest + `system-configuration`
        // and panics on some macOS setups. None of the contract invariants
        // touch the `bot` field (see module-level "Scope" doc), so we use
        // the `#[cfg(test)]` `new_for_contract_test` path to keep this
        // portable across developer machines and CI.
        let state = TelegramState::new_for_contract_test(
            -100_1234567890,
            HashMap::new(),
            PathBuf::from("/tmp/agend-contract-home"),
            HashMap::new(),
            None,
        );
        let channel = TelegramChannel::new(Arc::new(Mutex::new(state)));
        run_registry_contract(channel, telegram_make_binding);
    }

    /// Locks in the scope boundary of `new_for_contract_test`: if a future
    /// contract assertion reaches a transport path (here: `send_reply`),
    /// the `.expect("telegram bot not initialized")` unwrap must fire
    /// loudly instead of being silently routed past. This guarantees the
    /// bot-free constructor cannot be abused to paper over a transport
    /// call sneaking into the harness.
    #[test]
    #[should_panic(expected = "telegram bot not initialized")]
    fn contract_state_panics_if_transport_reached() {
        let state = Arc::new(Mutex::new(TelegramState::new_for_contract_test(
            -100_1234567890,
            HashMap::new(),
            PathBuf::from("/tmp/agend-contract-home"),
            HashMap::new(),
            None,
        )));
        // `send_reply` unwraps `bot` before hitting the tokio runtime, so
        // this panics without needing a live reactor.
        let _ = crate::channel::telegram::send_reply(&state, "unknown-instance", "hi");
    }
}
