# agend-terminal Review 驗證測試 — 對照表

- **日期**：2026-06-14
- **分支**：`test/review-repro-2026-06-14`（worktree: `.worktrees/review-repro`，從 `origin/main`@`66dcbf6`）
- **對應 review**：`docs/CODE-REVIEW-2026-06-14.md` 的 105 條發現
- **狀態**：105 個測試全部編譯通過；**104 RED**（成功重現對應 bug）、**1 GREEN**（已在新 commit 修掉，測試當 regression guard）。全部 `#[ignore]`，CI 預設跳過。

> 每個測試現在是「紅」（重現 bug）。**修好對應 bug 後，移除該測試的 `#[ignore]`，它轉綠即確認修復**。

## 如何執行

```bash
cd .worktrees/review-repro

# 針對單一 finding（substring 即可，注意：勿加 --exact，且 --ignored 才會跑）：
cargo test -- --ignored <test_name>

# 跑「全部」repro 測試（確認現在都是紅的）：本表底部每一列的測試名存成清單後當 filter。
#   zsh 需用 ${=NAMES} 強制分詞，bash 直接 $NAMES 即可。
NAMES=$(awk -F'`' '/^\| [0-9]/{print $6}' docs/CODE-REVIEW-2026-06-14-REPRO-TESTS.md | tr '\n' ' ')
cargo test --no-fail-fast -- --ignored ${=NAMES}     # zsh
# cargo test --no-fail-fast -- --ignored $NAMES       # bash

# in-module 那批（behavioral）可單獨用模組名前綴跑：
cargo test --bin agend-terminal review_repro -- --ignored --no-fail-fast

