# Git Server Agnostic Audit

**Sprint 12 PR-AB** — Audit of all GitHub-specific assumptions in the agend-terminal codebase.

**Scope**: ripgrep all `gh` CLI calls, `github.com` URLs, GitHub REST API paths, and `octocrab` imports. Classify each hit as git-server-agnostic core or GitHub-specific extension. No production code changes.

**Base**: `origin/main` at `45549b5`

**Decision ref**: `d-20260425080249077056-10` (git-server-agnostic strategic direction)

---

## 1. GitHub-Specific Call Sites (Production Code)

### 1.1 `src/daemon/ci_watch.rs` — HEAVIEST (entire module)

The CI watch module is 100% GitHub Actions specific. Every function assumes GitHub REST API.

| Line(s) | Usage | Detail |
|---|---|---|
| 69-79 | `github_token_warning()` | Checks `GITHUB_TOKEN` env var, returns warning string mentioning `gh auth token` |
| 84-86 | `github_token_warning_from_env()` | Reads `GITHUB_TOKEN` from `std::env` |
| 110-111 | Watch state fields | `head_sha`, `last_notified_head_sha` — GitHub Actions JSON schema |
| 196-236 | `classify_runs_response()` | Parses `workflow_runs` JSON array — GitHub Actions response schema |
| 242-263 | `select_runs_to_notify()` | Reads `id`, `conclusion`, `head_sha` from GitHub run objects |
| 269-294 | `dedupe_notifications_by_head_sha()` | Deduplicates by `head_sha` — GitHub-specific field |
| 298-329 | `build_inbox_body()` / `ci_notification_message()` | Formats `html_url` from GitHub run, references `gh run view` |
| 335-478 | `ci_check_repo()` | Full GitHub REST poll: `GET https://api.github.com/repos/{repo}/actions/runs?branch={branch}&per_page=5`, Bearer auth with `GITHUB_TOKEN`, parses `workflow_runs` JSON |
| 486-508 | `update_watch_state_with_notify()` | Writes `last_run_id` / `head_sha` — GitHub run ID scheme |
| 510-530 | `check_pr_terminal()` | `GET https://api.github.com/repos/{repo}/pulls?head={branch}&state=all&per_page=1`, checks `state == "closed"` — GitHub Pulls API |
| 533-568 | `fetch_failure_summary()` | `GET https://api.github.com/repos/{repo}/actions/runs/{run_id}/jobs`, parses `jobs[].steps[].conclusion` — GitHub Actions Jobs API |
| 572-1191 | `mod tests` | All test fixtures use GitHub API JSON shapes |

**Classification**: **GitHub-specific extension**. Zero git-agnostic content. This is effectively `watch_github_actions`.

### 1.2 `src/mcp/tools.rs` — Tool Schema Descriptions

| Line | Usage | Detail |
|---|---|---|
| 252 | `watch_ci` description | "Watch **GitHub Actions** CI for a repo... If **GITHUB_TOKEN** is not set..." |
| 254 | `watch_ci` param | `repo` described as "**GitHub** repo (owner/repo)" |
| 258 | `unwatch_ci` description | "Stop watching CI for a repo." (neutral wording, but implementation is GitHub-only) |

**Classification**: **GitHub-specific** (description text hardcodes GitHub Actions).

### 1.3 `src/mcp/handlers.rs` — watch_ci Handler

| Line | Usage | Detail |
|---|---|---|
| 1212 | `"watch_ci"` match arm | Routes to ci_watch module |
| 1248-1250 | `github_token_warning_from_env()` | Injects GitHub-specific token warning into MCP response |
| 1255 | `"unwatch_ci"` match arm | Routes to ci_watch::remove_watch |

**Classification**: **GitHub-specific** (handler wires directly to GitHub-only ci_watch module).

### 1.4 `src/agent.rs:78` — Env Passthrough

| Line | Usage | Detail |
|---|---|---|
| 78 | `"GITHUB_TOKEN"` in `SENSITIVE_ENV_KEYS` | Listed alongside `GITLAB_TOKEN` — prevents accidental passthrough to child agents |

**Classification**: **Git-agnostic** ✓. The list already includes `GITLAB_TOKEN`. This is a security passthrough filter, not a GitHub API consumer. Adding more forge tokens is additive.

### 1.5 `src/tray/autostart/macos.rs:19` — Reverse-DNS Label

| Line | Usage | Detail |
|---|---|---|
| 19 | `"io.github.suzuke.agend-terminal"` | LaunchAgent label, reverse-DNS convention for GitHub Pages hosting |

