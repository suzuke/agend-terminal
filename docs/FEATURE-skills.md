# Skills — Cross-Backend Skill System

The Skills system lets you install reusable skills (prompts, tool definitions, reference files) once and have them automatically available to agents on every backend. No need to configure each backend separately.

## Usage Scenarios

> **Target audience:** Both operators and agents.

**Operator installs a code review skill.** You find a community-maintained code review skill on GitHub. You run `agend-terminal skills add https://github.com/user/code-review-expert.git` once, and every agent in your fleet — regardless of whether it runs Claude, Gemini, or Kiro — can now invoke the skill. No per-backend configuration needed.

**Agent loads skills at startup.** When the daemon spawns a dev agent, the skills system has already created symlinks in the agent's working directory. The agent's backend reads `SKILL.md` from its conventional path and gains the skill's capabilities — prompt templates, behavioral guidelines, or reference material — without any explicit loading step.

**Per-agent filtering.** Your reviewer should only have access to review-related skills, not deployment or refactoring skills. You add `skills: [code-review-expert]` to the reviewer's fleet.yaml entry. The daemon creates a filtered stage directory so the reviewer only sees what it needs.

## Design Philosophy

Different AI backends (Claude, Codex, Gemini, OpenCode, Kiro) each have their own skill directory conventions (`.claude/skills/`, `.codex/skills/`, etc.). Manually maintaining identical skill files across every directory is tedious and error-prone.

The Skills system provides a single source directory `~/.agend-terminal/skills/` and automatically maps it to each backend's conventional path via symlinks. Install once, effective across all five backends.

---

## Quick Start

```bash
# Install a skill from GitHub
agend-terminal skills add https://github.com/user/my-skill.git

# Install from a local directory
agend-terminal skills add ~/projects/my-skill

# List installed skills
agend-terminal skills list

# Update a skill
agend-terminal skills update my-skill

# Remove a skill
agend-terminal skills remove my-skill
```

Skills take effect the next time an agent is launched.

---

## Directory Structure

### Unified Source

All skills are stored under a single directory:

```
~/.agend-terminal/
├── skills/                    <- Unified skill source
│   ├── skill-forge/
│   │   ├── SKILL.md          <- Skill description file (required)
│   │   └── [supporting files]
│   ├── code-review-expert/
│   │   └── SKILL.md
│   └── ...
└── skills-lock.json           <- Version lock record
```

### Per-Agent Mapping

When the daemon starts an agent, it automatically creates symlinks in the agent's working directory:

```
<agent-working-dir>/
├── .claude/skills/   -> ~/.agend-terminal/skills/
├── .codex/skills/    -> ~/.agend-terminal/skills/
├── .gemini/skills/   -> ~/.agend-terminal/skills/
├── .opencode/skills/ -> ~/.agend-terminal/skills/
└── .kiro/skills/     -> ~/.agend-terminal/skills/
```

Each backend reads `SKILL.md` from its own conventional path, all pointing to the same source.

---

## CLI Commands

### `skills add <source>`

Install a skill from a git repo or local path.

```bash
# Git source (auto shallow clone)
agend-terminal skills add https://github.com/user/skill-forge.git
agend-terminal skills add git@github.com:user/skill-forge.git

# Local path (full copy)
agend-terminal skills add /path/to/my-skill
agend-terminal skills add ./relative/path
```

Source type is auto-detected: URLs or `.git` suffixes are treated as git; everything else as a local path.

After installation, `skills-lock.json` records the source and version (git SHA or file modification time) for subsequent `update` use.

Re-installing a skill with the same name overwrites it.

### `skills list`

List all installed skills with source and version information.

```bash
agend-terminal skills list
```

Example output:

```
skill-forge
  source: https://github.com/user/skill-forge.git
  version: abc123d
  installed_at: 2026-05-16T10:00:00Z

code-review-expert
  source: /Users/suzuke/projects/code-review
  version: 1747402800
  installed_at: 2026-05-20T08:30:00Z
```

### `skills update [<name>]`

Re-fetch the latest version from the original source.

```bash
# Update a single skill
agend-terminal skills update skill-forge

# Update all skills
agend-terminal skills update
```

Git sources are re-cloned for the latest commit; local paths are re-copied. The version lock updates automatically.

### `skills remove <name>`

Remove a skill and its lock record.

