---
name: Bug Report
about: User-visible problem with witnessed evidence
title: 'bug: '
labels: ['bug']
---

## Prior art check (required)

<!-- Reports that duplicate an existing (open OR closed) issue, or re-raise an
already-resolved topic without new evidence, may be closed without further
review. A quick search saves everyone time. -->

- [ ] I checked [docs/KNOWN_ISSUES.md](https://github.com/suzuke/agend-terminal/blob/main/docs/KNOWN_ISSUES.md) — this isn't an item already listed there as intentionally deferred.
- [ ] I searched **open and closed** issues for prior reports or prior
      conclusions on this, and linked any I found below.
- [ ] This is not a re-report of an already-resolved issue — or, if it
      revisits one, I explain what new evidence or change warrants reopening it.

## Symptom

<!-- One sentence: what's user/operator-visible going wrong.
Write the problem, not the fix. -->

## Reproduction

<!-- Minimal steps to trigger. Copy-pasteable is best.
Example:
1. `agend-terminal create_instance name=foo backend=claude`
2. Wait for telegram topic (~3s)
3. Check `~/.agend-terminal/fleet.yaml` → expected topic_id=<n>, actual null

If not reliably reproducible, mark "intermittent" and describe the
trigger conditions you've observed. -->

## Expected vs Actual

<!-- Expected: …
     Actual:   … -->

## Version / Environment

<!-- - agend-terminal: `cargo pkgid` or commit SHA (`git rev-parse HEAD`)
     - daemon build: commit on the first line of daemon startup log
     - OS: macOS 14.5 / Linux Ubuntu 24.04 / Windows 11 / …
     - Backend (if relevant): claude-code 2.1.81 / codex-cli x.x /
       kiro-cli x.x / gemini-cli 0.42.0 / opencode x.x
     - Shell: zsh / bash / fish -->

## Concrete evidence

<!-- Witness timestamp (UTC), log lines, PR/commit refs, screenshots,
relevant inbox / fleet.yaml / topics.json snippets.
Keep raw output verbatim; minimize jargon when paraphrasing. -->

## Root cause (if known)

<!-- file:line where the bug lives. Skip this section if you haven't
traced it yet — operator may dispatch a root-cause investigation. -->

## When introduced (if known)

<!-- git blame or PR ref. Skip for greenfield bugs. -->

## Proposed fix (if any)

<!-- Brief sketch only. Don't pre-bake the implementation — that's
PR-time discussion. -->

## Priority hint

<!-- P1  operator-blocking / data-loss / silent-failure-class
     P2  degrades quality but workaround exists
     P3  cleanup / polish / latent risk -->
