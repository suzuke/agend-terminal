<!-- Thanks for contributing! Please complete the checklist below. PRs that
duplicate an already-merged change, or re-implement an already-decided approach
without advancing it, may be closed without further review. -->

## What & why

<!-- One or two sentences: what this changes and the problem it solves.
Link the issue it closes, e.g. `Closes #123`. -->

## Prior art check (required)

- [ ] I checked [docs/KNOWN_ISSUES.md](https://github.com/suzuke/agend-terminal/blob/main/docs/KNOWN_ISSUES.md) — this isn't an item listed there as intentionally deferred.
- [ ] I compared this change against the **current `main`** branch and
      confirmed it isn't already implemented.
- [ ] I searched **closed** issues and PRs for prior attempts or decisions in
      this area, and linked any I found below.
- [ ] If this revisits an existing decision, I explain below what new evidence
      or change warrants advancing it (rather than re-proposing the same thing).

## How it was verified

<!-- Tests added/updated, manual steps, or commands run
(e.g. `cargo test`, `cargo fmt --check`, `cargo clippy`). State what you RAN and
its result — "tests pass" with no command is a claim, not evidence. -->

## Compatibility (on-disk formats)

<!-- If this PR changes any on-disk format the daemon reads or writes (files under
$AGEND_HOME, or files written into agent working directories), check it against
[docs/COMPATIBILITY.md](https://github.com/suzuke/agend-terminal/blob/main/docs/COMPATIBILITY.md).
Tier (a) hand-edited interfaces (e.g. `fleet.yaml`) and tier (b) persisted state
(inbox, task events, the sidecar stores) are ADDITIVE-ONLY until a migration
framework exists; tier (c) (caches, locks, transcripts) may change freely. -->

- [ ] No tier (a)/(b) on-disk format is changed, **or** the change is additive-only — a new optional field with a serde default; no existing field renamed, retyped, repurposed, or removed.
- [ ] If it IS a breaking format change: it bumps the relevant `schema_version`, ships a migration (or an explicit refuse-with-instructions), and is called out above.

## Notes for reviewers

<!-- Anything reviewers should know: trade-offs, follow-ups, out-of-scope. -->
