# Skill System Architecture Report

**Author**: fixup-dev  
**Date**: 2026-05-23  
**Codebase**: `agend-terminal` @ `1598632` (post-#1081 merge)  
**Task**: t-20260522153551059657-0

---

## 1. Module Index

| File | Lines | Role |
|---|---|---|
| `src/skills.rs` | 1–649 (impl) + 650–1199 (tests) | Core lifecycle: add/remove/list/update, install_for_agent, stage filtering, GC |
| `src/main.rs` | 54, 312–318, 435–472, 1060–1065 | `mod skills;` declaration, CLI enum `SkillsAction`, CLI dispatch |
| `src/cli.rs` | 443–568 | CLI handlers: `run_skills_add/remove/list/update/install` |
| `src/fleet.rs` | 227–235 | `InstanceConfig.skills: Option<Vec<String>>` per-instance allowlist field |
| `src/daemon/mod.rs` | 395–409 | Daemon-init skills-stage GC sweep |
| `src/daemon/mod.rs` | 1374–1407 | Pre-spawn `install_for_agent()` hook in `spawn_and_register_agent()` |

---

## 2. Data Flow

### 2.1 CLI: `agend skills add <source>`

```
main.rs:1061   SkillsAction::Add { source }
  → cli.rs:444   run_skills_add(&home, &source)
    → skills.rs:166  add(home, source)
      1. classify_source(source)        → (name, SourceKind::Path | Git)
      2. Path: copy_dir_recursive(src, ~/.agend-terminal/skills/<name>/)
         Git:  git clone --depth=1 <url> ~/.agend-terminal/skills/<name>/
      3. compute_version(dest, source)  → SHA (git) or mtime epoch (path)
      4. SkillsLock::read → insert entry → SkillsLock::write (atomic)
```

### 2.2 Daemon: Agent Spawn → Skill Install

```
daemon/mod.rs:1342  spawn_and_register_agent(name, ...)
  │
  ├─ [1374–1389] Resolve fleet.yaml per-instance filter
  │   FleetConfig::load → instances[name].skills → Option<Vec<String>>
  │
  ├─ [1390] skills::install_for_agent(home, working_dir, filter)
  │   │
  │   ├─ filter=None  → source = ~/.agend-terminal/skills/ (all)
  │   ├─ filter=Some([...]) → stage_filtered_source() → .skills-stage/<digest>/
  │   └─ filter=Some([])   → stage_filtered_source() → empty stage dir
  │   │
  │   └─ for (backend, rel) in BACKEND_SKILL_DIRS:
  │       install_one(source, working_dir/<rel>, backend)
  │         1. create parent dirs
  │         2. if target exists:
  │            - is_symlink OR has .agend-skills-managed → replace
  │            - else → skip (non-managed, preserve operator's dir)
  │         3. try_symlink(source, target) → InstallMode::Symlink
  │         4. on fail: copy_with_marker(source, target) → InstallMode::Copy
  │         5. both fail → InstallMode::Skipped
  │
  └─ [1409] agent::spawn_agent(...)  ← skills guaranteed in place
```

### 2.3 Per-Backend Target Layout

```
<agent-working-dir>/
├── .claude/skills/    → symlink → ~/.agend-terminal/skills/  (or .skills-stage/<digest>/)
├── .codex/skills/     → symlink → ...
├── .gemini/skills/    → symlink → ...
├── .opencode/skills/  → symlink → ...
└── .kiro/skills/      → symlink → ...
```

Defined in `BACKEND_SKILL_DIRS` constant (`skills.rs:44–50`):
```rust
pub const BACKEND_SKILL_DIRS: &[(&str, &str)] = &[
    ("claude", ".claude/skills"),
    ("codex", ".codex/skills"),
    ("gemini", ".gemini/skills"),
    ("opencode", ".opencode/skills"),
    ("kiro", ".kiro/skills"),
];
```

---

## 3. Call Path: `install_for_agent` Invocation Sites

### Site 1: Daemon `spawn_and_register_agent()` — All Spawn Paths

**Location**: `daemon/mod.rs:1390`

```rust
crate::skills::install_for_agent(home, wd, skills_filter.as_deref())
```

This is the sole daemon-side call site. `spawn_and_register_agent()` is itself called from:

| Trigger | Description |
|---|---|
| **Cold boot** | Fleet init loop — all configured instances |
| **Crash respawn** | Respawn worker after `exit_code != 0` |
| **Stage 2 restart** | `handle_stage2_restart()` — clean session restart |
| **Fleet update** | Dynamic fleet.yaml change detection → spawn new agents |

**Key property**: Synchronous pre-spawn — `install_for_agent()` completes before `agent::spawn_agent()` is called (line 1409), guaranteeing SKILL.md files exist at first backend read.

**Failure mode**: Best-effort. Errors log `warn!` + continue; skill install failures never block agent boot.

### Site 2: CLI `agend skills install <working_dir>`

**Location**: `cli.rs:515`

```rust
crate::skills::install_for_agent(home, &wd, None)?
```

Manual operator command. Always passes `None` filter (installs all skills). Used for debugging or one-off agent directory setup.

---

## 4. `skills-lock.json` Schema

### Path
`<home>/skills-lock.json` (typically `~/.agend-terminal/skills-lock.json`)

### Struct Definition (`skills.rs:70–114`)

```rust
pub struct SkillsLock {
    pub skills: BTreeMap<String, SkillLockEntry>,
}

pub struct SkillLockEntry {
    pub source: String,       // "https://github.com/foo/bar.git" or "/abs/path/to/skill"
    pub version: String,      // Git: commit SHA; Path: mtime epoch seconds; Empty: unpinned
    pub installed_at: String,  // RFC 3339 timestamp
}
```

### Example

```json
{
  "skills": {
    "agend-skill-canary": {
      "source": "/Users/suzuke/Documents/Hack/agend-skills/agend-skill-canary",
      "version": "1747402800",
      "installed_at": "2026-05-16T10:00:00+00:00"
    },
    "fleet-review-skill": {
      "source": "https://github.com/example/fleet-review-skill.git",
      "version": "abc123def456789...",
      "installed_at": "2026-05-20T08:30:00+00:00"
    }
  }
}
```

### Version Pinning Strategy (`skills.rs:621–644`)

| Source Type | Pin Method | Value |
|---|---|---|
| Git (`http*`, `git@`, `ssh://`) | `git rev-parse HEAD` in cloned tree | Full SHA (40 hex chars) |
| Path (local directory) | `fs::metadata(dest).modified()` | Unix epoch seconds as string |
| Failure | Fallback | Empty string (recorded but unpinned) |

### Persistence
- **Read**: `SkillsLock::read(home)` — returns `Default` if file missing
- **Write**: `SkillsLock::write(home)` — atomic via `crate::store::atomic_write()` (crash-safe, no partial writes)

---

## 5. Per-Instance Filter — `fleet.yaml` `skills:` Field

### Schema (`fleet.rs:227–235`)

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub skills: Option<Vec<String>>,
```

### Fleet.yaml Usage

```yaml
instances:
  fixup-dev:
    # No skills field → None → install all (default)

  fixup-reviewer:
    skills:
      - agend-skill-canary
      - fleet-review-skill
    # Some(["agend-skill-canary", "fleet-review-skill"]) → allowlist

  mas-eval-runner:
    skills: []
    # Some([]) → opt-out, no skills installed
```

### Semantics

| Value | Behavior |
|---|---|
| **`None`** (omitted) | Install ALL skills from unified source. Default. |
| **`Some(["a", "b"])`** | Allowlist — only `a` and `b` installed. Others excluded. |
| **`Some([])`** | Empty allowlist — explicitly opt-out. Per-backend dirs get marker only. |

### Resolution at Spawn (`daemon/mod.rs:1386–1389`)

```rust
let skills_filter: Option<Vec<String>> =
    crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|c| c.instances.get(name).and_then(|i| i.skills.clone()));
