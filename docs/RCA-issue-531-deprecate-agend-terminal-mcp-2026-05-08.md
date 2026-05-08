# RCA — issue #531: deprecate `agend-terminal mcp` in favor of `agend-mcp-bridge`

**Date:** 2026-05-08
**Author:** dev (Sprint 56 Track I-Phase1, Path B doc-only)
**Issue:** [#531](https://github.com/suzuke/agend-terminal/issues/531) — *Windows: reply MCP still fails in v0.6.0 — daemon overwrites mcp.json with `agend-terminal mcp` instead of `agend-mcp-bridge`*
**Reporter:** changhansung (Windows 11, agend-terminal v0.6.0, kiro-cli backend)
**Verdict:** **REMOVE-WITH-MIGRATION** — `agend-terminal mcp` is not safe to remove outright (hand-edited operator configs + widespread doc references), and not safe to leave as-is (release artifacts don't ship the bridge binary, so the fallback at `mcp_config.rs:34` permanently lands operators on the broken path). Recommended Phase 2 IMPL: a 2-step migration that ships the bridge first, then deprecates the local-mode fallback.

This document covers the four audit dimensions lead requested in the dispatch (m-20260508125646716650-189): code references, binary distribution, migration impact, and test coverage / invariant gap.

---

## 1 — Code reference grep

### `Commands::Mcp` enum surface

`src/main.rs:593-599`

```rust
Some(Commands::Mcp) => {
    let instance_name = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
    if instance_name.is_empty() {
        tracing::warn!("AGEND_INSTANCE_NAME not set, running in standalone mode");
    }
    mcp::run()?;
}
```

Single CLI arm. Strips down to a thin wrapper around `mcp::run()` in `src/mcp/mod.rs:146`.

### `mcp::run` body

`src/mcp/mod.rs:146-247` runs an NDJSON-over-stdio JSON-RPC loop. For each `tools/call` it dispatches through `proxy_or_local(tool, args, instance_name)` (line 222).

`proxy_or_local` at `src/mcp/mod.rs:327-396` is **NOT pure-local-mode** despite the issue body's framing:

- If running inside the daemon process → direct `handle_tool` (line 330-332). Not the relevant arm here.
- Else: call daemon API via `crate::api::call(home, "mcp_tool", …)` (line 343-353).
- On daemon proxy success → return daemon's response.
- On daemon `ok=false` OR connect-fail → log a warn + fall back to local `handle_tool` UNLESS the tool requires daemon state (line 374-375 + 387-388).
- `requires_daemon_state` lists `reply` / `react` / `download_attachment` (Sprint 54 PR #488 hotfix; pinned by `requires_daemon_state_tags_channel_tools` test). For these tools the proxy returns a structured `daemon_state_unreachable_error` instead of falling back.

### Bridge fallback at `mcp_config.rs:26-35`

```rust
fn bridge_binary_path() -> (String, Vec<&'static str>) {
    if let Ok(exe) = std::env::current_exe() {
        let bridge = exe.with_file_name("agend-mcp-bridge");
        if bridge.exists() {
            return (bridge.display().to_string(), vec![]);
        }
    }
    // Fallback: use main binary with --mcp flag (pre-Option-F behaviour)
    (binary_path(), vec!["mcp"])
}
```

Daemon writes the same struct into every backend's mcp.json (Claude / codex / kiro / gemini / opencode). The fallback is the issue's load-bearing failure mode: when the bridge binary is not next to `agend-terminal`, the daemon writes `command: agend-terminal, args: [mcp]` — which is exactly what the reporter observed.

### `agend-terminal mcp` literal references

`grep -rn "agend-terminal mcp" src/ docs/ scripts/`:

- `docs/USAGE.md:69-72` — operator-facing doc shows the literal command.
- `docs/CLI.md:95` — CLI reference shows the same.
- `docs/MCP-TOOLS.md:272` — operator-facing description points at `agend-terminal mcp` as the spawned subprocess.
- `docs/archived/MCP-CHANNEL-OPS-BRIDGE-INVESTIGATION.md` — Sprint 54 investigation doc (archived) describes the failure mode this RCA re-confirms.
- No `src/` references outside the `Commands::Mcp` arm + the `mcp_config.rs` fallback.

### Conclusion (§1)

`agend-terminal mcp` is a thin CLI surface (1 arm + stdio JSON-RPC loop) but the proxy-or-local fallback in `mcp::run` IS the design — the issue body's "runs in local mode" framing is technically inaccurate. The actual fault is that for daemon-state-required tools (`reply` / `react` / `download_attachment`) the proxy MUST succeed, and on Windows the proxy attempt against the running daemon's TCP API is what fails or the daemon's response carries `ok=false`. The shipped binary does not include `agend-mcp-bridge` (next dimension), so the fallback path bakes the broken `agend-terminal mcp` config into every backend's mcp.json on every daemon start, surviving any hand-edit.

---

## 2 — Binary distribution

### Cargo.toml `[[bin]]` declarations

`cargo metadata --no-deps` reports three bin targets in the `agend-terminal` package:

- `agend-terminal` (main binary, `src/main.rs`)
- `agend-mcp-bridge` (`src/bin/agend-mcp-bridge.rs`)
- `agend-git` (`src/bin/agend-git.rs`)

`cargo build --release` builds **all three** by default. So a from-source install (`cargo install --path .`) places the bridge alongside the main binary; the fallback at `mcp_config.rs` would prefer the bridge.

### Release-artifact contents

`.github/workflows/release.yml:77-96` packages the release tarballs/zips:

```yaml
- name: Package (Unix)
  if: matrix.archive == 'tar.gz'
  run: |
    cd target/${{ matrix.target }}/release
    tar czf ../../../agend-terminal-${{ matrix.target }}.tar.gz agend-terminal

- name: Package (Windows)
  if: matrix.archive == 'zip'
  shell: pwsh
  run: |
    Compress-Archive `
      -Path target/${{ matrix.target }}/release/agend-terminal.exe `
      -DestinationPath agend-terminal-${{ matrix.target }}.zip
```

**Both arms only include the main binary.** The AppImage build at line 124-139 follows suit (`cp target/release/agend-terminal AppDir/usr/bin/agend-terminal`). Across all 5 release platforms (Linux x86_64, Linux aarch64, macOS x86_64, macOS aarch64, Windows), the published artifact contains exactly one binary: `agend-terminal`.

### Practical impact on the reporter's setup

The reporter installed v0.6.0 on Windows 11, presumably via the GitHub release zip. Their `agend-terminal.exe` shipped solo in `<install-dir>`; the bridge binary was never copied there. `bridge_binary_path()` checks `exe.with_file_name("agend-mcp-bridge")` (line 27-28) — that path doesn't exist, so the fallback at line 34 fires unconditionally and the daemon writes `agend-terminal mcp` into every mcp.json.

The bridge itself works fine on Windows (the reporter verified manually after copying `agend-mcp-bridge.exe` next to the main binary and editing mcp.json). The bug is purely a packaging gap.

### Conclusion (§2)

The bridge is built but not shipped. This is the *single* most load-bearing finding in this RCA — fixing the packaging would resolve the reporter's symptom even without removing `agend-terminal mcp`. Any Phase 2 IMPL plan that doesn't ship the bridge first would be defending the wrong cause.

---

## 3 — Migration impact

### Operator-facing config files

- `mcp_config.rs::mcp_server_entry` (line 43-56) returns the standard MCP server stanza for **all** backends. Daemon calls this from `create_instance` and on every `agend-terminal start` to upsert mcp.json. The fallback's `agend-terminal mcp` form is the durable on-disk record an operator's hand-edits would compete against — daemon's atomic-write flow at `mcp_config.rs:upsert_mcp_servers` (line 77+) clobbers any manual changes on the next start.
- Backend-side templates: `src/backend.rs:426-438` adds `--mcp-config <path>` flags only for Claude Code; other backends rely on their own config file path (per the comment at line 426-428). No backend has a hardcoded `agend-terminal mcp` literal — they all read whatever the daemon's mcp_config.rs wrote.
- `mcp-config.json` `[mcp]` arg occurrences: zero hits in `src/`. The literal lives only in (a) the runtime-generated mcp.json files in operator working dirs (which we don't track) and (b) docs.

### Documentation references

`docs/USAGE.md:69`, `docs/CLI.md:95`, `docs/MCP-TOOLS.md:272` — three operator-facing docs all reference `agend-terminal mcp` as the canonical subprocess command. Removing the CLI arm without updating these would leave operators hitting "command 'mcp' not found" with no migration path.

### Hand-edit risk

The reporter's workaround was: "Manually editing mcp.json to use agend-mcp-bridge works, but daemon overwrites it on every restart." Other Windows operators who applied the same workaround would have *exactly one form* of `agend-mcp-bridge.exe` in their on-disk mcp.json — easy to forward-migrate. But operators on macOS / Linux with `agend-terminal mcp` (the daemon-default) and no manual edit are the silent majority; their migration path is "next daemon start writes `agend-mcp-bridge`" — IF the bridge is present.

### Conclusion (§3)

Migration is **not zero-cost**:

- Daemon's mcp_config.rs is single source of truth for the on-disk mcp.json contents — straightforward to flip from "bridge-or-fallback" to "bridge-only" once the bridge is shipped.
- Three doc files need a coordinated update.
- No backend templates or script files hardcode `agend-terminal mcp`, so no per-backend porting.
- Removing the `Commands::Mcp` CLI arm is a behavioral break for operators who scripted automation around `agend-terminal mcp` (rare, but possible — `docs/CLI.md` lists it as a stable command).

---

## 4 — Test coverage gap + invariant test plan

### Existing MCP-related test files

`tests/`:
- `mcp_bridge_client_handshake.rs` — bridge protocol handshake + framing.
- `mcp_bridge_idle_reconnect.rs` — bridge-side reconnect logic.
- `mcp_proxy_behavioral_parity.rs` / `mcp_proxy_lifecycle.rs` / `mcp_proxy_parity.rs` — bridge-vs-daemon parity.
- `mcp_subprocess_is_zero_state.rs` — pins that the MCP subprocess starts with no fleet state.

Phase 2c (#531) note: `tests/mcp_characterization.rs`, `tests/mcp_roundtrip.rs`, and `tests/pane_snapshot_wire_format.rs` were deleted alongside the `agend-terminal mcp` subcommand and `proxy_or_local` helper. They pinned behavior that no longer exists. Validation invariants moved to handler-level unit tests under `src/mcp/handlers/`; the wire path is now pinned by the bridge tests above + `tests/no_local_mcp_mode_invariant.rs::bridge_emits_daemon_error_when_daemon_down`.

The test suite covers bridge↔daemon parity comprehensively. What is **NOT covered**: the on-disk `mcp_config.rs` output never has `agend-terminal mcp` in production builds. There's no invariant test that catches the packaging gap.

### Proposed invariant test (Phase 2)

`tests/no_local_mcp_mode_invariant.rs` (sketch):

```rust
//! Invariant: production builds must ship `agend-mcp-bridge` next to
//! `agend-terminal`. The mcp_config fallback path
//! (`mcp_config.rs:bridge_binary_path` returning `("agend-terminal",
//! ["mcp"])`) is the load-bearing failure mode in issue #531; this test
//! catches packaging regressions before they reach release artifacts.

#[test]
fn release_target_dir_contains_agend_mcp_bridge() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let release = std::path::Path::new(manifest_dir).join("target/release");
    if !release.exists() {
        eprintln!("skip: target/release not built (run `cargo build --release` first)");
        return;
    }
    let bin_name = if cfg!(windows) { "agend-mcp-bridge.exe" } else { "agend-mcp-bridge" };
    let bridge = release.join(bin_name);
    assert!(
        bridge.exists(),
        "agend-mcp-bridge not built in target/release — packaging would ship a broken default"
    );
}
```

Also recommended: a `release_yaml_packages_bridge_binary` test that parses `.github/workflows/release.yml` and asserts both binaries appear in the tar/zip steps.

### Conclusion (§4)

The functional gap (bridge proxy works, local fallback doesn't reach channel state) is well-covered by the existing `requires_daemon_state` + `proxy_or_local` test suite. The packaging gap (bridge built but not shipped) has zero test coverage — this is the regression-proof anchor a Phase 2 PR would establish.

---

## Verdict

**REMOVE-WITH-MIGRATION.** A clean removal of `agend-terminal mcp` is not safe today (operators with hand-edited mcp.json + 3 doc references + backwards-compat for scripted automation), but leaving the fallback in place permanently lands new Windows installs on the broken path. The migration must ship in two ordered steps:

### Phase 2a — Packaging + invariant (LOW RISK, ~30-50 LOC + workflow change)

1. **Update `.github/workflows/release.yml`** to package both binaries (and `agend-git` if dispatch confirms — out of #531 scope, but symmetric):
   - Unix tar.gz step adds `agend-mcp-bridge`
   - Windows zip step adds `agend-mcp-bridge.exe`
   - AppImage step copies bridge into `AppDir/usr/bin/`
2. **Add `tests/no_local_mcp_mode_invariant.rs`** per §4 sketch.
3. **No code change to `mcp_config.rs`** — the existing `bridge_binary_path()` fallback continues to work; with the bridge now shipped, it'll find and prefer the bridge automatically. Existing operator installs that upgrade get the bridge for free on next package extract.

This phase alone resolves #531's reported symptom for new installs and post-upgrade operators.

### Phase 2b — Deprecation (HIGHER RISK, ~30 LOC + doc update)

> **Historical note (added Sprint 57 Wave 1 Track B)**: the "remove in Sprint 57+" timeline below was superseded mid-Sprint-56 by an operator escalation directive (m-20260508141217922488-238) collapsing the deprecation cycle into a single hard-removal at Phase 2c. Phase 2b shipped at PR #544 (commit `cbdac10`) with the loud-FATAL change in step 1 and the doc deprecation notes in step 2; Phase 2c at PR #547 (commit `8725118` on main) hard-removed `Commands::Mcp` instead of waiting for Sprint 57. The Phase 2b plan as written below was followed for steps 1 and 2 only; step 3's "one Sprint" buffer was zeroed.

Conditional on Phase 2a landing first and no operator escalation surfacing:

1. **`mcp_config.rs::bridge_binary_path`**: drop the `agend-terminal mcp` fallback and emit `tracing::error!("FATAL: agend-mcp-bridge missing — install agend-terminal v0.6.X+ which ships both binaries")`. This makes the failure loud rather than silent.
2. **Update `docs/USAGE.md` / `docs/CLI.md` / `docs/MCP-TOOLS.md`** to recommend `agend-mcp-bridge` and mark `agend-terminal mcp` as deprecated (one Sprint deprecation window before removal).
3. ~~**Keep `Commands::Mcp` for one Sprint** with a deprecation log line at startup; remove in Sprint 57+.~~ *Superseded by Phase 2c hard-removal — see historical note above.*

### Out of scope for Phase 2

- Removing `mcp::run` or `proxy_or_local` infrastructure (the bridge re-uses pieces of this — investigate before purging).
- Migrating operators with hand-edited mcp.json (best-effort: daemon's atomic upsert clobbers their edits anyway, but Phase 2b's deprecation window gives them visibility).

### Why not SAFE-TO-REMOVE

- Release-bundle gap means today's Windows operators upgrading to v0.6.0 don't get the bridge. A SAFE-TO-REMOVE path would assume the bridge is already universally available — that's the *opposite* of the reporter's reality.

### Why not CANNOT-REMOVE

- The functional separation (bridge for proxy, daemon for state) is well-tested and clean. The blockers are packaging + docs, both straightforward.
- No operator-facing API contract that promises `agend-terminal mcp` will exist forever — `docs/CLI.md` lists it but a deprecation cycle is the standard mitigation.
