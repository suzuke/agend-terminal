# Task 278 evidence — PR3582 runtime fixture ownership

## Source and scope

- Integration source: `5351eff81027bb11eb9c79b93d27221fa85c6300`.
- Latest main integrated: `07ca9298cfbbf7a0c5f46d4d9135e373141500b7`.
- Integration merge: `9e337901`.
- Code-change head: `df69274b0cc78b31a0fe853fea8fb5b6cfd78ede` (the branch tip also contains the documentation commits listed below).
- Feature requirements from decisions 60/61 were retained; this task changes test fixtures only.
- No production runtime permission or process-kill behavior was broadened.

## Containment design and implementation

The stopped-leader fixture installs its guard before spawning anything. It retains the leader and a separately held sibling process in the same process group. The leader is killed and reaped through its exact `Child` handle while the sibling remains live; the test then writes a stopped receipt and asserts `runtime.stop` refuses cleanup and leaves the registry entry. The surviving sibling is explicitly reaped through its own `Child` handle.

`GroupGuard::reap_child` uses `Child::try_wait`, exact-handle `Child::kill`, and a bounded three-second polling loop. Drop is best-effort diagnostics only; it does not issue a negative-PGID kill or inspect the process table. The fixture documents that the second process is a sibling group member, not an escaped biological descendant (`src/schedule_jobs/runtime.rs:730-827`).

The registered-worker fixture also installs cleanup before `runtime.start`. Its explicit cleanup returns and asserts success, then asserts the retained worker has exited (`src/schedule_jobs/runtime.rs:837-889`).

The group fixture asserts the sibling's numeric PGID matches the leader's PGID before and after leader reap. `GroupGuard::reap` attempts both held children and aggregates errors, so a failure reaping one child cannot skip cleanup of the other.

## Evidence

### Evidence

ran: `cargo test --locked --bin agend-terminal schedule_jobs::runtime::tests -- --nocapture` → 14 passed, 0 failed.

ran: `cargo test --locked --bin agend-terminal stopped_leader_receipt_does_not_authorize_cleanup_of_live_group -- --nocapture` → 1 passed, 0 failed.

ran: `cargo test --locked --bin agend-terminal launched_registered_worker_is_retained_for_explicit_recovery -- --nocapture` → 1 passed, 0 failed.

ran: `cargo fmt --package agend-terminal -- --check` → passed.

ran: `cargo clippy --locked --bin agend-terminal --tests -- -D warnings` → passed.

ran: `git diff --check` → passed.

ran: `cargo test --locked --bin agend-terminal` at the pre-final terminology-only fixture head → 7,491 passed, 9 failed, 11 ignored. The nine failures were in unrelated API, daemon, dispatch-tracking, binding-state, and checkout-submodule fixtures; none were in `schedule_jobs::runtime`, and the failures showed shared temporary-state/timing issues. The final correction is covered by the post-correction 14-test runtime suite above. This result is reported as suite red, not waived.

Exact isolation record from that run:

- `api::tests::dispatch_spawn_backend_not_found` — API bootstrap reported malformed `api.cookie` trailing bytes; test then saw `ConnectionRefused`.
- `daemon::dispatch_idle::tests::target_is_working_reads_silence_clock_1694` — assertion `absent → not working (fire)`.
- `daemon::per_tick::hourly_gc::tests::run_does_not_block_tick_loop_during_slow_round_p1_2607` — assertion `in_flight must clear once the background round completes`.
- `daemon::per_tick::hourly_gc::tests::run_is_no_op_on_empty_fixtures` — assertion `background round should finish on empty fixtures`.
- `daemon::supervisor::tests::recovery_notice_is_fenced_when_the_handle_is_deleted_after_the_snapshot_78207_1` — expected two authorized notifications, got zero; logs show operator-mode suppression.
- `dispatch_tracking::tests::cleanup_for_instance_drops_assignee_keeps_deleted_dispatcher_1923_g10` — temporary `dispatch_tracking.json` had trailing characters and load fell back to empty; subsequent `Option::unwrap()` failed.
- `dispatch_tracking::tests::test_dispatch_ask_after_30min` — temporary `dispatch_tracking.json` had trailing characters and load fell back to empty; expected one reminder, got zero.
- `mcp::handlers::binding_state::signature_tests::base_from_stale_view_key_keeps_the_signed_binding_verifiable_3546` — integrity key missing before signing.
- `mcp::handlers::ci::checkout_submodule_tests::txn_open_handle_remove_failure_retained_then_recovered` — quarantined corrupt checkout transaction journal; expected recovery count one, got zero.

No separate reviewer agent was assigned for task 278. The completed review was a local self-review of the final diff; external independent verification remains pending with the orchestrator.

## Exact-head isolation comparison

After inspecting each fixture's setup and containment, the nine tests were run one at a time with the exact command template `cargo test --locked --bin agend-terminal '<test>' -- --nocapture --test-threads=1`.

- On task head `951f028007c17f462f476d6e050bea423ca17c49`, all nine commands exited 0 and each reported `1 passed, 0 failed`.
- On base `07ca9298cfbbf7a0c5f46d4d9135e373141500b7`, the same nine commands were run in separately managed disposable branch `validation/278-base-07ca-3`; all nine exited 0 and each reported `1 passed, 0 failed`.

Therefore the nine failures were not reproduced in isolated serial runs on either exact head. The aggregate cause remains unproven; the aggregate red result is preserved and no failure is waived. The task branch was restored without reset to `951f028...` and remains clean.

## Commit history

- `188e11c0e764f68fb43dfe65405fc670b7cc8ce1` — anchor schedule-job fixture cleanup.
- `8793788a0e06e4a80417c6751bf258bec6330dd8` — name the sibling group-member ownership explicitly.
- `28206e15990b77cb4233ec464ed41da3ebcc2bfb` — record validation and handoff evidence.
- `df69274b0cc78b31a0fe853fea8fb5b6cfd78ede` — verify anchored group membership and complete cleanup assertions.

No push, PR publication, merge, or reviewer worktree mutation was performed.