**Classification**: **Cosmetic / hosting identity**. Not a GitHub API dependency. Uses `github.io` domain convention for app identity, not for API calls. No abstraction needed.

### 1.6 `src/channel/caps.rs:98` — Markdown Dialect Comment

| Line | Usage | Detail |
|---|---|---|
| 98 | `/// Discord / GitHub-flavoured markdown.` | Comment on `DiscordMd` enum variant |

**Classification**: **Cosmetic**. Comment references a markdown spec name, not a GitHub API. No abstraction needed.

### 1.7 `src/mcp_config.rs:3` — Reference URL Comment

| Line | Usage | Detail |
|---|---|---|
| 3 | `//! Reference: https://github.com/suzuke/AgEnD (TypeScript version)` | Doc comment linking to upstream repo |

**Classification**: **Cosmetic**. Source reference, not runtime dependency.

---

## 2. GitHub-Specific Call Sites (Non-Production)

### 2.1 `Cargo.toml:7` — Repository Metadata

```toml
repository = "https://github.com/suzuke/agend-terminal"
```

**Classification**: **Metadata**. Cargo registry field. Changes if repo moves, but not a runtime dependency.

### 2.2 `CHANGELOG.md:213-217` — Compare URLs

```
[Unreleased]: https://github.com/suzuke/agend-terminal/compare/v0.4.1...HEAD
```

**Classification**: **Metadata**. Standard changelog convention. Changes if repo moves.

### 2.3 `.github/workflows/ci.yml` + `release.yml` — CI/CD Pipelines

GitHub Actions workflow files. Expected to be GitHub-specific.

**Classification**: **Expected GitHub-specific**. Any CI provider migration would replace these files entirely. Out of scope for abstraction.

### 2.4 `docs/FLEET-DEV-PROTOCOL-v1.md:317,434` — Protocol Examples

| Line | Usage | Detail |
|---|---|---|
| 317 | "No manual `gh pr checks --watch`." | Example of what NOT to do |
| 434 | "Manual `gh pr checks`" in tool reference table | Same |

**Classification**: **Documentation**. Protocol examples reference `gh` CLI as a concrete tool. Would need updating if protocol becomes git-server-agnostic, but not a code dependency.

### 2.5 `.github/workflows/release.yml` — Build Infra GitHub References

| Line | Usage | Detail |
|---|---|---|
| 153 | Comment | `https://github.com/linuxdeploy/linuxdeploy/releases` — reference to upstream release |
| 154 | Comment | `gh api repos/linuxdeploy/linuxdeploy/releases/tags/<tag>` — asset digest lookup hint |
| 156 | Comment | `https://github.com/linuxdeploy/linuxdeploy-plugin-gtk/commits/master` — upstream ref |
| 165 | `wget` | `https://github.com/linuxdeploy/linuxdeploy/releases/download/...` — downloads AppImage build tool |

**Classification**: **GitHub-specific build infra** (non-runtime). Release workflow fetches build tools from GitHub Releases. Replaced entirely when adopting another CI provider.

### 2.6 Docs Cosmetic GitHub URLs

| File:Line | Usage | Detail |
|---|---|---|
| `docs/PLAN-channel-abstraction.md:244` | Reference link | `https://github.com/serenity-rs/serenity` — external crate reference |
| `docs/PLAN-channel-ux-layer.md:197` | Self-reference | `https://github.com/suzuke/agend-terminal` — link to own repo |

**Classification**: **Cosmetic / documentation**. No runtime impact.

### 2.7 Test Fixture Data References

| File:Line | Usage | Detail |
|---|---|---|
| `tests/fixtures/state-replay/codex-tooluse.raw:7` | Snapshot data | `https://github.com/openai/codex/releases/latest` — Codex update banner captured in PTY replay |
| `tests/fixtures/state-replay/codex-thinking.raw:7` | Snapshot data | Same URL in different Codex state replay |
| `tests/fixtures/state-replay/codex-update.raw:7` | Snapshot data | Same URL in Codex update prompt replay |
| `tests/fixtures/state-replay/codex-perm.raw:7` | Snapshot data | Same URL in Codex permission prompt replay |

**Classification**: **Test fixture data** (non-runtime). These are raw PTY output captures used for state-replay testing. The GitHub URL is part of Codex's own update banner, not an agend-terminal API consumer.

### 2.8 `docs/archived/HANDOVER-windows-conpty-nested.md` — Archived References

External GitHub issue URLs (pinokio, codex, gemini-cli). Archived documentation.

