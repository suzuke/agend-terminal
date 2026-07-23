# agentic-git

> Embedding agentic-git in your own orchestrator? Start with the [Embedder Contract v1](docs/embedder-contract-v1.md).

[![ci](https://github.com/suzuke/agend-terminal/actions/workflows/ci.yml/badge.svg)](https://github.com/suzuke/agend-terminal/actions/workflows/ci.yml)

`agentic-git` is maintained as an independent Cargo workspace inside the
[AgEnD-Terminal monorepo](https://github.com/suzuke/agend-terminal/tree/main/vendor/agentic-git).
After one monorepo release is verified, the former standalone repository will
be archived as read-only history.

**A guarded, transparent `git` for AI coding agents.**

`agentic-git` is a small Rust binary that masquerades as `git` on an agent's
`PATH`. The agent keeps speaking the git it already knows — the shim is
invisible until the moment it matters:

- **routes** every mutating command into the worktree the agent is *bound* to,
- **denies** the operations that wreck multi-agent setups (with an actionable,
  LLM-readable explanation of what to do instead),
- **recovers** — snapshots the worktree before any destructive op, so *one*
  command (`agentic-git snapshots restore`) puts back what a `reset --hard`
  or `clean -fd` just erased. Plain git has no undo for that,
- **attributes** every commit to the agent that made it, and
- lets the operator **bypass** any of it, deliberately and audited.

It was extracted from [agend-terminal](https://github.com/suzuke/agend-terminal),
where it has been running a production fleet of coding agents (Claude Code,
Codex, and friends) sharing real repositories on one machine. The full commit
history of the shim came along.

> **Honest positioning:** this is a seatbelt, not a cage. It is a same-uid
> userspace shim aimed at *semi-trusted, accident-prone* agents — a
> prompt-injected or buggy agent is stopped from trashing your checkout by
> habit or mistake; a determined adversary calling `/usr/bin/git` directly is
> not. For a hard boundary you want kernel-level isolation (containers,
> Landlock, sandbox-exec) *underneath* this.

## Why

Coding agents make mistakes at machine speed, and stock git amplifies them:

| Agent habit | Blast radius without a guard |
|---|---|
| `git reset --hard` / `clean -fd` with uncommitted work | the work is gone — plain git keeps no copy to restore from |
| `git checkout main` / `git switch <other-branch>` | tramples the branch another agent (or you) is working on |
| `git worktree add/remove` on its own | corrupts the worktree layout your orchestrator manages |
| running git *in your canonical checkout* | detaches or moves **your** HEAD (this really happened; it is why this tool exists) |
| `git push` with a force-added secret | your HMAC key / audit logs leave the machine, irreversibly |
| commits from six agents on one repo | no way to tell who did what |

`agentic-git` enforces the answers at the git layer, instead of hoping every
agent's system prompt says "please be careful".

## How it works

```
agent runs `git <args>`
        │  (PATH: <home>/bin/git → agentic-git)
        ▼
classify(argv, cwd, binding)          binding = HMAC-signed
        │                             agent → branch → worktree
        ├─ passthrough      read-only / safe → exec real git as-is
        ├─ chdir-pass       mutating + bound → exec real git -C <bound worktree>
        ├─ silent-exempt    known tool noise (e.g. gh post-merge) → exit 0
        └─ deny             exit 1 + reason + literal next step for the agent
```

- **Binding:** the orchestrator writes `runtime/<agent>/binding.json`
  (+ `.sig`, HMAC-SHA256 over an operator-owned key). Signature invalid or
  missing → the agent is *unbound* and every mutating command is denied,
  with guidance on how to get a worktree. Fail-closed.
- **Deny matrix** (the interesting cases): `git worktree *` (worktree
  lifecycle belongs to the orchestrator) · `checkout`/`switch` to a different
  or protected branch (`main`/`master` by default, extendable via
  `policy.toml`) · any mutation while unbound (including plumbing:
  `read-tree`, `update-index`, `apply`) · agent git in a canonical-rooted
  repo (protects *your* checkout) · **push ranges carrying trust-root files**
  (`.config-integrity-key`, `policy.toml`, `fleet.yaml`, audit `*.jsonl`) —
  the one place the shim blocks on *content*, because that mistake is
  irreversible.
- **Provenance:** a `prepare-commit-msg` hook (installed per-worktree by the
  orchestrator) appends `Agentic-Agent`, `Agentic-Branch`, `Agentic-Task`,
  `Agentic-Issued-At` trailers, idempotently. A `reference-transaction` hook
  journals every ref move with the agent's identity.
- **Bypass, audited:** `AGENTIC_GIT_BYPASS=1` (one-shot) ·
  `AGENTIC_GIT_BYPASS_AGENT=<name>` (per-agent) ·
  `AGENTIC_GIT_BYPASS_UNTIL=<epoch>` (time-boxed). Bypassed mutations are
  logged to the fleet event log. The deny messages themselves tell the agent
  these exist — transparency over obscurity.
- **Robustness:** recursion guard (a mis-resolved `git` can't spawn-storm
  itself), target-override stripping (a caller's own `-C`/`--git-dir` can't
  out-vote the binding), Unix `exec()` process replacement, Windows
  `status()`+exit.

## Quickstart

Install, then launch your agent inside a guarded session. **One command**
provisions a worktree, a signed binding, and the hooks — the agent just runs
the `git` it already knows:

```sh
cargo install agentic-git          # or: cargo build --release

# Launch ANY agent command in a guarded session on its own branch:
agentic-git run --agent my-agent --branch my-agent/work -- \
    claude --dangerously-skip-permissions     # …or codex, or a shell, or make
```

Inside the session the agent's `git` is the shim:

```
git status          → passthrough
git checkout main   → denied, with the reason and the way out
git reset --hard    → runs, but the worktree is snapshotted first
```

If the agent erases uncommitted work, put it back with **one command** — no
snapshot ref, no git internals (the session prints its worktree path on exit):

```sh
agentic-git snapshots restore --repo <the session's worktree>
```

See the whole thing end-to-end — an agent wipes real work, one command brings
it back — in **[`demo/recovery-demo.sh`](demo/recovery-demo.sh)**. It asserts
every step, so it also doubles as a cold-start acceptance check on your machine.

### Embedding it in your own orchestrator

Already running a fleet and want the shim *without* `run`'s provisioning? Put
the binary on the agent's `PATH` as `git` and write the binding yourself:

```sh
export AGENTIC_GIT_HOME=$HOME/.agentic-git
mkdir -p "$AGENTIC_GIT_HOME/bin"
ln -sf "$(command -v agentic-git)" "$AGENTIC_GIT_HOME/bin/git"

# In the agent's environment (NOT your own shell):
export AGENTIC_GIT_REAL_GIT="$(command -v git)"   # resolve BEFORE touching PATH
export PATH="$AGENTIC_GIT_HOME/bin:$PATH"
export AGENTIC_GIT_AGENT=my-agent
# (Order matters: `command -v git` after the PATH prepend would capture the
# shim itself. The shim detects and ignores a self-referential REAL_GIT and
# falls back to a self-excluding PATH search, but don't rely on it.)
```

agend-terminal plays this orchestrator role in the original fleet; the
`agentic-git-core` crate is the contract surface for doing the same in yours.

## Environment contract

Every variable also accepts its legacy `agend-terminal` name as a fallback,
so an existing agend fleet can adopt this binary with **zero daemon-side
changes**:

| Primary | Legacy fallback | Meaning |
|---|---|---|
| `AGENTIC_GIT_HOME` | `AGEND_HOME` | state root (bindings, hooks, event log) |
| `AGENTIC_GIT_AGENT` | `AGEND_INSTANCE_NAME` | the calling agent's identity |
| `AGENTIC_GIT_REAL_GIT` | `AGEND_REAL_GIT` | path to the real git binary |
| `AGENTIC_GIT_BYPASS` | `AGEND_GIT_BYPASS` | one-shot bypass |
| `AGENTIC_GIT_BYPASS_AGENT` | `AGEND_GIT_BYPASS_AGENT` | per-agent bypass |
| `AGENTIC_GIT_BYPASS_UNTIL` | `AGEND_GIT_BYPASS_UNTIL` | time-boxed bypass (unix epoch) |
| `AGENTIC_GIT_SHIM_DEPTH` | `AGEND_GIT_SHIM_DEPTH` | recursion-guard sentinel (internal) |
| `AGENTIC_GIT_ALLOW_CANONICAL_MUTATE` | `AGEND_GIT_ALLOW_CANONICAL_MUTATE` | canonical-repo escape hatch |
| `AGENTIC_GIT_SNAPSHOTS` | `AGEND_GIT_SNAPSHOTS` | pre-destructive-op recovery snapshots (`=1` to enable; default off in raw shim mode, on by default inside `run` sessions; `=0`/`off` force-disables) |

On-disk contract (also unchanged from upstream): `runtime/<agent>/binding.json`
+ `.sig` · `.config-integrity-key` (32-byte HMAC key, 0600) ·
`fleet_events.jsonl` (append-only audit) · `policy.toml` (optional
protected-ref override, fail-closed) · `.agend-managed` worktree marker.

## Workspace layout

| Crate | What it is |
|---|---|
| `agentic-git` | the shim binary |
| `agentic-git-core` | the contract surface for **embedders**: `integrity_core` (HMAC sign/verify — link this in your daemon and signer/verifier can never drift) and `protected_refs` |

## Status & roadmap

Alpha. Battle-tested logic (the history in this repo is the battle), fresh
packaging.

**Recovery layer:** before a destructive op (`reset --hard`, `clean -f*`,
any worktree-overwriting `checkout`/`restore`, `switch -f`/`--discard-changes`,
`stash drop|clear`, `merge`/`rebase`/`pull`/`cherry-pick`/`revert`/`am`) runs
in a git work tree, the shim snapshots the tree into a private
`refs/agentic-git/snapshots/<who>/…` ref first (skipped when clean; fails open
+ loud, never blocks the op). The snapshot namespace is itself guarded against
being pushed. Recover in one command — `agentic-git snapshots restore` writes
the snapshot's files back to the working tree (with no ref, it uses the only
snapshot, or the newest with `--yes`). It is **non-destructive** (files created
after the snapshot are left in place, never deleted), lands the recovery
unstaged, and saves your current state first so the restore is itself undoable.
Inspect and prune with `agentic-git snapshots list|prune [--repo <path>]`. Off
by default in raw shim mode (`AGENTIC_GIT_SNAPSHOTS=1` to enable); on by default
inside `run` sessions.

Known rough edges:

- **Windows is unverified.** The `cfg(windows)` paths (copy-instead-of-symlink
  wiring, `status()`+exit process replacement, case-insensitive dispatch) have
  never run in the wild; CI exercises them as an *advisory* (non-blocking) job.
- `policy.toml` covers protected-ref overrides; snapshot TTL / destructive-op
  list are hardcoded defaults + flags — richer config is planned.

## License

Apache-2.0. See [NOTICE](NOTICE) for provenance.

*讀中文?* 見 [README.zh-TW.md](README.zh-TW.md)。
