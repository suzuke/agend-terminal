# agend-terminal — Claude working notes

## Rust workflow

Before committing any Rust change, **always** run:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
```

CI runs these in the first two steps of `ci.yml`. Skipping them locally means
the next push fails and needs an extra "fix fmt / fix clippy" round trip.

A pre-commit hook at `.git/hooks/pre-commit` auto-formats staged `.rs` files
and re-stages them. It does NOT run clippy — clippy is too slow for a
pre-commit path. Run clippy yourself before `git push`.

A pre-push hook verifies push claims (e.g. "no other changes", "deps
unchanged") against the actual diff. Override with `git push --no-verify`
in emergencies.

A post-merge hook triggers a background `cargo build --release` when `src/`
files change. Desktop notification on completion. Does not auto-restart the
daemon — operator decides restart timing. Disable with
`git config core.hooksPath /dev/null`.

### If the hook isn't installed

Hooks are per-clone. After a fresh `git clone`, install with:

```bash
scripts/install-hooks.sh
```

## Worktree branch policy

When creating a worktree, **always use `-b <dedicated-branch>`** to create a
fresh branch. **Never** check out `main` (or any other long-lived shared
branch) directly into a worktree:

```bash
# Good — dedicated branch
git worktree add ../path -b feat/short-name origin/main

# Bad — locks main checkout, blocks operator/CI build
git worktree add ../path main
```

A worktree that holds `main` makes the canonical repo go `detached HEAD` (or
errors on `git switch main`) and breaks `cargo build --release` for operator
and tooling. The fix is `cd <worktree> && git switch -c <dedicated-branch>`,
but it is far cheaper to never take main in the first place.

This applies to fleet agents (lead/dev/reviewer) and to the MCP `repo` tool —
when calling `mcp__agend-terminal__repo action=checkout`, pass a non-main
`branch` argument (the daemon honours it verbatim and will not derive a fresh
branch for you).

## Release

Tags matching `v*` trigger `.github/workflows/release.yml`, which builds 5
targets (macOS x64/arm64, Linux x64/arm64, Windows x64) and uploads tarballs
+ `SHA256SUMS` to the GitHub release.

Before tagging: confirm the latest `ci.yml` run on `main` is green.