# 修好某 bug 後 → 移除該測試的 #[ignore] → 普通 cargo test 即驗證轉綠（red→green 才算確認）。
```

## 測試方法分佈

| 方法 | 數量 | 說明 |
|------|------|------|
| `behavioral_unit` | 33 | 行為單元測試（呼叫函式 / catch_unwind 驗 panic） |
| `behavioral_fs` | 14 | 行為整合測試（建臨時 HOME，驅動檔案系統行為） |
| `static_invariant` | 53 | 靜態 invariant（掃 source，斷言壞 pattern 消失 / 守門存在） |
| `redesign_required` | 5 | 無法在不改架構下測行為 → 過渡靜態守門 + 重設計建議 |

## ⚠️ 需要重設計架構才能測「行為」的 5 條（你的原則：測不了 = 架構訊號）

這些目前只能用靜態守門測試「壞 pattern」；要直接測「修復後的正確行為」，需先加下列接縫（seam）：

### R1. [api] Dedup cache keys only on request_id, allowing a replayed id to return a stale response for a different operation
- **過渡守門測試**：`dedup_entry_carries_a_request_fingerprint_api`（`tests/review_api_dedup_fingerprint_api.rs`）
- **需要的架構改動**：A behavioral test of the fix cannot compile against current code: DedupCache::dispatch(request_id, wait_timeout, handler) has no parameter to supply a (method,params) fingerprint and Entry has no field to store one, so 'mismatched fingerprint re-executes instead of replaying' is unexercisable without a signature change. Structural change: add a fingerprint argument to dispatch, thread it through check_or_register/finalize, store it on Entry, and on a Cached/InProgress hit compare the incoming request's fingerprint — on mismatch skip dedup (treat as fresh) or return an explicit error. That seam makes the cross-operation-replay behavior directly testable.

### R2. [bootstrap-config-cli] Zombie kill primitive has TOCTOU between liveness check and signal — can kill a recycled PID
- **過渡守門測試**：`cleanup_zombie_daemon_must_recheck_identity_before_signal_bootstrap_config_cli`（`tests/review_bootstrap_config_cli_3.rs`）
- **需要的架構改動**：cleanup_zombie_daemon's signature is `(pid: u32, term_grace: Duration, kill_grace: Duration)` — a bare PID with no recorded-identity material, so the PID-reuse window cannot be closed without an API change. The primitive must accept the run-dir's recorded identity (e.g. the ZombieInfo.run_dir / the .daemon `pid:start_time` line) and compare it against the live process (start-time via /proc or sysctl, or argv/comm) between is_alive and the signal; bail if it no longer matches. This is the static interim guard; the structural change is the parameter+identity-compare seam.

### R3. [tasks] handle_update status check is out-of-lock for non-status fields; emitter identity drift in done arm
- **過渡守門測試**：`handle_update_resolves_actor_in_lock_not_from_stale_record_tasks`（`tests/review_tasks_6.rs`）
- **需要的架構改動**：The transition events are constructed BEFORE entering append_batch_checked_at, so the actor cannot be re-resolved against committed state with the current shape. The fix requires moving event construction (or at least the `by`/owner resolution) INTO the precondition closure so the actor is read from the in-lock fresh replay state — a structural change to handle_update's emit ordering. The static guard is the interim verification per the all-code-is-testable principle; a true behavioral race test becomes possible once that seam exists.

### R4. [tasks] board_router index repair re-appends duplicate entries on every miss → unbounded task_index.jsonl growth
- **過渡守門測試**：`index_repair_is_guarded_against_duplicate_reappend_tasks`（`tests/review_tasks_10.rs`）
- **需要的架構改動**：Truly demonstrating UNBOUNDED growth behaviorally needs repeated cross-boot index loss with the on-disk append-only file surviving between losses — the simple in-process repro self-heals after the first repair (lookup then hits O(1)). The robust fix is architectural: dedupe on read or compact task_index.jsonl so repeated repairs cannot accumulate. The static guard (unconditional repair must gain a dedup/existence guard) is the interim verification; a behavioral growth test becomes deterministic once the index has a compaction/dedup seam to observe.

### R5. [xcut-security] Same-user process holding api.cookie gains full operator authority (defense-in-depth note)
- **過渡守門測試**：`operator_connection_uses_kernel_verified_peer_credential_xcut_security`（`tests/review_xcut_security_2.rs`）
- **需要的架構改動**：This is `info`/documented as the intentional same-user trust model, and the runtime gap is genuinely not closable without an architecture change: `check_operation_allowed(method, params, state)` does not even RECEIVE a peer credential, and TCP loopback (the current transport) has no OS peer-UID mechanism. The structural fix is to (1) switch the operator/CLI transport to a Unix domain socket and read `SO_PEERCRED`/`getpeereid` at accept time (or on Windows verify the peer process owner), (2) thread that kernel-verified credential into `check_operation_allowed` and require it (not the self-reported handshake `pid`) for the direct-method `return Ok(())` operator branch, and (3) keep cookie hygiene (never log/print the cookie; the Cookie type is already `[u8;32]`, never Display-formatted). Until that lands, this source-scanning guard pins the precise defect (self-reported pid + no verified credential) and is the mandated interim test.

## 全部對照表（finding → 測試 → 狀態）

| # | scope | 方法 | 狀態 | 測試名 | 檔案 |
|---|-------|------|------|--------|------|
| 1 | agent-binding | behavioral-fs | 🔴 RED | `cleanup_working_dir_does_not_follow_symlink_out_of_workspace_agent_binding` | `src/agent_ops/review_repro_agent_binding.rs` |
| 2 | agent-binding | behavioral-unit | 🔴 RED | `resolve_instance_idless_is_deterministic_agent_binding` | `src/agent/review_repro_agent_binding.rs` |
| 3 | agent-binding | behavioral-unit | 🔴 RED | `classify_exit_none_is_not_respawnable_crash_agent_binding` | `src/agent/review_repro_agent_binding.rs` |
| 4 | agent-binding | behavioral-unit | 🔴 RED | `install_hooks_warns_when_git_config_fails_agent_binding` | `src/binding/review_repro_agent_binding.rs` |
| 5 | agent-binding | static-invariant | 🔴 RED | `dismiss_thread_has_no_raw_unbounded_writer_lock_agent_binding` | `tests/review_dismiss_writer_lock_agent_binding.rs` |
| 6 | agent-binding | static-invariant | 🔴 RED | `bootstrap_does_not_nest_core_lock_in_registry_lock_agent_binding` | `tests/review_bootstrap_lock_nesting_agent_binding.rs` |
| 7 | api | behavioral-unit | 🔴 RED | `zero_byte_oversized_entries_are_count_bounded_api` | `src/api/request_dedup/review_repro_api.rs` |
| 8 | api | redesign-required | 🔴 RED | `dedup_entry_carries_a_request_fingerprint_api` | `tests/review_api_dedup_fingerprint_api.rs` |
| 9 | api | static-invariant | 🔴 RED | `handle_list_does_not_hold_registry_lock_across_dispatch_idle_io_api` | `tests/review_api_query_lock_io_api.rs` |
| 10 | api | static-invariant | 🔴 RED | `preauth_read_timeout_is_set_on_the_read_fd_api` | `tests/review_api_preauth_timeout_fd_api.rs` |
| 11 | api | static-invariant | 🔴 RED | `register_external_releases_registry_lock_before_taking_external_api` | `tests/review_api_external_lock_order_api.rs` |
| 12 | app-tui | static-invariant | 🔴 RED | `confirmclose_kills_nonfleet_pane_agent_app_tui` | `src/app/overlay/review_repro_app_tui.rs` |
| 13 | app-tui | static-invariant | 🔴 RED | `agent_is_alive_doc_drops_parking_lot_poison_claim_app_tui` | `src/app/review_repro_app_tui.rs` |
| 14 | app-tui | static-invariant | 🔴 RED | `terminal_resize_arm_performs_ghost_clear_app_tui` | `src/app/review_repro_app_tui.rs` |
| 15 | bootstrap-config-cli | behavioral-fs | 🔴 RED | `upsert_state_hooks_backs_up_corrupt_settings_before_discard_bootstrap_config_cli` | `src/mcp_config/review_repro_bootstrap_config_cli.rs` |
| 16 | bootstrap-config-cli | behavioral-unit | 🔴 RED | `find_run_dir_skips_dead_pid_run_dir_bootstrap_config_cli` | `src/bin/agend-mcp-bridge/review_repro_bootstrap_config_cli.rs` |
| 17 | bootstrap-config-cli | redesign-required | 🔴 RED | `cleanup_zombie_daemon_must_recheck_identity_before_signal_bootstrap_config_cli` | `tests/review_bootstrap_config_cli_3.rs` |
| 18 | bootstrap-config-cli | static-invariant | 🔴 RED | `upsert_mcp_servers_must_not_swallow_corrupt_backup_copy_bootstrap_config_cli` | `tests/review_bootstrap_config_cli_1.rs` |
| 19 | bootstrap-config-cli | static-invariant | 🔴 RED | `macos_service_install_writes_plist_atomically_bootstrap_config_cli` | `tests/review_bootstrap_config_cli_4.rs` |
| 20 | channel | behavioral-unit | 🟢 GREEN (已修) | `inject_provenance_failure_leaves_real_topic_registry_untouched_channel` | `src/channel/telegram/reply/review_repro_channel.rs` |
| 21 | channel | static-invariant | 🔴 RED | `notify_does_not_fire_and_forget_onto_undriven_runtime_channel` | `tests/notify_undriven_runtime_invariant_channel.rs` |
| 22 | channel | static-invariant | 🔴 RED | `discord_keepalive_does_not_sleep_before_first_refresh_channel` | `tests/discord_keepalive_sleep_position_channel.rs` |
| 23 | channel | static-invariant | 🔴 RED | `notify_recreated_topic_retry_rekeys_dedup_claim_channel` | `tests/notify_recreated_topic_dedup_rekey_channel.rs` |
| 24 | daemon-ci-pr | behavioral-unit | 🔴 RED | `github_poll_runs_percent_encodes_branch_in_query_daemon_ci_pr` | `src/daemon/ci_watch/provider/review_repro_daemon_ci_pr.rs` |
| 25 | daemon-ci-pr | static-invariant | 🔴 RED | `pr_ready_dedup_flag_recovers_on_enqueue_failure_daemon_ci_pr` | `src/daemon/pr_state/scanner/review_repro_daemon_ci_pr.rs` |
| 26 | daemon-ci-pr | static-invariant | 🔴 RED | `freshness_gate_not_anchored_on_immutable_created_at_daemon_ci_pr` | `src/daemon/pr_state/scanner/review_repro_daemon_ci_pr.rs` |
| 27 | daemon-dispatch-idle | static-invariant | 🔴 RED | `record_pending_decision_cancel_is_flocked_daemon_dispatch_idle` | `tests/decision_timeout_cancel_locked_daemon_dispatch_idle.rs` |
| 28 | daemon-dispatch-idle | static-invariant | 🔴 RED | `record_dispatch_dedup_refresh_uses_locked_rmw_daemon_dispatch_idle` | `tests/dispatch_idle_dedup_locked_daemon_dispatch_idle.rs` |
| 29 | daemon-dispatch-idle | static-invariant | 🔴 RED | `idle_watchdog_last_alerted_map_is_gc_pruned_daemon_dispatch_idle` | `tests/idle_watchdog_last_alerted_gc_daemon_dispatch_idle.rs` |
| 30 | daemon-dispatch-idle | static-invariant | 🔴 RED | `anti_stall_docs_do_not_reference_nonexistent_dispatched_at_daemon_dispatch_idle` | `tests/anti_stall_dispatched_at_doc_daemon_dispatch_idle.rs` |
| 31 | daemon-retention | behavioral-fs | 🔴 RED | `unreadable_daemon_pid_must_not_be_killed_in_destructive_sweep_daemon_retention` | `src/daemon/boot_sweep/review_repro_daemon_retention.rs` |
| 32 | daemon-retention | behavioral-unit | 🔴 RED | `unverified_body_is_not_a_passing_review_verdict_daemon_retention` | `src/daemon/task_sweep/review_repro_daemon_retention.rs` |
| 33 | daemon-retention | static-invariant | 🔴 RED | `waiting_on_stale_has_retain_active_and_is_wired_daemon_retention` | `tests/review_waiting_on_stale_retain_daemon_retention.rs` |
| 34 | daemon-retention | static-invariant | 🔴 RED | `compliance_sweep_does_not_refetch_merged_prs_daemon_retention` | `tests/review_task_sweep_double_fetch_daemon_retention.rs` |
| 35 | daemon-retention | static-invariant | 🔴 RED | `hook_shadow_store_has_an_eviction_path_daemon_retention` | `tests/review_hook_shadow_eviction_daemon_retention.rs` |
| 36 | daemon-supervisor | behavioral-unit | 🔴 RED | `parse_unlock_at_does_not_panic_on_nonascii_pane_content_daemon_supervisor` | `src/daemon/supervisor/review_repro_daemon_supervisor.rs` |
| 37 | daemon-supervisor | static-invariant | 🔴 RED | `pending_auth_map_is_gc_pruned_for_deleted_agents_daemon_supervisor` | `tests/pending_auth_sweep_daemon_supervisor.rs` |
| 38 | daemon-supervisor | static-invariant | 🔴 RED | `disk_io_not_performed_under_core_lock_daemon_supervisor` | `tests/core_lock_disk_io_daemon_supervisor.rs` |
| 39 | daemon-supervisor | static-invariant | 🔴 RED | `router_retain_probe_does_not_ungated_push_into_mirror_buffer_daemon_supervisor` | `tests/router_retain_gate_daemon_supervisor.rs` |
| 40 | deployments-health-teams | behavioral-fs | 🔴 RED | `deploy_rejects_duplicate_name_under_lock_deployments_health_teams` | `src/deployments/review_repro_deployments_health_teams.rs` |
| 41 | deployments-health-teams | behavioral-fs | 🔴 RED | `add_team_to_yaml_enforces_one_agent_one_team_deployments_health_teams` | `src/fleet/persist/review_repro_deployments_health_teams.rs` |
| 42 | deployments-health-teams | static-invariant | 🔴 RED | `create_deployment_team_handles_ok_false_deployments_health_teams` | `tests/review_deployments_health_teams_2.rs` |
| 43 | inbox-notify | behavioral-fs | 🔴 RED | `msg_already_drained_reads_resolved_uuid_path_inbox_notify` | `src/inbox/storage/review_repro_inbox_notify.rs` |
| 44 | inbox-notify | behavioral-fs | 🔴 RED | `migrated_inbox_thread_not_double_counted_via_symlink_inbox_notify` | `src/inbox/storage/review_repro_inbox_notify.rs` |
| 45 | inbox-notify | behavioral-fs | 🔴 RED | `drain_preserves_forward_schema_version_row_on_disk_inbox_notify` | `src/inbox/storage/review_repro_inbox_notify.rs` |
| 46 | inbox-notify | behavioral-unit | 🔴 RED | `enqueue_returning_unread_count_excludes_superseded_rows_inbox_notify` | `src/inbox/storage/review_repro_inbox_notify.rs` |
| 47 | inbox-notify | static-invariant | 🔴 RED | `find_message_does_not_abort_scan_on_unreadable_file_inbox_notify` | `tests/review_inbox_find_message_scan_inbox_notify.rs` |
| 48 | inbox-notify | static-invariant | 🔴 RED | `enqueue_doc_does_not_claim_tmp_rename_atomicity_inbox_notify` | `tests/review_inbox_enqueue_doc_atomicity_inbox_notify.rs` |
| 49 | mcp-ci-worktree | behavioral-fs | 🔴 RED | `explicit_empty_next_after_ci_clears_stale_handoff_target_mcp_ci_worktree` | `src/mcp/handlers/ci/review_repro_mcp_ci_worktree.rs` |
| 50 | mcp-ci-worktree | behavioral-unit | 🔴 RED | `unwatch_uses_validated_caller_not_clear_all_mcp_ci_worktree` | `src/mcp/handlers/ci/review_repro_mcp_ci_worktree.rs` |
| 51 | mcp-ci-worktree | behavioral-unit | 🔴 RED | `compute_next_poll_eta_does_not_overflow_on_huge_interval_mcp_ci_worktree` | `src/mcp/handlers/ci/review_repro_mcp_ci_worktree.rs` |
| 52 | mcp-ci-worktree | static-invariant | 🔴 RED | `mcp_ci_watch_handlers_hold_per_watch_flock_mcp_ci_worktree` | `tests/review_mcp_ci_worktree_2.rs` |
| 53 | mcp-ci-worktree | static-invariant | 🔴 RED | `release_repo_surfaces_unprunable_metadata_leak_mcp_ci_worktree` | `tests/review_mcp_ci_worktree_3.rs` |
| 54 | mcp-core-surface | behavioral-unit | 🔴 RED | `create_instance_branch_main_rejected_e4_5_mcp_core_surface` | `src/mcp/handlers/review_repro_mcp_core_surface.rs` |
| 55 | mcp-core-surface | behavioral-unit | 🔴 RED | `create_instance_team_name_traversal_rejected_mcp_core_surface` | `src/mcp/handlers/review_repro_mcp_core_surface.rs` |
| 56 | mcp-core-surface | behavioral-unit | 🔴 RED | `create_instance_team_count_capped_mcp_core_surface` | `src/mcp/handlers/review_repro_mcp_core_surface.rs` |
| 57 | mcp-core-surface | behavioral-unit | 🔴 RED | `clear_blocked_reason_validates_instance_name_mcp_core_surface` | `src/mcp/handlers/review_repro_mcp_core_surface.rs` |
| 58 | mcp-dispatch-comms | behavioral-unit | 🔴 RED | `ensure_branch_exists_rejects_option_injection_branch_before_git_mcp_dispatch_comms` | `src/mcp/handlers/dispatch_hook/review_repro_mcp_dispatch_comms.rs` |
| 59 | mcp-dispatch-comms | behavioral-unit | 🔴 RED | `parse_duration_secs_does_not_overflow_on_huge_input_mcp_dispatch_comms` | `src/mcp/handlers/dispatch/review_repro_mcp_dispatch_comms.rs` |
| 60 | mcp-dispatch-comms | static-invariant | 🔴 RED | `ensure_branch_exists_doc_claim_matches_code_mcp_dispatch_comms` | `src/mcp/handlers/dispatch_hook/review_repro_mcp_dispatch_comms.rs` |
| 61 | mcp-dispatch-comms | static-invariant | 🔴 RED | `ensure_branch_exists_fetch_budget_under_send_timeout_mcp_dispatch_comms` | `src/mcp/handlers/dispatch_hook/review_repro_mcp_dispatch_comms.rs` |
| 62 | mcp-dispatch-comms | static-invariant | 🔴 RED | `self_dispatch_rejected_before_auto_bind_lease_mcp_dispatch_comms` | `tests/review_mcp_dispatch_comms_4.rs` |
| 63 | mcp-dispatch-comms | static-invariant | 🔴 RED | `handle_inbox_pickup_id_rmw_has_no_toctou_lost_update_mcp_dispatch_comms` | `tests/review_mcp_dispatch_comms_6.rs` |
| 64 | mcp-dispatch-comms | static-invariant | 🔴 RED | `dispatch_outcome_surfaces_watch_arm_failure_mcp_dispatch_comms` | `tests/review_mcp_dispatch_comms_7.rs` |
| 65 | panic-io-extra | behavioral-fs | 🔴 RED | `timeout_emit_gated_on_successful_persist_panic_io_extra` | `src/daemon/decision_timeout/review_repro_panic_io_extra.rs` |
| 66 | panic-io-extra | behavioral-unit | 🔴 RED | `extract_task_id_non_ascii_prefix_does_not_panic_panic_io_extra` | `src/claim_verifier/review_repro_panic_io_extra.rs` |
| 67 | panic-io-extra | behavioral-unit | 🔴 RED | `mask_token_multibyte_does_not_panic_panic_io_extra` | `src/quickstart/review_repro_panic_io_extra.rs` |
| 68 | panic-io-extra | static-invariant | 🔴 RED | `operator_mode_data_write_is_atomic_panic_io_extra` | `tests/review_panic_io_extra_operator_mode.rs` |
| 69 | panic-io-extra | static-invariant | 🔴 RED | `fleet_binding_topic_persist_result_not_dropped_panic_io_extra` | `tests/review_panic_io_extra_telegram_bootstrap.rs` |
| 70 | panic-io-extra | static-invariant | 🔴 RED | `session_json_data_write_is_atomic_panic_io_extra` | `tests/review_panic_io_extra_session.rs` |
| 71 | state-capture | behavioral-fs | 🔴 RED | `unclassified_throttle_static_screen_logs_once_not_per_tick` | `src/state/review_repro_state_capture.rs` |
| 72 | state-capture | behavioral-unit | 🔴 RED | `srl_keep_latched_warn_dedups_across_spinner_ticks` | `src/state/review_repro_state_capture.rs` |
| 73 | state-capture | behavioral-unit | 🔴 RED | `scan_context_pct_no_capture_group_does_not_panic` | `src/state/review_repro_state_capture.rs` |
| 74 | state-capture | behavioral-unit | 🔴 RED | `srl_phantom_consecutive_rematch_warn_dedups_on_static_throttle` | `src/state/review_repro_state_capture.rs` |
| 75 | state-capture | static-invariant | 🔴 RED | `unclassified_throttle_dedup_comment_is_not_misleading` | `tests/review_misleading_dedup_comment_state_capture.rs` |
| 76 | tasks | behavioral-fs | 🔴 RED | `seq_cache_cross_process_does_not_drop_real_event_tasks` | `src/task_events/review_repro_tasks.rs` |
| 77 | tasks | behavioral-fs | 🔴 RED | `archive_does_not_flip_completed_task_to_cancelled_tasks` | `src/tasks/lifecycle/review_repro_tasks.rs` |
| 78 | tasks | behavioral-unit | 🔴 RED | `caller_cannot_forge_pr_merge_provenance_on_done_tasks` | `src/tasks/handler/review_repro_tasks.rs` |
| 79 | tasks | behavioral-unit | 🔴 RED | `board_root_dotdot_project_does_not_escape_to_home_tasks` | `src/task_events/review_repro_tasks.rs` |
| 80 | tasks | redesign-required | 🔴 RED | `handle_update_resolves_actor_in_lock_not_from_stale_record_tasks` | `tests/review_tasks_6.rs` |
| 81 | tasks | redesign-required | 🔴 RED | `index_repair_is_guarded_against_duplicate_reappend_tasks` | `tests/review_tasks_10.rs` |
| 82 | tasks | static-invariant | 🔴 RED | `lifecycle_archive_write_is_durable_and_error_checked_tasks` | `tests/review_tasks_3.rs` |
| 83 | tasks | static-invariant | 🔴 RED | `sweep_cancel_uses_legality_guard_not_bare_append_tasks` | `tests/review_tasks_7.rs` |
| 84 | tasks | static-invariant | 🔴 RED | `task_id_has_process_unique_component_tasks` | `src/tasks/handler/review_repro_tasks.rs` |
| 85 | tasks | static-invariant | 🔴 RED | `auto_close_emitter_matches_acl_allow_list_tasks` | `tests/review_tasks_9.rs` |
| 86 | verify-claim-cost | behavioral-unit | 🔴 RED | `parse_since_huge_value_does_not_panic_or_overflow_verify_claim_cost` | `src/token_cost/review_repro_verify_claim_cost.rs` |
| 87 | verify-claim-cost | behavioral-unit | 🔴 RED | `find_cargo_test_payload_skips_testbed_and_finds_real_invocation_verify_claim_cost` | `src/claim_verifier/review_repro_verify_claim_cost.rs` |
| 88 | verify-claim-cost | behavioral-unit | 🔴 RED | `prose_mentioning_fn_pattern_stays_unknown_verify_claim_cost` | `src/claim_verifier/review_repro_verify_claim_cost.rs` |
| 89 | verify-claim-cost | static-invariant | 🔴 RED | `token_cost_comment_does_not_claim_max_per_file_verify_claim_cost` | `tests/review_verify_claim_cost_4.rs` |
| 90 | verify-claim-cost | static-invariant | 🔴 RED | `git_show_and_fmt_does_not_swallow_stdin_write_error_verify_claim_cost` | `tests/review_verify_claim_cost_5.rs` |
| 91 | worktree-git | behavioral-fs | 🔴 RED | `prune_keeps_remote_gone_branch_with_unpushed_commits_worktree_git` | `src/worktree_cleanup/review_repro_worktree_git.rs` |
| 92 | worktree-git | behavioral-unit | 🔴 RED | `cleanup_merged_branch_uses_true_default_not_main_worktree_git` | `src/worktree_pool/review_repro_worktree_git.rs` |
| 93 | worktree-git | behavioral-unit | 🔴 RED | `evaluate_candidate_derives_real_agent_for_slash_branch_worktree_git` | `src/worktree_pool/review_repro_worktree_git.rs` |
| 94 | worktree-git | behavioral-unit | 🔴 RED | `is_in_use_fails_closed_on_canonicalize_error_worktree_git` | `src/worktree_cleanup/review_repro_worktree_git.rs` |
| 95 | worktree-git | static-invariant | 🔴 RED | `release_marker_rmw_is_lock_guarded_or_record_based_worktree_git_3` | `tests/review_worktree_git_3.rs` |
| 96 | worktree-git | static-invariant | 🔴 RED | `gc_remove_one_worktree_remove_has_mandatory_cwd_worktree_git_4` | `tests/review_worktree_git_4.rs` |
| 97 | worktree-git | static-invariant | 🔴 RED | `github_scm_provider_run_is_timeout_bounded_worktree_git_5` | `tests/review_worktree_git_5.rs` |
| 98 | worktree-git | static-invariant | 🔴 RED | `unsubscribe_all_ci_watches_dead_code_removed_worktree_git_8` | `tests/review_worktree_git_8.rs` |
| 99 | xcut-concurrency | behavioral-unit | 🔴 RED | `create_rejects_path_traversal_in_agent_name_xcut_concurrency` | `src/worktree/review_repro_xcut_concurrency.rs` |
| 100 | xcut-concurrency | static-invariant | 🔴 RED | `ci_mergeable_blocking_has_no_raw_shared_runtime_block_on_xcut_concurrency` | `tests/review_xcut_concurrency_1.rs` |
| 101 | xcut-concurrency | static-invariant | 🔴 RED | `atomic_write_fsyncs_parent_dir_after_rename_xcut_concurrency` | `tests/review_xcut_concurrency_2.rs` |
| 102 | xcut-concurrency | static-invariant | 🔴 RED | `ci_poller_bounds_detached_spawn_with_inflight_guard_xcut_concurrency` | `tests/review_xcut_concurrency_3.rs` |
| 103 | xcut-security | behavioral-unit | 🔴 RED | `validate_branch_rejects_leading_dot_and_git_invalid_refs_xcut_security` | `src/agent_ops/review_repro_xcut_security.rs` |
| 104 | xcut-security | redesign-required | 🔴 RED | `operator_connection_uses_kernel_verified_peer_credential_xcut_security` | `tests/review_xcut_security_2.rs` |
| 105 | xcut-security | static-invariant | 🔴 RED | `quickstart_does_not_embed_bot_token_in_telegram_url_xcut_security` | `tests/review_xcut_security_1.rs` |
