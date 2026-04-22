# Discord as Default Feature

**Date:** 2026-04-22
**Branch:** `discord`

---

## Rationale

Telegram (teloxide) compiles unconditionally. Discord (serenity) requires `--features discord`, creating an asymmetry. Making `discord` a default feature aligns both adapters. Users who don't need Discord can opt out with `--no-default-features`.

## Changes

### 1. Cargo.toml

```toml
# Before
default = []

# After
default = ["discord"]
```

One line change. The `discord = ["dep:serenity"]` feature definition and `serenity` optional dep stay as-is.

### 2. CI Workflow (`.github/workflows/ci.yml`)

```yaml
# Before (lines 38, 47) — only enables tray, discord not tested by default
run: cargo clippy --all-targets --features tray -- -D warnings
run: cargo test --bin agend-terminal --features tray

# After — discord now included via default, just add tray
# No change needed — default features are always active unless --no-default-features
# The existing commands already compile with default features + tray
```

CI already uses default features implicitly. The `--features tray` flag adds tray on top. No CI change needed.

**Optional improvement:** Add a `--no-default-features` job to verify the discord-less build still compiles:

```yaml
- name: Build without discord
  run: cargo check --no-default-features
```

### 3. `#[cfg(feature = "discord")]` Guards

No change. All existing `#[cfg(feature = "discord")]` guards remain correct — `default = ["discord"]` means the feature is active by default, so the gated code compiles. `--no-default-features` disables it, and the guards correctly exclude Discord code.

### 4. README.md

```markdown
# Before (line 64)
cargo build --release --features discord

# After
cargo build --release
```

Remove `--features discord` from build instructions. Add a note:

```markdown
# Build without Discord support:
cargo build --release --no-default-features
```

### 5. Cargo.toml Comment

```toml
# Before (line 63)
# Discord — optional, behind `--features discord`

# After
# Discord — included by default; opt out with --no-default-features
```

## Files to Modify

| File | Change |
|------|--------|
| `Cargo.toml` | `default = []` → `default = ["discord"]`, update comment |
| `README.md` | Remove `--features discord` from build command, add opt-out note |
| `.github/workflows/ci.yml` | Optional: add `--no-default-features` check job |

## Not Changed

- All `#[cfg(feature = "discord")]` guards — unchanged
- `serenity` dependency declaration — unchanged
- `discord = ["dep:serenity"]` feature definition — unchanged