```

The resolved `Option<Vec<String>>` is passed as `filter` to `install_for_agent()`.

---

## 6. Stage Directory — `.skills-stage/` Filtered Staging

### Purpose
When a per-instance allowlist is active (`filter=Some([...])`), the daemon cannot symlink the entire unified source (that would expose all skills). Instead, it builds a **filtered stage directory** containing only the allowlisted skills, then symlinks per-backend dirs to the stage.

### Path Convention
```
<home>/.skills-stage/<digest>/
```

Where `<digest>` = first 16 hex chars (8 bytes) of SHA-256 over sorted newline-joined allowlist names.

### Stage Function (`skills.rs:316–342`)

```rust
fn stage_filtered_source(home: &Path, source: &Path, allowlist: &[String]) -> Result<PathBuf>
```

1. Sort allowlist alphabetically
2. Join with `\n` → compute SHA-256 → take first 8 bytes → hex-encode (16 chars)
3. Create `<home>/.skills-stage/<digest>/`
4. For each allowlisted name: `copy_dir_recursive(source/<name>, stage/<name>)`
5. Missing allowlisted skills: `warn!` + skip (non-fatal)

### Digest Function (`skills.rs:430–434`)

```rust
fn stage_digest(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let full = Sha256::digest(data);
    hex::encode(&full[..8])  // 64-bit collision resistance
}
```

### Properties
- **Deterministic**: Same allowlist always produces same digest → stage dirs reuse
- **Rebuilt each call**: Stage dir is wiped + rebuilt (idempotent, cheap)
- **64-bit collision resistance**: Birthday bound at ~2^32 distinct allowlists — unreachable at AgEnD's scale

### GC: `cleanup_stale_stages()` (`skills.rs:370–421`)

```rust
pub fn cleanup_stale_stages(
    home: &Path,
    retention_secs: u64,
    exclude_digests: &[String],
) -> Result<StageGcReport>
```

- **Invoked at**: Daemon init (`daemon/mod.rs:404`), retention = 7 days
- **Eligible**: Stage dirs with mtime older than `retention_secs`
- **Excluded**: Dirs named in `exclude_digests` (TOCTOU safety for concurrent install)
- **Fail-open**: Individual removal errors log + continue
- **Report struct**: `{ candidates, deleted, preserved_recent, preserved_excluded }`

### GC Report (`skills.rs:346–352`)

```rust
pub struct StageGcReport {
    pub candidates: usize,
    pub deleted: usize,
    pub preserved_recent: usize,
    pub preserved_excluded: usize,
}
```

---

## 7. Windows Fallback — Symlink vs Copy + Marker

### Platform-Specific Symlink (`skills.rs:530–538`)

```rust
#[cfg(unix)]
fn try_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(windows)]
fn try_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(source, target)
}
```

### Fallback Logic (`skills.rs:506–527`)

```
1. try_symlink(source, target)  → success: InstallMode::Symlink
2. symlink failed → copy_with_marker(source, target) → success: InstallMode::Copy
3. both failed → InstallMode::Skipped + error reason
```

Windows typically fails symlink (requires developer mode or elevated privileges), triggering copy fallback.

### Copy + Marker (`skills.rs:540–544`)

```rust
fn copy_with_marker(source: &Path, target: &Path) -> Result<()> {
    copy_dir_recursive(source, target)?;
    std::fs::write(target.join(".agend-skills-managed"), b"daemon-managed\n")?;
    Ok(())
}
```

### `.agend-skills-managed` Marker File

- **Content**: `"daemon-managed\n"`
- **Purpose**: Distinguish daemon-installed copies from operator hand-crafted skill dirs
- **Detection** (`skills.rs:481`):
  ```rust
  let is_managed_copy = target.join(".agend-skills-managed").exists();
  ```

### Pre-existing Directory Handling (`skills.rs:476–504`)

| Existing Dir State | Action |
|---|---|
| Is symlink | Remove + reinstall |
| Has `.agend-skills-managed` | Remove + reinstall |
| Neither (operator's dir) | **Skip** — never clobber |

Symlink removal is cross-platform:
```rust
let _ = std::fs::remove_file(target)      // Unix: symlink is a file
    .or_else(|_| std::fs::remove_dir(target));  // Windows: directory symlink
