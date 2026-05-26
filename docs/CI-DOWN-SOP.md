# CI-Down SOP: GitHub Actions Degradation Response

Standard operating procedure when GitHub Actions is degraded or experiencing an outage.

## 1. Trigger Conditions

Both conditions must be met before activating this SOP:

- **A.** Same repo has **>=2 PRs** with no workflow run **10 minutes** after push
- **B.** [githubstatus.com](https://www.githubstatus.com) shows Actions **degraded** or **outage**

If only one condition is met, wait and re-check before escalating.

## 2. Local Test Gate

Run all three commands in the PR's worktree. All must pass.

```bash
cargo test --all
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

**Note:** This covers compilation, unit/integration tests, lint, and formatting. Cross-platform verification (Windows/Linux) is deferred to post-recovery.

## 3. PR Comment Template

After local verification passes, post this comment on the PR:

```
## Local CI Verification (GitHub Actions degraded)

- [x] `cargo test --all` — passed (N tests)
- [x] `cargo clippy --all-targets -- -D warnings` — clean
- [x] `cargo fmt --check` — clean
- [ ] Cross-platform (deferred to post-recovery)

GitHub Status: [degraded since HH:MM UTC](https://githubstatus.com)
Verified by: @agent-name
```

Replace `N tests`, `HH:MM UTC`, and `@agent-name` with actual values.

## 4. Merge Flow

1. Agent runs the 3 local test gate commands, posts PR comment using the template above
2. Lead confirms review is complete + local verification posted → `gh pr merge --admin`
3. After Actions recovery, main branch CI backfills verification automatically

## 5. Post-Recovery

No special action needed. The main branch CI pipeline runs on every push to main, so merged PRs are verified automatically once Actions recovers.
