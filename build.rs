// Build provenance resolution (slice α) — logic lives in build/provenance.rs so
// tests/build_provenance_version.rs can cover it via a #[path] module.
include!("build/provenance.rs");

fn main() {
    #[cfg(windows)]
    {
        // Embed a Windows application manifest declaring Win10/11 support and
        // UTF-8 as the active code page. Without this, Windows treats the
        // binary as a legacy app and applies ConPTY/console compatibility
        // shims — which on Insider Dev builds (>=26200) silently break child
        // output from `CreatePseudoConsole`. WezTerm and conhost-derived tools
        // ship an equivalent manifest. See docs/archived/HANDOVER-windows-conpty-nested.md.
        println!("cargo:rerun-if-changed=assets/windows/agend-terminal.rc");
        println!("cargo:rerun-if-changed=assets/windows/agend-terminal.manifest");
        let _ = embed_resource::compile("assets/windows/agend-terminal.rc", embed_resource::NONE)
            .manifest_optional();
    }
    emit_build_identity();
}

/// Slice α (d-20260712073535548541-12): stamp the build identity into four
/// compile-time envs — `AGEND_BUILD_SHA` (full lowercase sha | `unknown`),
/// `AGEND_BUILD_DIRTY` (`0`|`1`), and the two ready-made surface strings
/// `AGEND_CLI_VERSION` / `AGEND_MCP_VERSION` — so every bin in the package
/// (main CLI, MCP bridge) reads a single canonical composition with zero
/// runtime plumbing. Identity hierarchy: `AGEND_BUILD_COMMIT` env (explicit
/// CI/release/source-archive pin, optional `AGEND_BUILD_DIRTY` env) >
/// worktree-safe git probe > honest `unknown`. Never fails the build.
fn emit_build_identity() {
    println!("cargo:rerun-if-env-changed=AGEND_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=AGEND_BUILD_DIRTY");
    let manifest_dir =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let env_commit = std::env::var("AGEND_BUILD_COMMIT").ok();
    let env_dirty = std::env::var("AGEND_BUILD_DIRTY").ok();
    let id = resolve_identity(env_commit.as_deref(), env_dirty.as_deref(), || {
        git_probe(&manifest_dir)
    });
    for w in &id.warnings {
        println!("cargo:warning={w}");
    }
    // Re-stamp on repo movement (commit / branch switch / ref update). Emitted
    // whenever the tree is a git repo — existing paths only (rerun_paths), since
    // a registered-but-missing path makes cargo re-run on every build.
    for p in rerun_paths(&manifest_dir) {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    let semver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    println!("cargo:rustc-env=AGEND_BUILD_SHA={}", id.sha);
    println!(
        "cargo:rustc-env=AGEND_BUILD_DIRTY={}",
        if id.dirty { "1" } else { "0" }
    );
    println!(
        "cargo:rustc-env=AGEND_CLI_VERSION={}",
        compose_cli_version(&semver, &id.sha, id.dirty)
    );
    println!(
        "cargo:rustc-env=AGEND_MCP_VERSION={}",
        compose_mcp_version(&semver, &id.sha, id.dirty)
    );
}
