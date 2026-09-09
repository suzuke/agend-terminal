# Task 278 evidence — PR3582 runtime fixture ownership

## Source and scope

- Integration source: `5351eff81027bb11eb9c79b93d27221fa85c6300`.
- Latest main integrated: `07ca9298cfbbf7a0c5f46d4d9135e373141500b7`.
- Integration merge: `9e337901`.
- Code-change head: `28206e15990b77cb4233ec464ed41da3ebcc2bfb` (the branch tip also contains the documentation commits listed below).
- Feature requirements from decisions 60/61 were retained; this task changes test fixtures only.
- No production runtime permission or process-kill behavior was broadened.

## Containment design and implementation

The stopped-leader fixture installs its guard before spawning anything. It retains the leader and a separately held sibling process in the same process group. The leader is killed and reaped through its exact `Child` handle while the sibling remains live; the test then writes a stopped receipt and asserts `runtime.stop` refuses cleanup and leaves the registry entry. The surviving sibling is explicitly reaped through its own `Child` handle.

`GroupGuard::reap_child` uses `Child::try_wait`, exact-handle `Child::kill`, and a bounded three-second polling loop. Drop is best-effort diagnostics only; it does not issue a negative-PGID kill or inspect the process table. The fixture documents that the second process is a sibling group member, not an escaped biological descendant (`src/schedule_jobs/runtime.rs:730-827`).

The registered-worker fixture also installs cleanup before `runtime.start`. Its explicit cleanup returns and asserts success, then asserts the retained worker has exited (`src/schedule_jobs/runtime.rs:837-889`).

## Evidence

### Evidence

ran: `cargo test --locked --bin agend-terminal schedule_jobs::runtime::tests -- --nocapture` → 14 passed, 0 failed.

ran: `cargo test --locked --bin agend-terminal stopped_leader_receipt_does_not_authorize_cleanup_of_live_group -- --nocapture` → 1 passed, 0 failed.

ran: `cargo test --locked --bin agend-terminal launched_registered_worker_is_retained_for_explicit_recovery -- --nocapture` → 1 passed, 0 failed.

ran: `cargo fmt --package agend-terminal -- --check` → passed.

ran: `cargo clippy --locked --bin agend-terminal --tests -- -D warnings` → passed.

ran: `git diff --check` → passed.

ran: `cargo test --locked --bin agend-terminal` → 7,491 passed, 9 failed, 11 ignored. The nine failures were in unrelated API, daemon, dispatch-tracking, binding-state, and checkout-submodule fixtures; none were in `schedule_jobs::runtime`, and the failures showed shared temporary-state/timing issues. This result is reported as suite red, not waived.

## Commit history

- `188e11c0e764f68fb43dfe65405fc670b7cc8ce1` — anchor schedule-job fixture cleanup.
- `8793788a0e06e4a80417c6751bf258bec6330dd8` — name the sibling group-member ownership explicitly.
- `28206e15990b77cb4233ec464ed41da3ebcc2bfb` — record validation and handoff evidence.

No push, PR publication, merge, or reviewer worktree mutation was performed.
