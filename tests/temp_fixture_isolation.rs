//! #3245 invariant: test temp-home fixtures must not be able to delete each
//! other's directory.
//!
//! Two collision shapes are possible and this guard rejects both:
//!
//! 1. **Prefix overlap between helpers.** `agend-986-{tag}` and
//!    `agend-986-int-{tag}` are different helpers, yet the first with tag
//!    `int-foo` resolves to the same path as the second with tag `foo`. Any two
//!    shapes where one literal prefix is a prefix of the other can collide this
//!    way, whatever their labels.
//! 2. **The same literal label in two different test functions** of one helper:
//!    both resolve to one directory, and helpers that wipe on entry then delete
//!    a neighbour's fixture mid-run. Reuse *within a single test* is legitimate
//!    — `tmp_home_starts_clean_when_suffix_is_reused` exists precisely to assert
//!    that reuse wipes stale content — so the check is per test function, not
//!    per file.
//!
//! Selection is BEHAVIORAL, not by name: any function whose body reaches
//! `std::env::temp_dir()` is a fixture helper here, whether it is called
//! `tmp_home`, `home_with_state`, or anything else. A name-anchored guard is
//! the kind a future `fn sandbox()` walks straight past.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Source files this guard scans: the crate's own Rust sources, excluding
/// vendored trees (which are not ours to police).
fn owned_rust_sources() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    for dir in ["src", "tests"] {
        collect_rs(&root.join(dir), &mut out);
    }
    out.sort();
    out
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "vendor") {
                continue;
            }
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// A temp-dir fixture helper: the function's name, the file it lives in, and
/// every `format!` literal its body builds a path from.
#[derive(Debug)]
struct Helper {
    file: String,
    name: String,
    shapes: Vec<String>,
    /// Wipes its directory on entry — the precondition for destroying a
    /// neighbour's fixture. Non-destructive helpers may share a path shape
    /// without anyone losing state.
    destructive: bool,
}

/// Split a file into `fn` bodies by brace depth, so "the body reaches
/// `temp_dir()`" is a structural question rather than a line-window guess.
fn functions_of(src: &str) -> Vec<(String, String)> {
    let bytes: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i..].starts_with(&['f', 'n', ' ']) && (i == 0 || !bytes[i - 1].is_alphanumeric()) {
            let name: String = bytes[i + 3..]
                .iter()
                .take_while(|c| c.is_alphanumeric() || **c == '_')
                .collect();
            if let Some(open) = bytes[i..].iter().position(|c| *c == '{') {
                let start = i + open;
                let mut depth = 0i32;
                let mut j = start;
                while j < bytes.len() {
                    match bytes[j] {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if depth == 0 && !name.is_empty() {
                    out.push((name, bytes[start..=j.min(bytes.len() - 1)].iter().collect()));
                    i = start;
                }
            }
        }
        i += 1;
    }
    out
}

/// Path shapes only: the literal a temp-dir path is actually built from.
///
/// A helper body may contain unrelated `format!`s (YAML fixtures, log lines).
/// Anchor on `temp_dir()` and take the literal that the immediately following
/// `join`/`format!` uses, so content strings never masquerade as path shapes.
fn path_shapes(body: &str) -> Vec<String> {
    const WINDOW: usize = 240;
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(pos) = body[from..].find("temp_dir()") {
        let start = from + pos + "temp_dir()".len();
        let mut end = (start + WINDOW).min(body.len());
        while end > start && !body.is_char_boundary(end) {
            end -= 1;
        }
        let window = &body[start..end];
        if let Some(lit) = first_literal_after(window, "format!(")
            .or_else(|| first_literal_after(window, ".join("))
        {
            out.push(lit);
        }
        from = start;
    }
    out
}

fn first_literal_after(window: &str, marker: &str) -> Option<String> {
    let idx = window.find(marker)?;
    let rest = window[idx + marker.len()..].trim_start();
    let stripped = rest.strip_prefix('"')?;
    let end = stripped.find('"')?;
    Some(stripped[..end].to_string())
}

