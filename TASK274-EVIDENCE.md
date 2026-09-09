# Task274 evidence

Repository: `suzuke/agend-terminal`

Base: `26a533bebf5d5274ee641a07e232aaaf02d62f15`

Original regression commit (RED): `1374b66af62589dec7650c08598196e7821c24dc`

Contained-fixture commit: `57897984e42bb656e93770a43096a4d33c85ffa6`

Immutable corrected-fixture RED commit: `510ae93ecc5ef59eb24409413fd2c24233d794e9`

Fix commit (GREEN): `bb661dc7d7693b815981d3582acaa674fae477ab`

Current branch GREEN head: `e3edc914fabd699201535f580d77552ff35f7d25`

## RED

Command:

```text
cargo test --locked --test cli_smoke stop_transport_failure_is_not_reported_as_absent_3559 -- --nocapture
```

Result on `1374b66af62589dec7650c08598196e7821c24dc`:

```text
thread 'stop_transport_failure_is_not_reported_as_absent_3559' panicked
at tests/cli_smoke.rs:723:5:
transport failure must not be reported as success: stdout= stderr=Daemon is not running.
  Start it with:  agend-terminal start
  Or first setup: agend-terminal quickstart
  Did it die unexpectedly?
    Diagnose with: agend-terminal doctor
    Check the log: $AGEND_HOME/daemon.log
test stop_transport_failure_is_not_reported_as_absent_3559 ... FAILED
test result: FAILED. 0 passed; 1 failed
regression_exit_code=101
```

The test started an owned live daemon, replaced its owned `run/<pid>/api.port`
with refused port `1`, and verified that the old implementation falsely
returned success while the daemon remained alive.

After the fixture was corrected, the same test was rerun with the production
two-line fix temporarily absent and the PR3559 production base restored:

```text
red_production_base=26a533bebf5d5274ee641a07e232aaaf02d62f15
immutable_red_head=510ae93ecc5ef59eb24409413fd2c24233d794e9
test stop_transport_failure_is_not_reported_as_absent_3559 ... FAILED
test result: FAILED. 0 passed; 1 failed
immutable_red_exit_code=101
```

## GREEN

Command:

```text
cargo test --locked --test cli_smoke stop_transport_failure_is_not_reported_as_absent_3559 -- --nocapture
```

Result with the fix restored on branch head
`e3edc914fabd699201535f580d77552ff35f7d25`:

```text
test stop_transport_failure_is_not_reported_as_absent_3559 ... ok
test result: ok. 1 passed; 0 failed
```

Additional controls on the same fix tree:

```text
cargo test --locked --test cli_smoke stop_no_wait_returns_on_the_accepted_request_only_3539 -- --nocapture
→ 1 passed; 0 failed

cargo test --locked --bin agend-terminal cli_stop::tests -- --nocapture
→ 6 passed; 0 failed
```

## No-daemon CLI control

Command:

```text
AGEND_HOME=<fresh isolated temp home> target/debug/agend-terminal stop
```

Result:

```text
exit_code=0
Daemon is not running.
  Start it with:  agend-terminal start
  Or first setup: agend-terminal quickstart
  Did it die unexpectedly?
    Diagnose with: agend-terminal doctor
    Check the log: $AGEND_HOME/daemon.log
```

## Lint and format

```text
cargo fmt --package agend-terminal -- --check
→ passed

cargo clippy --locked --bin agend-terminal --tests -- -D warnings
→ Finished `dev` profile; no warnings/errors

git diff --check
→ passed
```

## Fixture ownership audit

The corrected regression uses `UniqueFixtureHome` and `OwnedForegroundDaemon`
(`tests/cli_smoke.rs:421-530,795-841`). The home is unique and removed only
by its exact path. The foreground daemon is spawned with an empty fleet (no
agents or descendants), and the `Child` is wrapped immediately after
`spawn()`—before readiness polling or any other fallible post-spawn work. If
polling or an assertion fails, the guard owns the exact `Child` and performs
bounded teardown: `Child::kill()` targets only that handle, `try_wait()` is
polled for at most 3 seconds, and no process-table/PID/argv/PPID discovery or
kill is used. Normal test teardown calls `reap() -> Result` and asserts success
plus `!pid_alive(pid)` before calling `home_guard.cleanup() -> Result` and
asserting successful exact-path removal. Panic unwinding remains best-effort
and emits a diagnostic if bounded reap fails; it does not claim no residue.
The home guard drops after the daemon guard, so the home is not removed while
the owned child is still being cleaned up.

The prior `FixtureHome` implementation is not used by this regression. Its
independent review found that its PID-directory reaper does not validate start
identity, so it is not claimed as exact-child containment here.

The older `PlantedResidue` fixture was not executed: its constructor can panic
before returning a Drop guard, and its raw-PID cleanup is only in Drop
(`tests/cli_smoke.rs:495-500,520-546,551-563`).