**Classification**: **Archived / cosmetic**. No action needed.

---

## 3. Git-Agnostic Core (Already Clean)

These modules use **plain `git` CLI** with no GitHub API assumptions:

| File | Operations | Notes |
|---|---|---|
| `src/worktree.rs` | `git worktree add/list/prune` | Pure git, no forge API |
| `src/worktree_cleanup.rs` | `git worktree list`, `git merge-base --is-ancestor` | Pure git |
| `src/agent_ops.rs:138+` | Branch name validation (`[a-zA-Z0-9/_.-]`) | Pure git convention |
| `src/bootstrap/agent_resolve.rs` | Worktree creation for agents | Pure git |
| `src/bootstrap/fleet_normalize.rs` | Worktree pruning on startup | Pure git |
| `src/deployments.rs` | Branch creation for deployments | Pure git |
| `src/fleet.rs:140` | `worktree_source` config field | Pure git |
| `src/daemon/mod.rs:722` | Residual worktree warning | Pure git |

**No octocrab or other GitHub Rust crate dependencies found** (`Cargo.toml` grep: zero hits).

---

## 4. Abstraction Line

### GitHub-Specific Extension (needs abstraction for multi-forge)

| Component | Scope | Effort |
|---|---|---|
| `src/daemon/ci_watch.rs` (entire module) | GitHub Actions polling, auth, JSON parsing, PR terminal check, failure summary | **L** — needs `CiProvider` trait or delegate-to-CLI approach |
| `src/mcp/tools.rs:252-258` | Tool descriptions hardcode "GitHub Actions" / "GITHUB_TOKEN" | **S** — parameterize description text |
| `src/mcp/handlers.rs:1248-1250` | `github_token_warning_from_env()` call | **S** — move behind provider abstraction |

### Git-Server-Agnostic Core (already clean)

| Component | Notes |
|---|---|
| Worktree management (`worktree.rs`, `worktree_cleanup.rs`) | Pure `git` CLI |
| Branch operations (`agent_ops.rs`, `deployments.rs`) | Pure `git` CLI |
| Bootstrap (`agent_resolve.rs`, `fleet_normalize.rs`) | Pure `git` CLI |
| Env security (`agent.rs` SENSITIVE_ENV_KEYS) | Already lists both `GITHUB_TOKEN` + `GITLAB_TOKEN` |
| All other daemon, MCP, TUI, channel code | No forge assumptions |

### Cosmetic / Metadata (no abstraction needed)

| Item | Reason |
|---|---|
| `tray/autostart/macos.rs` reverse-DNS label | App identity, not API |
| `channel/caps.rs` "GitHub-flavoured markdown" comment | Markdown spec name |
| `mcp_config.rs` reference URL comment | Source link |
| `Cargo.toml` repository field | Cargo metadata |
| `CHANGELOG.md` compare URLs | Changelog convention |
| `.github/workflows/ci.yml` | Expected; replaced per-provider |
| `.github/workflows/release.yml` (lines 153-165) | Build infra; downloads linuxdeploy from GitHub Releases |
| `docs/FLEET-DEV-PROTOCOL-v1.md` `gh` CLI examples | Documentation |
| `docs/PLAN-channel-abstraction.md:244` serenity ref | External crate link |
| `docs/PLAN-channel-ux-layer.md:197` self-ref | Link to own repo |
| `tests/fixtures/state-replay/codex-*.raw:7` (×4) | Codex update banner in PTY replay snapshots |
| `docs/archived/HANDOVER-windows-conpty-nested.md` | Archived external issue refs |

---

## 5. Summary

**Abstraction line one-liner**: The only runtime GitHub dependency is `src/daemon/ci_watch.rs` (CI polling + PR terminal check); everything else is either pure git CLI, cosmetic references, or metadata.

**Key numbers**:
- **1 module** with deep GitHub API coupling (`ci_watch.rs`, ~600 lines production + ~600 lines tests)
- **2 files** with shallow GitHub references in tool schema/handler (`tools.rs`, `handlers.rs`)
- **13 files** with cosmetic/metadata GitHub URLs (no runtime impact): release workflow (4 hits), docs refs (2), test fixtures (4), archived docs, Cargo.toml, CHANGELOG.md
- **8+ files** already git-server-agnostic (worktree, branch, bootstrap operations)
- **0** third-party GitHub crate dependencies (no octocrab)

**Recommended next step** (out of scope for this PR): Design a `CiProvider` trait or delegate-to-CLI strategy per task `t-20260424015240518790-0` options A/B/C.