fn temp_dir_helpers() -> Vec<Helper> {
    let mut helpers = Vec::new();
    for path in owned_rust_sources() {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !src.contains("temp_dir()") {
            continue;
        }
        let file = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(&path)
            .display()
            .to_string();
        for (name, body) in functions_of(&src) {
            if !body.contains("temp_dir()") {
                continue;
            }
            let shapes = path_shapes(&body);
            if shapes.is_empty() {
                continue;
            }
            let destructive = body.contains("remove_dir_all") || body.contains("remove_dir(");
            helpers.push(Helper {
                file: file.clone(),
                name,
                shapes,
                destructive,
            });
        }
    }
    helpers
}

/// The literal text before the first `{}`/`{name}` placeholder — the part of a
/// path shape that is fixed regardless of the caller's label.
fn fixed_prefix(shape: &str) -> String {
    match shape.find('{') {
        Some(idx) => shape[..idx].to_string(),
        None => shape.to_string(),
    }
}

/// Pre-existing prefix-overlapping pairs, recorded 2026-08-14 as the baseline
/// for the #3245 ratchet decision (dual-RCA consensus; lead decision "(a)
/// ratchet", correlation t-20260814085625832974-5272-7).
///
/// THIS LIST IS NOT AN APPROVAL. Every entry is a real overlap that can still
/// resolve two helpers to one directory; they are exempted only so the guard
/// could be introduced without the 15-file rename the consensus excluded. The
/// invariant this file enforces is therefore "no NEW offenders", and the list
/// may only ever shrink — `known_overlaps_have_no_stale_entries` fails if an
/// entry stops offending and is not removed.
const KNOWN_PREFIX_OVERLAPS: &[(&str, &str)] = &[
    ("src/api/handlers/mcp_proxy.rs::restart_daemon_real_entry_shutdown_flag_2454", "src/api/handlers/mcp_proxy.rs::restart_daemon_real_entry_without_shutdown_fails_closed_2454"),
    ("src/daemon/delivery_worker.rs::cleanup_rejects_enqueue_during_final_release_tail", "src/daemon/delivery_worker.rs::cleanup_tail_is_keyed_and_unrelated_enqueue_completes"),
    ("src/daemon/delivery_worker.rs::stale_transport_delivery_cannot_recreate_receipts_after_cleanup", "src/daemon/delivery_worker.rs::delivery_enqueued_during_teardown_cannot_resurrect_receipts"),
    ("src/daemon/per_tick/reconcile_backups_gc.rs::backups_root", "src/daemon/per_tick/reconcile_backups_gc.rs::gc_missing_root_is_noop"),
    ("src/daemon/router.rs::mirror_dispatch_uses_named_channel_in_multi_channel_fleet", "src/daemon/router.rs::mirror_dispatch_falls_back_to_active_channel_when_lookup_fails"),
    ("src/daemon/shadow/grok.rs::exact_cwd_and_session_tail_reaches_shared_buffer", "src/daemon/shadow/grok.rs::discovery_rejects_same_named_workspace_outside_daemon_home"),
    ("src/fleet/tests.rs::resolve_instance_model_tier_policy_2477", "src/fleet/tests.rs::resolve_instance_instance_model_tier_overrides_defaults_model_2477"),
    ("src/fleet/tests.rs::test_channel_config_telegram_parsing", "src/fleet/tests.rs::test_channel_absent_when_neither_form_set"),
    ("src/fleet/tests.rs::test_channel_config_telegram_parsing", "src/fleet/tests.rs::test_channel_singular_wins_when_both_set"),
    ("src/fleet/tests.rs::test_channel_config_telegram_parsing", "src/fleet/tests.rs::test_channels_plural_multi_entry_picks_first_by_name"),
    ("src/fleet/tests.rs::test_channel_config_telegram_parsing", "src/fleet/tests.rs::test_channels_plural_single_entry_collapses_to_singular"),
    ("src/inbox/tests.rs::tmp_home", "src/daemon/per_tick/inbox_maintenance.rs::run_is_no_op_on_empty_fixtures"),
    ("src/inbox/tests.rs::tmp_home_fails_closed_when_the_directory_cannot_be_created", "src/daemon/per_tick/inbox_maintenance.rs::run_is_no_op_on_empty_fixtures"),
    ("src/mcp/handlers/ci/ack_handoff_tests.rs::ack_handoff_settles_feature_row_after_track_resolved_3179", "src/mcp/handlers/ci/ack_handoff_tests.rs::ack_handoff_resolved_feature_episode_mismatch_leaves_row_untouched_3179"),
    ("src/mcp/handlers/ci/ack_handoff_tests.rs::ack_handoff_settles_feature_row_after_track_resolved_3179", "src/mcp/handlers/ci/ack_handoff_tests.rs::ack_handoff_resolved_feature_lock_failure_is_reported_3179"),
    ("src/mcp/handlers/ci/ack_handoff_tests.rs::ack_handoff_settles_feature_row_after_track_resolved_3179", "src/mcp/handlers/ci/ack_handoff_tests.rs::ack_handoff_resolved_protected_row_is_not_feature_fallback_3179"),
    ("src/mcp/handlers/ci/checkout_path.rs::existing_legacy_target_is_adopted", "src/mcp/handlers/ci/checkout_path.rs::existing_legacy_journal_is_adopted"),
    ("src/mcp/handlers/ci/notification_only_watch_tests.rs::tmp_home", "src/mcp/handlers/ci/notification_only_watch_tests.rs::notification_only_pre_terminal_repeat_idempotent"),
    ("src/mcp/handlers/ci/tests.rs::dispatch_with_branch_and_repo_auto_invokes_watch_ci", "src/mcp/handlers/ci/tests.rs::dispatch_idempotent_double_watch_safe"),
    ("src/mcp/handlers/ci/watch_tests.rs::status_message_id_matches_by_target_correlation_episode", "src/mcp/handlers/ci/watch_tests.rs::status_message_id_rejects_wrong_correlation"),
    ("src/mcp/handlers/dispatch_hook/tests.rs::delegate_task_idempotent_existing_watch", "src/mcp/handlers/dispatch_hook/tests.rs::delegate_task_with_repo_creates_ci_watch_via_handle_delegate_task"),
    ("src/mcp/handlers/dispatch_hook/tests.rs::delegate_task_with_repo_creates_ci_watch", "src/mcp/handlers/dispatch_hook/tests.rs::delegate_task_with_repo_creates_ci_watch_via_handle_delegate_task"),
    ("src/mcp/handlers/dispatch_hook/tests.rs::delegate_task_without_repo_no_ci_watch", "src/mcp/handlers/dispatch_hook/tests.rs::delegate_task_with_repo_creates_ci_watch_via_handle_delegate_task"),
    ("src/mcp/handlers/dispatch_hook/tests.rs::repo_with_remotes", "src/mcp/handlers/dispatch_hook/tests.rs::ensure_branch_exists_creates_from_non_origin_remote_2010"),
    ("src/mcp/handlers/dispatch_hook/tests.rs::same_agent_different_branch_rejects", "src/mcp/handlers/dispatch_hook/tests.rs::delegate_task_same_agent_different_branch_without_delivering"),
    ("src/mcp/handlers/instance_queries.rs::list_instances_exposes_topic_binding_mode_991", "src/mcp/handlers/instance_queries.rs::list_instances_omits_topic_binding_mode_when_unset_991"),
    ("src/mcp/handlers/r3_46776_red_tests.rs::tmp_home", "src/inbox/storage/perf_r3_equiv.rs::tmp_home"),
    ("src/mcp/usage_stats.rs::tmp_home", "src/daemon/supervisor/usage_limit_control.rs::tmp_home"),
    ("src/mcp/usage_stats.rs::tmp_home", "src/mcp/usage_stats.rs::record_appends_durable_jsonl_lines_2055"),
    ("src/quickstart/tests.rs::emitted_yaml_with_channel_includes_user_allowlist", "src/quickstart/tests.rs::emitted_yaml_without_channel_mentions_user_allowlist"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::context_pcts_default_and_toggleable"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::copy_on_select_default_on_and_toggleable_via_on_off"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::future_schema_version_config_kept_last_good"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::invalid_key_rejected"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::nan_env_override_falls_back_not_poisons"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::observed_badge_default_on_and_toggleable_via_on_off"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::old_config_without_schema_version_loads"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::reload_tolerates_retired_progress_mode_key_2549"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::resolve_effective_thresholds_drives_warn_latch"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::set_hang_auto_recovery_enabled"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::set_refuses_to_overwrite_future_version_file"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::set_uses_disk_base_preserves_concurrent_key_audit2_012"),
    ("src/runtime_config.rs::set_and_get_key", "src/runtime_config.rs::show_pane_state_default_on_and_toggleable"),
    ("src/transport/opencode_server/tests.rs::restore_probe_failure_clears_gate_as_ambiguous", "src/transport/opencode_server/tests.rs::restore_idle_without_target_history_proof_is_ambiguous"),
    ("src/transport/opencode_server/tests.rs::restore_probe_failure_clears_gate_as_ambiguous", "src/transport/opencode_server/tests.rs::restore_reconciles_msg_prefixed_history_target"),
    ("src/transport/receipt.rs::receipt_store_survives_and_reconciles_state_transitions", "src/transport/receipt.rs::opencode_seed_lookup_prefers_the_receipts_actual_backend_session"),
    ("src/transport/receipt.rs::receipt_store_survives_and_reconciles_state_transitions", "src/transport/receipt.rs::opencode_seed_lookup_scopes_session_and_ignores_legacy_ids_after_reload"),
    ("src/transport/receipt.rs::receipt_store_survives_and_reconciles_state_transitions", "src/transport/receipt.rs::protocol_request_id_survives_terminal_compaction_and_reload"),
    ("src/transport/receipt.rs::receipt_store_survives_and_reconciles_state_transitions", "src/transport/receipt.rs::receipt_store_compacts_and_keeps_one_body_owner_per_delivery"),
];

