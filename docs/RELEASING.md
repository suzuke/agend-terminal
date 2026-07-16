[繁體中文](RELEASING.zh-TW.md)

# Releasing agend-terminal

Audience: any maintainer cutting a release. The pipeline is fully automated
after the tag push — your job is the four steps below, in order. Nothing here
relies on tribal knowledge; if a step is unclear, fix this document in the
same PR that confused you.

## TL;DR

```sh
# on an up-to-date main checkout
# 1. bump version
$EDITOR Cargo.toml          # version = "X.Y.Z"
cargo update -w             # refresh Cargo.lock for the new version

# 2. changelog
$EDITOR CHANGELOG.md        # move [Unreleased] items into a new "## [X.Y.Z] — YYYY-MM-DD"
                            # section AND add the compare link at the bottom:
                            #   [X.Y.Z]: .../compare/vPREV...vX.Y.Z
                            #   point [Unreleased] at vX.Y.Z...HEAD

# 3. land the bump via the normal PR flow, then tag the merge commit
git tag -a vX.Y.Z -m "vX.Y.Z"   # ANNOTATED tag — not lightweight
git push origin vX.Y.Z

# 4. watch the Release workflow — everything below is automatic
```

## What the tag push triggers (`.github/workflows/release.yml`)

```
gate ──► build (5 targets) ──┐
     └─► appimage ───────────┴─► release (GH Release + SHA256SUMS) ──► publish (crates.io)
```

1. **gate** — fails the release before any artifact is built when:
   - `Cargo.toml version` ≠ tag (strip the `v`),
   - `CHANGELOG.md` has no `## [X.Y.Z]` section,
   - the crate no longer compiles on the declared MSRV (`rust-version = "1.88"`).
   - `cargo-semver-checks` also runs here but is **soft-fail while pre-1.0**:
     it prints a breaking-change report for human judgment without blocking.
     Promote it to hard-fail when 1.0 ships.
2. **build / appimage** — unchanged artifact matrix (5 targets + AppImage).
3. **release** — GitHub Release with `generate_release_notes` + `SHA256SUMS`.
4. **publish** — `cargo publish --dry-run` then `cargo publish` to crates.io.
   - Uses the `CRATES_IO_TOKEN` repository secret. If the secret is not
     configured, the job **skips gracefully** (green, with a warning in the
     job summary) — it never reddens a release. To enable publishing:
     crates.io → Account Settings → API Tokens (scope: `publish-update`,
     restricted to this crate), then `Settings → Secrets and variables →
     Actions → New repository secret → CRATES_IO_TOKEN`.
   - **Never runs for pre-release tags** (any tag containing `-`).
   - `cargo publish` fails if the version already exists on crates.io.
     That's expected protection, not a bug: the job only runs on a freshly
     pushed tag, and the gate already proved the tag matches `Cargo.toml` —
     hitting it means the version was published out-of-band (re-run the
     release with the next patch version instead).

## Pre-releases

Tag as `vX.Y.Z-rc.N` (annotated, same as releases). The pipeline runs gate →
build → GitHub Release, but **skips crates.io publish** (the publish job's
`if` excludes tags containing `-`). The gate's changelog check looks for the
base `## [X.Y.Z]` section, so draft the release notes before the first rc.
Mark the GitHub Release as a pre-release manually if you want it flagged on
the releases page.

## Tag hygiene

- Always **annotated** tags (`git tag -a`). Historical note: v0.5.0–v0.7.0
  were created lightweight; from v0.7.1 on, annotated is the rule (annotated
  tags carry tagger/date metadata and are what `git describe` prefers).
- Tag the **merge commit on `main`** that contains the version bump — never
  a branch head. The gate enforces version/changelog consistency either way.

## Validating workflow changes without a tag

`workflow_dispatch` runs the same pipeline against `main`: the gate's
tag-coupled checks (version==tag, changelog) self-skip, MSRV + semver-checks
still run, artifacts are built and uploaded, and the `release`/`publish`
jobs stay off (tag-ref guarded). Use this before merging release.yml edits.

## Yanking a bad release

A yank hides the version from new `cargo install`/dependency resolution
without deleting it (existing lockfiles keep working):

```sh
cargo yank --version X.Y.Z            # needs a token with yank scope
cargo yank --version X.Y.Z --undo     # if yanked by mistake
```

