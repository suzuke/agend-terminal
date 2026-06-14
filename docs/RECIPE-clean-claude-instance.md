[繁體中文](RECIPE-clean-claude-instance.zh-TW.md)

# Recipe: spawn a Claude Code instance without inherited context

Sometimes you want a Claude Code instance that does **not** read the operator's
global `~/.claude/CLAUDE.md` instructions or any accumulated auto-memory — for
example to A/B compare agent behavior, run an isolated experiment, or hand
someone else a clean instance without leaking your personal preferences.

This recipe lists what Claude Code actually inherits across sessions and the
official knobs to opt out.

## What gets inherited

| Source | Scope | Default |
|--------|-------|---------|
| `~/.claude/CLAUDE.md` | All sessions on the machine | Always loaded |
| `~/.claude/projects/<pwd-slug>/memory/MEMORY.md` (+ files in `memory/`) | Per working directory | Loaded if present |
| `<pwd>/CLAUDE.md` and `<pwd>/.claude/CLAUDE.md` | Per project | Loaded if present |

agend-terminal already gives every managed instance its own working directory
under `~/.agend-terminal/workspace/<instance-name>/` (or a dedicated worktree
under `~/.agend-terminal/worktrees/<…>/` when a branch is assigned). Because the
auto-memory path is derived from the pwd slug, **each instance automatically
gets its own empty auto-memory directory** — nothing leaks between instances on
that axis.

The thing that *does* leak across instances is the global `~/.claude/CLAUDE.md`
and any auto-memory written under the instance's own pwd slug on prior runs.

## Opt-out knobs

### 1. Disable auto-memory loading (official, recommended)

Either of:

- Env var: `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`
- Settings file: `"autoMemoryEnabled": false` in `<workspace>/.claude/settings.json`

Disables the entire auto-memory system — both the load at session start *and*
the write side, so the instance also won't append new memory files. Existing
memory files on disk are not deleted, just not read or written.

### 2. Exclude the global CLAUDE.md (settings-based, partial)

In `<workspace>/.claude/settings.json`:

```json
{
  "autoMemoryEnabled": false,
  "claudeMdExcludes": ["~/.claude/CLAUDE.md"]
}
```

There is no first-class `--no-user-claude-md` flag in Claude Code today;
`claudeMdExcludes` with the explicit path is the documented escape hatch.

### 3. Isolate via `HOME` (most thorough, most disruptive)

Launch the instance under a throwaway `HOME`:

```bash
HOME=/tmp/clean-claude-home claude
```

Nothing under your real `~/.claude/` is read. Cost: you have to re-provision
every Claude Code setting (MCP servers, auth, themes) inside the throwaway home
before the instance is useful. Generally only worth it for security-sensitive
testing or when verifying default behavior from scratch.

## Applying it inside agend-terminal

`create_instance` does not currently inject environment variables, but it does
provision a per-instance workspace at `~/.agend-terminal/workspace/<name>/`. The
practical recipe:

1. Pick the instance name you intend to spawn — say, `clean-agent`.
2. Pre-create the workspace and drop a settings file:
   ```bash
   mkdir -p ~/.agend-terminal/workspace/clean-agent/.claude
   cat > ~/.agend-terminal/workspace/clean-agent/.claude/settings.json <<'JSON'
   {
     "autoMemoryEnabled": false,
     "claudeMdExcludes": ["~/.claude/CLAUDE.md"]
   }
   JSON
   ```
3. Spawn normally with `create_instance(name="clean-agent", backend="claude")`.
   Claude Code will read the workspace-local `settings.json` when it boots and
   skip both the global instructions and any auto-memory load.

If you need full `HOME` isolation, that has to happen at the agend-terminal
launch level today (set `HOME` before `agend-terminal start`) — there is no
per-instance `env` injection in `create_instance`.

## What this recipe is *not*

- It does not remove the agend-terminal fleet protocol the daemon injects into
  each instance's system prompt. That comes from the daemon, not from
  `~/.claude/`.
- It does not stop MCP servers from being attached — those are configured at
  the workspace `mcp-config.json` level, not by CLAUDE.md.
- It does not retroactively wipe existing auto-memory; if you want that, delete
  `~/.claude/projects/<pwd-slug>/memory/` for the relevant slug.