#[test]
fn temp_fixture_shapes_do_not_prefix_overlap() {
    let helpers = temp_dir_helpers();
    assert!(
        helpers.len() > 20,
        "behavioral selection found only {} temp-dir helpers — the scan is broken, \
         not the tree",
        helpers.len()
    );

    let mut shapes: Vec<(String, String, String)> = Vec::new();
    for helper in helpers.iter().filter(|h| h.destructive) {
        for shape in &helper.shapes {
            let prefix = fixed_prefix(shape);
            // Only path-like shapes participate; a bare "{}" carries no family.
            if prefix.len() >= 4 {
                shapes.push((prefix, helper.file.clone(), helper.name.clone()));
            }
        }
    }

    let mut offenders = Vec::new();
    for (a_prefix, a_file, a_name) in &shapes {
        for (b_prefix, b_file, b_name) in &shapes {
            if (a_file, a_name) == (b_file, b_name) || a_prefix == b_prefix {
                continue;
            }
            if b_prefix.starts_with(a_prefix.as_str()) {
                let key = (format!("{a_file}::{a_name}"), format!("{b_file}::{b_name}"));
                if KNOWN_PREFIX_OVERLAPS
                    .iter()
                    .any(|(a, b)| *a == key.0 && *b == key.1)
                {
                    continue;
                }
                offenders.push(format!(
                    "{a_file}::{a_name} {a_prefix:?} is a prefix of {b_file}::{b_name} {b_prefix:?} \
                     — a label starting with {:?} makes the two resolve to one directory",
                    &b_prefix[a_prefix.len()..]
                ));
            }
        }
    }
    offenders.sort();
    offenders.dedup();
    assert!(
        offenders.is_empty(),
        "NEW temp-fixture path-shape collisions (the #3245 ratchet only permits \
         the dated baseline, never additions):\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn temp_fixture_labels_are_not_reused_across_test_functions() {
    // Only helpers that WIPE on entry can destroy a neighbour's fixture, so the
    // harm condition — not merely a shared name — defines the offence.
    let mut offenders = Vec::new();
    for path in owned_rust_sources() {
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !src.contains("temp_dir()") {
            continue;
        }
        let file = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(&path)
            .display()
            .to_string();
        let destructive: Vec<String> = functions_of(&src)
            .into_iter()
            .filter(|(_, body)| {
                body.contains("temp_dir()")
                    && (body.contains("remove_dir_all") || body.contains("remove_dir("))
            })
            .map(|(name, _)| name)
            .collect();
        if destructive.is_empty() {
            continue;
        }
        // label -> the test functions using it. Reuse inside ONE function is
        // legitimate; reuse across two is the collision.
        let mut by_label: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for helper in &destructive {
            let marker = format!("{helper}(");
            let mut from = 0usize;
            while let Some(pos) = src[from..].find(&marker) {
                let at = from + pos;
                from = at + marker.len();
                let rest = src[from..].trim_start();
                let Some(stripped) = rest.strip_prefix('"') else {
                    continue;
                };
                let Some(end) = stripped.find('"') else {
                    continue;
                };
                let label = stripped[..end].to_string();
                let caller = enclosing_fn(&src, at);
                if caller == *helper {
                    continue;
                }
                let users = by_label.entry(label).or_default();
                if !users.contains(&caller) {
                    users.push(caller);
                }
            }
        }
        for (label, users) in by_label {
            if users.len() > 1 {
                offenders.push(format!("{file}: label {label:?} used by {users:?}"));
            }
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "labels are reused across different test functions of a WIPE-ON-ENTRY temp \
         helper, so one test can delete another's directory:\n  {}",
        offenders.join("\n  ")
    );
}

/// Name of the `fn` textually preceding `offset` — attribution that does not
/// depend on brace matching over nested items.
fn enclosing_fn(src: &str, offset: usize) -> String {
    let head = &src[..offset];
    match head.rfind("fn ") {
        Some(idx) => head[idx + 3..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect(),
        None => "<file>".to_string(),
    }
}

/// The ratchet may only tighten: an allowlisted pair that no longer overlaps
/// must be deleted from the baseline, or the list silently protects nothing.
#[test]
fn known_overlaps_have_no_stale_entries() {
    let helpers = temp_dir_helpers();
    let mut live = Vec::new();
    let mut shapes: Vec<(String, String)> = Vec::new();
    for helper in helpers.iter().filter(|h| h.destructive) {
        for shape in &helper.shapes {
            let prefix = fixed_prefix(shape);
            if prefix.len() >= 4 {
                shapes.push((prefix, format!("{}::{}", helper.file, helper.name)));
            }
        }
    }
    for (a_prefix, a_id) in &shapes {
        for (b_prefix, b_id) in &shapes {
            if a_id == b_id || a_prefix == b_prefix {
                continue;
            }
            if b_prefix.starts_with(a_prefix.as_str()) {
                live.push((a_id.clone(), b_id.clone()));
            }
        }
    }
    let stale: Vec<String> = KNOWN_PREFIX_OVERLAPS
        .iter()
        .filter(|(a, b)| !live.iter().any(|(la, lb)| la == a && lb == b))
        .map(|(a, b)| format!("{a} -> {b}"))
        .collect();
    assert!(
        stale.is_empty(),
        "these baseline entries no longer overlap and must be removed from \
         KNOWN_PREFIX_OVERLAPS:\n  {}",
        stale.join("\n  ")
    );
}
