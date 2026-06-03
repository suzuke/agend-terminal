# Contributing

Thanks for considering a contribution. This is a Rust CLI + daemon; the workflow below keeps diffs reviewable and CI green.

## Build & Test

```bash
cargo build                          # debug build
cargo build --release                # release (strip + LTO, matches CI)
cargo test                           # unit + integration + MCP round-trip
cargo test --bin agend-terminal      # unit tests only
cargo test --test integration        # integration tests (Unix-only for now)
cargo fmt --check
cargo clippy -- -D warnings          # must be warning-free
```

`cargo clippy` enforces `unwrap_used = "deny"` (see `Cargo.toml`). Handle errors with `?` / `anyhow::Result`.

CI mirrors these steps on Ubuntu + macOS + Windows (`.github/workflows/ci.yml`).

### Before pushing: `scripts/preflight.sh`

Run the one-shot CI-parity preflight to catch failures locally instead of after a push:

```bash
scripts/preflight.sh          # full matrix; --quick skips the Windows cross-check
```

It runs exactly CI's `check` job — `cargo fmt --check`, `cargo clippy --all-targets --features tray -- -D warnings`, `cargo test --tests --features tray`, and a **Windows cross-check** (`x86_64-pc-windows-msvc`) that catches windows-only compile errors a unix dev box would otherwise miss. The Windows step prefers [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin) (`cargo install cargo-xwin && rustup target add x86_64-pc-windows-msvc`) since a transitive C dependency (`ring`) won't cross-compile on macOS/Linux without the MSVC toolchain; if it's not installed the step SKIPs with a hint rather than false-failing.

### Coverage (optional, local)

