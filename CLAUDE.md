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

### If the hook isn't installed

Hooks are per-clone. After a fresh `git clone`, install with:

```bash
scripts/install-hooks.sh
```

## Release

Tags matching `v*` trigger `.github/workflows/release.yml`, which builds 5
targets (macOS x64/arm64, Linux x64/arm64, Windows x64) and uploads tarballs
+ `SHA256SUMS` to the GitHub release.

Before tagging: confirm the latest `ci.yml` run on `main` is green.