```

---

## 8. Known Limitations

### L1: Windows file-watch staleness detection not implemented
- **Source**: `skills.rs:22–25` (module doc, Sprint 60 P2-C deferral)
- **Impact**: Windows copy-mode skills are not auto-updated when unified source changes. Operator must manually run `agend skills update <name>`.
- **Unix**: Not affected — symlinks resolve live.

### L2: Symlinks inside skill sources are not duplicated
- **Source**: `skills.rs:562–564`
- **Impact**: `copy_dir_recursive()` copies only regular files and directories. Symlinks within a skill source are silently skipped.
- **Workaround**: Operators should flatten symlinks in skill sources before `add`.

### L3: Version pinning is opaque, not used for drift detection
- **Source**: `skills.rs:31–33` (module doc)
- **Impact**: `version` field records SHA or mtime but is currently write-only — no tooling compares it to detect drift. Future optimization path identified.

### L4: CLI `skills install` does not support per-instance filtering
- **Source**: `cli.rs:515` — passes `None` filter
- **Impact**: `agend skills install <wd>` always installs ALL skills. Per-instance filtering is daemon-only (reads fleet.yaml). CLI operator cannot simulate filtered install.

### L5: No periodic skills-stage GC
- **Source**: `daemon/mod.rs:399–401` (comment)
- **Impact**: GC only runs at daemon-init. Long-running daemons with frequent fleet.yaml skill filter changes may accumulate stale stage dirs until next restart. 7-day retention mitigates unbounded growth.

### L6: `git clone --depth=1` for git skills does not preserve tags
- **Source**: `skills.rs:192–199`
- **Impact**: Shallow clone. Version pin is HEAD SHA. Tag-based version tracking not supported.

---

*— fixup-dev (empirical from codebase @ 1598632; 6 files, 2 call sites, 6 known limitations)*
