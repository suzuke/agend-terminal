//! #2524 P3 (agentic-git migration): agend-terminal builds the in-tree
//! `agentic-git` shim as its OWN `[[bin]]` target so a plain `cargo build`
//! produces the sibling binary — retiring `scripts/build_agentic_git_shim.sh`
//! and making the shim ride `cargo install` / release archives automatically.
//!
//! ## Why this invariant exists
//! Two structural facts must never silently regress:
//!  1. The `[[bin]]` entry itself. Drop it and `cargo build` stops producing
//!     `agentic-git`; `symlink_shim` (src/binding/shim_install.rs) then finds no
//!     sibling and the flag-gated git guard silently falls back to agend-git — the
//!     distribution gap the P3 build-together was meant to close reopens.
//!  2. `test = false`. The binary compiles on every target (unix code is
//!     `#[cfg(unix)]`-gated, with cfg(windows) paths), but its inline
//!     `#[cfg(test)] mod tests` fake argv[0] via unix-only `CommandExt::arg0` /
//!     `std::os::unix` and CANNOT compile on Windows (the vendored crate's own CI
//!     never test-compiles them off-unix). With `test = false` those tests stay
//!     out of agend-terminal's surface (they already run in agentic-git's CI), so
//!     `cargo nextest` on Windows never tries to compile them. Remove the flag and
//!     the 3-OS CI breaks on Windows — this test catches that BEFORE CI does.
//!
//! Parses the committed `Cargo.toml` with string ops (no `toml` dep — it is only
//! an optional `tray`-feature dependency here), matching the dependency-free idiom
//! of `agentic_core_single_source.rs`.

use std::path::Path;

const BIN_NAME: &str = "agentic-git";
const BIN_PATH: &str = "src/bin/agentic-git.rs";

/// The body of the `[[bin]]` table whose `name = "<BIN_NAME>"`, or `None` if no
/// such target is declared. A `[[bin]]` table runs until the next top-level `[`.
fn agentic_git_bin_block(manifest: &str) -> Option<String> {
    manifest
        .split("[[bin]]")
        .skip(1) // drop everything before the first [[bin]]
        .map(|after| {
            // A target table ends at the next top-level table header (`\n[`).
            match after.find("\n[") {
                Some(end) => &after[..end],
                None => after,
            }
        })
        .find(|block| {
            block
                .lines()
                .any(|l| l.trim() == format!(r#"name = "{BIN_NAME}""#))
        })
        .map(str::to_owned)
}

/// True if `block` carries a `key = value` line (exact trim match on either the
/// bare form or a `value` that may itself be quoted).
fn has_kv(block: &str, key: &str, value: &str) -> bool {
    block
        .lines()
        .any(|l| l.trim() == format!("{key} = {value}"))
}

#[test]
fn agentic_git_bin_target_is_declared_with_windows_safe_test_false() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));

    let block = agentic_git_bin_block(&manifest).unwrap_or_else(|| {
        panic!(
            "P3: no `[[bin]] name = \"{BIN_NAME}\"` target in Cargo.toml — the sibling \
             binary is no longer built by `cargo build`; symlink_shim would find no \
             agentic-git sibling and the distribution/script gap reopens."
        )
    });

    assert!(
        has_kv(&block, "path", &format!("\"{BIN_PATH}\"")),
        "P3: the agentic-git `[[bin]]` must compile the in-tree crate source \
         (path = \"{BIN_PATH}\"); found block:\n{block}"
    );

    assert!(
        has_kv(&block, "test", "false"),
        "P3 LOAD-BEARING: the agentic-git `[[bin]]` must set `test = false` — its \
         inline unix-only `#[cfg(test)] mod tests` cannot compile on Windows, so \
         without this `cargo nextest` on Windows breaks. Those tests run in \
         agentic-git's own CI. Found block:\n{block}"
    );
    assert!(
        !BIN_PATH.starts_with("vendor/"),
        "the parent Cargo target must never point beneath vendor"
    );
}

/// Regression-proof (RED-first): the parser + guard MUST fire when the `[[bin]]`
/// is absent or missing `test = false`. Feeds the SAME `agentic_git_bin_block`
/// entry point hand-authored manifests and asserts the exact guards the green
/// test relies on would fail — no mock-only code path.
#[test]
fn checker_fires_on_missing_target_or_test_flag() {
    // (a) No agentic-git [[bin]] at all → block is None.
    const NO_BIN: &str = "[package]\nname = \"agend-terminal\"\n\n[dependencies]\nserde = \"1\"\n";
    assert!(
        agentic_git_bin_block(NO_BIN).is_none(),
        "a manifest without the agentic-git [[bin]] must yield None"
    );

    // (b) [[bin]] present but WITHOUT `test = false` → block found, but the
    // `test = false` guard fails (this is the Windows-break a silent edit causes).
    const NO_TEST_FALSE: &str = "[package]\nname = \"agend-terminal\"\n\n\
         [[bin]]\nname = \"agentic-git\"\npath = \"vendor/agentic-git/crates/agentic-git/src/main.rs\"\n\n\
         [dependencies]\nserde = \"1\"\n";
    let block = agentic_git_bin_block(NO_TEST_FALSE)
        .expect("the [[bin]] block must be found even without test=false");
    assert!(
        has_kv(
            &block,
            "path",
            "\"vendor/agentic-git/crates/agentic-git/src/main.rs\""
        ),
        "path guard must still recognize the legacy path in this negative fixture"
    );
    assert!(
        !has_kv(&block, "test", "false"),
        "the Windows-safety guard MUST fire (be absent) when test=false is missing"
    );

    // (c) A different [[bin]] must not be mistaken for the agentic-git one.
    const OTHER_BIN: &str = "[package]\nname = \"agend-terminal\"\n\n\
         [[bin]]\nname = \"agend-mcp-bridge\"\npath = \"src/bin/bridge.rs\"\n";
    assert!(
        agentic_git_bin_block(OTHER_BIN).is_none(),
        "an unrelated [[bin]] must not match the agentic-git name"
    );
}
