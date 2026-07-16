[繁體中文](CI-DOWN-SOP.zh-TW.md)

# CI-Down SOP: GitHub Actions Degradation Response

Standard operating procedure when GitHub Actions is degraded or experiencing an outage.

## 1. Trigger Conditions

Both conditions must be met before activating this SOP:

- **A.** Same repo has **>=2 PRs** with no workflow run **10 minutes** after push
- **B.** [githubstatus.com](https://www.githubstatus.com) shows Actions **degraded** or **outage**

If only one condition is met, wait and re-check before escalating.

## 2. Merge Freeze and Local Diagnostics

While this SOP is active, **do not merge affected PRs**. A local pass is useful
diagnostic evidence, but it is not CI evidence and never authorizes
`--admin`, `--force`, or any other merge-gate bypass.

Run all three commands in the PR's worktree. All must pass.

```bash
cargo test --all
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

**Note:** This covers compilation, unit/integration tests, lint, and formatting
on the local platform only. It does not replace the repository's hosted,
cross-platform checks.

## 3. PR Comment Template

After local verification passes, post this comment on the PR:

```
## Local CI Verification (GitHub Actions degraded)

- [x] `cargo test --all` — passed (N tests)
- [x] `cargo clippy --all-targets -- -D warnings` — clean
- [x] `cargo fmt --check` — clean
- [ ] Hosted cross-platform CI — blocked by the current Actions incident

GitHub Status: [degraded since HH:MM UTC](https://githubstatus.com)
Verified by: @agent-name

Merge status: FROZEN until CI runs on this PR head and all required checks pass.
```

Replace `N tests`, `HH:MM UTC`, and `@agent-name` with actual values.

## 4. Merge Flow

1. Record the affected PR, branch, and immutable PR head SHA; keep the PR open.
2. Run the three local diagnostics and post the result using the template above.
3. Keep the merge frozen while Actions is degraded. Local green and completed
   review do not relax the CI gate.
4. After recovery, ensure Actions runs against the same PR head. Re-arm the
   branch's `ci` watch if necessary; if no workflow run was created, trigger the
   workflow for the unchanged PR branch rather than merging first.
5. Independently run `gh pr checks <PR#>`. Merge is eligible only when it exits
   0 with every required check successful and the review/verdict gates also hold.

## 5. Post-Recovery

Do not treat a later `main` run as backfilled evidence for an unverified PR.
Resume each frozen PR from its recorded head SHA, obtain that PR's complete CI
result, and then follow the normal merge procedure. If the head changed during
the outage, the previous review is stale and must be refreshed for the new head.