```bash
agend-terminal skills remove skill-forge
```

Removed skills are no longer visible on next agent launch. The operation is idempotent — removing a non-existent skill does not error.

### `skills install <working_dir>`

Manually install skills to a specific working directory. Normally done automatically by the daemon; this command is for debugging or one-off setup.

```bash
agend-terminal skills install /tmp/test-agent-wd
```

---

## Writing Skills

A skill is simply a directory containing a `SKILL.md` file. `SKILL.md` is the entry point that backends read, formatted as Markdown.

### Minimal Structure

```
my-skill/
└── SKILL.md
```

### With Supporting Files

```
my-skill/
├── SKILL.md           <- Main description file
├── templates/
│   └── review.md      <- Template file
└── examples/
    └── usage.py       <- Example code
```

The contents of `SKILL.md` are parsed according to backend-specific rules. For Claude, `SKILL.md` typically contains the skill description, trigger conditions, and prompt content. AgEnD Terminal itself does not parse `SKILL.md` — it only delivers the directory to the right location.

---

## Per-Agent Skill Filtering

Use the `skills` field in `fleet.yaml` to control which skills each agent can see.

### Default: Install All Skills

```yaml
instances:
  dev:
    backend: claude
    # No skills field -> all skills installed
```

### Install Only Specific Skills

```yaml
instances:
  reviewer:
    backend: claude
    skills:
      - code-review-expert
      - skill-forge
```

The reviewer only sees `code-review-expert` and `skill-forge`; other skills are invisible to it.

### Install No Skills

```yaml
instances:
  eval-runner:
    backend: gemini
    skills: []    # Empty array = no skills installed
```

### How Filtering Works

When a `skills` allowlist is specified:

1. The system computes a SHA-256 digest of the allowlist
2. Creates a filtered copy in `~/.agend-terminal/.skills-stage/<digest>/`
3. The agent's symlinks point to the filtered stage directory instead of the unified source

Stage directories are automatically cleaned up after 7 days. Identical allowlists reuse the same stage directory.

---

## Automatic Installation Timing

The daemon automatically installs skills for agents at the following points:

| Timing | Description |
|--------|-------------|
| Cold start | Before spawning each agent during daemon startup |
| Crash restart | Before respawning after an agent crash |
| Stage 2 restart | During the clean restart flow |
| TUI new agent | When adding an agent via `Ctrl+B c` or command palette |

Installation is synchronous — `SKILL.md` is in place before the agent starts.

---

## Installation Modes

### Symlink (Default)

On Unix systems, symlinks are used for zero-copy, instant reflection of source changes.

### Copy + Marker (Fallback)

On Windows or when symlinks are unavailable, the full directory is copied and a `.agend-skills-managed` marker file is written.

Marker file purpose:
- Marker present -> daemon-managed copy; safe to overwrite on update
- No marker -> user-created skill directory; daemon will not overwrite

---

## Version Locking

`skills-lock.json` records installation information for each skill:

```json
{
  "skills": {
    "skill-forge": {
      "source": "https://github.com/user/skill-forge.git",
      "version": "abc123def456...",
      "installed_at": "2026-05-16T10:00:00Z"
    }
  }
}
```

- **source**: Original source (`update` pulls from here)
- **version**: Git commit SHA or file modification timestamp
- **installed_at**: Installation time

Writes use atomic write to prevent corruption from crashes.

---

## Troubleshooting

### Skills not taking effect

1. Confirm the skill directory contains a `SKILL.md`
2. Use `agend-terminal skills list` to verify the skill is installed
3. Check symlinks in the agent's working directory:
   ```bash
   ls -la <agent-wd>/.claude/skills/
   ```
4. If fleet.yaml has a `skills:` allowlist, confirm the skill name is on the list

### Manually created skill directories are skipped

This is expected behavior. If you manually created `.claude/skills/` in an agent's working directory, the daemon won't overwrite it. To switch to daemon-managed, delete the directory manually; the daemon will recreate the symlink on next startup.

### Symlink fails on Windows

Windows requires Developer Mode or admin privileges to create symlinks by default. The system automatically falls back to copy mode. To enable symlinks:

1. Open Settings -> Developer Options -> Developer Mode
2. Or run as administrator

### Skills not updated after running update

In symlink mode, changes are reflected instantly. In copy mode, re-run `agend-terminal skills update` or restart the daemon.
