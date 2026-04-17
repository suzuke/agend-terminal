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

CI mirrors these steps on Ubuntu + macOS (`.github/workflows/ci.yml`). Windows is not yet in the matrix — see `docs/PLAN-windows-support.md`.

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
- **New MCP tool** → exercise it in `tests/mcp_roundtrip.rs`.
- **New CLI flag** → cover it in `tests/integration.rs` or a focused unit test.
- **Test fixtures** — use `std::env::temp_dir()` + `std::process::id()` for isolation, never hardcode `/tmp/...`. Tests that must clean up should `drop` the temp dir explicitly or use scope guards.

## Style

- `cargo fmt` always. CI will fail on unformatted diffs.
- `cargo clippy -- -D warnings` — fix warnings, don't `#[allow]` them unless the check is genuinely wrong and you leave a one-line comment explaining why.
- No `unwrap()` / `expect()` in non-test code. Use `?` with `anyhow::Context` for error annotation.
- No `println!` / `eprintln!` in production code paths. Use `tracing::{info, warn, error, debug}`.
- Keep module responsibilities tight — `ops.rs` for high-level operations, `api.rs` for wire protocol, `mcp/` for MCP surface, `src/<area>.rs` for domain logic.

## Documentation

- Architectural changes → update `docs/architecture.md`.
- New CLI command → update `docs/CLI.md` and `README.md` command table.
- New MCP tool → update `docs/MCP-TOOLS.md` and the "35 MCP Tools" table in `README.md`.
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