Also edit the GitHub Release: mark it as a pre-release or add a warning to
the notes pointing at the fixed version. Then ship the fix as a new patch
release through the normal flow — never reuse or move a published tag.

## Toolchain policy (MSRV floor vs CI Check)

Two different pins, on purpose (#1994 / #2339 / #2340):

| Role | Toolchain | Where | Purpose |
|------|-----------|--------|---------|
| **MSRV floor** | **1.88** (declared) | `Cargo.toml` `rust-version`, `ci.yml` job `MSRV check (1.88)`, `release.yml` gate | Anyone on rustc ≥ 1.88 can `cargo install` / compile the locked tree. Blocks Dependabot from silently raising the floor (the sysinfo 0.39 → rustc 1.95 class). |
| **CI Check** | **current stable** (floating) | `ci.yml` `check` matrix (`dtolnay/rust-toolchain@stable` + fmt/clippy/test) | Catch new clippy lints and compiler behavior. **Not** pinned to 1.88. |

Do **not** bump `rust-version` just because stable advanced (e.g. 1.96 / 1.97).
Clippy denials on a new stable are fixed with a small mechanical PR on `main`
while leaving MSRV at 1.88. Raise MSRV only when a dependency **must** move
and no 1.88-compatible pin exists — see below.

## MSRV bumps

`rust-version` in `Cargo.toml` is the source of truth; the gate's
`cargo +1.88 check` pin must be updated in the same PR that bumps it
(grep `ci.yml` and `release.yml` for `1.88`). Treat an MSRV bump as a
minor-version event and call it out in the changelog. Prefer a still-
conservative new floor over tracking the latest stable.

## Release smoke test (target: 30 minutes)

Run this against the exact release commit or its CI artifact before tagging.

### Preflight

- [ ] Stop any daemon left by an earlier session and work from the repository root.
- [ ] Build with `cargo build --release`; confirm `agend-terminal doctor` exits 0.
- [ ] Confirm credentials for every backend under test; set
  `AGEND_TELEGRAM_BOT_TOKEN` when testing Telegram.

### Backend matrix

For every installed backend, spawn it, send `echo hello`, exercise one tool
call such as `list files in /tmp`, exit normally, and confirm no orphan process
remains. Record skipped backends in the sign-off.

| Backend | Ready evidence | Normal exit | Extra check |
|---|---|---|---|
| Claude Code (`claude`) | `❯` or `bypass permissions` within 30 s | `/exit` | `admin cleanup-branches` preview exits 0 and deletes nothing |
| Kiro CLI (`kiro-cli`) | `Trust All Tools active` or `ask a question` within 30 s | `/quit` | trust dialog is dismissed |
| Codex (`codex`) | `OpenAI Codex` or `›` within 20 s | `exit` | trust-directory dialog is dismissed |
| OpenCode (`opencode`) | `Ask anything` or `tab agents` within 45 s | `/exit` | mouse wheel inside alt-screen stays with the backend (#744) |
| Agy (`agy`) | `Antigravity CLI` or `Type your message` within 20 s | `/exit` | `.agents/mcp_config.json` loads fleet MCP tools (#1547) |
| Grok (`grok`) | `Grok Build` or `❯` within 30 s | `/exit` | project-trust dialog is dismissed |

### Cross-cutting checks

- [ ] `Ctrl+B n` / `Ctrl+B p` cycles tabs, `Ctrl+B o` cycles panes, and
  `Ctrl+B d` detaches cleanly.
- [ ] Mouse wheel scrolls history in a normal, non-alt-screen pane.
- [ ] A Telegram message reaches the correct agent pane when the channel is enabled.
- [ ] A disposable `repo(action=checkout, bind=true)` followed by
  `release_worktree` leaves `binding_state` unbound and no dangling worktree.
- [ ] With `AGEND_CAPTURE_FIXTURES=1`, one backend run writes a `.cap` plus
  `.cap.meta.json`; unset the variable afterwards.

### Sign-off

Paste this in the release PR:

```text
Date: YYYY-MM-DD
Operator: <name>
agend-terminal version: <version>
OS / arch: <value>

Backend versions tested:
- claude:
- kiro-cli:
- codex:
- opencode:
- agy:
- grok:

Backends skipped (reason):
-

Known deviations / failures:
-

Overall verdict: [ ] PASS  [ ] PASS with caveats  [ ] FAIL
```

When all six pass, include `Real-backend smoke: ✓ all 6 backends` in the PR.
