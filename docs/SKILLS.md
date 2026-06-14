[繁體中文](SKILLS.zh-TW.md)

# Skills

Unified community-skill discovery for the five backends agend-terminal supports (Claude Code, Codex, Gemini CLI, OpenCode, Kiro CLI).

A skill is a directory (typically containing `SKILL.md` plus supporting files) that an agent's backend picks up at launch. agend-terminal stores **one copy** of each skill under `~/.agend-terminal/skills/` and lets every backend discover it through its conventional sub-directory — usually via a symlink, with a copy fallback on Windows.

## Why

Different backends each have their own skill-discovery path:

| Backend | Discovery path inside working directory |
|---------|------------------------------------------|
| Claude  | `.claude/skills/`                        |
| Codex   | `.codex/skills/`                         |
| Gemini  | `.gemini/skills/`                        |
| OpenCode| `.opencode/skills/`                      |
| Kiro    | `.kiro/skills/`                          |

Without agend-terminal you would copy every skill into every backend's directory for every agent. agend-terminal stores one canonical source and surfaces it to each backend automatically.

## Architecture

```
~/.agend-terminal/skills/                ← single source of truth
  ├── skill-forge/
  │   └── SKILL.md
  ├── opencli-adapter-author/
  │   └── SKILL.md
  └── ...

agent working directory/
  ├── .claude/skills/   → symlink → ~/.agend-terminal/skills/
  ├── .codex/skills/    → symlink → ~/.agend-terminal/skills/
  ├── .gemini/skills/   → symlink → ~/.agend-terminal/skills/
  ├── .opencode/skills/ → symlink → ~/.agend-terminal/skills/
  └── .kiro/skills/     → symlink → ~/.agend-terminal/skills/
```

On Unix the per-backend entries are symlinks (zero maintenance). On Windows agend-terminal falls back to copying the files; re-running `install` replaces managed targets.

State files:

- `~/.agend-terminal/skills/<name>/` — the canonical skill content
- `~/.agend-terminal/skills-lock.json` — per-skill source + pinned version (commit SHA for git, mtime for local paths) + install timestamp
- `~/.agend-terminal/.skills-stage/<digest>/` — short-lived staged copies used when a particular agent only wants a subset of skills (see fleet.yaml integration below). GC'd after 7 days.

## CLI

All commands run as `agend-terminal skills <subcommand>`.

### Add

```
agend-terminal skills add <source>
```

`<source>` is either a local path or a git URL (`https://…`, `git@…`, `ssh://…`, anything ending in `.git`). The skill directory name is taken from the source basename, so `git clone … repo-foo` becomes `~/.agend-terminal/skills/repo-foo/`.

- Local path: copied recursively into the canonical source root.
- Git URL: `git clone --depth=1` into the canonical source root; the pinned version is the resulting HEAD SHA.

Re-adding an existing name overwrites in place and updates the lock entry; if you want to refresh from the original source use `update` instead.

### Remove

```
agend-terminal skills remove <name>
```

Deletes `~/.agend-terminal/skills/<name>/` and clears its lock entry. Idempotent — running against a name that does not exist is a no-op.

### List

```
agend-terminal skills list
```

Prints every directory under `~/.agend-terminal/skills/` together with the recorded source and pinned version (`(unrecorded)` / `(unpinned)` if missing).

### Update

```
agend-terminal skills update          # update every skill with a recorded source
agend-terminal skills update <name>   # update just one
```

Replays `add` against the source stored in `skills-lock.json`. Skills that were added before the lock existed (or imported manually) need to be re-added; `update` surfaces a clear error in that case.

### Install (manual)

```
agend-terminal skills install <working_dir>
```

Creates the five per-backend sub-directories under `<working_dir>` and points each at `~/.agend-terminal/skills/` (symlink, or copy on Windows). Used when you want to make skills visible inside a directory that the daemon did not spawn — the daemon performs the same install automatically for managed agents (see below).

## Daemon integration

When the daemon launches an agent it calls `skills::install_for_agent` against the agent's working directory. The result is identical to running `skills install <working_dir>` by hand, with one extra capability: per-instance filtering.

### Per-instance allowlist via fleet.yaml

```yaml
instances:
  reviewer:
    backend: claude
    skills:
      - skill-forge
      - code-review-expert
```

Behaviour:

- `skills:` omitted (default) — every skill under `~/.agend-terminal/skills/` is exposed to the agent.
- `skills: [name1, name2]` — only the named skills are staged into a temporary digest directory under `~/.agend-terminal/.skills-stage/<digest>/`, and that staged directory is symlinked into the backend paths. The agent sees only those skills.
- `skills: []` — explicit opt-out: the per-backend directories are created but contain no skills (only the daemon's `.agend-skills-managed` marker).
- Names that don't exist in the canonical source are skipped with a warning; the agent still launches.

The staged copies have stable per-allowlist names (SHA-256 prefix of the sorted allowlist). Multiple agents asking for different subsets coexist without collision, and the daemon GCs stages older than seven days at startup.

## Smoke test

```
cargo test skills::
```

Runs the 24 unit/integration tests in `src/skills.rs::tests` — covers add/remove/list/install/update, the skills-lock round-trip, SHA-256 staging digest, and the stage GC including TOCTOU same-run exclusion. All pass on a clean checkout.

Other useful one-shot checks:

```
agend-terminal skills list                            # canonical source inventory
agend-terminal skills install /tmp/scratch-agent      # exercises the symlink/copy path
cat ~/.agend-terminal/skills-lock.json                # inspect pinned versions
```

## Examples

Install a community skill from GitHub:

```
agend-terminal skills add https://github.com/mattpocock/skills.git
agend-terminal skills list
```

Pin a local skill you are iterating on:

```
agend-terminal skills add ~/projects/my-skill
# … edit files …
agend-terminal skills update my-skill   # re-runs add → updates the mtime version
```

Restrict an agent to a subset:

```yaml
# fleet.yaml
instances:
  doc-writer:
    backend: claude
    skills: [writing-style-guide, markdown-linter]
```

After `agend-terminal start`, `~/.agend-terminal/workspace/doc-writer/.claude/skills/` resolves to a stage containing only those two skills.

## Troubleshooting

| Symptom | Likely cause |
|---------|--------------|
| `no skills installed under …` from `list` | Nothing added yet; `add` first. |
| `git clone failed for <url>` | git is unavailable or the URL needs auth; clone manually then `add` the local path. |
| Backend ignores a newly added skill | The agent process is still running with its old launch state; restart the instance so the daemon re-runs `install_for_agent`. |
| Backend directory exists but is empty | An older non-daemon-managed directory is at the path. agend-terminal refuses to touch directories without its `.agend-skills-managed` marker — move or delete it, then re-install. |
| Skill is unexpectedly absent for one instance | Check `fleet.yaml` — that instance may have an explicit `skills:` allowlist that excludes it. |
| Disk space grows under `.skills-stage/` | Stages GC at daemon start after seven days; force a sweep by restarting the daemon. |

## Provenance

The skills feature shipped across Sprints 60–62:

- #585 — auto-install at agent launch (Sprint 61 W1 PR-1)
- #586 — `fleet.yaml` per-instance allowlist (Sprint 61 W1 PR-2)
- #590 — SHA-256 prefix staging digest (Sprint 62 W1 PR-1)
- #591 — stage GC with TOCTOU same-run exclusion (Sprint 62 W1 PR-2)

Implementation lives in `src/skills.rs` (single module, ~650 LOC + 24 tests). The CLI surface is in `src/cli.rs` under the `Sprint 60 W2 PR-1 — agend skills CLI subcommands` heading.