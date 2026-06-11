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

## MSRV bumps

`rust-version` in `Cargo.toml` is the source of truth; the gate's
`cargo +1.88 check` pin must be updated in the same PR that bumps it
(grep release.yml for `1.88`). Treat an MSRV bump as a minor-version event
and call it out in the changelog.