The `coverage` CI job (#686) runs `cargo-llvm-cov` on Ubuntu and uploads an lcov report to Codecov. To measure locally:

```bash
cargo install cargo-llvm-cov                  # one-time
cargo llvm-cov --workspace --tests            # text summary
cargo llvm-cov --workspace --tests --html     # HTML report at target/llvm-cov/html/index.html
```

Coverage is observation-only — not a merge gate. PRs that drop project coverage by more than 2% surface as a red Codecov status; the merge decision stays with the reviewer. New code in a PR is expected to hit 80% coverage (configurable in `codecov.yml`).

## Workflow

1. **Branch from `main`** — never modify `main` directly. Worktrees are preferred over in-place branch switching:
   ```bash
   git worktree add ../agend-terminal.feature feature/<short-name>
   cd ../agend-terminal.feature
   ```
2. **Keep commits atomic.** One logical change per commit; run tests before each.
3. **Commit message prefix** — follow the existing history:
   - `feat:` new capability
   - `fix:` bug fix
   - `refactor:` no behavior change
   - `test:` tests only
   - `docs:` docs only
   - `style:` formatting (`cargo fmt`)
   - `ci:` GitHub Actions / tooling
   - `chore:` housekeeping
   - `merge:` PR-style aggregation (used when landing review rounds)
4. **PR description** — what changed and why. Tie bug fixes to evidence (stack trace, repro, test that failed before).

## Scope Discipline

- Don't broaden a PR beyond the stated change. A `fix:` commit should not also rename unrelated types.
- Don't add speculative abstractions, configuration flags, or error branches for cases that can't happen. See existing code for the preferred tight style.
- Comments are English-only.

## Testing Expectations

- **Bug fix** → regression test that fails before the fix and passes after. `tests/integration.rs` for daemon-level behavior; per-module `#[cfg(test)]` for unit tests.
- **Format fidelity — test a consumer/parser against the *producer's real output*, never a hand-crafted shape.** When a test exercises code that consumes a string/struct another function produces (a parser, a matcher, a predicate over a wire format), build the input by calling the producer — don't hand-write the expected shape. Ask in review: *"is this fixture identical to what production actually sends?"* This is the #1483 false-green class: the `notification_is_actionable_wake` matcher keyed on bracketed body markers that the real `[AGEND-MSG-PENDING]` pointer never contains, and its tests hand-crafted that never-emitted shape — so the matcher was dead code while the tests stayed green (and later drifted again when #1487 added a `now=` field the crafted strings lacked). The fix: route both the producer and the tests through one builder (`build_pending_pointer`) so a field added in one place is exercised everywhere. Hand-crafted input is still fine for testing a parser's handling of *malformed/edge* input (empty/missing fields) — just pin the happy-path contract against the real producer (see `extract_msg_id_round_trips_real_format_header`).
- **New MCP tool** → unit-test the handler under `src/mcp/handlers/` and exercise the bridge wire path in `tests/mcp_bridge_client_handshake.rs` (handshake/framing) or `tests/mcp_proxy_parity.rs` (daemon-proxy parity). The legacy `agend-terminal mcp` subcommand was hard-removed in Sprint 56 Track I-Phase2c (#531); `agend-mcp-bridge` is the canonical wire entry point.
- **New CLI flag** → cover it in `tests/integration.rs` or a focused unit test.
- **Test fixtures** — use `std::env::temp_dir()` + `std::process::id()` for isolation, never hardcode `/tmp/...`. Tests that must clean up should `drop` the temp dir explicitly or use scope guards.
- **Deterministic waits, not sleeps.** SOP 1 (§3.20) bans
  `thread::sleep(N)` patterns in tests that wait for asynchronous state.
  Use the existing `pub(crate)` primitives instead — they poll at a fast
  cadence (10 ms) with a bounded timeout and return a `bool` /
  `Option<T>` you can assert on:
  - `admin::cleanup_zombies::poll_until_dead(pid, timeout) -> bool`
    (#934) — wait for a child process to exit (kill -0 on Unix /
    OpenProcess on Windows).
  - `api::handlers::instance::await_sentinel_nonempty(path) -> Option<String>`
    (#949) — wait until a sentinel file has non-empty content. Note the
    rename: pre-#949 the helper was named for the file *existing*, but
    instance-boot callers needed the *content* to be present.
  - When adding a new fixture, check the existing primitives first
    rather than rolling your own sleep loop.

## Capturing a new fixture (#704 sub-task 1)

Real-PTY captures grow the regression corpus in `tests/fixtures/state-replay/` and lock backend-output drift. The capture pipeline is operator-side and opt-in.

### When to capture

- **New backend** — every preset backend needs at least a `*-thinking.raw` + `*-tooluse.raw` baseline so `replay_manifest_regression` covers it (#987 agy was the most recent addition).
- **New state-detection regex** — when adding a state pattern (e.g. a Thinking spinner shape), capture a fixture that exercises it so the regex doesn't drift silently across CLI updates.
- **Reproducing a regression** — file a fixture for any operator-reported state-detection bug so the post-fix regression test has a real byte stream to replay.
- **F9 corpus gap (#1014)** — productive-marker-fire and productive-silence scenarios per backend; see [#1014](https://github.com/suzuke/agend-terminal/issues/1014) for the recording recipe.

### 5-step recipe

1. **Enable capture** (privacy warning fires at daemon boot):
   ```bash
   export AGEND_CAPTURE_FIXTURES=1
   agend-terminal start --agents capture-target:<backend>
   ```
   `<backend>` is one of `claude` / `kiro-cli` / `codex` / `opencode` / `gemini` / `agy`.

2. **Drive the target state.** Interact with the agent until it renders the screen you want to fix. Examples: prompt a tool call to land a completion banner; pause mid-prompt to capture a Thinking spinner; trigger an error path to capture a rate-limit banner.

3. **Promote the capture** to a fixture with a v2 measurement label:
   ```bash
   agend-terminal capture promote \
     $AGEND_HOME/captures/capture-target/*.cap \
     <scenario-name> \
     --scenario-kind <productive_marker_fire|productive_silence|silent_stuck> \
     --expected-hung <hung|not_hung> \
     --scenario-description "<one-line summary>"
   ```
   Promote writes `tests/fixtures/state-replay/<scenario-name>.raw` and appends a schema-v2 entry to `tests/fixtures/state-replay/MANIFEST.yaml`. Phase 1a (#1020) shipped this CLI. `priority_oscillation` is reserved for a future measurement category and is not currently a valid `--scenario-kind` value — add it to the #1020 parser before listing it here.

4. **Review the .raw bytes BEFORE commit.** Captures contain raw PTY output including your prompts and any tool output. Open the file (`less tests/fixtures/state-replay/<scenario-name>.raw`) and scan for:
   - API keys / OAuth tokens echoed during error paths
   - File paths containing your username (`/Users/<you>/...`)
   - Internal URLs / Slack handles / customer names mentioned in prompts
   - Anything from a private repo you don't want public

   If you find anything sensitive: delete the capture, redact the prompt, recapture. There is no built-in scrubber yet — operator review is the v1 safety net.

5. **Commit + PR**:
   - Stage `tests/fixtures/state-replay/<scenario-name>.raw` + `tests/fixtures/state-replay/MANIFEST.yaml`
   - Run `cargo test --bin agend-terminal corpus_measurement_smoke_f9_marker_signals` to confirm the smoke harness classification matches `--scenario-kind`
   - Reference the capturing branch + recording conditions in the PR body so future operators can replay if drift forces a recapture

### Privacy + storage

- Captures land at `$AGEND_HOME/captures/<agent>/<epoch_ms>.cap` + sidecar `.meta.json`.
- Per-agent rotation budget is 50 MB (oldest-mtime first); see `src/capture.rs::rotate_captures`.
- Tunable opt-out: `unset AGEND_CAPTURE_FIXTURES` returns the daemon to zero-overhead NoOp.
- See [#1014](https://github.com/suzuke/agend-terminal/issues/1014) for the F9 fixture gap (real-PTY productive markers per backend) and the upstream tracker for the agy MCP integration limitation.

## Style

- `cargo fmt` always. CI will fail on unformatted diffs.
- `cargo clippy -- -D warnings` — fix warnings, don't `#[allow]` them unless the check is genuinely wrong and you leave a one-line comment explaining why.
- No `unwrap()` / `expect()` in non-test code. Use `?` with `anyhow::Context` for error annotation.
- No `println!` / `eprintln!` in production code paths. Use `tracing::{info, warn, error, debug}`.
- Keep module responsibilities tight:
  - `src/agent_ops.rs` — shared helpers (messaging, fleet mutation, branch validation) called by both the daemon API and the MCP handler path. Drop new duplication here rather than inlining it in two places; `tests/no_dual_track_drift.rs` enforces no drift between `src/agent_ops.rs` and `src/mcp/handlers.rs`.
  - `src/api/` — daemon JSON control API (wire protocol + per-method handlers under `src/api/handlers/`).
  - `src/mcp/` — MCP surface for agents. `handlers.rs` proxies most tool calls to the daemon API; `start_instance` is handled inline there (no separate `ops.rs` since Task #12).
  - `src/<area>.rs` — domain logic (agent, fleet, telegram, health, schedules, …).

## Documentation

- Architectural changes → update `docs/architecture.md`.
- New CLI command → update `docs/CLI.md` and `README.md` command table.
- New MCP tool → update `docs/MCP-TOOLS.md` and the MCP Tools table in `README.md`.
- Major user-facing change → add an entry to `CHANGELOG.md` under `## [Unreleased]`.
- Plan / eval docs (`docs/PLAN-*.md`, `docs/EVAL-*.md`) represent intent at a point in time — when work ships, update status or fold the doc.

## Releasing

Tags are created on `main` by the release workflow (`.github/workflows/release.yml`). Before tagging:

1. Bump `version` in `Cargo.toml`.
2. Move `## [Unreleased]` entries under a new `## [x.y.z] — YYYY-MM-DD` heading in `CHANGELOG.md`.
3. Commit, push, tag `vX.Y.Z`, let the workflow publish.

## Agent-Assisted Development

This repo is also used as a host for AgEnD itself. Agent configs and worktrees live under:

```
.agents/        .continue/      .factory/       .kiro/
.claude/        .worktrees/     fleet.yaml
```

All of these are `.gitignore`d. Don't commit them.
